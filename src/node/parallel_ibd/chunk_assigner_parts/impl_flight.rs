impl ChunkAssigner {
    fn top_peer_in_flight_cap() -> usize {
        // Shared latch with `sole_tip_workers_per_peer` (mod.rs).
        super::top_peer_in_flight_cap()
    }

    /// A4: top-scoring half of peers may hold two chunks; others one.
    /// A6l: preferred sticky tip owner always gets the top-peer cap — tip downloaders
    /// floor at ~0.1 while idle ready workers sit ~0.19, so sticky falls below median,
    /// gets `max_in_flight=1`, cannot re-arm the next tip span while finishing the current
    /// chunk, and a higher-scored idle peer steals ownership (live A6k: 103.228→146.190
    /// at chunk boundary while sticky still streaming; sticky tenure BPS already only ~4.4).
    fn max_in_flight_for(&self, peer_id: &str) -> usize {
        // Synth bulk local-disk: single peer falls into scores.len()<4 → cap 1, which
        // serializes tip-owner complete→reassign. Cap 2 overlaps tip+next without the
        // 4-way ahead flood that buried tip in the FIFO (live collapse ~450→~8 @332k).
        if super::synthetic_wan::bulk_local_disk_stream()
            && super::download::is_local_disk_peer(peer_id)
        {
            return std::env::var("BLVM_IBD_SYNTH_LOCAL_IN_FLIGHT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(4)
                .clamp(2, 4);
        }
        if self.preferred_tip_owner().as_deref() == Some(peer_id) && self.tip_sticky_usable(peer_id)
        {
            // Mode T tip-priority: sticky max_in_flight from TOP_PEER_IN_FLIGHT (harness=1).
            // in_flight=2 flooded archive (tc172). Dual parked (tc168/171).
            // A2 attempt1 2026-08-09: sole-ready always-2 REVERT (tc220a2overlap tip90≈40
            // tip_gd@404≈1648 — flood; P0 baseline tip90≈59 with tip_gd@404 KEEP).
            // A2 v2 2026-08-09: in-place tip span extend REVERT (tc220a2v2cover tip90≈45.6
            // tip30≈126.7 < P0; gd_max=5144 — mega-span flood). Next: A1/A3 not cover-grow.
            if super::sole_tip_priority_enabled() {
                return Self::top_peer_in_flight_cap().clamp(1, 2);
            }
            return Self::top_peer_in_flight_cap();
        }
        let scores = self.peer_scores.lock().unwrap();
        Self::max_in_flight_for_scores(&scores, peer_id)
    }

    fn max_in_flight_for_scores(scores: &HashMap<String, f64>, peer_id: &str) -> usize {
        if scores.len() < 4 {
            return 1;
        }
        let my = scores.get(peer_id).copied().unwrap_or(0.0);
        let mut vals: Vec<f64> = scores.values().copied().collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = vals[vals.len() / 2];
        if my >= median {
            Self::top_peer_in_flight_cap()
        } else {
            1
        }
    }

    /// Rate-limited diag: sticky at flight cap cannot re-arm yet.
    fn log_sticky_cap_block(peer_id: &str, flight: usize, cap: usize) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 2_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_STICKY_CAP] sticky={} flight={}/{} — cannot re-arm tip yet",
            peer_id,
            flight,
            cap
        );
    }

    /// Rate-limited diag: non-sticky caller blocked while sticky finishes current span.
    fn log_sticky_busy_hold(sticky: &str, caller: &str) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 2_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_STICKY_HOLD] sticky={} at capacity — denying tip steal by {}",
            sticky,
            caller
        );
    }

    /// Whether `peer_id` currently has capacity for another in-flight range.
    fn peer_has_flight_capacity(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if self.is_peer_blacklisted(peer_id) {
            return false;
        }
        let flight = Self::peer_flight_count(in_flight, peer_id);
        flight < self.max_in_flight_for(peer_id)
    }

    /// W28c: tip ownership is sticky + best-scored, not first worker to call `get_work`.
    ///
    /// Live W28b: tip owner rotated across ~14 peers; slow peers kept winning the lottery.
    /// W33a: highest-scored non-blacklisted peer (WAN tip owner candidate).
    fn top_scored_peer_id(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        let wan = self.wan_tip_gap_crawl(self.next_needed_height());
        scores
            .iter()
            .filter(|(p, _)| !self.is_peer_blacklisted(p))
            .filter(|(p, _)| !self.tip_owner_in_fail_cooldown(p))
            .filter(|(p, _)| !wan || self.peer_is_ibd_ready(p))
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// Highest-scored ready ACTIVE download worker (workers). Used for open-slot
    /// re-arm so idle scored peers without get_work callers cannot win tip.
    fn top_scored_active_ready_worker(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, _)| self.is_active_download_worker(p))
            .filter(|(p, _)| !self.is_peer_blacklisted(p))
            .filter(|(p, _)| !self.tip_owner_in_fail_cooldown(p))
            .filter(|(p, _)| self.peer_is_ibd_ready(p))
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// E12/E13 OPEN_SLOT fallback: walk active download workers (not only `peer_scores`).
    /// Live E12: `top_scored` returned None while other ready workers existed → preferred=None
    /// lottery re-elected the GD_SLOW sticky after blacklist.
    fn any_ready_active_worker_except(&self, exclude: &str) -> Option<String> {
        let mut best: Option<(String, f64)> = None;
        for p in self.active_download_worker_ids() {
            if p == exclude
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
                || !self.peer_is_ibd_ready(&p)
            {
                continue;
            }
            let s = self.peer_score_of(&p);
            if best.as_ref().map(|(_, bs)| s > *bs).unwrap_or(true) {
                best = Some((p, s));
            }
        }
        best.map(|(p, _)| p)
    }

    /// True when peer's in-flight ranges are all strictly ahead of tip (no tip cover).
    /// Must not lock `in_flight_per_peer` — callers may already hold it (`get_work`).
    fn peer_inflight_ahead_only_map(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        tip: u64,
    ) -> bool {
        let Some(ranges) = in_flight.get(peer_id) else {
            return false;
        };
        !ranges.is_empty() && ranges.iter().all(|(s, _)| *s > tip)
    }

    /// W126: covering=0 TIP_PIN candidate — prefer idle tip-STREAM, then idle top score.
    /// Live W125 @326975: pin top_w mid W35 `327039-70` (`max_in_flight=1`) → covering=0
    /// for 16s while ready=51; tip60-watch killed at rate 27.2.
    fn best_covering0_tip_pin_candidate(&self, tip: u64) -> Option<String> {
        let inflight = self.in_flight_per_peer.lock().unwrap().clone();
        // 1) Idle tip-STREAM hero.
        if let Some((cand, _)) = self.best_a6n_tip_candidate("") {
            if self.peer_has_flight_capacity(&cand, &inflight)
                && !Self::peer_inflight_ahead_only_map(&inflight, &cand, tip)
            {
                return Some(cand);
            }
        }
        // 2) Idle highest-scored ready active worker.
        let scores = self.peer_scores.lock().unwrap().clone();
        let mut idle: Option<(String, f64)> = None;
        for (p, s) in &scores {
            if !self.is_active_download_worker(p)
                || self.is_peer_blacklisted(p)
                || self.tip_owner_in_fail_cooldown(p)
                || !self.peer_is_ibd_ready(p)
            {
                continue;
            }
            if !self.peer_has_flight_capacity(p, &inflight)
                || Self::peer_inflight_ahead_only_map(&inflight, p, tip)
            {
                continue;
            }
            if idle.as_ref().map(|(_, bs)| *s > *bs).unwrap_or(true) {
                idle = Some((p.clone(), *s));
            }
        }
        if let Some((p, _)) = idle {
            return Some(p);
        }
        // 3) Fall back to top scored (caller may release ahead-only).
        self.top_scored_active_ready_worker()
    }

    /// W126: idle tip-STREAM peer exists (for W113 open-slot gate).
    /// Uses caller's in-flight map — never re-locks (W126a: get_work holds the mutex).
    fn idle_tip_streamer_exists(
        &self,
        tip: u64,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        for peer_id in self.active_download_worker_ids() {
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            if self.wan_tip_stream_bps(&peer_id) <= 0.0 {
                continue;
            }
            if self.peer_has_flight_capacity(&peer_id, in_flight)
                && !Self::peer_inflight_ahead_only_map(in_flight, &peer_id, tip)
            {
                return true;
            }
        }
        false
    }

    /// Ready active worker with score `> min_score`.
    /// `ignore_cooldown`: W95 — when gating *unproven* tip owners, still count cooled
    /// mid-peers as alternatives so CAP cooldowns cannot open a score=0.001 lottery
    /// (live W94 freeze @313142: mid pool cooled → 0.001 owners × CAP=8 → 23s).
    fn active_ready_worker_above(&self, min_score: f64, ignore_cooldown: bool) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, s)| {
                **s > min_score
                    && self.is_active_download_worker(p)
                    && !self.is_peer_blacklisted(p)
                    && (ignore_cooldown || !self.tip_owner_in_fail_cooldown(p))
                    && self.peer_is_ibd_ready(p)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// W82: any ready active worker with score above the floor/mid band.
    fn active_ready_worker_above_mid(&self) -> Option<String> {
        let scores = self.peer_scores.lock().unwrap();
        scores
            .iter()
            .filter(|(p, s)| {
                **s > Self::TIP_OWNER_MID_SCORE
                    && self.is_active_download_worker(p)
                    && !self.is_peer_blacklisted(p)
                    && self.peer_is_ibd_ready(p)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.clone())
    }

    /// Prefer `preferred_tip_owner` while available; otherwise the highest-scored peer with
    /// capacity. During failover (covering already ≥1), any score-ok peer may take the
    /// second covering slot.
    fn peer_may_take_tip_owner(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        covering_next: usize,
    ) -> bool {
        // Dead sticky must not block open-slot / score race (also opens tip slot).
        self.drop_unusable_preferred_tip_sticky();
        // W92: CAP/tip-fail cooldown — must beat TIP_PIN re-elect of same top_w.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            return false;
        }
        let wan = self.wan_tip_gap_crawl(
            self.validation_height
                .load(Ordering::Relaxed)
                .saturating_add(1),
        );
        // W95: refuse unproven deep tip owners while any ready active worker (including
        // fail-cooled mid-peers) scores above the unproven band.
        // W127: when covering==0, do **not** count fail-cooled peers as blockers —
        // live W126b @337k / W126c mute-CAP thrash: mid pool cooled → floor denied →
        // OPEN_STALL ready=50 covering=0 with zero assignees until cooldown expired.
        let mid_alt_ignore_cooldown = covering_next > 0;
        // Strict `< UNPROVEN`: score==0.05 is A6k below-floor usable sticky (p0a_below_floor),
        // not the unproven@0.001 lottery W95 was written to block.
        if wan
            && self.peer_score_of(peer_id) < Self::TIP_OWNER_UNPROVEN_SCORE
            && self
                .active_ready_worker_above(Self::TIP_OWNER_UNPROVEN_SCORE, mid_alt_ignore_cooldown)
                .is_some()
        {
            return false;
        }
        // W36/P0-A open slot: handshake-ready + top-half; sticky first unless A6d upgrade.
        // Live A6b soak: any top-half ready peer → score=1.0 lottery after covering=0 nudge
        // (8 owners / 5 min, ~4.6 blk/s). Idle scored peers must not win — no get_work caller.
        // A6d: sticky score=1.000 locked out breakthrough-class peers → ~6 blk/s.
        // A6e: preferred=None must NOT require sole top_w — that worker may not be polling
        // (live: post-SLA 1666 nudges / 0 assigns while ready=50, busy=0).
        if wan && self.tip_owner_open.load(Ordering::Relaxed) {
            if !self.peer_is_ibd_ready(peer_id) {
                return false;
            }
            if let Some(ref pref) = self.preferred_tip_owner.lock().unwrap().clone() {
                // A6h/A6k: usable sticky (ready+active) owns open tip — even below peer_ok floor.
                // W65: shallow walk-promote sticky does not own the open tip slot.
                if self.tip_sticky_usable(pref) && self.peer_holds_substantial_tip_cover(pref) {
                    if let Some(top_w) = self.top_scored_active_ready_worker() {
                        if self.tip_owner_should_upgrade(pref, &top_w) {
                            return peer_id == top_w && self.peer_ok_for_gap_race(peer_id);
                        }
                    }
                    return peer_id == pref;
                }
                // A6k dens KEEP (p0a_below_floor): usable preferred with covering=0 must
                // still re-arm tip — W82 floor refuse would leave preferred stuck forever.
                if self.tip_sticky_usable(pref) && peer_id == pref {
                    return true;
                }
            }
            // No usable sticky: top-half ready ACTIVE worker may re-arm (first caller).
            // A6e: do not require sole top_w — that worker may not be polling (post-SLA
            // covering=0 forever). W82: still refuse *floor-score* peers while any mid/high
            // ready active worker exists — live mid-chain WAN: open-slot lottery elected
            // score=0.001 stickies → 25s TIP_SLA stalls (tip=347948 holes=20, tip60≈1).
            if !self.is_active_download_worker(peer_id) || !self.peer_ok_for_gap_race(peer_id) {
                return false;
            }
            // W95: ignore fail-cooldown when detecting mid alternatives (covering>0).
            // W127: covering==0 → only live (non-cooled) mids block floor peers.
            if self.peer_score_of(peer_id) <= Self::TIP_OWNER_MID_SCORE
                && self
                    .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, mid_alt_ignore_cooldown)
                    .is_some()
            {
                return false;
            }
            // W113: empty tip + proven tip-STREAM peer ready → refuse non-streamers for
            // deep owner. Live W112b @331209: open-slot elected score=0.100 while ready=62
            // included tip STREAM heroes; covering=3 still rate-failed 33.5 vs 35.
            // W126: only refuse when an *idle* streamer exists — if the STREAM hero is
            // mid W35 ahead (`max_in_flight=1`), blocking others freezes covering=0
            // (live W125 @326975, 16s OPEN_STALL).
            let tip_h = self.next_needed_height();
            if covering_next == 0
                && self.empty_tip_owner_prefer_streamer()
                && self.idle_tip_streamer_exists(tip_h, in_flight)
                && !self.peer_recently_tip_streaming(
                    peer_id,
                    Duration::from_secs(Self::tip_stream_owner_hot_secs()),
                )
            {
                return false;
            }
            return true;
        }
        // A6k: preferred sticky re-arms next span without clearing peer_ok floor.
        if wan {
            if let Some(ref pref) = self.preferred_tip_owner.lock().unwrap().clone() {
                if pref == peer_id && self.tip_sticky_usable(pref) {
                    return true;
                }
            }
        }
        if !self.peer_ok_for_gap_race(peer_id) {
            return false;
        }
        if wan && !self.peer_is_ibd_ready(peer_id) {
            return false;
        }
        // W33a/W47: on WAN tip gap only the top-scored peer may own the deep pipeline —
        // except during tip-distress race (soft-retry / late body; not bridge holes).
        // TPP L1 REVERT (300→320 L1 cell): wall 377<C0 390 — manual undo peer_may C1g.
        let tip_distress = Self::tip_is_distressed() || self.c1t_tip_height_race();
        if wan {
            let distress_race = covering_next >= 1 && tip_distress;
            if !distress_race {
                if let Some(top) = self.top_scored_peer_id() {
                    if peer_id != top {
                        return false;
                    }
                }
            }
        }
        // Failover racer: primary already covers tip; score-ok peer may take slot 2.
        // W30/W37/W47: second slot only while tip is distressed (soft-retry / late body).
        // C1t: also after tip missing ≥C1T_MS under gd-fast (tip height only).
        // W180: still apply W95 floor/unproven gates — live W179b mute CAP + distress armed
        // score=0.001 owners while mid+ workers existed (ready_active_ok=10).
        if covering_next >= 1 && tip_distress {
            if wan
                && self.peer_score_of(peer_id) <= Self::TIP_OWNER_MID_SCORE
                && self
                    .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, true)
                    .is_some()
            {
                return false;
            }
            return true;
        }

        // Preferred sticky while covering is empty.
        let preferred = self.preferred_tip_owner.lock().unwrap().clone();
        if let Some(ref pref) = preferred {
            if pref == peer_id && self.tip_sticky_usable(pref) {
                return true;
            }
            // A6l: wait for usable sticky even when it is at capacity (finishing current span).
            // Prior logic only waited while sticky had spare capacity — so a busy sticky with
            // max_in_flight=1 yielded tip to idle higher-scored peers at every chunk boundary
            // (live A6k: STICKY_DROP=0 but ownership lottery after first sticky stretch).
            // Hung sticky is rotated by tip-SLA / blacklist / tip_owner_open, not by steal.
            // W65: do not wait on a sticky whose only tip claim is a shallow walk-promote.
            if self.tip_sticky_usable(pref) && self.peer_holds_substantial_tip_cover(pref) {
                if !self.peer_has_flight_capacity(pref, in_flight) {
                    Self::log_sticky_busy_hold(pref, peer_id);
                }
                return false;
            }
        }

        let scores = self.peer_scores.lock().unwrap().clone();
        if scores.len() < 2 {
            return true;
        }
        let my = scores.get(peer_id).copied().unwrap_or(0.0);
        // Deny if any higher-scored (or tie-break winner) peer has capacity.
        for (p, s) in &scores {
            if p.as_str() == peer_id {
                continue;
            }
            let better = *s > my || ((*s - my).abs() < f64::EPSILON && p.as_str() < peer_id);
            if !better {
                continue;
            }
            if self.is_peer_blacklisted(p) {
                continue;
            }
            // P0-A: higher-scored peer must be IBD-ready to block this peer on WAN gap.
            if wan && !self.peer_is_ibd_ready(p) {
                continue;
            }
            let cap = Self::max_in_flight_for_scores(&scores, p);
            if Self::peer_flight_count(in_flight, p) >= cap {
                continue;
            }
            // Better peer must also clear the gap-race floor.
            let mut vals: Vec<f64> = scores.values().copied().collect();
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let wan = self.wan_tip_gap_crawl(
                self.validation_height
                    .load(Ordering::Relaxed)
                    .saturating_add(1),
            );
            let floor = if wan {
                vals[vals.len() / 2]
            } else {
                vals[vals.len() / 4]
            };
            if *s < floor {
                continue;
            }
            return false;
        }
        true
    }

    /// P5: bottom-quartile peers skip gap preempt / single-height micro races.
    ///
    /// **W28:** during WAN tip crawl, require **top-half** scores (median+) — bottom half
    /// still gets main-queue ahead work, but not the tip pipeline. Live: 8 racers included
    /// slow peers that soft-retried for 12s while covering looked "healthy".
    ///
    /// **A6e:** on WAN, median is taken over **active download workers only**. Live A6d score
    /// refresh injected idle network peers at default score=1.0 into `peer_scores`; tip
    /// workers dropped to ~0.1–0.2 after `record_block`, failed the polluted median, and
    /// open-slot assign died (covering=0 forever after tip-SLA).
    ///
    /// **A6j:** WAN median prefers **ready** active workers. Live A6i OPEN_STALL:
    /// `top_w_score=0.127 floor=0.153 ready_active_ok=0/9` — disconnected high scorers
    /// inflated the floor above every live worker → permanent covering=0.
    fn peer_ok_for_gap_race(&self, peer_id: &str) -> bool {
        let my = {
            let scores = self.peer_scores.lock().unwrap();
            scores.get(peer_id).copied().unwrap_or(0.0)
        };
        let wan = self.wan_tip_gap_crawl(
            self.validation_height
                .load(Ordering::Relaxed)
                .saturating_add(1),
        );
        if wan {
            let mut vals = self.wan_gap_score_floor_vals();
            if vals.len() < 4 {
                return true;
            }
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A6k: use Q1 (same as non-WAN), not median. Tip downloaders floor at ~0.1 while
            // lightly-proven ready workers sit at ~0.19; median (~0.18) excluded the delivering
            // sticky and forced owner lottery (live: same-peer re-arms 5 vs 146).
            let floor = vals[vals.len() / 4];
            return my >= floor;
        }
        let scores = self.peer_scores.lock().unwrap();
        let mut vals: Vec<f64> = scores.values().copied().collect();
        if vals.len() < 4 {
            return true;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let floor = vals[vals.len() / 4]; // Q1 — not bottom quartile
        my >= floor
    }

    fn stall_requeue_debounce() -> Duration {
        std::env::var("BLVM_IBD_STALL_REQUEUE_DEBOUNCE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(5))
    }

    /// Blacklist a peer for `duration`. During this window, `get_work` will not assign it chunks.
    pub(crate) fn blacklist_peer(&self, peer_id: &str, duration: Duration) {
        let until = Instant::now() + duration;
        let mut bl = self.blacklisted_until.lock().unwrap();
        let entry = bl.entry(peer_id.to_string()).or_insert(until);
        if until > *entry {
            *entry = until;
        }
        tracing::warn!(
            "IBD: blacklisted peer {} for {}s",
            peer_id,
            duration.as_secs()
        );
    }

    /// Returns true if the peer is currently blacklisted.
    pub(crate) fn is_peer_blacklisted(&self, peer_id: &str) -> bool {
        let mut bl = self.blacklisted_until.lock().unwrap();
        if let Some(until) = bl.get(peer_id) {
            if Instant::now() < *until {
                return true;
            }
            bl.remove(peer_id);
        }
        false
    }

    /// Mark bootstrap chunk (0..N) complete — enables parallel chunk assignment for start_height > 0.
    pub(crate) fn mark_bootstrap_complete(&self) {
        self.bootstrap_complete.store(true, Ordering::Relaxed);
    }

    /// Max peers whose in-flight range may **cover** `next_needed` (tip owners).
    ///
    /// **W28b:** default **1**. Racing N peers on the same tip height wastes bandwidth and
    /// still waits on the slowest useful response. Other peers should take **non-overlapping
    /// ahead partitions** (see tip-fill path), not duplicate the tip. Override via
    /// `BLVM_IBD_GAP_FETCHERS`.
    ///
    /// **W28c:** when tip soft-retry / tip-owner failure arms failover, allow **2** so a
    /// second peer can cover the tip without standing 8-way tip races.
    ///
    /// **W31:** on WAN tip gap default **1**. During tip soft-retry ahead-freeze, allow **2**
    /// so a second peer can race the tip height (live: covering=1 zombie while soft-retrying
    /// 30–40s → applied stalls; ahead freeze alone only lifted mean ~4→~8 blk/s).
    ///
    /// **W37:** non-WAN (LOCAL_AHEAD soft-resume) must also require ahead-freeze — armed
    /// alone sticks across polls and opens a permanent covering=2/2 (H,H) treadmill.
    fn max_gap_fetchers_per_height(&self) -> usize {
        let next_needed = self.next_needed_height();
        if self.wan_tip_gap_crawl(next_needed) {
            // W112: empty-bridge tip starve — deep + 2 failover micros.
            if self.empty_tip_triple_race() {
                return 3;
            }
            // W47: soft-retry or late tip body — allow one failover micro.
            if Self::tip_is_distressed() {
                return 2;
            }
            // W41: no deep tip owner → allow a second racer to re-arm (COVERING_ZERO /
            // boundary gap). Once a deep claim exists, back to single tip-height pipe.
            if self.deep_tip_cover_count(next_needed) == 0 {
                return 2;
            }
            // C1h: tip body still missing — deep owner stripe must NOT pin fetchers_cap=1.
            // Live C1g: freeze past-tip + cap=1 → no (H,H) race → ~3 BPS EMPTY_TIP.
            if self.tip_gap_missing.load(Ordering::Relaxed)
                && super::tip_stage::tip_awaiting_secs_for_cap() >= Self::c1g_tip_race_await_secs()
            {
                return 2;
            }
            // C1t: sub-second tip-height race (gd-fast only).
            if self.c1t_tip_height_race() {
                return 2;
            }
            return 1;
        }
        let base = latch_env!(usize, {
            std::env::var("BLVM_IBD_GAP_FETCHERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .clamp(1, 32)
        });
        if super::tip_stage::tip_ahead_frozen_for_soft_retry() {
            base.max(2)
        } else {
            base
        }
    }

    /// W31: on WAN tip gap, stall-recovery retry covering `next_needed` goes to sticky owner only.
    fn peer_may_take_wan_gap_retry(&self, peer_id: &str, start: u64, end: u64) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return true;
        }
        if start > next_needed || end < next_needed {
            return true;
        }
        self.drop_unusable_preferred_tip_sticky();
        match self.preferred_tip_owner() {
            Some(ref owner) if self.tip_sticky_usable(owner) => owner.as_str() == peer_id,
            // P0-B: no usable sticky — do not fan tip *bulks*. Tip-height `(H,H)` micro
            // (FORCE_REQUEUE covering=0) may go to any ready active worker.
            _ => {
                start == next_needed
                    && end == next_needed
                    && self.peer_is_ibd_ready(peer_id)
                    && self.is_active_download_worker(peer_id)
                    && !self.tip_owner_in_fail_cooldown(peer_id)
                    && !self.is_peer_blacklisted(peer_id)
            }
        }
    }

    /// W31: retry-path tip claims on WAN gap only for sticky owner (avoids phantom healthy=2).
    fn maybe_note_tip_cover_claim_retry(&self, peer_id: &str, start: u64, end: u64) {
        let next_needed = self.next_needed_height();
        if !(start <= next_needed && next_needed <= end) {
            return;
        }
        if !self.wan_tip_gap_crawl(next_needed) {
            self.note_tip_cover_claim(peer_id, start, end);
            return;
        }
        let pref = self.preferred_tip_owner();
        let ok = match pref.as_deref() {
            Some(owner) if self.tip_sticky_usable(owner) => owner == peer_id,
            _ => {
                start == next_needed
                    && end == next_needed
                    && self.is_active_download_worker(peer_id)
            }
        };
        if ok {
            self.note_tip_cover_claim(peer_id, start, end);
        }
    }

    /// Highest end among in-flight ranges that overlap `[next_needed, next_needed+window]`.
    /// Used to place the next non-overlapping ahead partition after the tip pipeline.
    ///
    /// **Caveat (C1e):** this is the *assigned* span end. Tip owner often claims tip..tip+127
    /// while download only GetData's tip_hole depth (8–32) → frontier jumps to tip+127 and
    /// other peers open tip+128 with tip still empty (Swiss cheese). Prefer
    /// [`Self::tip_contiguous_assign_frontier`] + [`Self::tip_runway_cap`] while tip missing.
    fn tip_pipeline_frontier(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        window: u64,
    ) -> u64 {
        let hi = next_needed.saturating_add(window);
        let mut frontier = next_needed.saturating_sub(1);
        for (s, e) in in_flight.values().flatten() {
            if *e >= next_needed && *s <= hi {
                frontier = frontier.max(*e);
            }
        }
        frontier
    }

    /// C1e: max contiguous assign band from tip while tip body missing (multi-peer stripes).
    /// Default **96**. Env `BLVM_IBD_TIP_RUNWAY_CAP` (clamp 32–256).
    pub(crate) fn tip_runway_cap() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_RUNWAY_CAP")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(96)
                .clamp(32, 256)
        })
    }

    /// C1e: stripe width for tip-owner / ahead peers inside the runway (default **32** = C1b).
    pub(crate) fn tip_runway_stripe() -> u64 {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_RUNWAY_STRIPE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32)
                .clamp(8, 128)
        });
        raw.min(Self::tip_runway_cap())
    }

    /// Raw `GAP_PREEMPT_BATCH` when set (defaults applied at call sites).
    fn gap_preempt_batch_raw() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_GAP_PREEMPT_BATCH")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }

    /// Raw `TIP_PARTITION_WINDOW` when set (WAN/local apply different defaults/clamps).
    fn tip_partition_window_raw() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_PARTITION_WINDOW")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }

    /// Mousetrap: log assign_F vs pipe_F vs body_F on tip-band secondary opens.
    fn log_pipe_f(tip: u64, assign_f: u64, pipe_f: u64, body_f: u64, peer_id: &str, reason: &str) {
        let gap = assign_f.saturating_sub(pipe_f);
        tracing::warn!(
            "[IBD_PIPE_F] tip={} assign_f={} pipe_f={} body_f={} gap={} peer={} reason={}",
            tip,
            assign_f,
            pipe_f,
            body_f,
            gap,
            peer_id,
            reason
        );
    }

    /// C1e: walk assigned in-flight ranges from `next_needed` while contiguous; stop at hole
    /// or `runway_end`. Returns last covered end, or `next_needed-1` if tip uncovered.
    /// Multiple peers' non-overlapping stripes compose one contiguous runway.
    fn tip_contiguous_assign_frontier(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        runway_end: u64,
    ) -> u64 {
        if runway_end < next_needed {
            return next_needed.saturating_sub(1);
        }
        let mut ranges: Vec<(u64, u64)> = in_flight
            .values()
            .flatten()
            .filter_map(|&(s, e)| {
                let cs = s.max(next_needed);
                let ce = e.min(runway_end);
                (cs <= ce).then_some((cs, ce))
            })
            .collect();
        if ranges.is_empty() {
            return next_needed.saturating_sub(1);
        }
        ranges.sort_by_key(|&(s, _)| s);
        let mut cursor = next_needed;
        let mut frontier = next_needed.saturating_sub(1);
        for (s, e) in ranges {
            if s > cursor {
                break;
            }
            if e >= cursor {
                frontier = frontier.max(e);
                cursor = e.saturating_add(1);
            }
            if cursor > runway_end {
                break;
            }
        }
        frontier
    }

    fn range_overlaps_inflight(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> bool {
        in_flight
            .values()
            .flatten()
            .any(|(s, e)| *s <= end && start <= *e)
    }

    /// P1c: true when `peer_id` already holds an in-flight range covering `next_needed`.
    fn peer_holds_tip_inflight(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        next_needed: u64,
    ) -> bool {
        in_flight.get(peer_id).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(s, e)| s <= next_needed && next_needed <= e)
        })
    }

    /// On coordinator stall, prefetch this many consecutive gap heights so peers can race ahead
    /// of validation instead of one micro-chunk per stall tick.
    fn gap_micro_chunk_batch(&self) -> u64 {
        std::env::var("BLVM_IBD_GAP_MICRO_BATCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(if self.work_stealing { 32 } else { 8 })
            .clamp(1, 128)
    }

    pub(crate) fn is_bootstrap_complete(&self) -> bool {
        self.bootstrap_complete.load(Ordering::Relaxed)
    }

    /// Returns true if any worker is already downloading this exact chunk range.
    ///
    /// W15: for multi-block ranges that cover `next_needed`, also treat *overlapping*
    /// covering ranges as in-flight — stall recovery used to enqueue 684849-864,
    /// 684850-864, 684851-864 as tip advanced; each exact `(s,e)` differed so all
    /// assigned, blowing past `max_gap_fetchers` (live: 10+ overlapping bulks).
    fn chunk_range_in_flight(
        &self,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> bool {
        if start == end {
            let n = in_flight
                .values()
                .flatten()
                .filter(|(s, e)| *s == start && *e == end)
                .count();
            return n >= self.max_gap_fetchers_per_height();
        }
        if in_flight
            .values()
            .flatten()
            .any(|(s, e)| *s == start && *e == end)
        {
            return true;
        }
        // W15: overlapping multi-block covering the same tip height counts toward cap.
        let next_needed = self
            .validation_height
            .load(Ordering::Relaxed)
            .saturating_add(1);
        if start <= next_needed && next_needed <= end {
            let covering = Self::covering_next_count(in_flight, next_needed);
            if covering >= self.max_gap_fetchers_per_height() {
                return true;
            }
        }
        false
    }

    fn covering_next_count(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) -> usize {
        in_flight
            .values()
            .flatten()
            .filter(|(s, e)| *s <= next_needed && next_needed <= *e)
            .count()
    }

    fn peer_flight_count(in_flight: &HashMap<String, Vec<(u64, u64)>>, peer_id: &str) -> usize {
        in_flight.get(peer_id).map(|v| v.len()).unwrap_or(0)
    }

    fn insert_in_flight(
        in_flight: &mut HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        start: u64,
        end: u64,
    ) {
        in_flight
            .entry(peer_id.to_string())
            .or_default()
            .push((start, end));
    }
}
