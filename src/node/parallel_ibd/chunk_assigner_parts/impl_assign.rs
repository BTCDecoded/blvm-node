impl ChunkAssigner {
    /// Deduplicate peer ids while preserving first-seen order.
    fn dedupe_workers(peers: Vec<String>) -> Vec<String> {
        let mut seen = HashSet::new();
        peers
            .into_iter()
            .filter(|p| !p.is_empty() && seen.insert(p.clone()))
            .collect()
    }

    /// Height ranges + active workers (independent lengths).
    ///
    /// - `work_stealing`: `workers` is only the active download set (deduped); no affinity.
    /// - `!work_stealing` and `workers.len() == chunks.len()`: treat `workers` as
    ///   preferred-per-range affinity (LAN / legacy call sites), then dedupe for membership.
    /// - otherwise: dedupe workers and round-robin affinity onto ranges.
    ///
    /// Prefer [`Self::from_block_chunks`] when constructing from [`create_chunks`].
    pub(crate) fn new(
        chunks: Vec<(u64, u64)>,
        workers: Vec<String>,
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        let (workers, preferred_peers) = if work_stealing {
            (Self::dedupe_workers(workers), Vec::new())
        } else if workers.len() == chunks.len() && !workers.is_empty() {
            let preferred = workers;
            let workers = Self::dedupe_workers(preferred.clone());
            (workers, preferred)
        } else {
            let workers = Self::dedupe_workers(workers);
            let preferred = if workers.is_empty() {
                Vec::new()
            } else {
                let n = workers.len();
                (0..chunks.len()).map(|i| workers[i % n].clone()).collect()
            };
            (workers, preferred)
        };
        Self::build(
            chunks,
            workers,
            preferred_peers,
            validation_height,
            start_height,
            work_stealing,
        )
    }

    /// From [`create_chunks`] / planned [`BlockChunk`] list. Workers = unique peer ids;
    /// preferred affinity kept for LAN (`!work_stealing`).
    pub(crate) fn from_block_chunks(
        planned: &[BlockChunk],
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        let chunks: Vec<(u64, u64)> = planned
            .iter()
            .map(|c| (c.start_height, c.end_height))
            .collect();
        let preferred: Vec<String> = planned.iter().map(|c| c.peer_id.clone()).collect();
        let workers = Self::dedupe_workers(preferred.clone());
        let preferred_peers = if work_stealing { Vec::new() } else { preferred };
        Self::build(
            chunks,
            workers,
            preferred_peers,
            validation_height,
            start_height,
            work_stealing,
        )
    }

    fn build(
        chunks: Vec<(u64, u64)>,
        workers: Vec<String>,
        preferred_peers: Vec<String>,
        validation_height: Arc<std::sync::atomic::AtomicU64>,
        start_height: u64,
        work_stealing: bool,
    ) -> Self {
        debug_assert!(
            work_stealing || preferred_peers.is_empty() || preferred_peers.len() == chunks.len(),
            "preferred_peers must match chunks when set"
        );
        // Always serialize the first chunk (contains `start_height`) before assigning later chunks.
        // On near-tip resume (`start_height > 0`) there is only one chunk — treat bootstrap done so
        // workers are not blocked waiting for a bootstrap marker that only applies to height 0.
        let bootstrap_complete = AtomicBool::new(start_height > 0);
        let ibd_end = chunks.iter().map(|(_, e)| *e).max().unwrap_or(0);
        Self {
            chunks,
            workers,
            extra_workers: Mutex::new(HashSet::new()),
            preferred_peers,
            next_index: AtomicUsize::new(0),
            retry_queue: Mutex::new(VecDeque::new()),
            validation_height,
            bootstrap_complete,
            start_height,
            in_flight_per_peer: Mutex::new(HashMap::new()),
            work_stealing,
            blacklisted_until: Mutex::new(HashMap::new()),
            last_stall_requeue: Mutex::new(None),
            peer_scores: Mutex::new(HashMap::new()),
            confirmed_body_height_at_start: AtomicU64::new(0),
            wan_body_tip: AtomicU64::new(0),
            tip_gap_missing: AtomicBool::new(false),
            tip_bridge_holes: AtomicU64::new(0),
            preferred_tip_owner: Mutex::new(None),
            tip_owner_fail_until: Mutex::new(HashMap::new()),
            tip_cover_claims: Mutex::new(Vec::new()),
            tip_owner_open: AtomicBool::new(false),
            tip_failover_once_h: AtomicU64::new(0),
            tip_failover_once_at_ms: AtomicU64::new(0),
            tip_ahead_hole_freeze: AtomicBool::new(false),
            tip_ahead_hole_clear_since_ms: AtomicU64::new(0),
            ibd_ready_peers: Mutex::new(HashSet::new()),
            sticky_wan_tenure: Mutex::new(None),
            last_a6m_rotate_at: Mutex::new(None),
            tip_progress_samples: Mutex::new(VecDeque::new()),
            peer_tip_streams: Mutex::new(HashMap::new()),
            tip_hole_depth: Mutex::new(HashMap::new()),
            tip_trial: Mutex::new(None),
            last_tip_trial_at: Mutex::new(None),
            tip_trial_post_open_at: Mutex::new(None),
            header_tip: AtomicU64::new(0),
            ibd_end_height: AtomicU64::new(ibd_end),
            shutdown: AtomicBool::new(false),
            synth_tip_dedup_block_since_ms: AtomicU64::new(0),
        }
    }

    /// C1c/C1k: sticky tip-hole depth across chunks for the same tip peer.
    ///
    /// Pre-C1i: sticky@32 REGRESS wall≈16 (cheese / ahead_ratio≈23). With C1i min_contig
    /// + C1j abort, harness retries default **on** (`BLVM_IBD_TIP_HOLE_STICKY=1`) — grow
    ///   was stuck at p50=8 every chunk (4× depth binder vs cap 32). Opt out: `=0`.
    pub(crate) fn tip_hole_sticky_enabled() -> bool {
        super::policy::tip_hole_sticky()
    }

    /// C1c/C1s: starting tip-hole depth for `peer_id` (sticky or grow_start).
    /// C1s: do **not** clamp sticky to live gd-fast effective cap — EWMA dips were
    /// ratcheting 48→32 every few chunks (live C1r: repeated `32→40→48`). Fill still
    /// mins against effective cap each request.
    pub(crate) fn tip_hole_depth_for(&self, peer_id: &str) -> usize {
        let start = super::download::tip_hole_grow_start();
        if !Self::tip_hole_sticky_enabled() {
            return start;
        }
        let abs_cap =
            super::download::tip_hole_sticky_abs_cap(self.peer_is_hot_tip_streamer(peer_id));
        let g = self.tip_hole_depth.lock().unwrap();
        g.get(peer_id)
            .copied()
            .unwrap_or(start)
            .max(start)
            .min(abs_cap)
    }

    /// C1c/C1s: persist grown tip-hole depth (ratchet **up** only; mute → reset).
    pub(crate) fn note_tip_hole_depth(&self, peer_id: &str, depth: usize) {
        if !Self::tip_hole_sticky_enabled() {
            return;
        }
        let start = super::download::tip_hole_grow_start();
        let abs_cap =
            super::download::tip_hole_sticky_abs_cap(self.peer_is_hot_tip_streamer(peer_id));
        let mut g = self.tip_hole_depth.lock().unwrap();
        let prev = g.get(peer_id).copied().unwrap_or(0);
        // Ratchet up only — never shrink sticky on a soft EWMA dip.
        let d = depth.clamp(start, abs_cap).max(prev.min(abs_cap));
        if prev == d {
            return;
        }
        g.insert(peer_id.to_string(), d);
        tracing::info!(
            "[IBD_TIP_HOLE_STICKY] peer={} depth {}→{} (abs_cap={})",
            peer_id,
            prev,
            d,
            abs_cap
        );
    }

    /// C1c: clear sticky depth on mute (do not reopen Swiss-cheese on a mute hero).
    pub(crate) fn reset_tip_hole_depth(&self, peer_id: &str) {
        let mut g = self.tip_hole_depth.lock().unwrap();
        if g.remove(peer_id).is_some() {
            tracing::info!("[IBD_TIP_HOLE_STICKY] peer={} reset (mute/fail)", peer_id);
        }
    }

    /// C1u: hard GD_SLOW may shrink sticky (C1s forbids soft-EWMA shrink only).
    pub(crate) fn clamp_tip_hole_depth(&self, peer_id: &str, depth: usize) {
        if !Self::tip_hole_sticky_enabled() {
            return;
        }
        let start = super::download::tip_hole_grow_start();
        let d = depth
            .max(start)
            .min(super::download::tip_hole_sticky_abs_cap(
                self.peer_is_hot_tip_streamer(peer_id),
            ));
        let mut g = self.tip_hole_depth.lock().unwrap();
        let prev = g.get(peer_id).copied().unwrap_or(0);
        if prev == 0 || prev <= d {
            return;
        }
        g.insert(peer_id.to_string(), d);
        tracing::info!(
            "[IBD_TIP_HOLE_STICKY] peer={} depth {}→{} (gd_slow clamp)",
            peer_id,
            prev,
            d
        );
    }

    /// Override / extend planned IBD end (e.g. tip-follow). Used for worker exit gating.
    pub(crate) fn set_ibd_end_height(&self, end: u64) {
        self.ibd_end_height.store(end, Ordering::Relaxed);
    }

    /// Signal all download workers to exit. Clears tip-gap keep-alive so `is_done()`
    /// becomes true even when past on-disk body tip.
    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.tip_gap_missing.store(false, Ordering::Relaxed);
        super::tip_stage::clear_tip_failover();
        tracing::info!(
            "[IBD_ASSIGNER_SHUTDOWN] download workers signaled to exit (vh={} end={})",
            self.validation_height.load(Ordering::Relaxed),
            self.ibd_end_height.load(Ordering::Relaxed)
        );
    }

    /// True once IBD validation has reached the planned end height.
    fn validation_reached_ibd_end(&self) -> bool {
        let end = self.ibd_end_height.load(Ordering::Relaxed);
        end > 0 && self.validation_height.load(Ordering::Relaxed) >= end
    }

    /// Coordinator: refresh highest stored header so tip assigns never exceed available hashes.
    pub(crate) fn set_header_tip(&self, tip: u64) {
        self.header_tip.store(tip, Ordering::Relaxed);
    }

    pub(crate) fn header_tip(&self) -> u64 {
        self.header_tip.load(Ordering::Relaxed)
    }

    /// Best available tip reference for bulk-catchup gating: live stored headers, else IBD end.
    /// `header_tip` stays 0 until the first peer-ready refresh — without this fallback, mid-chain
    /// catch-up is misclassified as near-tip crawl (max_ahead=256, ahead peers blocked).
    fn header_tip_for_bulk(&self) -> u64 {
        self.header_tip()
            .max(self.ibd_end_height.load(Ordering::Relaxed))
    }

    /// Past body tip but still far from header tip — multi-peer catch-up (not tip crawl).
    fn wan_bulk_catchup(&self, next_needed: u64) -> bool {
        self.wan_tip_gap_crawl(next_needed)
            && super::wan_bulk_catchup(self.header_tip_for_bulk(), next_needed)
    }

    /// Clip a tip/ahead end height to the last stored header (0 = unknown → no clip).
    fn clip_end_to_headers(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        let ht = self.header_tip();
        if ht == 0 {
            return Some((start, end));
        }
        if start > ht {
            return None;
        }
        Some((start, end.min(ht)))
    }

    /// P0-A: refresh from coordinator (async network poll → sync snapshot).
    pub(crate) fn set_ibd_ready_peers(&self, ready: HashSet<String>) {
        *self.ibd_ready_peers.lock().unwrap() = ready;
    }

    /// Peer ids scored for IBD — used to refresh the ready snapshot.
    pub(crate) fn peer_ids_for_ibd_ready(&self) -> Vec<String> {
        self.peer_scores.lock().unwrap().keys().cloned().collect()
    }

    /// P0-A: on WAN tip gap, tip-owner paths require a live handshook peer.
    /// Empty ready snapshot → deny (no allow-all carousel on dead owners).
    /// Dense-local unit setups (`wan_body_tip=0` and no coordinator `header_tip`) skip the
    /// ready gate so W16 mid-chunk tip-fill / main-queue tip-cover still assign.
    fn peer_is_ibd_ready(&self, peer_id: &str) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return true;
        }
        if self.wan_body_tip.load(Ordering::Relaxed) == 0 && self.header_tip() == 0 {
            return true;
        }
        let ready = self.ibd_ready_peers.lock().unwrap();
        if ready.is_empty() {
            return false;
        }
        ready.contains(peer_id)
    }

    /// True when `peer_id` is an ACTIVE_PEERS download worker (listed in workers).
    fn is_active_download_worker(&self, peer_id: &str) -> bool {
        self.workers.iter().any(|p| p == peer_id)
            || self.extra_workers.lock().unwrap().contains(peer_id)
    }

    /// Active download worker peer ids (construction set + peer-watcher replacements).
    pub(crate) fn active_download_worker_ids(&self) -> Vec<String> {
        let mut out = self.workers.clone();
        let extra = self.extra_workers.lock().unwrap();
        for p in extra.iter() {
            if !out.iter().any(|w| w == p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// Register a download worker spawned after assigner construction (peer watcher).
    /// Without this, tip-owner / OPEN_STALL / mid-clear treat the peer as non-worker
    /// even while it polls `get_work` — live freeze: ready>0, ready_active_ok=0/0.
    pub(crate) fn register_download_worker(&self, peer_id: &str) {
        if peer_id.is_empty() || self.workers.iter().any(|p| p == peer_id) {
            return;
        }
        self.extra_workers
            .lock()
            .unwrap()
            .insert(peer_id.to_string());
    }

    /// P0-A A8: observability when a WAN tip owner is assigned.
    fn log_wan_tip_owner_ready(&self, peer_id: &str, start: u64, end: u64) {
        if !self.wan_tip_gap_crawl(start) {
            return;
        }
        let ready = self.peer_is_ibd_ready(peer_id);
        let worker = self.is_active_download_worker(peer_id);
        let score = self
            .peer_scores
            .lock()
            .unwrap()
            .get(peer_id)
            .copied()
            .unwrap_or(0.0);
        tracing::warn!(
            "[IBD_TIP_PEER_READY] owner={} ibd_ready={} active_worker={} score={:.3} span={}-{}",
            peer_id,
            ready,
            worker,
            score,
            start,
            end
        );
    }

    /// Observability: how many peers are currently marked IBD-ready.
    pub(crate) fn ibd_ready_peer_count(&self) -> usize {
        self.ibd_ready_peers.lock().unwrap().len()
    }

    /// WAN tip crawl: validation past on-disk body tip (live soak past ~689k).
    ///
    /// Gates on [`Self::wan_body_tip`] (coordinator live tip), not contiguous `confirmed`
    /// alone — see field docs. `confirmed > 0` was wrong for genesis (`confirmed=0`).
    fn wan_tip_gap_crawl(&self, next_needed: u64) -> bool {
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        self.bootstrap_complete.load(Ordering::Relaxed) && next_needed > body_tip
    }

    /// P0-B/W84: on WAN tip crawl, suppress *ahead* stall micro storms — but always allow
    /// micro recovery for the **exact tip height**.
    ///
    /// Pre-W84: `!wan_tip_gap_crawl(height)` made every stall requeue a no-op during
    /// genesis/WAN (`body_tip=0`). Live freeze tip=256687 (~8+ min): `IBD_STALL` while
    /// Case C saw covering≥1 (zombie) so skipped [`Self::requeue_stall_gaps_force`], and
    /// `wan_stall_micro_allowed` blocked the non-force path → tip never re-armed.
    ///
    /// W73 force path still covers covering=0. Debounce inside `requeue_stall_gaps` remains.
    pub(crate) fn wan_stall_micro_allowed(&self, height: u64) -> bool {
        if !self.wan_tip_gap_crawl(height) {
            return true;
        }
        height == self.next_needed_height()
    }

    pub(crate) fn set_confirmed_body_height_at_start(&self, height: u64) {
        self.confirmed_body_height_at_start
            .store(height, Ordering::Relaxed);
        // Default WAN tip gate to confirmed; raise to live/sparse tip via [`Self::set_wan_body_tip`].
        self.set_wan_body_tip(height);
    }

    /// Align assigner WAN tip crawl with coordinator `live_body_tip` (confirmed ∨ sparse ∨ probe).
    pub(crate) fn set_wan_body_tip(&self, height: u64) {
        self.wan_body_tip.store(height, Ordering::Relaxed);
        // Land E: validation / index consult tip-crawl supply via tip_stage mirror.
        super::tip_stage::publish_wan_body_tip(height);
    }

    /// How many local blocks before `body_tip` to start GetData-priming past tip.
    ///
    /// Binder soak (2026-07-28): local ahead ~450–690 BPS then cliff at body tip
    /// (`SUPPLY_TIP_HOLE` / `SUPPLY_GD_SLOW`, gd_ewma 5–9s). Priming `body_tip+1..`
    /// while still validating local bodies warms the tip pipe. Default **64**.
    /// Opt out: `BLVM_IBD_HANDOFF_PRIME=0`.
    /// Default **256** — local ahead fills to body tip while validation is still hundreds
    /// behind at ~500 BPS; 64 was only ~130ms and missed the window (C1u soak 2026-07-28).
    pub(crate) fn handoff_prime_blocks() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_HANDOFF_PRIME")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(256)
                .min(1024)
        })
    }

    /// True while validating inside the last `HANDOFF_PRIME` local bodies before WAN tip.
    pub(crate) fn handoff_prime_active(&self, next_needed: u64) -> bool {
        let prime = Self::handoff_prime_blocks();
        if prime == 0 || !self.bootstrap_complete.load(Ordering::Relaxed) {
            return false;
        }
        // Synth delay=0: bodies stay local — never open network prime.
        if super::synthetic_wan::enabled() && super::synthetic_wan::getdata_delay_ms() == 0 {
            return false;
        }
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        if body_tip == 0 || next_needed > body_tip {
            return false;
        }
        next_needed.saturating_add(prime) > body_tip
    }

    /// Tip-owner GetData for `body_tip+1..+stripe` while still local (C1u).
    /// Returns assigned range when inserted into `guard`; caller must `drop(guard)` + return.
    fn try_assign_handoff_prime(
        &self,
        peer_id: &str,
        next_needed: u64,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        reason: &str,
    ) -> Option<(u64, u64)> {
        if Self::handoff_prime_blocks() == 0
            || !self.bootstrap_complete.load(Ordering::Relaxed)
            || (super::synthetic_wan::enabled() && super::synthetic_wan::getdata_delay_ms() == 0)
        {
            return None;
        }
        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
        if body_tip == 0 || next_needed > body_tip {
            return None;
        }
        let prime_start = body_tip.saturating_add(1);
        let stripe = Self::tip_runway_stripe();
        let mut prime_end = prime_start.saturating_add(stripe.saturating_sub(1));
        let headers_ok = match self.clip_end_to_headers(prime_start, prime_end) {
            Some((_, clipped)) => {
                prime_end = clipped;
                true
            }
            None => false,
        };
        if !headers_ok || prime_end < prime_start {
            return None;
        }
        let prime_covering = Self::covering_next_count(guard, prime_start);
        let claim_hit = {
            let claims = self.tip_cover_claims.lock().unwrap();
            claims
                .iter()
                .any(|(_, s, e)| *s <= prime_end && prime_start <= *e)
        };
        // Sticky tip-owner preferred; if at flight cap on local tip, idle top-scored
        // ready worker may prime (local ahead reaches body tip while sticky is busy).
        let may_prime = self.peer_may_take_tip_owner(peer_id, guard, 0)
            || self.handoff_prime_fallback_ok(peer_id, guard);
        // Never prime body_tip+1 while next_needed < body_tip — sticky max_in_flight=1
        // steals onto WAN tip while local gap remains (C0 T025719Z freeze wait 304664
        // @ next=304649; also blocks ahead_frontier when cover spans to body_tip).
        if next_needed < body_tip {
            return None;
        }
        // near_tip: also require local tip height covered in-flight.
        if reason == "near_tip" && Self::covering_next_count(guard, next_needed) == 0 {
            return None;
        }
        if prime_covering != 0
            || claim_hit
            || !may_prime
            || Self::range_overlaps_inflight(guard, prime_start, prime_end)
            || Self::peer_holds_tip_inflight(guard, peer_id, prime_start)
        {
            return None;
        }
        let seed = super::download::tip_hole_grow_start()
            .max(32)
            .min(super::download::tip_hole_grow_cap().max(32));
        self.note_tip_hole_depth(peer_id, seed);
        Self::insert_in_flight(guard, peer_id, prime_start, prime_end);
        self.note_tip_cover_claim(peer_id, prime_start, prime_end);
        self.note_tip_owner_assigned(peer_id);
        tracing::warn!(
            "[IBD_HANDOFF_PRIME] peer={} priming {}-{} while next_needed={} body_tip={} seed_depth={} reason={} — GetData warm before WAN tip",
            peer_id,
            prime_start,
            prime_end,
            next_needed,
            body_tip,
            seed,
            reason
        );
        Some((prime_start, prime_end))
    }

    /// When the tip-covering peer is at capacity on local bodies, allow one idle
    /// top-scored ready worker to GetData-prime past body tip (C1u ahead_frontier).
    fn handoff_prime_fallback_ok(
        &self,
        peer_id: &str,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
    ) -> bool {
        if !self.peer_ok_for_gap_race(peer_id)
            || !self.peer_is_ibd_ready(peer_id)
            || !self.is_active_download_worker(peer_id)
            || !self.peer_has_flight_capacity(peer_id, in_flight)
        {
            return false;
        }
        let next_needed = self
            .validation_height
            .load(Ordering::Relaxed)
            .saturating_add(1);
        // Tip already covered in-flight, and that cover peer has no spare slot.
        let tip_cover_busy = in_flight.iter().any(|(p, ranges)| {
            ranges
                .iter()
                .any(|(s, e)| *s <= next_needed && next_needed <= *e)
                && !self.peer_has_flight_capacity(p, in_flight)
        });
        if !tip_cover_busy {
            return false;
        }
        let preferred = self.preferred_tip_owner.lock().unwrap().clone();
        let scores = self.peer_scores.lock().unwrap();
        let best_idle = scores
            .iter()
            .filter(|(p, _)| {
                // Skip tip-covering peers (busy on local tip).
                !in_flight.get(*p).is_some_and(|ranges| {
                    ranges
                        .iter()
                        .any(|(s, e)| *s <= next_needed && next_needed <= *e)
                }) && preferred.as_ref().map(|pref| pref != *p).unwrap_or(true)
                    && self.is_active_download_worker(p)
                    && self.peer_is_ibd_ready(p)
                    && !self.is_peer_blacklisted(p)
                    && self.peer_has_flight_capacity(p, in_flight)
            })
            .max_by(|(a, sa), (b, sb)| {
                sa.partial_cmp(sb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            })
            .map(|(p, _)| p.as_str());
        best_idle == Some(peer_id)
    }

    /// W25a: coordinator mirrors `gap_missing` from reorder_buffer each loop.
    pub(crate) fn set_tip_gap_missing(&self, missing: bool) {
        self.tip_gap_missing.store(missing, Ordering::Relaxed);
        // W75: export thread reads the process-wide atomic (no assigner handle there).
        super::IBD_TIP_GAP_MISSING.store(missing, Ordering::Relaxed);
        // Tip arrived (or was never missing) — drop temporary failover covering.
        if !missing {
            super::tip_stage::clear_tip_failover();
            super::tip_stage::clear_tip_ahead_soft_freeze();
        }
    }

    /// Coordinator mirrors bridge `holes_in_span` for tip-band ahead gating.
    pub(crate) fn set_tip_bridge_holes(&self, holes: u64) {
        self.tip_bridge_holes.store(holes, Ordering::Relaxed);
        super::IBD_TIP_BRIDGE_HOLES.store(holes, Ordering::Relaxed);
    }

    /// Returns the next assignable chunk for this peer, or None if nothing ready.
    /// Per-peer serial: returns None if this peer already has a chunk in flight (eliminates chunk-boundary stalls).
    /// Round-robin: prioritizes critical chunk (containing next_needed) from retry, then earliest available.
    /// CRITICAL: Entire operation under one lock to prevent duplicate chunk assignment (race: two workers
    /// for same peer both getting chunk 116240-116255, both requesting same blocks, one starves).
    pub(crate) fn get_work(&self, peer_id: &str, max_ahead: u64) -> Option<(u64, u64)> {
        // IBD range complete or explicit teardown — no new work (Phase 3 must proceed).
        if self.shutdown.load(Ordering::Acquire) || self.validation_reached_ibd_end() {
            return None;
        }
        // E1: while ladder export isolation is on, do not issue new GetData — let
        // checkpoint compact/write own the disk. In-flight chunks may still finish.
        if crate::node::parallel_ibd::export_isolation_active() {
            return None;
        }

        let bootstrap_done = self.bootstrap_complete.load(Ordering::Relaxed);
        let current_validation = self.validation_height.load(Ordering::Relaxed);
        let next_needed = current_validation + 1;
        let max_start = current_validation.saturating_add(max_ahead);

        // Bootstrap serialization: until bootstrap chunk completes, only assign chunks with start==0
        let allow_chunk = |start: u64| bootstrap_done || start == self.start_height;

        // Blacklisted peers get no work until their cooldown expires.
        if self.is_peer_blacklisted(peer_id) {
            return None;
        }

        // W80: clear behind-tip retry junk before tip fill / retry / main queue.
        self.purge_obsolete_retries(next_needed);

        // Single lock: check in-flight + find chunk + insert. Prevents duplicate assignment.
        let t_inflight_wait = Instant::now();
        let mut guard = self.in_flight_per_peer.lock().unwrap();
        let _gw_timer = AssignerGetWorkTimer::start(t_inflight_wait.elapsed().as_nanos() as u64);
        let cap = self.max_in_flight_for(peer_id);
        let flight = Self::peer_flight_count(&guard, peer_id);
        if flight >= cap {
            if self.preferred_tip_owner().as_deref() == Some(peer_id) {
                Self::log_sticky_cap_block(peer_id, flight, cap);
            }
            return None;
        }

        // W4/W16/W28b/W28c/W28d tip fill: sticky tip owner + non-overlapping ahead partitions.
        //
        // Multi-peer is useful when peers download **different** heights. Racing the same tip
        // (W18 covering=8) is not. Hornet pipelines deep on one peer; we keep multi-peer by
        // partitioning the ahead window. W28c: sticky best owner; soft-retry arms one failover
        // covering peer (tip height only), not a standing N-way tip lottery.
        //
        // W28d: ahead partitions that tip walks into do NOT count as healthy tip coverage —
        // only explicit tip-owner/failover claims do. Live W28c: 0 deep tip owners past body
        // tip because covering stayed at 2 from walk-ins; only (H,H) failover ran.
        //
        // Synth bulk: tip-fill stays enabled (skipping it regressed wall to ~7–9 BPS — W16
        // tip_band + tip-reserve spin). Overlap guard below blocks same-span reassign storms
        // (live ~30k W28c reassigns + GAP_STREAM ~3.4× overread → 350→400 wall 178).
        if allow_chunk(next_needed) {
            let containing = self
                .chunks
                .iter()
                .find(|(s, e)| *s <= next_needed && next_needed <= *e)
                .copied();
            // P0: on WAN tip gap, tip owner must assign even if next_needed walked past the
            // static chunk map (headers advanced after IBD start). Synthetic containing range.
            let containing = containing.or_else(|| {
                if self.wan_tip_gap_crawl(next_needed) || self.handoff_prime_active(next_needed) {
                    let ht = self.header_tip();
                    let end = if ht >= next_needed {
                        next_needed.saturating_add(255).min(ht)
                    } else {
                        // No headers past tip yet — still advertise tip height so assign
                        // can wait / clip rather than invent tip+255 past store.
                        next_needed
                    };
                    Some((next_needed, end))
                } else {
                    None
                }
            });
            if let Some((cs, ce)) = containing {
                let raw_covering = Self::covering_next_count(&guard, next_needed);
                let at_chunk_start = next_needed == cs;
                let wan_gap = self.wan_tip_gap_crawl(next_needed);
                let handoff_prime = self.handoff_prime_active(next_needed);
                // W30/W37: tip owner gating uses deep claims only — (H,H) failover micros
                // must not block deep re-arm on WAN gap *or* LOCAL_AHEAD soft-resume
                // (live 2026-07-16: covering=2/2 (H,H) treadmill, 0 deep owners, ~0.2 blk/s).
                // W4/N12: one tip_cover_claims snapshot for deep+healthy counts (re-snap after promote).
                // C1u: GetData-prime body_tip+1 while still local.
                // (1) near_tip window (HANDOFF_PRIME of body) — only after local tip is
                //     already covered. Live FAIL true-wan-…T004239Z: early near_tip prime
                //     stole sticky max_in_flight=1 onto body_tip+1 while next_needed≪tip
                //     and W28c ahead cheese'd the hole → freeze wait 437206 (tip60 never).
                // (2) ahead frontier already at body tip (far local dens KEEP) — otherwise
                //     tip-fill re-arms next_needed forever and never reaches ahead_frontier.
                if !wan_gap {
                    let body_tip_c1u = self.wan_body_tip.load(Ordering::Relaxed);
                    let frontier_at_body = body_tip_c1u > 0
                        && next_needed <= body_tip_c1u
                        && Self::tip_pipeline_frontier(&guard, next_needed, 2048) >= body_tip_c1u;
                    if handoff_prime || frontier_at_body {
                        let tip_covered = Self::covering_next_count(&guard, next_needed) > 0;
                        // near_tip: only on the last local height (next>=body_tip) with cover.
                        // frontier_at_body may still prime earlier (contig already at body tip).
                        let near_tip_ready = tip_covered && next_needed >= body_tip_c1u;
                        if frontier_at_body || (handoff_prime && near_tip_ready) {
                            let reason = if handoff_prime && near_tip_ready {
                                "near_tip"
                            } else {
                                "ahead_frontier"
                            };
                            if let Some(range) = self.try_assign_handoff_prime(
                                peer_id,
                                next_needed,
                                &mut guard,
                                reason,
                            ) {
                                drop(guard);
                                return Some(range);
                            }
                        }
                    }
                }
                // Stale tip_cover prune: tried 2026-08-03 (exact + tip-cover variants).
                // CLAIM_STALE never armed on tip-now; claimfix tip90≈61–63 REVERT vs
                // forcedeb 108.9. Manual REVERT — keep force debounce + tip_nudge gate.
                let mut tip_claims = self.snapshot_tip_cover_claims();
                let mut effective_healthy =
                    Self::deep_tip_cover_count_from(&tip_claims, next_needed);
                // W49b: ahead walk-in already covers tip in-flight — promote before assigning
                // a competing tip owner (closes race: promote only on abort tick after W28d).
                // W111: skip cooldown peers (mute residual in-flight must not re-sticky).
                if wan_gap && effective_healthy == 0 {
                    if let Some((wp, ws, we)) =
                        Self::find_inflight_deep_covering(&guard, next_needed)
                    {
                        if !self.tip_owner_in_fail_cooldown(&wp) {
                            self.promote_tip_walk_in(&wp, ws, we);
                            tip_claims = self.snapshot_tip_cover_claims();
                            effective_healthy =
                                Self::deep_tip_cover_count_from(&tip_claims, next_needed);
                        }
                    }
                }
                // W28c/W32/W35‴: WAN tip owner deep pipe in one download session.
                // Near tip: tip-owner batch must match GetData pipe depth (default 128).
                // Live 2026-07-15: `wan_bulk_catchup` is true for most of IBD (header tip ≫
                // next_needed) so this path used **64** while `IBD_TIP_PIPE` showed
                // pipe_depth=128 span=64 — half the pipe idle; reassign cadence ~64/2.6s
                // ≈24 blk/s ceiling (observed tip ~17–34). Always use 128 on WAN tip owner
                // regardless of bulk; ahead partitions stay small (32) below.
                // Env `BLVM_IBD_GAP_PREEMPT_BATCH` overrides.
                let bulk = self.wan_bulk_catchup(next_needed);
                // W40: LOCAL_AHEAD soft-resume tip holes need a deep pipe too (bodies sparse
                // under body_tip). Live 2026-07-16: default 16 + chunk-start gate → 0 tip-owner
                // assigns, behind-tip main-queue storm, ~0.06 blk/s.
                // Synth bulk (GETDATA_DELAY_MS=0): bodies are already local — W40 tip-hole pipe
                // re-preempts the same spans 8–17×/s, feeder stays 0, wall ~6–8 blk/s (2026-07-23).
                // Keep W40 for real soft-resume and for synth tip-crawl with delay>0.
                let tip_missing = self.tip_gap_missing.load(Ordering::Relaxed);
                let local_tip_hole = !wan_gap
                    && tip_missing
                    && !(super::synthetic_wan::enabled()
                        && super::synthetic_wan::getdata_delay_ms() == 0);
                // Dense local W16 tip-fill (bootstrap / gap_preempt dens KEEP): body_tip=0
                // makes wan_tip_gap_crawl true for genesis stall/nudge, but without a
                // coordinator body/header tip and without tip_missing, tip-owner stays on
                // the non-tip_pipe batch=16 path (not WAN 128 / C1e stripe).
                let assign_wan_gap = wan_gap
                    && (tip_missing
                        || self.header_tip() > 0
                        || self.wan_body_tip.load(Ordering::Relaxed) > 0);
                // C1u: handoff_prime uses tip-pipe GetData depth while still local.
                let tip_pipe = assign_wan_gap || local_tip_hole || handoff_prime;
                // C1e: while tip missing, tip-owner takes a *stripe* (default 32), not 128.
                // Assigned tip..tip+127 with GetData depth 8 left frontier at tip+127 and
                // other peers opened tip+128 — multi-peer Swiss cheese. Multiple peers fill
                // contiguous stripes inside TIP_RUNWAY_CAP instead.
                let runway_cap = Self::tip_runway_cap();
                let runway_stripe = Self::tip_runway_stripe();
                // C1g/C1i: freeze past-tip stripes until tip is present AND contig runway
                // reaches min (default grow-start 8). C1h still cheese'd: tip lands 1 block
                // → ahead opens tip+32 → tip drains → TIP_HOLE_AHEAD (ahead_buf_p50=40).
                // C1i KEEP: freeze past-tip until contig≥8 (iter ~114 BPS). min_contig=0/1
                // reopened ahead and C1j-alone peerday collapsed ~4 BPS. C1j abort still
                // kills cheese GetData while tip missing. Env `BLVM_IBD_C1I_MIN_CONTIG`.
                //
                // C1q REVERT (2026-07-27): allowing ahead on tip-in-feeder / feeder≥min
                // reopened CHEESE on mute peerdays (cheese=4–11 vs C1p cheese=0). Public
                // WAN stays tip-owner-serial under C1i; squeeze via pipe depth / gd-fast.
                let contig_now = super::IBD_TIP_CONTIG_RUNWAY.load(Ordering::Relaxed);
                // Coordinator contig can lag unit tests / refresh; deep in-flight tip cover
                // already proves runway (w49 ahead after owner stripe).
                // Mousetrap F2 REVERT (2026-08-01): pipe_contig C1i flattened mid-gap wall
                // (365<390) and past-body tip_crawl 47<<B0 81 — restore claim credit.
                let contig_from_claims = tip_claims
                    .iter()
                    .map(|(_, s, e)| Self::claim_remaining_tip_depth(next_needed, *s, *e))
                    .max()
                    .unwrap_or(0);
                let contig_eff = contig_now.max(contig_from_claims);
                let min_contig_for_ahead = {
                    let raw = latch_env!(u64, {
                        std::env::var("BLVM_IBD_C1I_MIN_CONTIG")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(8u64)
                    });
                    raw.clamp(0, runway_stripe)
                };
                // Freeze past-tip only on real WAN assign (not dense-local body_tip=0 unit).
                let c1g_freeze_past_tip = assign_wan_gap
                    && (tip_missing
                        || (min_contig_for_ahead > 0 && contig_eff < min_contig_for_ahead));
                let holes_now = self.tip_bridge_holes.load(Ordering::Relaxed);

                // --- Tip owner: sticky/best peer (gated on healthy claims, not walk-ins) ---
                // W40: never skip tip owner at chunk-map starts when covering=0 — that gate
                // left LOCAL_AHEAD uncovered at every chunk boundary.
                let fetchers_cap = self.max_gap_fetchers_per_height();
                let healthy_claims = Self::healthy_tip_cover_count_from(&tip_claims, next_needed);
                // W65: shallow tip-cover remnants (deep==0, healthy>0) need a full deep
                // re-arm (≥64), not C1e stripe-32 (dens KEEP w65 expects end≥tip+63).
                let shallow_rearm = tip_missing && effective_healthy == 0 && healthy_claims > 0;
                let default_batch = if tip_pipe && assign_wan_gap && tip_missing && !shallow_rearm {
                    runway_stripe
                } else if tip_pipe {
                    128
                } else {
                    16
                };
                let mut preempt_batch: u64 = Self::gap_preempt_batch_raw()
                    .unwrap_or(default_batch)
                    .clamp(1, if tip_pipe { 256 } else { 128 });
                if tip_pipe && assign_wan_gap && tip_missing && !shallow_rearm {
                    // Explicit GAP_PREEMPT_BATCH must not re-open phantom 128 assign while tip empty.
                    preempt_batch = preempt_batch.min(runway_stripe);
                }
                // W47: tip-pipe shrink on holes is opt-in only (was default holes≥1 → 32 forever).
                if tip_pipe && Self::tip_pipe_shrink_holes_opt().is_some_and(|thr| holes_now >= thr)
                {
                    preempt_batch = preempt_batch.min(32);
                }
                let c1g_tip_race = wan_gap
                    && tip_missing
                    && super::tip_stage::tip_awaiting_secs_for_cap()
                        >= Self::c1g_tip_race_await_secs();
                let c1t_tip_race = wan_gap && tip_missing && self.c1t_tip_height_race();
                let tip_distress = Self::tip_is_distressed() || c1g_tip_race || c1t_tip_race;
                // W120: revert W117–W119 shallow_tip_cover / cover_for_gate. Live W117–W119
                // all rate-failed @306–311k (W116 DNA reached 344k). Accept CAP-soft wait
                // on end-of-pipe shallow cover (W116 @344580 ~9s) rather than early regress.
                // Keep overlaps_ok failover-first below (harmless on W116 path; required if
                // failover ever races a shallow remnant while deep cover exists).
                // W88: one failover per tip-*stall episode* (not per tip height).
                // W87 cleared on +1 advance → CAP at H then failover H,H+1,… every ~2s
                // (live 2026-07-18: 10× cascade, tip60 45→9).
                let failover_already = self.tip_failover_episode_active(next_needed);
                // W112: empty triple race may assign a second (H,H) despite W88 episode
                // latch — still hard-capped by fetchers_cap / raw_covering.
                let empty_triple = self.empty_tip_triple_race();
                // W122/W149: covering=1 mute + awaiting≥3s reopens one (H,H) under W88
                // latch without empty_triple covering=3 (W121 soft-resume regress).
                let mute_reopen = self.mute_single_cover_reopen(raw_covering);
                // W86: mute_reopen must not stack another (H,H) while a *deep* owner still
                // covers tip (failover peer dropped in-flight → raw_covering=1). Only reopen
                // under episode latch when deep cover is gone (true mute) or empty_triple.
                // TPP L1 REVERT: understudy pierce undone with peer_may C1g.
                let mute_reopen_open =
                    mute_reopen && effective_healthy == 0 && raw_covering < fetchers_cap;
                // empty_triple may open a second (H,H) under episode latch (w112/w153) even
                // with a deep owner — W86 stacking is blocked on the main-queue path instead.
                let failover_slot_open = !failover_already
                    || (empty_triple && raw_covering < fetchers_cap)
                    || mute_reopen_open;
                // W30: deep==0 must re-arm even with (H,H) micros present.
                // W41c/W47: failover only under tip distress (not standing hole-keyed race).
                // W86: also gate on raw_covering — healthy/deep counts ignore (H,H), so
                // distress + `overlaps_ok=failover` previously stacked unbounded tip micros
                // (live W85 fail: 2241× W28c tip failover on only 17 tip heights → tip60~30).
                // W87/W88: episode latch (advance≥32 or ~30s) — not one-per-height.
                // P1d/H6: GAP_STREAM DEDUP hold blocks tip-owner re-preempt (WAN + synth).
                let want_tip_owner = self.peer_may_take_tip_owner(peer_id, &guard, effective_healthy)
                    && (effective_healthy == 0
                        || (tip_distress
                            && healthy_claims < fetchers_cap
                            && raw_covering < fetchers_cap
                            && failover_slot_open))
                    && !self.tip_owner_blocked_by_dedup(next_needed)
                    // Dense local (!tip_pipe): mid-chunk W16 only. At chunk-map starts the
                    // main queue owns the full span (max_ahead=0 / A4 / sequential assign).
                    && (tip_pipe || !at_chunk_start);
                let _ = at_chunk_start;
                if want_tip_owner {
                    // W30/W37/W41c: (H,H) failover only while tip is in distress.
                    // W112: empty triple may open a second failover micro under fetchers_cap=3.
                    let failover = effective_healthy >= 1
                        && tip_distress
                        && failover_slot_open
                        && raw_covering < fetchers_cap;
                    let mut preempt_end = if failover {
                        // Failover races only the tip height — primary keeps the deep pipeline.
                        next_needed
                    } else {
                        next_needed.saturating_add(preempt_batch.saturating_sub(1))
                    };
                    // W32a/W40: tip-pipe (WAN gap or LOCAL_AHEAD hole) skips chunk-map clip.
                    if !failover && !tip_pipe {
                        // Dense local replay: clip to chunk-map boundaries.
                        if ce > next_needed {
                            preempt_end = preempt_end.min(ce);
                        } else if let Some((_, nce)) =
                            self.chunks.iter().find(|(s, _)| *s == ce + 1).copied()
                        {
                            preempt_end = preempt_end.min(nce);
                        }
                    }
                    // C1u: while still local, tip-fill must not claim past on-disk body tip
                    // even when tip_pipe=handoff_prime (batch 128). Past-tip warm is only
                    // via try_assign_handoff_prime — else next..next+127 spans the hole and
                    // abandons local cover (true-wan freeze class @437205).
                    if !failover && !wan_gap {
                        let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
                        if body_tip > 0 && next_needed <= body_tip {
                            preempt_end = preempt_end.min(body_tip);
                        }
                    }
                    // W35‴-h: never assign past stored headers (live: 912× "hash not found
                    // for height 957742" → mass blacklist → tip deadlock for hours).
                    let tip_headers_ok = match self.clip_end_to_headers(next_needed, preempt_end) {
                        Some((_, clipped)) => {
                            preempt_end = clipped;
                            true
                        }
                        None => false,
                    };
                    if !tip_headers_ok {
                        // next_needed past header tip — skip tip assign this poll.
                    } else {
                        // W28d: when healthy==0, allow overlap with walk-in ahead ranges — those
                        // peers will abort via should_abort_tip_walk_in. Only block on another
                        // tip-cover claim overlapping the new range.
                        let claim_overlap = {
                            let min_depth = Self::tip_deep_cover_min_depth();
                            let claims = self.tip_cover_claims.lock().unwrap();
                            claims.iter().any(|(_, s, e)| {
                                // W30/W37: (H,H) failover micro-claims must not block deep re-arm.
                                if *s == *e {
                                    return false;
                                }
                                // W65: shallow walk-promote remnants must not block a real
                                // deep tip pipe (live tip=218 claim 218-224 vs owner 218-345).
                                if !failover
                                    && Self::claim_remaining_tip_depth(next_needed, *s, *e)
                                        < min_depth
                                {
                                    return false;
                                }
                                *s <= preempt_end && next_needed <= *e
                            })
                        };
                        // W117: check failover first — `effective_healthy==0` + shallow claim
                        // made claim_overlap true under failover (shallow not skipped when
                        // failover=true) and blocked the (H,H) assign (live W116 @344580).
                        let overlaps_ok = if failover {
                            // W86: allow overlap with deep/shallow tip owner, but never stack a
                            // second (H,H) while raw covering already meets fetchers_cap.
                            raw_covering < fetchers_cap
                        } else if effective_healthy == 0 {
                            // Refuse overlap with another *deep* tip pipe (synth same-span
                            // storms). W28d/W65: shallow walk-in cover must NOT block deep
                            // re-arm — raw_covering==0 froze owner behind promote remnants
                            // (live tip=218 claim 218-224 → owner only got (H,H)).
                            !claim_overlap
                                && Self::find_inflight_deep_covering(&guard, next_needed).is_none()
                        } else {
                            !Self::range_overlaps_inflight(&guard, next_needed, preempt_end)
                        };
                        // P1c: never re-preempt tip onto a peer that already holds tip in-flight
                        // (even failover (H,H) or top_peer max_in_flight=2). Ahead dual-pipe OK.
                        let peer_already_tip =
                            Self::peer_holds_tip_inflight(&guard, peer_id, next_needed);
                        if preempt_end >= next_needed && overlaps_ok && !peer_already_tip {
                            let walk_in = raw_covering > effective_healthy;
                            Self::insert_in_flight(&mut guard, peer_id, next_needed, preempt_end);
                            self.note_tip_cover_claim(peer_id, next_needed, preempt_end);
                            if effective_healthy == 0 {
                                drop(guard);
                                self.note_tip_owner_assigned(peer_id);
                                if wan_gap || local_tip_hole {
                                    let score = self
                                        .peer_scores
                                        .lock()
                                        .unwrap()
                                        .get(peer_id)
                                        .copied()
                                        .unwrap_or(0.0);
                                    tracing::debug!(
                                        "[IBD_TIP_PEER] owner={} score={:.3} span={}-{} ({})",
                                        peer_id,
                                        score,
                                        next_needed,
                                        preempt_end,
                                        if wan_gap {
                                            "W33 top-peer pipe"
                                        } else {
                                            "W40 local tip-hole pipe"
                                        }
                                    );
                                    if wan_gap {
                                        self.log_wan_tip_owner_ready(
                                            peer_id,
                                            next_needed,
                                            preempt_end,
                                        );
                                    }
                                }
                                if walk_in {
                                    tracing::debug!(
                                        "gap preempt: assigning {}-{} to {} (W28d tip owner after walk-in preempt, raw_covering={})",
                                        next_needed,
                                        preempt_end,
                                        peer_id,
                                        raw_covering
                                    );
                                } else {
                                    tracing::debug!(
                                        "gap preempt: assigning {}-{} to {} (W28c tip owner, covering={}/{})",
                                        next_needed,
                                        preempt_end,
                                        peer_id,
                                        1,
                                        self.max_gap_fetchers_per_height()
                                    );
                                }
                                return Some((next_needed, preempt_end));
                            }
                            self.latch_tip_failover_episode(next_needed);
                            tracing::debug!(
                                "gap preempt: assigning {}-{} to {} (W28c tip failover{}, covering={}/{})",
                                next_needed,
                                preempt_end,
                                peer_id,
                                if c1t_tip_race { " C1t" } else { "" },
                                raw_covering.saturating_add(1),
                                self.max_gap_fetchers_per_height()
                            );
                            return Some((next_needed, preempt_end));
                        }
                    } // tip_headers_ok
                }

                // --- Ahead partition: any free peer, non-overlapping after tip frontier ---
                // Require a *healthy* tip cover (not walk-in) before handing out more ahead.
                //
                // C1g supersedes A6g "ahead while gap_missing": opening tip+32 while tip
                // empty produced TIP_HOLE_AHEAD binder (C1f). Multi-peer ahead runs when
                // tip is present in reorder (`!tip_missing`); between tip bodies the tip
                // owner stripe + optional `(H,H)` race fill the hole.
                //
                // W47: gate on deep tip cover + tip distress, not bridge holes alone.
                // Soft-retry freezes multi-peer ahead; late-body alone does not when tip
                // already has healthy cover (2026-07-31 W102b/late-body narrow).
                let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed);
                let feeder_len = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
                if c1g_freeze_past_tip {
                    static C1G_FREEZE_LOG: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let prev = C1G_FREEZE_LOG.load(Ordering::Relaxed);
                    if now.saturating_sub(prev) >= 5
                        && C1G_FREEZE_LOG
                            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        tracing::warn!(
                            "[IBD_C1G_FREEZE] tip={} covering={} ready={} — past-tip stripes frozen while tip missing; race tip H only",
                            next_needed,
                            raw_covering,
                            self.ibd_ready_peer_count()
                        );
                    }
                }
                // Mousetrap F3 REVERT (2026-08-01): secondary under tip_missing + pipe
                // solid → past-body cheese=45 / tip_crawl regress. C1g freeze stays absolute.
                if assign_wan_gap
                    && !c1g_freeze_past_tip
                    && effective_healthy > 0
                    && self.wan_allow_multi_peer_ahead(effective_healthy, feeder_len)
                    && super::tip_stage::tip_soft_retries() == 0
                    && !super::tip_stage::tip_ahead_frozen_for_soft_retry()
                    && self.preferred_tip_owner().as_deref() == Some(peer_id)
                    && self.tip_sticky_usable(peer_id)
                {
                    // Sticky dual-pipe after tip lands (C1g: not while tip_missing).
                    let sticky_window: u64 = latch_env!(u64, {
                        std::env::var("BLVM_IBD_STICKY_PIPE_WINDOW")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(512)
                            .clamp(256, 2048)
                    });
                    // Mousetrap F1 REVERT: sticky frontier = claim pipeline (pre-F KEEP).
                    let frontier = Self::tip_pipeline_frontier(&guard, next_needed, sticky_window);
                    let assign_f = frontier;
                    let pipe_f = super::tip_stage::pipe_frontier(next_needed);
                    let body_f = if contig_now > 0 {
                        next_needed.saturating_add(contig_now.saturating_sub(1))
                    } else {
                        next_needed.saturating_sub(1)
                    };
                    let sticky_batch: u64 =
                        Self::gap_preempt_batch_raw().unwrap_or(128).clamp(64, 256);
                    let band_end = next_needed.saturating_add(sticky_window);
                    let part_start = frontier.saturating_add(1);
                    let part_end_raw = part_start
                        .saturating_add(sticky_batch.saturating_sub(1))
                        .min(band_end);
                    // Manual REVERT 2026-08-03: part_start>tip gate cratered tip90 (c1ggate
                    // 77.8 / c1gclaim 100 vs forcedeb 108.9). Tip reassigns were accidental
                    // re-arm when claims outlived in_flight; fix is stale-claim prune above,
                    // not blocking C1g when frontier falls to tip-1.
                    if part_start <= band_end {
                        if let Some((_, part_end)) =
                            self.clip_end_to_headers(part_start, part_end_raw)
                        {
                            if part_start <= max_start
                                && part_end >= part_start
                                && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                && !self.chunk_range_in_flight(&guard, part_start, part_end)
                            {
                                Self::insert_in_flight(&mut guard, peer_id, part_start, part_end);
                                Self::log_pipe_f(
                                    next_needed,
                                    assign_f,
                                    pipe_f,
                                    body_f,
                                    peer_id,
                                    "sticky",
                                );
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (C1g sticky after tip, tip={}, frontier={}, holes={})",
                                    part_start,
                                    part_end,
                                    peer_id,
                                    next_needed,
                                    frontier,
                                    self.tip_bridge_holes.load(Ordering::Relaxed),
                                );
                                return Some((part_start, part_end));
                            }
                        }
                    }
                }
                // Dense-local (wan crawl with body_tip=0 / no headers): use LOCAL ahead
                // gating — WAN distress/contig freeze must not starve dens KEEP partitions.
                // During HANDOFF_PRIME, freeze local ahead too: multi-peer W28c partitions
                // through body tip while sticky primes past tip leave Swiss-cheese holes
                // (true-wan freeze @437205 / wait 437206, reorder≈108, tip60 never armed).
                let allow_ahead = if assign_wan_gap {
                    !c1g_freeze_past_tip
                        && self.wan_allow_multi_peer_ahead(effective_healthy, feeder_len)
                } else {
                    !gap_missing && !handoff_prime
                };
                if effective_healthy > 0 && allow_ahead {
                    // Multi-peer ahead after tip lands (C1g). Contiguous assign frontier
                    // still used so stripes do not jump holes.
                    let runway_end = next_needed.saturating_add(runway_cap.saturating_sub(1));
                    let part_window: u64 = if assign_wan_gap {
                        Self::tip_partition_window_raw()
                            .unwrap_or(256)
                            .clamp(64, 512)
                    } else {
                        Self::tip_partition_window_raw()
                            .unwrap_or(512)
                            .clamp(64, 2048)
                    };
                    // Mousetrap F1 REVERT (2026-08-01): stripe frontier = claim contig
                    // (pre-F KEEP). Still log assign_F vs pipe_F for forensics.
                    let assign_f_wan = if assign_wan_gap {
                        Self::tip_contiguous_assign_frontier(&guard, next_needed, runway_end)
                    } else {
                        0
                    };
                    let pipe_f_wan = if assign_wan_gap {
                        super::tip_stage::pipe_frontier(next_needed)
                    } else {
                        0
                    };
                    let body_f_wan = if contig_now > 0 {
                        next_needed.saturating_add(contig_now.saturating_sub(1))
                    } else {
                        next_needed.saturating_sub(1)
                    };
                    let frontier = if assign_wan_gap {
                        // Tip present: walk contiguous in-flight from tip inside runway,
                        // then legacy window past runway.
                        let contig =
                            Self::tip_contiguous_assign_frontier(&guard, next_needed, runway_end);
                        if contig >= runway_end {
                            Self::tip_pipeline_frontier(&guard, next_needed, part_window)
                        } else {
                            contig
                        }
                    } else {
                        Self::tip_pipeline_frontier(&guard, next_needed, part_window)
                    };
                    let part_start = frontier.saturating_add(1);
                    if part_start <= max_start && part_start >= next_needed {
                        let ahead_peers = guard
                            .iter()
                            .filter(|(p, ranges)| {
                                p.as_str() != peer_id
                                    && ranges.iter().any(|(s, _)| *s > next_needed)
                            })
                            .count();
                        let holes_now = self.tip_bridge_holes.load(Ordering::Relaxed);
                        let ahead_cap = if assign_wan_gap {
                            let floor_stall = self.preferred_is_floor_sticky()
                                && self
                                    .sticky_recent_bps(next_needed, a6m_recent_window_secs())
                                    .map(|(bps, _, elapsed)| {
                                        elapsed >= (a6m_recent_window_secs() as f64) * 0.8
                                            && bps < a6m_floor_open_slot_min_bps()
                                    })
                                    .unwrap_or(false);
                            // W123: sticky hole-band freeze (not raw holes — W47).
                            self.tip_ahead_hole_band_update(feeder_len);
                            let tip_distress = Self::tip_is_distressed()
                                || self.tip_ahead_hole_freeze.load(Ordering::Relaxed);
                            // No ahead_cap bypass on under-target (W3c ahead flood FAIL; W3 REVERT).
                            if floor_stall || tip_distress {
                                0
                            } else {
                                Self::tip_ahead_peer_cap()
                            }
                        } else {
                            usize::MAX
                        };
                        if assign_wan_gap && ahead_peers >= ahead_cap {
                            // Tip owner / retry only — ahead cap reached.
                        } else if assign_wan_gap {
                            // C1g: tip already in reorder here; stripe from contiguous frontier.
                            let ahead_batch = runway_stripe.min(32u64.min(preempt_batch));
                            let part_end = part_start.saturating_add(ahead_batch.saturating_sub(1));
                            if part_end >= part_start
                                && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                && !self.chunk_range_in_flight(&guard, part_start, part_end)
                            {
                                Self::insert_in_flight(&mut guard, peer_id, part_start, part_end);
                                Self::log_pipe_f(
                                    next_needed,
                                    assign_f_wan,
                                    pipe_f_wan,
                                    body_f_wan,
                                    peer_id,
                                    "stripe",
                                );
                                tracing::debug!(
                                    "gap preempt: assigning {}-{} to {} (C1g runway stripe, tip={}, frontier={}, ahead_peers={}, holes={}, runway={})",
                                    part_start,
                                    part_end,
                                    peer_id,
                                    next_needed,
                                    frontier,
                                    ahead_peers + 1,
                                    holes_now,
                                    runway_cap
                                );
                                return Some((part_start, part_end));
                            }
                        } else {
                            // Non-WAN: ahead partitions stay at 32 with chunk-map clip.
                            // C1u: never multi-peer past body tip (live cheese: ahead
                            // GetData'd 304672 while tip=304418 → TIP_HOLE_AHEAD cliff).
                            // When frontier reaches body tip, tip-owner primes WAN GetData.
                            let body_tip = self.wan_body_tip.load(Ordering::Relaxed);
                            if body_tip > 0 && part_start > body_tip {
                                if !super::synthetic_wan::bulk_local_disk_stream() {
                                    if let Some(range) = self.try_assign_handoff_prime(
                                        peer_id,
                                        next_needed,
                                        &mut guard,
                                        "ahead_frontier",
                                    ) {
                                        drop(guard);
                                        return Some(range);
                                    }
                                }
                            } else {
                                // C1u: far-local fill to body tip — allow up to 256 so two
                                // max_in_flight=1 peers can cover HANDOFF_PRIME without
                                // completes (dens KEEP c1u_local_ahead…). Default stays 32.
                                let ahead_batch = if body_tip > 0 && part_start <= body_tip {
                                    body_tip
                                        .saturating_sub(part_start)
                                        .saturating_add(1)
                                        .min(256)
                                        .max(preempt_batch.min(32))
                                } else {
                                    preempt_batch.min(32)
                                };
                                let mut part_end =
                                    part_start.saturating_add(ahead_batch.saturating_sub(1));
                                if body_tip > 0 {
                                    part_end = part_end.min(body_tip);
                                }
                                if let Some((_, pe)) = self
                                    .chunks
                                    .iter()
                                    .find(|(s, e)| *s <= part_start && part_start <= *e)
                                    .copied()
                                {
                                    part_end = part_end.min(pe);
                                }
                                if part_end >= part_start
                                    && !Self::range_overlaps_inflight(&guard, part_start, part_end)
                                    && !self.chunk_range_in_flight(&guard, part_start, part_end)
                                {
                                    Self::insert_in_flight(
                                        &mut guard, peer_id, part_start, part_end,
                                    );
                                    tracing::debug!(
                                        "gap preempt: assigning {}-{} to {} (W28c ahead partition, tip={}, frontier={})",
                                        part_start,
                                        part_end,
                                        peer_id,
                                        next_needed,
                                        frontier
                                    );
                                    return Some((part_start, part_end));
                                }
                            }
                        }
                    }
                }
            }
        }

        // Try retry queue first (critical chunk, then earliest).
        //
        // IMPORTANT: retry-queue chunks are NOT filtered by max_start. These are stall-recovery
        // chunks — the coordinator explicitly decided they're needed to unblock progress. Applying
        // the max_ahead window to retry chunks causes a deadlock when the missing chunk starts just
        // past max_start: validation stalls (can't advance), max_start can't grow (validation stuck),
        // and the chunk can never be taken (max_start check fails). The retry_queue is always small
        // (0–1 entries in practice), so skipping the window check here poses no memory risk.
        {
            let mut retry = self.retry_queue.lock().unwrap();
            // W4: prefer multi-block critical ranges that cover next_needed BEFORE single-height
            // micros. Live WAN: gap-preempt + micro-first drained the pipeline into 1-block
            // GetData storms (~0.3 BPS). Bulk GetData recovers network efficiency; keep micros
            // as fallback when no bulk critical is queued.
            let critical = retry.iter().enumerate().find(|(_, (s, e, ex))| {
                *s <= next_needed
                    && next_needed <= *e
                    && *s != *e
                    && ex.as_ref() != Some(&peer_id.to_string())
                    && allow_chunk(*s)
            });
            if let Some((i, _)) = critical {
                let (start, end, ex) = retry.remove(i).unwrap();
                // H6: synth tip already GAP_STREAM'd — do not re-download via retry either.
                if start <= next_needed
                    && next_needed <= end
                    && self.tip_owner_blocked_by_dedup(next_needed)
                {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if !self.peer_may_take_wan_gap_retry(peer_id, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if self.chunk_range_in_flight(&guard, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                Self::insert_in_flight(&mut guard, peer_id, start, end);
                // Critical retry covering tip = tip claim (WAN gap: owner only — W31d).
                self.maybe_note_tip_cover_claim_retry(peer_id, start, end);
                return Some((start, end));
            }
            // Gap micro-chunks (H,H) — coalesce consecutive singles into a merged range.
            //
            // Micro-batch coalescing: instead of returning a single-block (H,H) range and
            // making the worker issue a separate GetData round-trip per block, we scan the retry
            // queue for consecutive single-block micro-chunks and return a merged (H, H+k) range.
            // This converts N serial RTTs into a single pipelined GetData for up to
            // MICRO_BATCH_COALESCE blocks — the download function naturally handles the range.
            //
            // Coalescing is safe because:
            //  - All heights in [H, H+k] are already present in the retry queue (assigner owns them).
            //  - The merged range replaces the N individual entries atomically (single lock).
            //  - On failure the whole range is re-queued, preserving stall-recovery semantics.
            //  - The exclude filter from the first entry applies to the merged range only;
            //    remaining entries in the queue may have different excludes (handled by rotation).
            const MICRO_BATCH_COALESCE: u64 = 16; // merge up to 16 consecutive micro-chunks
            let peer_str = peer_id.to_string();
            let gap_micro_start = retry
                .iter()
                .enumerate()
                .filter(|(_, (s, e, ex))| {
                    *s == *e
                        && *s >= next_needed
                        && ex.as_ref() != Some(&peer_str)
                        && allow_chunk(*s)
                })
                .min_by_key(|(_, (s, _, _))| *s)
                .map(|(i, _)| i);
            if let Some(i) = gap_micro_start {
                // W32e: on WAN gap, non-owner peers must not coalesce retry micros on the tip.
                let wan_gap_retry = self.wan_tip_gap_crawl(next_needed);
                let non_owner_micro = wan_gap_retry
                    && self
                        .preferred_tip_owner()
                        .is_some_and(|o| o.as_str() != peer_id);
                if non_owner_micro {
                    // fall through — owner or main queue only
                } else if self.peer_ok_for_gap_race(peer_id) {
                    let (start, _end, _ex) = retry.remove(i).unwrap();
                    if !self.peer_may_take_wan_gap_retry(peer_id, start, start) {
                        retry.push_back((start, start, _ex));
                        return None;
                    }
                    if self.chunk_range_in_flight(&guard, start, start) {
                        retry.push_back((start, start, _ex));
                        return None;
                    }
                    // Coalesce consecutive (H+1,H+1), (H+2,H+2), ... micro-chunks for this peer.
                    // Scan for consecutive heights; remove each found entry by value (one entry
                    // per height) to avoid index-shifting bugs when removing from a VecDeque.
                    let mut merged_end = start;
                    while merged_end - start < MICRO_BATCH_COALESCE - 1 {
                        let next_h = merged_end + 1;
                        let found_idx = retry.iter().position(|(s, e, ex)| {
                            *s == next_h
                                && *e == next_h
                                && ex.as_ref() != Some(&peer_str)
                                && allow_chunk(next_h)
                        });
                        if let Some(idx) = found_idx {
                            retry.remove(idx);
                            merged_end = next_h;
                        } else {
                            break;
                        }
                    }
                    Self::insert_in_flight(&mut guard, &peer_str, start, merged_end);
                    self.maybe_note_tip_cover_claim_retry(&peer_str, start, merged_end);
                    if merged_end > start {
                        tracing::debug!(
                            "micro-batch coalesced {}-{} ({} blocks) for peer {}",
                            start,
                            merged_end,
                            merged_end - start + 1,
                            peer_id
                        );
                    }
                    return Some((start, merged_end));
                }
            }
            let ok_gap = self.peer_ok_for_gap_race(peer_id);
            let candidate = retry
                .iter()
                .enumerate()
                .filter(|(_, (_, _, ex))| ex.as_ref() != Some(&peer_id.to_string()))
                .filter(|(_, (s, _, _))| allow_chunk(*s))
                // P5: bottom-quartile peers skip single-height micros in the fallback path too.
                .filter(|(_, (s, e, _))| ok_gap || *s != *e)
                .min_by_key(|(_, (s, _, _))| *s);
            if let Some((i, _)) = candidate {
                let (start, end, ex) = retry.remove(i).unwrap();
                if !self.peer_may_take_wan_gap_retry(peer_id, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                if self.chunk_range_in_flight(&guard, start, end) {
                    retry.push_back((start, end, ex));
                    return None;
                }
                Self::insert_in_flight(&mut guard, peer_id, start, end);
                return Some((start, end));
            }
        }

        // Main queue — try the next sequential chunk.
        //
        // Peer binding: enforced for LAN/single-peer modes so a fast LAN peer isn't displaced by
        // slow WAN peers stealing its pre-assigned chunks. For WAN multi-peer (work_stealing=true),
        // binding is skipped — any free peer takes the next available chunk, giving us work-stealing
        // semantics that maximize throughput when peers have heterogeneous speeds.
        let idx = self.next_index.load(Ordering::Relaxed);
        if idx >= self.chunks.len() {
            return None;
        }
        // Peer binding check: skip in work_stealing mode (WAN multi-peer).
        if !self.work_stealing
            && idx < self.preferred_peers.len()
            && self.preferred_peers[idx] != peer_id
        {
            return None;
        }
        let (start, end) = self.chunks[idx];
        if self.chunk_range_in_flight(&guard, start, end) {
            return None;
        }
        // Allow the chunk that contains the next validation height even when max_ahead is 0
        // (near-tip catch-up). Otherwise workers poll forever and the retry queue never drains.
        if start > max_start && start > next_needed {
            return None;
        }
        // W16: while tip is uncovered **and tip is inside the chunk map**, do not hand out
        // far main-queue work — force peers through tip fill / retry first (live: 685057
        // assigned with need=684805). Skip when next_needed is outside the map (tests /
        // pre-start) so sequential assignment still works.
        let tip_in_map = self
            .chunks
            .iter()
            .any(|(s, e)| *s <= next_needed && next_needed <= *e);
        if tip_in_map {
            let tip_band: u64 = latch_env!(u64, {
                std::env::var("BLVM_IBD_TIP_ASSIGN_BAND")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(64)
                    .clamp(16, 256)
            });
            let covering_next = Self::covering_next_count(&guard, next_needed);
            let healthy = self.healthy_tip_cover_count(next_needed);
            let gap_missing = self.tip_gap_missing.load(Ordering::Relaxed);
            // W40/W80: never assign a chunk that ends entirely behind tip (even when
            // gap_missing is briefly false). Live loop-1: 309798-309925 recirculated
            // while tip≈321k via main-queue/retry while tip hole looked covered.
            if end < next_needed {
                return None;
            }
            // C1g/C1h: while tip body missing, block ANY main-queue start past tip
            // (was tip_band=64 → tip+32.. still assigned → cheese / TIP_HOLE_AHEAD).
            // Tip fill + (H,H) race own the hole; ahead stripes reopen when tip lands.
            if gap_missing && start > next_needed {
                return None;
            }
            // W25a/W28d: when tip covered but healthy==0, still keep far main-queue off.
            if !gap_missing && healthy == 0 && start > next_needed.saturating_add(tip_band) {
                return None;
            }
            // P1c: peer already holding tip in-flight must not take tip-covering MQ/(H,H).
            if start <= next_needed
                && next_needed <= end
                && Self::peer_holds_tip_inflight(&guard, peer_id, next_needed)
            {
                return None;
            }
            // W28c: a main-queue chunk that covers the tip *is* tip ownership — do not hand it
            // to a peer that failed sticky/best tip selection (live: slow peer took 1000-1200
            // via main queue while "fast" still had capacity).
            if healthy == 0
                && start <= next_needed
                && next_needed <= end
                && !self.peer_may_take_tip_owner(peer_id, &guard, healthy)
            {
                return None;
            }
            // H6: tip-owner DEDUP hold must also block main-queue tip-covering reassign
            // (tip fill skips W28c, then main queue re-issued the same span — live short).
            if start <= next_needed
                && next_needed <= end
                && self.tip_owner_blocked_by_dedup(next_needed)
            {
                return None;
            }
            let _ = covering_next; // retained for possible future diag in this path
        }
        if !allow_chunk(start) {
            return None;
        }
        // C1g/C1h: while tip missing on WAN, never hand the map-span chunk as ahead cheese.
        // - No deep owner yet → clip to tip runway stripe (owner path).
        // - Deep owner already → tip-height `(H,H)` only (race), not tip..header_tip.
        let mut assign_start = start;
        let mut assign_end = end;
        if self.tip_gap_missing.load(Ordering::Relaxed)
            && self.wan_tip_gap_crawl(next_needed)
            && start <= next_needed
            && next_needed <= end
        {
            let healthy_mq = self.healthy_tip_cover_count(next_needed);
            if healthy_mq >= 1 {
                // W86 dens KEEP: tip-fill owns distress failover micros + W88 episode latch.
                // MQ `(H,H)` bypassed fetchers_cap/episode and re-stacked tip micros after
                // the failover peer dropped in-flight (claim remained, tip-fill blocked).
                if self.tip_failover_episode_active(next_needed)
                    || Self::covering_next_count(&guard, next_needed)
                        >= self.max_gap_fetchers_per_height()
                {
                    return None;
                }
                assign_start = next_needed;
                assign_end = next_needed;
            } else {
                let stripe_end =
                    next_needed.saturating_add(Self::tip_runway_stripe().saturating_sub(1));
                assign_end = assign_end.min(stripe_end);
            }
        }
        if self.chunk_range_in_flight(&guard, assign_start, assign_end) {
            return None;
        }
        self.next_index.store(idx + 1, Ordering::Relaxed);
        Self::insert_in_flight(&mut guard, peer_id, assign_start, assign_end);
        // W28d: main-queue tip-covering work is an explicit tip claim (not an ahead walk-in).
        if assign_start <= next_needed && next_needed <= assign_end {
            self.note_tip_cover_claim(peer_id, assign_start, assign_end);
            if assign_end > assign_start {
                self.note_tip_owner_assigned(peer_id);
                self.log_wan_tip_owner_ready(peer_id, assign_start, assign_end);
            }
        }
        Some((assign_start, assign_end))
    }

    /// Called when a worker completes (or fails) a chunk. Clears in-flight so peer can get next chunk.
    pub(crate) fn on_chunk_complete(&self, peer_id: &str) {
        // Pop one in-flight range (A4 may hold two). Callers complete one chunk at a time.
        let mut g = self.in_flight_per_peer.lock().unwrap();
        if let Some(v) = g.get_mut(peer_id) {
            if let Some((s, e)) = v.pop() {
                drop(g);
                self.clear_tip_cover_claim(peer_id, s, e);
                return;
            }
            if v.is_empty() {
                g.remove(peer_id);
            }
        }
    }

    /// TRUE WAN tip_crawl: after download complete, keep tip-cover claim only while tip
    /// still lies in the span **and** tip body is already present (`!tip_gap_missing`).
    /// Blind keep (tc2ck) blocked re-fetch while tip still missing → tip90≈44.
    /// Present-only keep (tc9ck2) tip90≈102 but FORCE/gd_max worse vs tc4gpu — opt-in.
    fn wan_tip_claim_keep_enabled() -> bool {
        matches!(
            std::env::var("BLVM_IBD_WAN_TIP_CLAIM_KEEP")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    }

    pub(crate) fn on_chunk_complete_range(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.in_flight_per_peer.lock().unwrap();
        if let Some(v) = g.get_mut(peer_id) {
            if let Some(i) = v.iter().position(|(s, e)| *s == start && *e == end) {
                v.remove(i);
            }
            if v.is_empty() {
                g.remove(peer_id);
            }
        }
        drop(g);
        let next = self.next_needed_height();
        // Drop claims tip has walked past (kept claims from prior stripes).
        {
            let mut claims = self.tip_cover_claims.lock().unwrap();
            claims.retain(|(_, _, e)| *e >= next);
        }
        let tip_present = !self.tip_gap_missing.load(Ordering::Relaxed);
        let wan_keep = Self::wan_tip_claim_keep_enabled()
            && self.wan_tip_gap_crawl(next)
            && !super::synthetic_wan::bulk_local_disk_stream()
            && tip_present
            && start <= next
            && next <= end;
        if wan_keep {
            tracing::debug!(
                "[IBD_WAN_TIP_CLAIM_KEEP] peer={} {}-{} next_needed={} — tip present, cover until advance",
                peer_id,
                start,
                end,
                next
            );
        } else {
            self.clear_tip_cover_claim(peer_id, start, end);
        }
        // W91: CHUNK_OBSOLETE returns Ok (not Err), so the download worker never called
        // note_tip_owner_failed. Sticky preferred stayed pinned to a peer that just
        // released a behind-tip pipe → covering=0 / no TIP_PEER re-arm for tens of
        // seconds (live W90b: obsolete 315327-315454 then validation freeze @315613).
        //
        // Synth bulk: do not TIP_OWNER_OPEN on obsolete — clear sticky only.
        // Keep-claim + OPEN both tip-crawled ignition again (2026-07-23 keepclaim2:
        // first block ~11s, wall ~5). Burst path is Case C inject-without-rewind +
        // tip-reserve 1ms sleep (not yield_now).
        let synth_bulk = super::synthetic_wan::bulk_local_disk_stream();
        if end < next {
            let mut pref = self.preferred_tip_owner.lock().unwrap();
            if pref.as_deref() == Some(peer_id) {
                *pref = None;
                drop(pref);
                if !synth_bulk {
                    self.tip_owner_open.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "[IBD_TIP_OWNER_OPEN] after obsolete/behind-tip complete {}-{} peer={} next_needed={}",
                        start,
                        end,
                        peer_id,
                        next
                    );
                }
            }
        }
    }

    /// Synth recovery: drop tip-cover claims that outlived delivery (download complete kept
    /// the claim while tip never entered reorder/bridge — Case C waited tip_inflight_grace
    /// with covering≥1 → LOCAL_GAP ~1/10s → ~6 BPS).
    pub(crate) fn clear_tip_cover_claims_for_synth_recovery(&self, next_needed: u64) {
        if !super::synthetic_wan::bulk_local_disk_stream() {
            return;
        }
        let mut g = self.tip_cover_claims.lock().unwrap();
        let before = g.len();
        g.retain(|(_, s, e)| !(*s <= next_needed && next_needed <= *e));
        if g.len() != before {
            tracing::warn!(
                "[IBD_SYNTH_TIP_CLAIM_CLEAR] dropped tip-cover claim(s) at next_needed={} \
                 (tip absent from reorder/bridge/feeder)",
                next_needed
            );
        }
    }

    /// Tip-owner must not re-download a tip already GAP_STREAM'd (synth **and** WAN).
    ///
    /// Synth short-band (2026-07-26): local-disk finishes tip chunks in tens of ms, clears
    /// in_flight, then W28c reassigns the same span (~9.8k assigns / 10k heights).
    /// True-WAN dens-hash160 iter10k (2026-07-28): same class — `same_start` W28c p50≈19ms
    /// (P1c only blocks while in_flight still held; obsolete→complete clears it).
    ///
    /// Escape: `tip_gap_missing` for rearm grace **only when the tip is not already in
    /// validation**. Env: `BLVM_IBD_TIP_DEDUP_REARM_MS` (fallback `BLVM_IBD_SYNTH_DEDUP_REARM_MS`,
    /// default 5000).
    fn tip_owner_blocked_by_dedup(&self, next_needed: u64) -> bool {
        let dedup = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
        if dedup < next_needed {
            self.synth_tip_dedup_block_since_ms
                .store(0, Ordering::Relaxed);
            return false;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Tip already in validation — not a lost body. Keep DEDUP hold and refresh the
        // grace clock so a slow AV=0 height cannot accumulate into a false REARM.
        if super::tip_stage::tip_taken_by_validation(next_needed) {
            self.synth_tip_dedup_block_since_ms
                .store(now_ms.max(1), Ordering::Relaxed);
            return true;
        }
        // Default 5s: kill complete→reassign storms (tens of ms); true tip-loss (not
        // tip_taken) can still recover via tip-owner. LOCAL_GAP remains the primary path.
        let grace_ms: u64 = std::env::var("BLVM_IBD_TIP_DEDUP_REARM_MS")
            .ok()
            .or_else(|| std::env::var("BLVM_IBD_SYNTH_DEDUP_REARM_MS").ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(5_000)
            .clamp(250, 30_000);
        let since = self.synth_tip_dedup_block_since_ms.load(Ordering::Relaxed);
        if since == 0 {
            self.synth_tip_dedup_block_since_ms
                .store(now_ms.max(1), Ordering::Relaxed);
            return true;
        }
        let waited = now_ms.saturating_sub(since);
        if waited >= grace_ms && self.tip_gap_missing.load(Ordering::Relaxed) {
            // Rewind DEDUP so the hold does not immediately re-engage on the same tip.
            let rewind = next_needed.saturating_sub(1);
            let _ = super::memory::GAP_STREAM_DEDUP_HEIGHT.compare_exchange(
                dedup,
                rewind,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            tracing::warn!(
                "[IBD_TIP_DEDUP_REARM] tip={} dedup={}→{} after {}ms gap_missing — \
                 allow tip-owner re-download",
                next_needed,
                dedup,
                rewind,
                waited
            );
            self.synth_tip_dedup_block_since_ms
                .store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Backward-compatible alias for unit tests / older call sites.
    #[cfg(test)]
    fn synth_tip_owner_blocked_by_dedup(&self, next_needed: u64) -> bool {
        self.tip_owner_blocked_by_dedup(next_needed)
    }

    pub(crate) fn requeue(&self, start: u64, end: u64, exclude_peer: Option<String>) {
        // W80: ChunkGuard Drop re-queues every aborted range. Behind-tip spans
        // (live: 309798-309925 while tip≈321k) recirculated as W40 "local tip-hole"
        // owner work and stole peers from the real tip. Drop obsolete ranges.
        let next = self.next_needed_height();
        if end < next {
            return;
        }
        // Use exclude_peer to avoid immediate retry with same peer, but stall recovery can clear it
        self.retry_queue
            .lock()
            .unwrap()
            .push_back((start, end, exclude_peer));
    }

    /// Drop retry-queue entries entirely behind the live tip (W80).
    fn purge_obsolete_retries(&self, next_needed: u64) {
        let mut rq = self.retry_queue.lock().unwrap();
        let before = rq.len();
        if before == 0 {
            return;
        }
        rq.retain(|(_, e, _)| *e >= next_needed);
        let dropped = before.saturating_sub(rq.len());
        if dropped > 0 {
            tracing::warn!(
                "[IBD_RETRY_PURGE] dropped {} obsolete behind-tip retry chunk(s) (next_needed={})",
                dropped,
                next_needed
            );
        }
    }

    pub(crate) fn next_needed_height(&self) -> u64 {
        self.validation_height.load(Ordering::Relaxed) + 1
    }

    /// W17/W28d diagnostics: `(raw_covering, in_flight_ranges, busy_peers)`.
    /// Prefer [`Self::tip_flight_diag_healthy`] when distinguishing walk-ins.
    pub(crate) fn tip_flight_diag(&self) -> (usize, usize, usize) {
        let next_needed = self.next_needed_height();
        let guard = self.in_flight_per_peer.lock().unwrap();
        let covering = Self::covering_next_count(&guard, next_needed);
        let ranges = guard.values().map(|v| v.len()).sum();
        let busy = guard.values().filter(|v| !v.is_empty()).count();
        (covering, ranges, busy)
    }

    /// W28d: `(healthy_tip_claims, raw_covering, in_flight_ranges)`.
    pub(crate) fn tip_flight_diag_healthy(&self) -> (usize, usize, usize) {
        let next_needed = self.next_needed_height();
        let healthy = self.healthy_tip_cover_count(next_needed);
        let guard = self.in_flight_per_peer.lock().unwrap();
        let raw = Self::covering_next_count(&guard, next_needed);
        let ranges = guard.values().map(|v| v.len()).sum();
        (healthy, raw, ranges)
    }

    /// Enqueue single-height micro-chunks at the front of the retry queue so multiple peers can
    /// race missing blocks without re-downloading entire 128-block chunks.
    ///
    /// `count` is not clamped — callers that know the full remaining range of a failed chunk
    /// (e.g. [`Self::requeue_stall_gaps`]) pass the exact block count so recovery covers the
    /// entire chunk in one stall event rather than drip-feeding 8 blocks per event.
    pub(crate) fn requeue_gap_heights(&self, from: u64, count: u64, exclude: Option<String>) {
        let count = count.max(1);
        let mut rq = self.retry_queue.lock().unwrap();
        let mut added = 0u64;
        for h in from..from.saturating_add(count) {
            if rq
                .iter()
                .any(|(s, e, ex)| *s == h && *e == h && ex == &exclude)
            {
                continue;
            }
            rq.push_front((h, h, exclude.clone()));
            added += 1;
        }
        if added > 0 {
            tracing::warn!(
                "stall recovery: gap micro-chunks {}-{} ({} new, up to {} fetchers/height)",
                from,
                from.saturating_add(count).saturating_sub(1),
                added,
                self.max_gap_fetchers_per_height()
            );
        }
    }

    pub(crate) fn requeue_gap_height(&self, height: u64) {
        self.requeue_gap_heights(height, 1, None);
    }

    /// Coordinator stall recovery: dispatch single-height micro-chunks for the full remaining
    /// range of the chunk containing `height`.
    ///
    /// **Why single-height micro-chunks (not one big range):**
    /// Single-height micro-chunks are fault-tolerant — each block is independently retried by
    /// a different peer. One big multi-block range fails atomically: if any block in the range
    /// stalls, the whole range re-queues and generates another stall, causing cascading recovery.
    ///
    /// `get_work()` coalesces consecutive `(H,H)` singles into a merged `(H, H+k)` range before
    /// assignment, so the network efficiency (16-block GetData batches) is recovered without
    /// sacrificing fault tolerance.
    ///
    /// `exclude` — pass `Some(peer_id)` when a specific peer caused the drop so it can't
    /// immediately re-claim the heights it just failed on.
    pub(crate) fn requeue_stall_gaps(&self, height: u64, exclude: Option<String>) {
        self.requeue_stall_gaps_inner(height, exclude, false);
    }

    /// W24: bypass debounce when tip covering collapsed to 0 (nothing in flight for the gap).
    pub(crate) fn requeue_stall_gaps_force(&self, height: u64, exclude: Option<String>) {
        self.requeue_stall_gaps_inner(height, exclude, true);
    }

    fn requeue_stall_gaps_inner(&self, height: u64, exclude: Option<String>, force: bool) {
        let wan_gap = self.wan_tip_gap_crawl(height);
        // Real WAN assign (body/header tip set): non-force stall stays preempt/nudge/SLA-only.
        // Dense-local unit (body_tip=0, header_tip=0) still enqueues stall micros — dens KEEP
        // requeue_chunk / debounce / exclude-clear tests rely on that path.
        let assign_wan =
            wan_gap && (self.wan_body_tip.load(Ordering::Relaxed) > 0 || self.header_tip() > 0);
        // P0-B: non-force WAN tip recovery stays preempt/nudge/SLA-only.
        // W73: force + covering==0 arms a single (H,H) tip hole (genesis 262716 hard stall).
        if assign_wan && !force {
            tracing::debug!(
                "stall recovery: skipped WAN tip gap height {} (gap preempt/nudge/SLA only)",
                height
            );
            return;
        }
        if assign_wan && force {
            let (covering, _, _) = self.tip_flight_diag();
            if covering > 1 {
                tracing::debug!(
                    "stall recovery: skip WAN force at {} — tip covering={}",
                    height,
                    covering
                );
                return;
            }
            let debounce = Self::stall_requeue_debounce();
            let mut last = self.last_stall_requeue.lock().unwrap();
            if let Some((h, t)) = *last {
                if t.elapsed() < debounce {
                    // Dens parity (2026-08-03): debounce was same-height only, so tip
                    // 400288→289→290 each got a fresh (H,H) force while covering flapped 0
                    // (nudgegate FORCE×32 vs grow80 dens FORCE×0). Cross-height suppress
                    // inside the window; true stall (same height after debounce) still arms.
                    tracing::debug!(
                        "stall recovery: debounced WAN force requeue for height {} (last={} within {}s)",
                        height,
                        h,
                        debounce.as_secs()
                    );
                    return;
                }
            }
            *last = Some((height, Instant::now()));
            drop(last);
            // W73: covering=0 → single (H,H). Stripe-32 FORCE (2026-08) bypasses C1g
            // "race tip H only" and re-cheeses TIP_HOLE_AHEAD (Land E soak 12 FORCE@403747
            // cratered dens 53/30; soak 15 C1G@402000 with ahead_buf=68). C1e owner stripe
            // still assigns via get_work when covering=0; recovery must not enqueue tip+31.
            tracing::warn!(
                "[IBD_WAN_TIP_FORCE_REQUEUE] height {} covering=0 — enqueue tip H (H,H)",
                height
            );
            self.requeue_gap_heights(height, 1, exclude);
            return;
        }
        let debounce = Self::stall_requeue_debounce();
        let mut last = self.last_stall_requeue.lock().unwrap();
        if !force {
            if let Some((h, t)) = *last {
                if h == height && t.elapsed() < debounce {
                    tracing::debug!(
                        "stall recovery: debounced requeue for height {} (within {}s)",
                        height,
                        debounce.as_secs()
                    );
                    return;
                }
            }
        }
        *last = Some((height, Instant::now()));
        let containing = self
            .chunks
            .iter()
            .find(|(s, e)| height >= *s && height <= *e)
            .copied();
        let Some((start, end)) = containing else {
            tracing::warn!(
                "stall recovery: height {} not in any assigner chunk (chunks={})",
                height,
                self.chunks.len()
            );
            // No containing chunk found — fall back to a small batch so recovery doesn't stall
            // forever on a gap that slipped out of the chunk map (e.g. very old retry).
            self.requeue_gap_heights(height, self.gap_micro_chunk_batch(), exclude);
            return;
        };
        // W4 / W9: enqueue one bulk multi-block range first. Do NOT also flood (H,H) micros
        // when bulk is queued — live: peer took bulk then 13 separate (H,H) assigns in 1ms
        // (coalesce lost across get_work cycles). Keep a single (H,H) race only when bulk
        // cannot be formed (narrow_count == 1).
        let narrow_count = 16u64.min(end.saturating_sub(height).saturating_add(1));
        if narrow_count > 1 {
            let bulk_end = height.saturating_add(narrow_count.saturating_sub(1));
            let mut rq = self.retry_queue.lock().unwrap();
            let already = rq
                .iter()
                .any(|(s, e, ex)| *s == height && *e == bulk_end && ex == &exclude);
            if !already {
                rq.push_front((height, bulk_end, exclude.clone()));
                tracing::warn!(
                    "stall recovery: bulk gap chunk {}-{} ({} blocks)",
                    height,
                    bulk_end,
                    narrow_count
                );
            }
            // One (H,H) race for the exact gap alongside bulk — no H+1..H+15 micro flood.
            drop(rq);
            self.requeue_gap_heights(height, 1, exclude.clone());
        } else {
            self.requeue_gap_heights(height, 1, exclude.clone());
        }
        // Also clear the exclude on any existing full-chunk retry so a different peer can take
        // the full chunk in parallel with the micro-chunks.
        let mut rq = self.retry_queue.lock().unwrap();
        if let Some(entry) = rq.iter_mut().find(|(s, e, _)| *s == start && *e == end) {
            if entry.2.is_some() {
                entry.2 = None;
                tracing::warn!(
                    "stall recovery: cleared exclude on chunk {}-{} for missing height {}",
                    start,
                    end,
                    height
                );
            }
        }
    }

    /// When validation/coordinator stalls on a missing height, workers may have no in-flight chunk
    /// covering that height (chunk was already marked complete after a bad download).
    pub(crate) fn requeue_chunk_containing_height(&self, height: u64) {
        self.requeue_stall_gaps(height, None);
    }

    pub(crate) fn is_done(&self) -> bool {
        // Explicit teardown or IBD end reached: workers must exit so Phase 3 / join
        // cannot hang. Mid-IBD tip-crawl keep-alive below must NOT apply past end.
        if self.shutdown.load(Ordering::Acquire) || self.validation_reached_ibd_end() {
            return true;
        }
        // W36: keep workers alive while tip is missing or we are past body tip — otherwise
        // free workers exit on empty main queue and only the stuck tip owner remains
        // (live: covering=1 zombie, no reassign for 17 min after SLA).
        if self.tip_gap_missing.load(Ordering::Relaxed) {
            return false;
        }
        let next = self.next_needed_height();
        if self.wan_tip_gap_crawl(next) {
            return false;
        }
        {
            let guard = self.in_flight_per_peer.lock().unwrap();
            if guard.values().any(|v| !v.is_empty()) {
                return false;
            }
        }
        let idx = self.next_index.load(Ordering::Relaxed);
        idx >= self.chunks.len() && self.retry_queue.lock().unwrap().is_empty()
    }

    pub(crate) fn total_chunks(&self) -> usize {
        self.chunks.len()
    }

    pub(crate) fn remaining_count(&self) -> usize {
        let idx = self.next_index.load(Ordering::Relaxed);
        let retry_len = self.retry_queue.lock().unwrap().len();
        self.chunks.len().saturating_sub(idx) + retry_len
    }
}
