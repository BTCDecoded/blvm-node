impl ChunkAssigner {
    /// Freeze multi-peer ahead when bridge holes ≥ N (optional hard gate).
    ///
    /// **W47:** holes alone ≠ tip distress (live: holes≈22 in both slow and fast
    /// buckets). **W123–W125:** sticky feeder-empty latch
    /// (`tip_ahead_hole_freeze`) — set `BLVM_IBD_TIP_AHEAD_MAX_HOLES=0` to disable.
    fn tip_ahead_max_holes_opt() -> Option<u64> {
        latch_env!(Option<u64>, {
            match std::env::var("BLVM_IBD_TIP_AHEAD_MAX_HOLES") {
                Ok(s) if s == "0" || s.eq_ignore_ascii_case("off") => None,
                Ok(s) => s.parse().ok().map(|v: u64| v.clamp(1, 512)),
                // W125: restore arm **24**. W124 arm=16 froze 58% of early TIP_CRAWL
                // samples → crawl≈25 h/s, fail @312k tip60=10.5.
                Err(_) => Some(24),
            }
        })
    }

    /// Clear sticky freeze only below this hole count (hysteresis).
    /// Env: `BLVM_IBD_TIP_AHEAD_HOLE_CLEAR` (default **8**).
    fn tip_ahead_hole_clear_opt(arm: u64) -> u64 {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_HOLE_CLEAR")
                .ok()
                .and_then(|s| s.parse().ok())
                // W125: keep clear@8 (W123 clear@12 released at holes=10–11 → W35 flood).
                // Do not default to arm/2 (that reverts to clear@12 when arm=24).
                .unwrap_or(8)
        });
        raw.clamp(4, arm.saturating_sub(1).max(4))
    }

    /// Sticky ahead-freeze while tip-band holes stay fat and feeder is empty.
    /// Survives tip+1 late-body clock resets.
    /// **W125:** arm **24** / clear **8**. **W181:** distress arm (default 16) when
    /// tip awaiting ≥ `BLVM_IBD_TIP_AHEAD_DISTRESS_AWAIT_SECS`. **W183:** feeder-empty
    /// clear is debounced (`BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS`, default 5s).
    fn tip_ahead_hole_band_update(&self, feeder_len: usize) {
        let holes = self.tip_bridge_holes.load(Ordering::Relaxed);
        let Some(arm_default) = Self::tip_ahead_max_holes_opt() else {
            self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
            return;
        };
        let distress_arm = {
            let raw = latch_env!(u64, {
                std::env::var("BLVM_IBD_TIP_AHEAD_DISTRESS_HOLES")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(16u64)
            });
            raw.clamp(8, arm_default)
        };
        let distress_await_secs = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_DISTRESS_AWAIT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3u64)
                .clamp(0, 30)
        });
        let awaiting = super::tip_stage::tip_awaiting_secs_for_cap();
        let arm = if awaiting >= distress_await_secs {
            arm_default.min(distress_arm)
        } else {
            arm_default
        };
        let clear = Self::tip_ahead_hole_clear_opt(arm_default);
        // W183: require holes < clear for debounce before releasing sticky (default **5s**).
        // Live W182 330–345k: holes oscillated 0↔24 → freeze clear every tip+1 → W35 flood.
        // Env: `BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS` (0 = immediate).
        let clear_debounce_ms = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_AHEAD_HOLE_CLEAR_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5_000)
                .clamp(0, 30_000)
        });
        let now_ms = Self::unix_now_ms();
        if holes >= arm && feeder_len == 0 {
            self.tip_ahead_hole_freeze.store(true, Ordering::Relaxed);
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
        } else if holes < clear {
            // Feeder runway ⇒ tip is draining — release immediately (W125 / W143).
            // Debounce only when feeder stays empty (hole oscillation across tip+1).
            if !self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
                // already clear
            } else if feeder_len > 0 || clear_debounce_ms == 0 {
                self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
                self.tip_ahead_hole_clear_since_ms
                    .store(0, Ordering::Relaxed);
            } else {
                let since = self.tip_ahead_hole_clear_since_ms.load(Ordering::Relaxed);
                if since == 0 {
                    self.tip_ahead_hole_clear_since_ms
                        .store(now_ms.max(1), Ordering::Relaxed);
                } else if now_ms.saturating_sub(since) >= clear_debounce_ms {
                    self.tip_ahead_hole_freeze.store(false, Ordering::Relaxed);
                    self.tip_ahead_hole_clear_since_ms
                        .store(0, Ordering::Relaxed);
                }
            }
        } else {
            // Mid-band holes while frozen — keep latch; cancel clear countdown.
            self.tip_ahead_hole_clear_since_ms
                .store(0, Ordering::Relaxed);
        }
        // W135: debounce-clear weak sticky whether or not freeze latched.
        self.nudge_weak_sticky();
    }

    /// W130/W132: preferred tip sticky is credible under hole-freeze (mid+ / holding tip).
    /// Floor stickies are not — open tip slot for STREAM/mid re-arm while ahead stays frozen.
    fn tip_owner_credible_for_hole_freeze(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if !self.tip_sticky_usable(&pref) {
            return false;
        }
        if self.peer_score_of(&pref) > Self::TIP_OWNER_MID_SCORE {
            return true;
        }
        self.peer_holds_tip_download(&pref, self.next_needed_height())
    }

    /// W132: `BLVM_IBD_WEAK_STICKY_OPEN_MS` (default **15000**, clamp 0–60000).
    fn weak_sticky_open_debounce_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WEAK_STICKY_OPEN_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15_000)
                .clamp(0, 60_000)
        })
    }

    /// W130/W132/W135: under hole-freeze, drop unusable/floor sticky and open tip slot
    /// (debounced). Ahead freeze stays latched — only tip ownership is unlocked.
    fn nudge_weak_sticky(&self) {
        if !self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
            return;
        }
        // Function-static debounce clocks (tipfix binary: WEAK_SINCE_MS / LAST_CLEAR_MS).
        static WEAK_SINCE_MS: AtomicU64 = AtomicU64::new(0);
        static LAST_CLEAR_MS: AtomicU64 = AtomicU64::new(0);
        if self.tip_owner_credible_for_hole_freeze() {
            WEAK_SINCE_MS.store(0, Ordering::Relaxed);
            return;
        }
        let now = Self::unix_now_ms();
        let debounce_ms = Self::weak_sticky_open_debounce_ms();
        let since = WEAK_SINCE_MS.load(Ordering::Relaxed);
        if since == 0 {
            WEAK_SINCE_MS.store(now.max(1), Ordering::Relaxed);
            if debounce_ms > 0 {
                return; // first sample arms countdown only (W132)
            }
        } else if debounce_ms > 0 {
            if now.saturating_sub(since) < debounce_ms {
                return;
            }
            // Rate-limit repeat clears.
            let last = LAST_CLEAR_MS.load(Ordering::Relaxed);
            if last > 0 && now.saturating_sub(last) < debounce_ms {
                return;
            }
        }
        let pref = self.preferred_tip_owner();
        if pref.is_none() && self.tip_owner_open.load(Ordering::Relaxed) {
            return;
        }
        if let Some(ref p) = pref {
            tracing::warn!(
                "[IBD_TIP_WEAK_STICKY_OPEN] peer={} score={:.3} — hole-freeze unlocks tip slot",
                p,
                self.peer_score_of(p)
            );
            let mut g = self.preferred_tip_owner.lock().unwrap();
            if g.as_deref() == Some(p.as_str()) {
                *g = None;
            }
        }
        LAST_CLEAR_MS.store(now.max(1), Ordering::Relaxed);
        WEAK_SINCE_MS.store(0, Ordering::Relaxed);
        self.open_tip_owner_slot();
    }

    /// Optional emergency: shrink tip-owner preempt batch when holes ≥ N.
    ///
    /// **W47 default: unset / disabled.** Former default **1** permanently shrunk the
    /// tip pipe 128→32 on mid-chain WAN (holes==0 only ~7% of samples).
    /// Env: `BLVM_IBD_TIP_PIPE_SHRINK_HOLES`.
    fn tip_pipe_shrink_holes_opt() -> Option<u64> {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_PIPE_SHRINK_HOLES")
                .ok()
                .and_then(|s| s.parse().ok())
                .map(|v: u64| v.clamp(1, 512))
        })
    }

    /// C1g/C1h: after this many seconds of tip await while tip missing, arm `(H,H)`
    /// failover (W88 episode latch still caps storms). Default **0** (immediate) —
    /// C1g iter with await=2 left fetchers_cap=1 under deep stripe → ~3 BPS EMPTY_TIP.
    /// Env `BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS` (clamp 0–30).
    fn c1g_tip_race_await_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_C1G_TIP_RACE_AWAIT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                .clamp(0, 30)
        })
    }

    /// C1t: tip-height `(H,H)` race after tip missing this many ms (default **120**).
    /// Good-day gd≈90 ms — 250 ms rarely armed (C1t@250 soak: covering≈1, wall≈331).
    /// Integer-second soft-retry / late-body freeze never arm on mid-gaps.
    /// `0` = off. Clamp 0–2000. Mute guard: also requires gd-fast elevated.
    fn c1t_tip_race_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_C1T_TIP_RACE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120)
                .clamp(0, 2_000)
        })
    }

    /// C1t: open one tip-height racer (no past-tip ahead). W88 episode still applies.
    fn c1t_tip_height_race(&self) -> bool {
        let ms = Self::c1t_tip_race_ms();
        if ms == 0 {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        // Mute peerday guard — same gate as C1n grow-fast (EWMA cold → no race).
        if super::download::tip_hole_grow_cap_effective() <= super::download::tip_hole_grow_cap() {
            return false;
        }
        let awaiting = super::tip_stage::tip_awaiting_ms_for_cap();
        if awaiting >= ms {
            return true;
        }
        // Phase 2 mid-gap: covering≥1 + feeder=0 but await clock stuck at 0 (stamp reset
        // while tip never lands). Live EMPTY_TIP SLOW_STRETCH had covering=1 await_ms=0
        // gd_ewma~180 — classic C1t never armed. Debounce ~80ms to avoid W172 storms.
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        if feeder > 0 || awaiting > 0 {
            return false;
        }
        let tip = self.next_needed_height();
        let covering = self.healthy_tip_cover_count(tip);
        // Cap is often 1 until tip_distress (C1t) raises it — don't gate on
        // max_gap_fetchers here or cold-clock never arms when covering=1/cap=1.
        if covering == 0 || covering >= 2 {
            return false;
        }
        static LAST_COLD: Mutex<Option<Instant>> = Mutex::new(None);
        let mut g = LAST_COLD.lock().unwrap();
        if let Some(t) = *g {
            if t.elapsed() < Duration::from_millis(80) {
                return false;
            }
        }
        *g = Some(Instant::now());
        tracing::warn!(
            "[IBD_C1T_COLD_CLOCK] tip={} covering={} await_ms=0 feeder=0 — tip-height race",
            tip,
            covering
        );
        true
    }

    /// True tip distress for race / ahead-cap (not bridge sparsity).
    #[inline]
    fn tip_is_distressed() -> bool {
        super::tip_stage::tip_ahead_frozen_for_soft_retry()
            || super::tip_stage::tip_ahead_frozen_for_late_body()
    }

    /// W112: empty-bridge tip starve → allow covering=3 (deep + 2× `(H,H)`).
    ///
    /// Live W111 @323780 / W110 @326324: covering=2 mute rotate still ~20–25s while
    /// a later peer STREAM'd tip in <1s. Escalate only after **two** mute CAP
    /// windows (default **12s**) — live W112a with trigger=5s / W121 with 8s opened
    /// covering=3 during soft-resume and collapsed tip60 (W121 peak~47, fail @318k).
    /// Keep **12s**. W122 opens a *single* `(H,H)` earlier via `mute_single_cover_reopen`
    /// (covering=1 + awaiting≥5s) without raising fetchers to 3.
    /// Env: `BLVM_IBD_EMPTY_TIP_TRIPLE_SECS` (clamp 5–30).
    fn empty_tip_triple_race(&self) -> bool {
        // Tipfix KEEP (2026-07-31): tip missing + awaiting≥trigger → covering=3.
        // W153: holey (BRIDGE_PENDING>0) still opens triple at ≥12s — do **not** gate
        // on pending==0 (that over-froze holey mute peerdays).
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let trigger = latch_env!(u64, {
            std::env::var("BLVM_IBD_EMPTY_TIP_TRIPLE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(12)
                .clamp(5, 30)
        });
        super::tip_stage::tip_awaiting_secs_for_cap() >= trigger
    }

    /// W122/W149: W88 episode latched + single mute tip cover → reopen one `(H,H)`.
    /// Live W120b @326368: no failover until empty_triple @12s; STREAM then &lt;200ms.
    /// Does **not** raise fetchers_cap to 3 (W121 8s triple thrash).
    ///
    /// **W149:** default trigger **5→3s**. Live W148 @329995–998 tip-stepped ~1h/5s
    /// with covering=1: late-body distress already true (≥2s) but W88 episode stayed
    /// latched and mute_reopen@5s was knife-edge with the dribble interval → no
    /// `(H,H)` race. Env: `BLVM_IBD_MUTE_SINGLE_REOPEN_SECS` (clamp 2–12).
    fn mute_single_cover_reopen(&self, raw_covering: usize) -> bool {
        if raw_covering != 1 {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let trigger = latch_env!(u64, {
            std::env::var("BLVM_IBD_MUTE_SINGLE_REOPEN_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
                .clamp(2, 12)
        });
        super::tip_stage::tip_awaiting_secs_for_cap() >= trigger
    }

    /// W88: tip heights that must advance before another distress failover (default 32).
    fn tip_failover_episode_advance() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_FAILOVER_EPISODE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(32)
                .clamp(1, 256)
        })
    }

    /// W88: max age of a failover episode before re-arm (default 30s).
    fn tip_failover_episode_ms() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_FAILOVER_EPISODE_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30_000)
                .clamp(5_000, 120_000)
        })
    }

    #[inline]
    fn unix_now_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// W88: clear / query distress-failover episode latch.
    /// Returns true if a failover was already assigned in the current episode.
    fn tip_failover_episode_active(&self, next_needed: u64) -> bool {
        let latch_h = self.tip_failover_once_h.load(Ordering::Relaxed);
        if latch_h == 0 {
            return false;
        }
        let latch_at = self.tip_failover_once_at_ms.load(Ordering::Relaxed);
        let now = Self::unix_now_ms();
        let advanced = next_needed >= latch_h.saturating_add(Self::tip_failover_episode_advance());
        let aged = latch_at != 0 && now.saturating_sub(latch_at) >= Self::tip_failover_episode_ms();
        if advanced || aged {
            self.tip_failover_once_h.store(0, Ordering::Relaxed);
            self.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn latch_tip_failover_episode(&self, next_needed: u64) {
        self.tip_failover_once_h
            .store(next_needed, Ordering::Relaxed);
        self.tip_failover_once_at_ms
            .store(Self::unix_now_ms(), Ordering::Relaxed);
    }

    /// Max non-owner peers with in-flight ranges past tip on WAN (default **8**).
    /// Env: `BLVM_IBD_TIP_AHEAD_PEERS`. Prior hard-cap 3 left ready≈52 idle at ~5 blk/s.
    fn tip_ahead_peer_cap() -> usize {
        latch_env!(usize, {
            std::env::var("BLVM_IBD_TIP_AHEAD_PEERS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8)
                .clamp(1, 24)
        })
    }

    /// WAN tip-band multi-peer ahead: require deep tip cover; freeze on tip distress / starve.
    ///
    /// Do **not** key off steady-state `gap_missing` alone (`!reorder.contains(next_needed)`),
    /// which is true on every tip poll between receives and permanently disabled ahead
    /// (A6g+W31 single-pipe → ~5 blk/s).
    ///
    /// **W47:** do **not** freeze on bridge `holes` by default — that metric is ahead-OOO
    /// sparsity and stays high while tip is healthy (architecture doc). Freeze on:
    /// soft-retry (W31), late tip body (W42). Optional `BLVM_IBD_TIP_AHEAD_MAX_HOLES` rollback.
    ///
    /// WAN tip-band multi-peer ahead: require deep tip cover; freeze on tip distress.
    ///
    /// Do **not** key off steady-state `gap_missing` / brief `feeder==0` (A6g / W61
    /// regressions). Freeze on soft-retry (W31) and late tip body (W42).
    fn wan_allow_multi_peer_ahead(&self, effective_healthy: usize, feeder_len: usize) -> bool {
        if effective_healthy == 0 {
            return false;
        }
        // Soft-retry still freezes ahead (real tip failover / W31).
        // W102b + late-body ahead freeze narrowed (2026-07-31 tipfix DNA):
        // `feeder==0 && awaiting≥3s` (W102b) and late-body (≥2s) both match *steady-state*
        // single tip-owner WAN crawl. STREAM hole-storm protection remains:
        //   • C1g freezes past-tip stripes while tip is missing
        //   • tip_ahead_hole_band_update latches on awaiting≥3s ∧ holes≥distress (W181)
        //   • soft-retry / tip-SLA rotate a stuck owner
        // Do **not** hard-block multi-peer ahead on awaiting/late-body alone when tip
        // already has healthy cover.
        if super::tip_stage::tip_ahead_frozen_for_soft_retry() {
            self.tip_ahead_hole_band_update(feeder_len);
            return false;
        }
        // W123: sticky freeze — holes≥arm + feeder empty latches until holes < clear.
        // Do **not** hard-gate on holes alone while feeder>0 (W47: holes≠distress).
        self.tip_ahead_hole_band_update(feeder_len);
        if self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
            return false;
        }
        true
    }

    /// W28c: remember who owns the tip pipeline after a successful assign.
    pub(crate) fn note_tip_owner_assigned(&self, peer_id: &str) {
        let mut g = self.preferred_tip_owner.lock().unwrap();
        let prev = g.clone();
        *g = Some(peer_id.to_string());
        drop(g);
        self.tip_owner_open.store(false, Ordering::Relaxed);
        // W33: start tip-SLA clock when owner takes WAN gap work (coordinator mark_needed may lag).
        if self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::mark_needed(self.next_needed_height());
            // Start tenure when sticky carried from LOCAL_AHEAD with the same peer —
            // peer *change* alone left WAN tenure None (A6m / sticky BPS blind).
            let need_tenure = prev.as_deref() != Some(peer_id)
                || self.sticky_wan_tenure.lock().unwrap().is_none();
            if need_tenure {
                self.reset_sticky_wan_tenure(peer_id, self.next_needed_height());
            }
        }
    }

    /// Current sticky tip owner (if any).
    pub(crate) fn preferred_tip_owner(&self) -> Option<String> {
        self.preferred_tip_owner.lock().unwrap().clone()
    }

    /// W28c: clear sticky owner after tip-covering failure so the next best peer can take over.
    pub(crate) fn note_tip_owner_failed(&self, peer_id: &str) {
        self.note_tip_owner_failed_with_cooldown(peer_id, Self::tip_owner_fail_cooldown_secs());
    }

    /// W103: mute tip-gap CAP abort — shorter cooldown + clear W88 failover episode so a
    /// (H,H) racer can arm immediately. Live W102b: 15s cooldown + episode latch left
    /// covering=1 mute peer for 8s×N inside tip60-watch 20s.
    ///
    /// **W111:** also `force_release_peer_inflight` — live W110 @326324 walk-promoted the
    /// mute-failed peer from residual in-flight in the same ms as mute (`TIP_WALK_PROMOTE`
    /// after `TIP_FAILOVER` armed), re-pinning sticky and burning another 5–15s.
    pub(crate) fn note_tip_owner_failed_mute(&self, peer_id: &str) {
        // P1e: tip-role ban after mute (PIPE_FILL / CAP). Default **120s** (was 5s) so
        // TIP_PIN cannot re-elect the mute hero; clamp 60–180. Failover still arms now.
        let secs = std::env::var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(120)
            .clamp(60, 180);
        // Drop W88 episode so want_tip_owner can assign failover micro on the next poll.
        self.tip_failover_once_h.store(0, Ordering::Relaxed);
        self.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
        self.note_tip_owner_failed_with_cooldown(peer_id, secs);
        self.force_release_peer_inflight(peer_id);
        // C1c: mute hero must not keep a deep sticky tip-hole pipe.
        self.reset_tip_hole_depth(peer_id);
        if self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
            tracing::warn!(
                "[IBD_TIP_FAILOVER] armed after mute CAP on tip {} (cleared W88 episode)",
                self.next_needed_height()
            );
        }
    }

    fn note_tip_owner_failed_with_cooldown(&self, peer_id: &str, cooldown_secs: u64) {
        let mut g = self.preferred_tip_owner.lock().unwrap();
        if g.as_deref() == Some(peer_id) {
            // Keep / re-pin forced tip owner — clearing lets ahead peer steal tip (tc168).
            if let Some(forced) = super::sole_tip_forced_owner() {
                *g = Some(forced);
            } else {
                *g = None;
            }
        }
        drop(g);
        // W92: soft cooldown so TIP_PIN / top_w cannot re-elect the CAP-aborted peer.
        // Sole ready peer skip lives inside [`Self::mark_tip_owner_fail_cooldown`].
        self.mark_tip_owner_fail_cooldown(peer_id, cooldown_secs);
        // Primary failed — allow a second covering peer immediately (also armed from soft-retry).
        // W31: never arm failover on WAN tip gap — keeps max_gap_fetchers at 1.
        // W103 mute path arms failover explicitly via [`Self::note_tip_owner_failed_mute`].
        if !self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
        } else {
            // W36: open tip slot so any top-half peer can re-arm (not only the failed top-1).
            self.tip_owner_open.store(true, Ordering::Relaxed);
        }
        self.clear_tip_cover_claims_for_peer(peer_id);
    }

    /// Default **15s** — escapes CAP same-peer thrash (W91) without burning the mid-score
    /// peer pool into score=0.001 open-slot lottery (live W93 @314596).
    fn tip_owner_fail_cooldown_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_OWNER_FAIL_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(15)
            .clamp(5, 300)
    }

    fn mark_tip_owner_fail_cooldown(&self, peer_id: &str, secs: u64) {
        // Mode T dual: never cool the forced tip pin (first BLVM_IBD_PEERS) — cooldown
        // lets TIP_PIN elect the ahead loopback peer (tc168 tip90≈7.8).
        if super::sole_tip_forced_owner().as_deref() == Some(peer_id) {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN_SKIP] peer={} — sole_tip forced owner (was {}s)",
                peer_id,
                secs
            );
            return;
        }
        // Mode T: assigner often has workers=6 slots but only one IBD-ready archive.
        // Cooldown on that peer freezes covering=0 (tc64/tc65). Skip when no alternate.
        if self.ibd_ready_peer_count() <= 1
            || self.any_ready_active_worker_except(peer_id).is_none()
        {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN_SKIP] peer={} — sole ready tip peer (was {}s)",
                peer_id,
                secs
            );
            return;
        }
        let until = Instant::now() + Duration::from_secs(secs);
        self.tip_owner_fail_until
            .lock()
            .unwrap()
            .insert(peer_id.to_string(), until);
        tracing::warn!(
            "[IBD_TIP_OWNER_COOLDOWN] peer={} {}s — skip tip-owner / TIP_PIN",
            peer_id,
            secs
        );
    }

    /// True while peer is inside a W92 tip-owner fail cooldown (expired entries purged).
    fn tip_owner_in_fail_cooldown(&self, peer_id: &str) -> bool {
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        match g.get(peer_id).copied() {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                g.remove(peer_id);
                false
            }
            None => false,
        }
    }

    /// W128/W137: clear tip-owner fail cooldowns for mid+ peers (score > MID).
    /// Floor stickies stay cooled — mute thrash must not re-elect score=0.1 heroes.
    fn clear_mid_plus_tip_owner_fail_cooldowns(&self) -> usize {
        let scores = self.peer_scores.lock().unwrap().clone();
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        let before = g.len();
        g.retain(|peer, _| scores.get(peer).copied().unwrap_or(0.0) <= Self::TIP_OWNER_MID_SCORE);
        before.saturating_sub(g.len())
    }

    /// Covering=0: if every mid+ worker is fail-cooled, clear mid+ cooldowns so TIP_PIN
    /// / open-slot can re-arm (live mute CAP lockout). Logs `[IBD_TIP_MID_COOLDOWN_CLEAR]`.
    fn maybe_clear_mid_plus_fail_cooldowns_covering0(&self, tip: u64) {
        // Any mid+ exists (ignore cooldown) but none are live → clear mid+ cooldowns.
        if self
            .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, true)
            .is_none()
        {
            return;
        }
        if self
            .active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, false)
            .is_some()
        {
            return;
        }
        let cleared = self.clear_mid_plus_tip_owner_fail_cooldowns();
        if cleared > 0 {
            tracing::warn!(
                "[IBD_TIP_MID_COOLDOWN_CLEAR] tip={} cleared={}",
                tip,
                cleared
            );
        }
    }

    /// E15: GD_SLOW ROTATE A→B cools A for 180s; OPEN on B 60s later finds no pin
    /// because A (only tip hero) is still fail-cooled — and `clear_mid_plus` skips
    /// score≫MID peers. Drop fail-cooldowns for everyone except `keep`.
    fn clear_tip_owner_fail_cooldowns_except(&self, keep: &str) -> usize {
        let mut g = self.tip_owner_fail_until.lock().unwrap();
        let before = g.len();
        g.retain(|peer, _| peer == keep);
        before.saturating_sub(g.len())
    }

    /// Clear preferred tip sticky when it is not usable (not ready / not worker / blacklisted)
    /// and open the tip slot. Returns true if sticky was dropped.
    ///
    /// Live wan10k: preferred stayed on a disconnected hero →
    /// [`Self::peer_may_take_wan_gap_retry`] refused every living peer → FORCE_REQUEUE
    /// `(H,H)` spun with covering=0 forever (nudge drop alone is rate-limited / not on
    /// the retry path).
    fn drop_unusable_preferred_tip_sticky(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if self.tip_sticky_usable(&pref) {
            return false;
        }
        // Mode T dual: never drop the forced tip pin to None (TIP_PIN would elect ahead).
        if super::sole_tip_forced_owner().as_deref() == Some(pref.as_str()) {
            tracing::warn!(
                "[IBD_TIP_STICKY_DROP_SKIP] sticky={} — sole_tip forced owner (score={:.3})",
                pref,
                self.peer_score_of(&pref)
            );
            return false;
        }
        tracing::warn!(
            "[IBD_TIP_STICKY_DROP] sticky={} score={:.3} — not usable for tip (ready/worker/blacklist)",
            pref,
            self.peer_score_of(&pref)
        );
        let mut g = self.preferred_tip_owner.lock().unwrap();
        if g.as_deref() == Some(pref.as_str()) {
            *g = None;
        }
        drop(g);
        self.open_tip_owner_slot();
        true
    }

    /// W29/W36 SLA: clear sticky owner, release zombie in-flight, re-arm SLA, open tip slot
    /// so the next best live peer can take a deep pipeline.
    pub(crate) fn rotate_tip_owner_on_sla(&self) -> Option<String> {
        // Mode T dual: SLA rotate must not clear the forced tip pin (tc168 steal).
        if let Some(forced) = super::sole_tip_forced_owner() {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            let prev = g.clone();
            *g = Some(forced.clone());
            drop(g);
            self.clear_all_tip_cover_claims();
            if let Some(ref p) = prev {
                if p != &forced {
                    self.force_release_peer_inflight(p);
                }
            }
            self.tip_owner_open.store(false, Ordering::Relaxed);
            self.reset_sticky_wan_tenure(&forced, self.next_needed_height());
            super::tip_stage::rearm_tip_sla();
            tracing::warn!(
                "[IBD_SLA_ROTATE_SKIP] kept forced tip owner {} (prev={:?})",
                forced,
                prev
            );
            return prev;
        }
        let mut g = self.preferred_tip_owner.lock().unwrap();
        let prev = g.take();
        drop(g);
        // W31: WAN gap uses clear-claims + deep re-arm, not failover (W30 coordinator path).
        if !self.wan_tip_gap_crawl(self.next_needed_height()) {
            super::tip_stage::arm_tip_failover();
        }
        self.clear_all_tip_cover_claims();
        if let Some(ref p) = prev {
            // Drop assigner in-flight so covering=0 and retry/tip-owner can reassign immediately.
            // Download aborts via blacklist poll (ChunkGuard drop is then a no-op pop).
            self.force_release_peer_inflight(p);
        }
        self.tip_owner_open.store(true, Ordering::Relaxed);
        super::tip_stage::rearm_tip_sla();
        prev
    }

    /// Remove all in-flight ranges for `peer_id` (SLA rotate / hard-fail recovery).
    pub(crate) fn force_release_peer_inflight(&self, peer_id: &str) {
        let mut g = self.in_flight_per_peer.lock().unwrap();
        g.remove(peer_id);
        drop(g);
        self.clear_tip_cover_claims_for_peer(peer_id);
    }

    /// W36: allow any top-half peer to take tip until the next owner is assigned.
    pub(crate) fn open_tip_owner_slot(&self) {
        self.tip_owner_open.store(true, Ordering::Relaxed);
    }

    /// Phase 2 EMPTY_TIP: covering=0 while tip missing — open tip-owner + re-arm SLA.
    /// Rate-limited (~80ms) so COVERING_ZERO thrash does not storm assigns.
    /// KEEP leaves sole-EMPTY release off (A51 deleted). Frontier dual on-path is gone (T2.5).
    pub(crate) fn force_empty_tip_rearm(&self, next_needed: u64) {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut g = LAST.lock().unwrap();
            if let Some(t) = *g {
                if t.elapsed() < Duration::from_millis(80) {
                    return;
                }
            }
            *g = Some(Instant::now());
        }
        if !self.wan_tip_gap_crawl(next_needed) {
            return;
        }
        self.clear_all_tip_cover_claims();
        self.tip_owner_open.store(true, Ordering::Relaxed);
        // Prefer pinning a ready worker so get_work does not wait on lottery.
        if self.preferred_tip_owner.lock().unwrap().is_none() {
            let pin = super::sole_tip_forced_owner()
                .filter(|p| !self.is_peer_blacklisted(p))
                .or_else(|| {
                    self.best_covering0_tip_pin_candidate(next_needed)
                        .or_else(|| self.top_scored_active_ready_worker())
                });
            if let Some(pin) = pin {
                *self.preferred_tip_owner.lock().unwrap() = Some(pin.clone());
                self.tip_owner_open.store(false, Ordering::Relaxed);
                self.reset_sticky_wan_tenure(&pin, next_needed);
                tracing::warn!(
                    "[IBD_EMPTY_REARM] tip={} pinned={} covering=0 — deep tip-owner re-arm",
                    next_needed,
                    pin
                );
                super::tip_stage::rearm_tip_sla();
                return;
            }
        }
        tracing::warn!(
            "[IBD_EMPTY_REARM] tip={} covering=0 tip_owner_open=1 — await deep assign",
            next_needed
        );
        super::tip_stage::rearm_tip_sla();
    }

    fn reset_sticky_wan_tenure(&self, peer_id: &str, start_next_needed: u64) {
        if !self.wan_tip_gap_crawl(start_next_needed) {
            *self.sticky_wan_tenure.lock().unwrap() = None;
            self.tip_progress_samples.lock().unwrap().clear();
            return;
        }
        *self.sticky_wan_tenure.lock().unwrap() = Some(StickyWanTenure {
            peer: peer_id.to_string(),
            start_next_needed,
            started_at: Instant::now(),
        });
        let mut samples = self.tip_progress_samples.lock().unwrap();
        samples.clear();
        samples.push_back((Instant::now(), start_next_needed));
    }

    fn sticky_tenure_bps(&self, next_needed: u64) -> Option<(f64, String, f64)> {
        let tenure = self.sticky_wan_tenure.lock().unwrap().clone()?;
        let elapsed = tenure.started_at.elapsed().as_secs_f64();
        if elapsed < 1.0 {
            return None;
        }
        let blocks = next_needed.saturating_sub(tenure.start_next_needed);
        let bps = blocks as f64 / elapsed;
        Some((bps, tenure.peer, elapsed))
    }

    /// Record `next_needed` for recent-window A6m BPS (coordinator / rotate path).
    pub(crate) fn note_tip_progress(&self, next_needed: u64) {
        let mut samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        if let Some((last_t, last_nn)) = samples.back().copied() {
            if last_nn == next_needed && now.duration_since(last_t) < Duration::from_millis(200) {
                return;
            }
        }
        samples.push_back((now, next_needed));
        let keep = Duration::from_secs(a6m_recent_window_secs().saturating_mul(3).max(180));
        while let Some((t, _)) = samples.front().copied() {
            if now.duration_since(t) > keep && samples.len() > 1 {
                samples.pop_front();
            } else {
                break;
            }
        }
        while samples.len() > 512 {
            samples.pop_front();
        }
    }

    /// Tip BPS over the recent window (preferred). Falls back to lifetime tenure when
    /// sample history is shorter than ~80% of the window (unit tests / early tenure).
    fn sticky_recent_bps(&self, next_needed: u64, window_secs: u64) -> Option<(f64, String, f64)> {
        let tenure = self.sticky_wan_tenure.lock().unwrap().clone()?;
        self.note_tip_progress(next_needed);
        let samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        let target = now.checked_sub(Duration::from_secs(window_secs))?;
        let mut older: Option<(Instant, u64)> = None;
        for &(t, nn) in samples.iter() {
            if t <= target {
                older = Some((t, nn));
            } else {
                break;
            }
        }
        if let Some((t, nn)) = older {
            let elapsed = now.duration_since(t).as_secs_f64();
            if elapsed >= (window_secs as f64) * 0.8 {
                let bps = next_needed.saturating_sub(nn) as f64 / elapsed.max(1e-3);
                return Some((bps, tenure.peer, elapsed));
            }
        }
        drop(samples);
        // Insufficient recent history — lifetime tenure only after full tenure window
        // (preserves pre-F-P unit tests; env `BLVM_IBD_A6M_TENURE_SECS`).
        let life = self.sticky_tenure_bps(next_needed)?;
        if life.2 < a6m_tenure_secs() as f64 {
            return None;
        }
        Some(life)
    }

    /// A6n: record a WAN tip GAP_STREAM from `peer_id` (download path).
    pub(crate) fn note_wan_tip_stream(&self, peer_id: &str) {
        let mut g = self.peer_tip_streams.lock().unwrap();
        let now = Instant::now();
        // Hot path: tip-adjacent bodies credit every STREAM — avoid `to_string` on hits.
        if let Some(entry) = g.get_mut(peer_id) {
            if entry.started.elapsed() > Duration::from_secs(600) {
                *entry = TipStreamWindow {
                    streams: 1,
                    started: now,
                    last_stream: now,
                };
            } else {
                entry.streams = entry.streams.saturating_add(1);
                entry.last_stream = now;
            }
            return;
        }
        g.insert(
            peer_id.to_string(),
            TipStreamWindow {
                streams: 1,
                started: now,
                last_stream: now,
            },
        );
    }

    /// True when `peer_id` delivered a WAN tip `GAP_STREAM` within `within`.
    ///
    /// Live 2026-07-14: floor-sticky 2× upgrade cleared tip-cover claims on a peer that
    /// was mid-`GAP_STREAM` → `should_abort_tip_walk_in` killed the productive owner
    /// (~1s after first stream). Hold upgrades/walk-in aborts while the peer is hot.
    pub(crate) fn peer_recently_tip_streaming(&self, peer_id: &str, within: Duration) -> bool {
        let g = self.peer_tip_streams.lock().unwrap();
        g.get(peer_id)
            .map(|e| e.streams > 0 && e.last_stream.elapsed() <= within)
            .unwrap_or(false)
    }

    /// W113/W114: peer delivered a tip GAP_STREAM within [`Self::tip_stream_owner_hot_secs`].
    pub(crate) fn peer_is_hot_tip_streamer(&self, peer_id: &str) -> bool {
        self.peer_recently_tip_streaming(
            peer_id,
            Duration::from_secs(Self::tip_stream_owner_hot_secs()),
        )
    }

    /// A6n: recent WAN tip GAP_STREAM rate for `peer_id` (0 if unknown).
    pub(crate) fn wan_tip_stream_bps(&self, peer_id: &str) -> f64 {
        let g = self.peer_tip_streams.lock().unwrap();
        let Some(entry) = g.get(peer_id) else {
            return 0.0;
        };
        if entry.streams == 0 {
            return 0.0;
        }
        let secs = entry.started.elapsed().as_secs_f64().max(1.0);
        entry.streams as f64 / secs
    }

    /// W113: how long a tip GAP_STREAM keeps a peer eligible for empty-tip deep owner.
    /// Default **90s**. Env `BLVM_IBD_TIP_STREAM_OWNER_SECS`.
    fn tip_stream_owner_hot_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_STREAM_OWNER_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(90)
            .clamp(15, 300)
    }

    /// W113: empty tip + at least one ready tip-STREAM peer → prefer streamers for
    /// deep tip owner (not floor-score open-slot lottery).
    fn empty_tip_owner_prefer_streamer(&self) -> bool {
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if !gap {
            return false;
        }
        self.best_a6n_tip_candidate("").is_some()
    }

    /// A6n: best ready worker by recent tip GAP_STREAM rate (never lifetime bulk BPS).
    fn best_a6n_tip_candidate(&self, exclude: &str) -> Option<(String, f64)> {
        // TPP L3 REVERT (L3-20260801T034357Z): last_tip_hedge GD_SLOW prefer →
        // wall 378 < C0 390; TIP_HOLE_AHEAD 7>5; A6M rotate=0 (path unused on hero day).
        let mut best: Option<(String, f64)> = None;
        for peer_id in self.active_download_worker_ids() {
            if peer_id == exclude {
                continue;
            }
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            let bps = self.wan_tip_stream_bps(&peer_id);
            // Require real tip streams — lifetime bulk heroes have 0 here.
            if bps <= 0.0 {
                continue;
            }
            if best.as_ref().map(|(_, b)| bps > *b).unwrap_or(true) {
                best = Some((peer_id, bps));
            }
        }
        best
    }

    /// A6m/A6n: if sticky **recent-window** tip BPS stays below threshold, rotate to a peer with
    /// higher **recent tip GAP_STREAM rate**. Never use lifetime `delivery_blocks_per_sec`
    /// (live A6m: bulk hero at 165 blk/s → worse WAN BPS).
    ///
    /// Live 2026-07-15 soak proof:
    /// - Lifetime tenure BPS over 300s never dropped below **11** → `min_bps=6` never fired
    ///   (`IBD_A6M_ROTATE` / `IBD_A6N_OPEN_SLOT` count = 0) while floor sticky sat at score 0.100.
    /// - Sticky monopolizes tip `GAP_STREAM` counts → alternate peers fail the 1.25× bar even when
    ///   tip crawl is ~1 blk/s; must open-slot instead of returning false.
    pub(crate) fn maybe_rotate_slow_sticky_a6m(
        &self,
        next_needed: u64,
        _peer_scorer: &crate::network::peer_scoring::PeerScorer,
    ) -> bool {
        if !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        let floor = self.preferred_is_floor_sticky();
        // Soft-retries must **not** block A6m on non-floor stickies. Live E10 (2026-07-25):
        // sticky@1.3 held 88% of tip assigns while wall ~17 BPS / getdata p50 ~2.7s and
        // soft_retry×39 — the old non-floor soft_retry gate returned false every poll so
        // `IBD_A6M_*=0`. Floor stickies already skipped that gate; mid-score slow owners
        // need the same escape (rotate / open-slot), not ahead flood (W3c FAIL).
        let cooldown = Duration::from_secs(if floor {
            a6m_floor_rotate_cooldown_secs()
        } else {
            a6m_rotate_cooldown_secs()
        });
        if let Some(last) = *self.last_a6m_rotate_at.lock().unwrap() {
            if last.elapsed() < cooldown {
                return false;
            }
        }
        let sticky = match self.preferred_tip_owner() {
            Some(p) if self.tip_sticky_usable(&p) => p,
            _ => return false,
        };
        let gd_ewma = super::tip_stage::getdata_body_ewma_ms();
        let gd_slow = gd_ewma
            .map(|(ms, _)| ms >= a6m_max_getdata_ms())
            .unwrap_or(false);
        let pipe_mute = super::tip_stage::pipe_fill_recv0_streak() > 0;
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        // Slow-drip mute (Phase4 run2): covering=1 + bodies trickle → await_ms≈0 and
        // tip_gap_missing clears, so classic mute-fast never armed; A6m waited full
        // RECENT_WINDOW (~30s) at ~10 BPS. Treat feeder=0 ∧ GD_SLOW ∧ covering≥1 as mute.
        let covering = self.healthy_tip_cover_count(next_needed);
        let slow_drip = feeder == 0 && gd_slow && covering >= 1;
        // Mute-fast: skip 0.8×window tenure when tip is empty + gap + (GD_SLOW or PIPE_FILL)
        // — or slow-drip (gap cleared but crawl is GD_SLOW).
        // Still respects rotate cooldown above. Supply-design §7.2/§7.3 — no 48s wait.
        // Also bypass sticky_recent_bps's internal 0.8× gate (falls back to 300s lifetime).
        let mute_fast = feeder == 0 && (gap || slow_drip) && (gd_slow || pipe_mute);
        let window_secs = a6m_recent_window_secs();
        let (recent_bps, tenure_peer, elapsed_secs) =
            match self.sticky_recent_bps(next_needed, window_secs) {
                Some(t) if t.1 == sticky => t,
                _ if mute_fast => match self.sticky_tenure_bps(next_needed) {
                    Some(t) if t.1 == sticky => t,
                    _ => return false,
                },
                _ => return false,
            };
        let _ = tenure_peer;
        let min_bps = if floor {
            a6m_floor_min_bps()
        } else {
            a6m_min_bps()
        };
        if !mute_fast && elapsed_secs < (window_secs as f64) * 0.8 {
            return false;
        }
        if recent_bps >= min_bps && !gd_slow && !mute_fast {
            return false;
        }
        // Mute-fast with PIPE_FILL but healthy tip BPS and not gd_slow still needs a reason
        // to rotate — treat pipe mute as gd_slow for the rotate body.
        let gd_slow = gd_slow || (mute_fast && pipe_mute);
        if gd_slow && recent_bps >= min_bps {
            let (ms, n) = gd_ewma.unwrap_or((0, 0));
            let feeder_keep = a6m_gd_slow_feeder_keep();
            let tip_keep = a6m_gd_slow_tip_bps_keep();
            // E16/E16b: runway or strong tip crawl ⇒ do not OPEN/blacklist on EWMA alone.
            let keep_feeder = feeder_keep > 0 && feeder >= feeder_keep && !mute_fast;
            let keep_tip = tip_keep > 0.0 && recent_bps >= tip_keep && !mute_fast;
            if keep_feeder || keep_tip {
                tracing::warn!(
                    "[IBD_A6M_GD_SLOW_KEEP] sticky={} tip_bps={:.1} tip_keep={:.0} feeder={}≥{} gd_ewma={}ms (n={}) ≥ {} pipe_recv0={} reason={} — skip rotate",
                    sticky,
                    recent_bps,
                    tip_keep,
                    feeder,
                    feeder_keep,
                    ms,
                    n,
                    a6m_max_getdata_ms(),
                    pipe_mute,
                    if keep_tip && keep_feeder {
                        "tip+feeder"
                    } else if keep_tip {
                        "tip_bps"
                    } else {
                        "feeder"
                    }
                );
                return false;
            }
            tracing::warn!(
                "[IBD_A6M_GD_SLOW] sticky={} tip_bps={:.1} ≥ min={:.0} but getdata→body ewma={}ms (n={}) ≥ {} mute_fast={} pipe_recv0={} feeder={} — LOCAL_GAP/mute; rotate",
                sticky,
                recent_bps,
                min_bps,
                ms,
                n,
                a6m_max_getdata_ms(),
                mute_fast,
                pipe_mute,
                feeder
            );
        }
        self.a6m_do_rotate(
            next_needed,
            &sticky,
            recent_bps,
            elapsed_secs,
            floor,
            gd_slow,
        )
    }

    /// Observability for mute-kill soaks (`[IBD_MUTE_KILL]`).
    /// C1u: also drop old peer sticky tip-hole — TRIAL/OPEN must not reopen depth=32
    /// on a drip hero after rotate (mute CAP already resets via `note_tip_owner_failed_mute`).
    fn log_mute_kill(
        &self,
        reason: &str,
        old: &str,
        new: Option<&str>,
        next_needed: u64,
        tip_bps: Option<f64>,
    ) {
        self.reset_tip_hole_depth(old);
        let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
        let gd = super::tip_stage::getdata_body_ewma_ms();
        let gd_ewma = gd.map(|(ms, _)| ms);
        let covering = self.healthy_tip_cover_count(next_needed);
        let tip_bps_s = tip_bps
            .map(|b| format!("{b:.1}"))
            .unwrap_or_else(|| "-".into());
        tracing::warn!(
            "[IBD_MUTE_KILL] reason={} old={} new={} await_ms={} gd_ewma={:?} tip_bps={} covering={} next_needed={} pipe_recv0={}",
            reason,
            old,
            new.unwrap_or("-"),
            await_ms,
            gd_ewma,
            tip_bps_s,
            covering,
            next_needed,
            super::tip_stage::pipe_fill_recv0_streak()
        );
    }

    /// P2: tip trials enabled (default **on**). Opt out: `BLVM_IBD_TIP_TRIAL=0`.
    fn tip_trial_enabled() -> bool {
        !matches!(
            std::env::var("BLVM_IBD_TIP_TRIAL")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("0") | Some("false") | Some("off") | Some("no")
        )
    }

    /// P2: challenger pin duration (default **12s**, clamp 8–20).
    fn tip_trial_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12)
            .clamp(8, 20)
    }

    /// P2: tip awaiting before arming a trial (default **3s**).
    fn tip_trial_await_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_AWAIT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
            .clamp(2, 15)
    }

    /// Rate-limited skip diagnostic (assigner polls ~50ms; avoid log storms).
    fn log_tip_trial_skip(
        reason: &str,
        sticky: &str,
        await_ms: u64,
        need_ms: u64,
        next_needed: u64,
    ) {
        static LAST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_MS.load(Ordering::Relaxed);
        if now.saturating_sub(prev) < 2000 {
            return;
        }
        LAST_MS.store(now, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_TIP_TRIAL_SKIP] reason={} sticky={} await_ms={} need_ms={} tip={}",
            reason,
            sticky,
            await_ms,
            need_ms,
            next_needed
        );
    }

    /// Ms await gate for tip trial. Post-OPEN boost uses 500ms (or env) for 20s.
    fn tip_trial_need_await_ms(&self) -> u64 {
        let normal = Self::tip_trial_await_secs().saturating_mul(1000);
        let boost_ms = std::env::var("BLVM_IBD_TIP_TRIAL_POST_OPEN_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(500u64);
        if boost_ms == 0 {
            return normal;
        }
        let boost_ms = boost_ms.clamp(200, 2000);
        let Ok(g) = self.tip_trial_post_open_at.lock() else {
            return normal;
        };
        match *g {
            Some(t) if t.elapsed() < Duration::from_secs(20) => boost_ms,
            _ => normal,
        }
    }

    /// P2: min seconds between trial starts (default **30s**).
    fn tip_trial_cooldown_secs() -> u64 {
        std::env::var("BLVM_IBD_TIP_TRIAL_COOLDOWN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30)
            .clamp(15, 300)
    }

    fn tip_stream_count(&self, peer_id: &str) -> u64 {
        self.peer_tip_streams
            .lock()
            .unwrap()
            .get(peer_id)
            .map(|e| e.streams)
            .unwrap_or(0)
    }

    /// P2: ready challenger for a tip trial (does **not** require prior tip streams).
    fn best_tip_trial_challenger(&self, exclude: &str) -> Option<String> {
        // Prefer a tip-proven alternate when one exists; else highest ready score.
        if let Some((p, _)) = self.best_a6n_tip_candidate(exclude) {
            return Some(p);
        }
        let mut best: Option<(String, f64)> = None;
        for peer_id in self.active_download_worker_ids() {
            if peer_id == exclude {
                continue;
            }
            if !self.peer_is_ibd_ready(&peer_id)
                || self.is_peer_blacklisted(&peer_id)
                || self.tip_owner_in_fail_cooldown(&peer_id)
            {
                continue;
            }
            let score = self.peer_score_of(&peer_id);
            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                best = Some((peer_id, score));
            }
        }
        best.map(|(p, _)| p)
            .or_else(|| self.any_ready_active_worker_except(exclude))
    }

    /// P2: poll tip trial — finish active trial or start one when tip is starving.
    /// Returns true when a trial started or finished (state changed).
    pub(crate) fn maybe_run_tip_trial(&self, next_needed: u64) -> bool {
        if !Self::tip_trial_enabled() || !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        // Finish first so we never stack trials.
        if self.tip_trial.lock().unwrap().is_some() {
            return self.maybe_finish_tip_trial(next_needed);
        }
        self.maybe_start_tip_trial(next_needed)
    }

    /// Short-window tip crawl BPS from progress samples only (no lifetime tenure fallback).
    /// Used to detect slow-drip mute while covering=1 keeps await_ms≈0.
    fn tip_crawl_recent_bps(&self, next_needed: u64, window_secs: u64) -> Option<(f64, f64)> {
        self.note_tip_progress(next_needed);
        let samples = self.tip_progress_samples.lock().unwrap();
        let now = Instant::now();
        let target = now.checked_sub(Duration::from_secs(window_secs))?;
        let mut older: Option<(Instant, u64)> = None;
        for &(t, nn) in samples.iter() {
            if t <= target {
                older = Some((t, nn));
            } else {
                break;
            }
        }
        let (t, nn) = older?;
        let elapsed = now.duration_since(t).as_secs_f64();
        if elapsed < (window_secs as f64) * 0.8 {
            return None;
        }
        let bps = next_needed.saturating_sub(nn) as f64 / elapsed.max(1e-3);
        Some((bps, elapsed))
    }

    fn maybe_start_tip_trial(&self, next_needed: u64) -> bool {
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        let gap = self.tip_gap_missing.load(Ordering::Relaxed)
            || super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        let await_ms = super::tip_stage::tip_awaiting_ms_for_cap();
        let need_ms = self.tip_trial_need_await_ms();
        let gd_slow = super::tip_stage::getdata_body_ewma_ms()
            .map(|(ms, _)| ms >= a6m_max_getdata_ms())
            .unwrap_or(false);
        let covering = self.healthy_tip_cover_count(next_needed);
        // Slow-drip: peer keeps covering=1 so await never hits tip_trial_await; arm when
        // short-window crawl stays below A6m min for ≥~6s (0.8×8s window).
        let drip_window = std::env::var("BLVM_IBD_TIP_SLOW_DRIP_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8u64)
            .clamp(5, 20);
        let slow_drip = feeder == 0
            && gd_slow
            && covering >= 1
            && self
                .tip_crawl_recent_bps(next_needed, drip_window)
                .is_some_and(|(bps, _)| bps < a6m_min_bps());
        // M4: tip-gd force — sticky getdata EWMA fat + challenger tip streams ahead.
        // No pin-vacuum blacklist wipe (PB1 REVERT). Prefers tip-window streams only.
        let sticky_for_gd = self.preferred_tip_owner();
        let tip_gd_force = feeder == 0
            && gd_slow
            && covering >= 1
            && sticky_for_gd.as_ref().is_some_and(|sticky| {
                self.best_tip_trial_challenger(sticky).is_some_and(|ch| {
                    let s = self.tip_stream_count(sticky);
                    let c = self.tip_stream_count(&ch);
                    c > s || self.wan_tip_stream_bps(&ch) > self.wan_tip_stream_bps(sticky) * 1.25
                })
            });
        // §7.2: feeder==0 ∧ gap_missing ∧ awaiting≥T — or slow-drip / tip-gd force.
        if feeder > 0 {
            return false;
        }
        if !slow_drip && !tip_gd_force && (!gap || await_ms < need_ms) {
            return false;
        }
        if let Some(last) = *self.last_tip_trial_at.lock().unwrap() {
            if last.elapsed() < Duration::from_secs(Self::tip_trial_cooldown_secs()) {
                return false;
            }
        }
        // E16: post-OPEN settle — do not displace the OPEN pin during handoff while
        // await briefly crosses the 500ms boost gate (C1u stall: OPEN→TRIAL 1.5s).
        let settle = tip_trial_post_open_settle_secs();
        if settle > 0 {
            if let Ok(g) = self.tip_trial_post_open_at.lock() {
                if let Some(t) = *g {
                    if t.elapsed() < Duration::from_secs(settle) {
                        Self::log_tip_trial_skip(
                            "post_open_settle",
                            self.preferred_tip_owner().as_deref().unwrap_or("-"),
                            await_ms,
                            need_ms,
                            next_needed,
                        );
                        return false;
                    }
                }
            }
        }
        // Prefer the pinned owner even when not currently "usable" (blacklisted / not
        // ready). Live Phase4: post-OPEN boost hit need_ms=500 but trials skipped
        // `no_usable_sticky` for seconds while mute burned wall — the whole point of a
        // trial is to replace a sticky that is failing tip delivery.
        let sticky = match self.preferred_tip_owner() {
            Some(p) => p,
            None => {
                Self::log_tip_trial_skip("no_preferred", "-", await_ms, need_ms, next_needed);
                return false;
            }
        };
        let Some(challenger) = self.best_tip_trial_challenger(&sticky) else {
            Self::log_tip_trial_skip("no_challenger", &sticky, await_ms, need_ms, next_needed);
            return false;
        };
        if challenger == sticky {
            return false;
        }
        let awaiting = await_ms / 1000;
        let sticky_streams = self.tip_stream_count(&sticky);
        let chall_streams = self.tip_stream_count(&challenger);
        // Cool sticky for tip-only during the trial (ahead can continue on other peers).
        self.mark_tip_owner_fail_cooldown(&sticky, Self::tip_trial_secs().saturating_add(2));
        self.force_release_peer_inflight(&sticky);
        self.clear_tip_cover_claims_for_peer(&sticky);
        *self.preferred_tip_owner.lock().unwrap() = Some(challenger.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&challenger, next_needed);
        *self.tip_trial.lock().unwrap() = Some(TipTrial {
            sticky: sticky.clone(),
            challenger: challenger.clone(),
            started: Instant::now(),
            sticky_streams_at_start: sticky_streams,
            challenger_streams_at_start: chall_streams,
            next_needed_at_start: next_needed,
        });
        *self.last_tip_trial_at.lock().unwrap() = Some(Instant::now());
        tracing::warn!(
            "[IBD_TIP_TRIAL_START] sticky={} challenger={} tip={} awaiting={}s trial={}s sticky_streams={} chall_streams={} slow_drip={} tip_gd_force={} await_ms={} covering={}",
            sticky,
            challenger,
            next_needed,
            awaiting,
            Self::tip_trial_secs(),
            sticky_streams,
            chall_streams,
            slow_drip,
            tip_gd_force,
            await_ms,
            covering
        );
        self.log_mute_kill(
            "TRIAL_START",
            &sticky,
            Some(&challenger),
            next_needed,
            Some(self.wan_tip_stream_bps(&sticky)),
        );
        true
    }

    fn maybe_finish_tip_trial(&self, next_needed: u64) -> bool {
        let trial = {
            let g = self.tip_trial.lock().unwrap();
            g.clone()
        };
        let Some(trial) = trial else {
            return false;
        };
        if trial.started.elapsed() < Duration::from_secs(Self::tip_trial_secs()) {
            return false;
        }
        let sticky_delta = self
            .tip_stream_count(&trial.sticky)
            .saturating_sub(trial.sticky_streams_at_start);
        let chall_delta = self
            .tip_stream_count(&trial.challenger)
            .saturating_sub(trial.challenger_streams_at_start);
        let height_delta = next_needed.saturating_sub(trial.next_needed_at_start);
        // Keep challenger if they delivered tip streams ≥ sticky×1.25, or sticky had
        // zero tip bodies while tip advanced / challenger streamed.
        // `chall_delta > 0` is restated inside so the height_delta arm stays readable.
        #[allow(clippy::overly_complex_bool_expr)]
        let keep = chall_delta > 0
            && (chall_delta as f64 >= (sticky_delta as f64) * 1.25
                || (sticky_delta == 0 && (chall_delta > 0 || height_delta > 0)));
        *self.tip_trial.lock().unwrap() = None;
        if keep {
            // Challenger stays preferred; sticky already cooled for tip-role.
            tracing::warn!(
                "[IBD_TIP_TRIAL_KEEP] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta
            );
            self.log_mute_kill(
                "TRIAL_KEEP",
                &trial.sticky,
                Some(&trial.challenger),
                next_needed,
                Some(self.wan_tip_stream_bps(&trial.challenger)),
            );
        } else {
            // Revert to sticky — clear trial cooldown on sticky so they can re-arm tip.
            {
                let mut g = self.tip_owner_fail_until.lock().unwrap();
                g.remove(&trial.sticky);
            }
            *self.preferred_tip_owner.lock().unwrap() = Some(trial.sticky.clone());
            self.tip_owner_open.store(false, Ordering::Relaxed);
            self.reset_sticky_wan_tenure(&trial.sticky, next_needed);
            // Brief cool on failed challenger so we don't thrash the same alt.
            self.mark_tip_owner_fail_cooldown(&trial.challenger, Self::tip_trial_cooldown_secs());
            self.force_release_peer_inflight(&trial.challenger);
            self.clear_tip_cover_claims_for_peer(&trial.challenger);
            tracing::warn!(
                "[IBD_TIP_TRIAL_REVERT] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta
            );
        }
        true
    }

    /// Shared A6m rotate / open-slot body (after recent-or-lifetime BPS proved slow).
    fn a6m_do_rotate(
        &self,
        next_needed: u64,
        sticky: &str,
        tenure_bps: f64,
        elapsed_secs: f64,
        floor: bool,
        gd_slow: bool,
    ) -> bool {
        // Mode T dual: never rotate tip off the forced first PEERS pin (tc168).
        if super::sole_tip_forced_owner().as_deref() == Some(sticky) {
            tracing::warn!(
                "[IBD_A6M_ROTATE_SKIP] sticky={} — sole_tip forced owner (tenure_bps={:.2} gd_slow={})",
                sticky,
                tenure_bps,
                gd_slow
            );
            return false;
        }
        let sticky_tip_bps = self.wan_tip_stream_bps(sticky);
        let candidate = self.best_a6n_tip_candidate(sticky);
        if let Some((ref cand_id, candidate_bps)) = candidate {
            // Live 2026-07-15: sticky_tip_bps EWMA stayed ~30 while tenure_bps (height
            // advance) was 0.33 → bar=38.59 and no alternate could clear it. Prefer tenure
            // (what A6m already decided is slow); keep tip-stream rate in the log only.
            // E12 GD_SLOW: sticky_tip_bps ~384 while getdata→body ewma ≥800 (LOCAL_GAP
            // mask) — never use stream rate for the 1.25× bar when GD_SLOW armed.
            let bar_basis = if floor || gd_slow {
                tenure_bps
            } else {
                sticky_tip_bps.max(tenure_bps)
            };
            let bar = bar_basis * 1.25;
            // GD_SLOW: a tip-streaming alternate beats a sticky whose GetData is slow,
            // even when it fails the 1.25× bar (sticky monopolizes GAP_STREAM counts).
            // Require a real tip-stream floor (E13 FORCE@3.86 was noise).
            let force_gd = gd_slow && candidate_bps >= a6m_gd_slow_force_min_tip_bps();
            if candidate_bps > bar || force_gd {
                self.blacklist_peer(sticky, Duration::from_secs(120));
                if gd_slow {
                    self.mark_tip_owner_fail_cooldown(sticky, a6m_gd_slow_owner_cooldown_secs());
                }
                self.force_release_peer_inflight(sticky);
                *self.preferred_tip_owner.lock().unwrap() = Some(cand_id.clone());
                self.tip_owner_open.store(false, Ordering::Relaxed);
                self.reset_sticky_wan_tenure(cand_id, next_needed);
                *self.last_a6m_rotate_at.lock().unwrap() = Some(Instant::now());
                super::tip_stage::rearm_tip_sla();
                if force_gd && candidate_bps <= bar {
                    tracing::warn!(
                        "[IBD_A6M_GD_SLOW_FORCE] from={} to={} tenure_bps={:.2} sticky_tip_bps={:.2} candidate_tip_bps={:.2} bar={:.2} next_needed={} — tip-stream alt despite bar",
                        sticky,
                        cand_id,
                        tenure_bps,
                        sticky_tip_bps,
                        candidate_bps,
                        bar,
                        next_needed
                    );
                    if gd_slow {
                        self.log_mute_kill(
                            "FORCE",
                            sticky,
                            Some(cand_id.as_str()),
                            next_needed,
                            Some(tenure_bps),
                        );
                    }
                } else {
                    tracing::warn!(
                        "[IBD_A6M_ROTATE] from={} to={} tenure_bps={:.2} tenure_secs={:.0} sticky_tip_bps={:.2} candidate_tip_bps={:.2} bar={:.2} floor={} gd_slow={} next_needed={}",
                        sticky,
                        cand_id,
                        tenure_bps,
                        elapsed_secs,
                        sticky_tip_bps,
                        candidate_bps,
                        bar,
                        floor,
                        gd_slow,
                        next_needed
                    );
                    if gd_slow {
                        self.log_mute_kill(
                            "GD_SLOW",
                            sticky,
                            Some(cand_id.as_str()),
                            next_needed,
                            Some(tenure_bps),
                        );
                    }
                }
                return true;
            }
            // Live: sticky owns tip streams → alternates fail 1.25× bar while tip is slow.
            // Open slot instead of keeping the slow sticky — but only below the historical
            // healthy floor (default 12). Live 2026-07-15: tenure=12.57 OPEN_SLOT blacklisted a
            // delivering sticky → pinned score lottery / 0.100 treadmill (~11 blk/s).
            tracing::warn!(
                "[IBD_A6N_BAR_FAIL] sticky={} tenure_bps={:.2} sticky_tip_bps={:.2} best_alt={} alt_bps={:.2} bar={:.2} floor={} open_slot_min={:.2}",
                sticky,
                tenure_bps,
                sticky_tip_bps,
                cand_id,
                candidate_bps,
                bar,
                floor,
                a6m_floor_open_slot_min_bps()
            );
            if floor && !gd_slow && tenure_bps >= a6m_floor_open_slot_min_bps() {
                tracing::warn!(
                    "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} ≥ open_slot_min={:.2} — no tip-proven alt; keep sticky",
                    sticky,
                    tenure_bps,
                    a6m_floor_open_slot_min_bps()
                );
                return false;
            }
        } else if floor && !gd_slow && tenure_bps >= a6m_floor_open_slot_min_bps() {
            // No tip-stream alternate at all — same keep gate (not GD_SLOW).
            tracing::warn!(
                "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} ≥ open_slot_min={:.2} — no tip-proven candidate; keep sticky",
                sticky,
                tenure_bps,
                a6m_floor_open_slot_min_bps()
            );
            return false;
        }
        // Mode T sole archive (tc65 2026-08-04): GD_SLOW OPEN_SLOT with no challenger
        // blacklisted the only ready peer 120s + OWNER_COOLDOWN 180s → MUTE_KILL new=- /
        // covering=0 for the rest of tip90. Keep sticky when no alternate can take tip.
        // E15: when GD_SLOW and another tip-streamer exists but is cooled/blacklisted,
        // fall through to OPEN so `clear_*_except` can un-cool that hero (dens KEEP
        // `a6m_gd_slow_open_uncools_prior_hero_when_pin_empty`).
        if self.any_ready_active_worker_except(sticky).is_none()
            && self.best_a6n_tip_candidate(sticky).is_none()
        {
            let e15_uncool = gd_slow
                && self
                    .active_download_worker_ids()
                    .iter()
                    .any(|p| p != sticky && self.tip_stream_count(p) > 0);
            if !e15_uncool {
                tracing::warn!(
                    "[IBD_A6N_KEEP] sticky={} tenure_bps={:.2} sticky_tip_bps={:.2} gd_slow={} — no alternate ready worker; keep sticky (sole tip peer)",
                    sticky,
                    tenure_bps,
                    sticky_tip_bps,
                    gd_slow
                );
                return false;
            }
        }
        // True stall / GD_SLOW / non-floor: open slot; pin a concrete ready worker.
        // E12: top_scored walked peer_scores only → often None while other download
        // workers were ready → preferred=None lottery re-elected the same sticky.
        // Cool sticky *after* pin attempt so we can un-cool prior GD_SLOW heroes first.
        // OPEN_SLOT always blacklists the opened sticky (dens KEEP a6m_gd_slow_open_uncools).
        // E16b NO_BL applies only when we *keep* the sticky (return false above), not OPEN.
        self.blacklist_peer(sticky, Duration::from_secs(120));
        self.force_release_peer_inflight(sticky);
        self.clear_all_tip_cover_claims();
        // Never pin the sticky we are opening away from (E12: score-map often only
        // lists the sticky → top_scored re-elected it; dens KEEP pins alt worker).
        let mut pinned = self
            .top_scored_active_ready_worker()
            .filter(|p| p != sticky)
            .or_else(|| self.any_ready_active_worker_except(sticky));
        let mut cleared_cd = 0usize;
        if pinned.is_none() && gd_slow {
            // E15: prior ROTATE cooled+blacklisted the only tip hero → pinned=None×3.
            let cleared_bl = self.clear_blacklist_except(sticky);
            cleared_cd = self.clear_tip_owner_fail_cooldowns_except(sticky);
            pinned = self
                .top_scored_active_ready_worker()
                .filter(|p| p != sticky)
                .or_else(|| self.any_ready_active_worker_except(sticky))
                .or_else(|| self.best_a6n_tip_candidate(sticky).map(|(p, _)| p));
            if cleared_cd > 0 || cleared_bl > 0 {
                tracing::warn!(
                    "[IBD_A6N_COOLDOWN_CLEAR] sticky={} cleared_cd={} cleared_bl={} — GD_SLOW OPEN pin retry",
                    sticky,
                    cleared_cd,
                    cleared_bl
                );
            }
        }
        if gd_slow {
            self.mark_tip_owner_fail_cooldown(sticky, a6m_gd_slow_owner_cooldown_secs());
        } else {
            // Always cool the opened-away sticky briefly so TIP_PIN cannot re-arm it
            // before an alternate polls (W92). Shorter than GD_SLOW path.
            self.mark_tip_owner_fail_cooldown(sticky, Self::tip_owner_fail_cooldown_secs());
        }
        {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            *g = pinned.clone();
        }
        // Concrete pin → closed sticky; None → leave open for first ready poller.
        self.tip_owner_open
            .store(pinned.is_none(), Ordering::Relaxed);
        if let Some(ref p) = pinned {
            self.reset_sticky_wan_tenure(p, next_needed);
        }
        super::tip_stage::rearm_tip_sla();
        *self.last_a6m_rotate_at.lock().unwrap() = Some(Instant::now());
        tracing::warn!(
            "[IBD_A6N_OPEN_SLOT] sticky={} tenure_bps={:.2} tenure_secs={:.0} sticky_tip_bps={:.2} floor={} gd_slow={} next_needed={} pinned={:?} cleared_cd={} — no tip-proven candidate above bar",
            sticky,
            tenure_bps,
            elapsed_secs,
            sticky_tip_bps,
            floor,
            gd_slow,
            next_needed,
            pinned,
            cleared_cd
        );
        if gd_slow {
            self.log_mute_kill(
                "OPEN",
                sticky,
                pinned.as_deref(),
                next_needed,
                Some(tenure_bps),
            );
            // SLA rearm zeroed await — arm post-OPEN boost after settle (E16).
            // Do not call maybe_start_tip_trial here: await≈0 right after rearm, and
            // immediate trial displaced OPEN pins during C1u handoff thrash.
            *self.tip_trial_post_open_at.lock().unwrap() = Some(Instant::now());
        }
        true
    }

    /// Score stored for `peer_id` (0.0 if unknown).
    fn peer_score_of(&self, peer_id: &str) -> f64 {
        self.peer_scores
            .lock()
            .unwrap()
            .get(peer_id)
            .copied()
            .unwrap_or(0.0)
    }

    /// A6d/A6f: sticky loses only for a *clearly* faster ready active worker.
    /// Tip ranks use `PeerScorer::tip_owner_score` (unproven demoted ≪ 0.1 floor).
    /// Require candidate ≥ 0.5 so floor-cluster proven peers (0.1–0.2) cannot thrash sticky;
    /// breakthrough-class bandwidth (~1.3+) still upgrades.
    ///
    /// **Floor 2× exception (2026-07-14):** sticky@0.100 with covering=1 never upgraded
    /// (MIN_CANDIDATE=0.5 never fires in the 0.1–0.3 cluster) → ~10 blk/s WAN tip crawl.
    /// Allow upgrade when `cand ≥ 2× sticky` so 0.100→0.203 escapes without A6d thrash
    /// (0.100→0.191 stays blocked).
    const TIP_OWNER_UPGRADE_EPS: f64 = 0.05;
    const TIP_OWNER_UPGRADE_MIN_CANDIDATE: f64 = 0.5;
    /// Proven tip downloaders floor near this; treat as "floor sticky".
    const TIP_OWNER_FLOOR_SCORE: f64 = 0.12;
    /// Mid-band ceiling for 2× tip-owner escape (covers live sticky wobble 0.100–0.136).
    const TIP_OWNER_MID_SCORE: f64 = 0.15;
    /// W95: below this = unproven / demoted — never deep tip owner while any ready
    /// active worker scores higher (cooldown-ignorant alternative check).
    const TIP_OWNER_UNPROVEN_SCORE: f64 = 0.05;

    /// Sticky may exclusively own tip only while it can actually take tip work.
    ///
    /// Live A6g: preferred stayed `Some(45.147…)` after span end while its live score
    /// A6h/A6k: sticky tip owner is usable while ready + active download worker + not blacklisted.
    ///
    /// **Do not** require `peer_ok_for_gap_race`. Live W32d″ soak (~3 blk/s, 14 owners): tip
    /// downloaders floor at **~0.1** while lightly-proven ready workers sit at **~0.19**; WAN
    /// median floor (~0.18) made sticky fail `peer_ok` → `STICKY_DROP` → lottery. A6i breakthrough
    /// soak had **146 same-peer re-arms** vs **5** here; `need→getdata` p50 **3s → 29s**.
    ///
    /// A6h deadlock (covering=0 forever) was sticky *preferred* but unable to pass `peer_ok` on
    /// open-slot — exemption lets the delivering sticky take tip; hung sticky is rotated by tip
    /// SLA / blacklist, not by score-floor eviction.
    fn tip_sticky_usable(&self, pref: &str) -> bool {
        !self.is_peer_blacklisted(pref)
            && self.peer_is_ibd_ready(pref)
            && self.is_active_download_worker(pref)
    }

    /// Hold floor-sticky upgrade / walk-in abort this long after a tip `GAP_STREAM`.
    const TIP_STREAM_HOT_SECS: u64 = 15;

    /// True when `candidate` should replace `sticky` as tip owner.
    fn tip_owner_should_upgrade(&self, sticky: &str, candidate: &str) -> bool {
        if sticky == candidate {
            return false;
        }
        // Never score-upgrade away from a peer that is actively filling the tip gap —
        // unless measured tip BPS is below the stretch floor target. Live 2026-07-15:
        // tip-adjacent receive notes keep peer_recently_tip_streaming true at ~11 blk/s
        // while sticky_recent_bps was often None (cold samples / short tenure) →
        // `below_stretch=false` blocked 2× forever (TIP_UPGRADE=0, OPEN_STALL top_w@0.201).
        // Missing history must allow escape; only proven ≥ stretch BPS holds the sticky.
        if self.peer_recently_tip_streaming(sticky, Duration::from_secs(Self::TIP_STREAM_HOT_SECS))
        {
            let next = self.next_needed_height();
            let window = a6m_recent_window_secs();
            let hold_hot =
                self.sticky_recent_bps(next, window)
                    .is_some_and(|(bps, peer, elapsed)| {
                        peer == sticky
                            && elapsed >= (window as f64) * 0.8
                            && bps >= a6m_floor_min_bps()
                    });
            if hold_hot {
                return false;
            }
        }
        let cand = self.peer_score_of(candidate);
        let cur = self.peer_score_of(sticky);
        // Demoted/unproven sticky (tip_owner_score ~0.001): any clearly better active
        // worker may take tip. MIN_CANDIDATE=0.5 blocked live upgrades 0.001→0.288 and
        // left covering=1 zombies for the full 90s tip-SLA (genesis WAN stalls).
        // Boundary score==UNPROVEN (0.05): A6k keep — not unproven-upgrade, not 2×.
        if cur < Self::TIP_OWNER_UNPROVEN_SCORE && cand > cur + Self::TIP_OWNER_UPGRADE_EPS {
            return true;
        }
        // Floor/mid-band sticky (UNPROVEN < score ≤0.15): require ~2× jump.
        if cur > Self::TIP_OWNER_UNPROVEN_SCORE
            && cur <= Self::TIP_OWNER_MID_SCORE
            && cand >= (cur * 2.0).max(cur + Self::TIP_OWNER_UPGRADE_EPS)
        {
            return true;
        }
        cand >= Self::TIP_OWNER_UPGRADE_MIN_CANDIDATE && cand > cur + Self::TIP_OWNER_UPGRADE_EPS
    }

    /// Preferred sticky is floor/mid-band AND (not tip-streaming, OR below stretch BPS).
    pub(crate) fn preferred_is_idle_floor_sticky(&self) -> bool {
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        if !self.preferred_is_floor_sticky() {
            return false;
        }
        if !self.peer_recently_tip_streaming(&pref, Duration::from_secs(Self::TIP_STREAM_HOT_SECS))
        {
            return true;
        }
        // Hot: idle for nudge when we lack proven stretch BPS (same polarity as upgrade gate).
        let next = self.next_needed_height();
        let window = a6m_recent_window_secs();
        !self
            .sticky_recent_bps(next, window)
            .is_some_and(|(bps, peer, elapsed)| {
                peer == pref && elapsed >= (window as f64) * 0.8 && bps >= a6m_floor_min_bps()
            })
    }

    /// Score of the current preferred tip owner, if any.
    pub(crate) fn preferred_tip_owner_score(&self) -> Option<f64> {
        self.preferred_tip_owner().map(|p| self.peer_score_of(&p))
    }

    /// True when preferred sticky is in the floor/mid band eligible for 2× escape.
    pub(crate) fn preferred_is_floor_sticky(&self) -> bool {
        self.preferred_tip_owner_score()
            .is_some_and(|s| s <= Self::TIP_OWNER_MID_SCORE)
    }

    /// Rate-limited stall diag when covering=0 with open tip slot.
    fn log_tip_open_stall_diag(&self, tip: u64) {
        static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) < 5_000 {
            return;
        }
        LAST_LOG_MS.store(now_ms, Ordering::Relaxed);

        let preferred = self.preferred_tip_owner();
        let top_w = self.top_scored_active_ready_worker();
        let floor = self.wan_active_worker_score_floor();
        let top_w_score = top_w.as_ref().map(|p| self.peer_score_of(p)).unwrap_or(0.0);
        let top_w_ok = top_w
            .as_ref()
            .map(|p| self.peer_ok_for_gap_race(p) && self.peer_is_ibd_ready(p))
            .unwrap_or(false);
        let mut ready_active = 0usize;
        let mut ready_active_ok = 0usize;
        for p in self.active_download_worker_ids() {
            if !self.peer_is_ibd_ready(&p) || self.is_peer_blacklisted(&p) {
                continue;
            }
            ready_active += 1;
            if self.peer_ok_for_gap_race(&p) {
                ready_active_ok += 1;
            }
        }
        let score_n = self.peer_scores.lock().unwrap().len();
        tracing::warn!(
            "[IBD_TIP_OPEN_STALL] tip={} preferred={:?} top_w={:?} top_w_score={:.3} floor={:.3} top_w_ok={} ready_active_ok={}/{} score_keys={} open={}",
            tip,
            preferred,
            top_w,
            top_w_score,
            floor,
            top_w_ok,
            ready_active_ok,
            ready_active,
            score_n,
            self.tip_owner_open.load(Ordering::Relaxed)
        );
    }

    /// WAN tip-gating score floor from **ready** active download workers when possible.
    ///
    /// Live A6i: floor over all scored workers (incl. not-ready / blacklisted) sat at 0.153
    /// while every ready worker was ≤0.127 → `ready_active_ok=0/9`, covering=0 forever.
    fn wan_active_worker_score_floor(&self) -> f64 {
        let mut vals = self.wan_gap_score_floor_vals();
        if vals.len() < 4 {
            return 0.0;
        }
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // A6k: match peer_ok Q1 (diag must agree with gate).
        vals[vals.len() / 4]
    }

    /// Unique active-worker scores for WAN peer_ok median (ready-first, else all).
    fn wan_gap_score_floor_vals(&self) -> Vec<f64> {
        let scored: Vec<(String, f64)> = {
            let scores = self.peer_scores.lock().unwrap();
            let mut out = Vec::new();
            for p in self.active_download_worker_ids() {
                if let Some(s) = scores.get(&p).copied() {
                    out.push((p, s));
                }
            }
            out
        };
        let mut ready_vals = Vec::new();
        let mut all_vals = Vec::new();
        for (p, s) in scored {
            all_vals.push(s);
            if !self.is_peer_blacklisted(&p) && self.peer_is_ibd_ready(&p) {
                ready_vals.push(s);
            }
        }
        if ready_vals.len() >= 4 {
            ready_vals
        } else {
            all_vals
        }
    }

    /// True when `peer_id` still owns tip via cover claim or in-flight range covering tip.
    fn peer_holds_tip_download(&self, peer_id: &str, next_needed: u64) -> bool {
        if self
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s <= next_needed && next_needed <= *e)
        {
            return true;
        }
        let g = self.in_flight_per_peer.lock().unwrap();
        g.get(peer_id).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(s, e)| s <= next_needed && next_needed <= e)
        })
    }

    /// P1-A: coordinator nudge when WAN gap has ready peers but no tip covering flight.
    /// Returns `false` when not in WAN tip crawl (caller must not log a successful re-arm).
    pub(crate) fn nudge_wan_tip_owner(&self) -> bool {
        let next_needed = self.next_needed_height();
        if !self.wan_tip_gap_crawl(next_needed) {
            return false;
        }
        // Keep sticky while ready + active (A6k: score-floor must not evict tip downloaders).
        // Blind clear → score lottery (A6b); blind keep of not-ready sticky → covering=0 (A6h).
        // A6k dens KEEP: after not-ready STICKY_DROP, leave preferred=None for open-slot
        // get_work — same-nudge TIP_PIN would re-elect top_w and fail a6k asserts.
        let mut skip_covering0_pin = false;
        let mut upgraded_from: Option<String> = None;
        {
            let mut g = self.preferred_tip_owner.lock().unwrap();
            if let Some(ref p) = *g {
                if !self.tip_sticky_usable(p) {
                    tracing::warn!(
                        "[IBD_TIP_STICKY_DROP] sticky={} score={:.3} — not usable for tip (ready/worker/blacklist)",
                        p,
                        self.peer_score_of(p)
                    );
                    *g = None;
                    skip_covering0_pin = true;
                } else if let Some(top_w) = self.top_scored_active_ready_worker() {
                    if self.tip_owner_should_upgrade(p, &top_w) {
                        let sticky_score = self.peer_score_of(p);
                        let holds = self.peer_holds_tip_download(p, next_needed);
                        // Dens KEEP / nudge_defers: never score-upgrade away from a peer
                        // mid tip-download (incl. unproven@0.001). Tip-SLA rotates stuck pipes.
                        if holds {
                            tracing::warn!(
                                "[IBD_TIP_UPGRADE_DEFER] sticky={} score={:.3} holds tip download — not upgrading to {} ({:.3})",
                                p,
                                sticky_score,
                                top_w,
                                self.peer_score_of(&top_w)
                            );
                        } else {
                            tracing::warn!(
                                "[IBD_TIP_UPGRADE] sticky={} score={:.3} → better_worker={} score={:.3}",
                                p,
                                sticky_score,
                                top_w,
                                self.peer_score_of(&top_w)
                            );
                            upgraded_from = Some(p.clone());
                            *g = Some(top_w);
                        }
                    }
                }
            }
        }
        // Release weak covering claims after upgrade; blacklist demoted unproven only when
        // they are not mid tip-download (pipe abort thrash).
        if let Some(ref weak) = upgraded_from {
            let weak_score = self.peer_score_of(weak);
            let holds_tip = self.peer_holds_tip_download(weak, next_needed);
            if !holds_tip || weak_score < Self::TIP_OWNER_UNPROVEN_SCORE {
                self.clear_tip_cover_claims_for_peer(weak);
            }
            if weak_score < Self::TIP_OWNER_UNPROVEN_SCORE && !holds_tip {
                self.blacklist_peer(weak, Duration::from_secs(45));
            }
        }
        let (covering, _, _) = self.tip_flight_diag();
        if covering == 0 {
            // Live A6n: header-past-tip fail storm blacklisted every active worker
            // (ready_active_ok=0/0) while ready=58 — tip dead forever. Clear blacklists
            // so open slot can re-arm once pipes are clipped to header tip.
            let mut ready_active = 0usize;
            for p in self.active_download_worker_ids() {
                if self.peer_is_ibd_ready(&p) && !self.is_peer_blacklisted(&p) {
                    ready_active += 1;
                }
            }
            if ready_active == 0 {
                let cleared = self.clear_active_worker_blacklists();
                let cleared_ready = self.clear_ready_peer_blacklists();
                if cleared + cleared_ready > 0 {
                    tracing::warn!(
                        "[IBD_TIP_BLACKLIST_CLEAR] cleared {} worker + {} ready blacklist(s) — covering=0 ready_active=0",
                        cleared,
                        cleared_ready
                    );
                }
            }
            // Live 2026-07-16 h≈450k: OPEN_STALL preferred=None top_w_ok=true ready_active_ok=1/1
            // for ~18 min while covering stayed 0 (open-slot lottery never re-armed). Pin the
            // top active ready worker so sticky path + tip SLA can take the hole.
            // W126: prefer idle tip-STREAM / idle top score — not a peer mid W35 ahead
            // (live W125 @326975: pin 162.247 on 327039-70 → covering=0 for 16s).
            // W126b: select candidate + release ahead **before** taking preferred lock.
            // Live W126a @305703: TIP_PIN held preferred then locked in_flight while
            // get_work held in_flight then preferred → AB-BA deadlock (watchdog:
            // "mutex contended/unavailable"); zero assigns after mute CAP.
            // W137: mid+ mute-CAP cooldown clear **before** pin (not after + 5s debounce).
            self.maybe_clear_mid_plus_fail_cooldowns_covering0(next_needed);
            let need_pin =
                !skip_covering0_pin && self.preferred_tip_owner.lock().unwrap().is_none();
            if need_pin {
                // First BLVM_IBD_PEERS entry wins even if momentarily not ibd_ready —
                // filtering on ready let TIP_PIN elect :18334 (tc170 mid-cell).
                let forced_tip =
                    super::sole_tip_forced_owner().filter(|p| !self.is_peer_blacklisted(p));
                let mut pin_target = forced_tip.clone().or_else(|| {
                    self.best_covering0_tip_pin_candidate(next_needed)
                        .or_else(|| self.top_scored_peer_id())
                });
                // E15b / wan10k: mute CAP cooled every scored ready peer → pin_target=None
                // (mid_clear=0 when heroes left workers). GD_SLOW OPEN already uncools;
                // covering=0 TIP_PIN must too or OPEN_STALL spins forever.
                if pin_target.is_none() {
                    let cleared_cd = self.clear_tip_owner_fail_cooldowns_except("");
                    if cleared_cd > 0 {
                        tracing::warn!(
                            "[IBD_TIP_PIN_COOLDOWN_CLEAR] tip={} cleared_cd={} — covering=0 pin retry after mute pool lockout",
                            next_needed,
                            cleared_cd
                        );
                    }
                    pin_target = forced_tip.clone().or_else(|| {
                        self.best_covering0_tip_pin_candidate(next_needed)
                            .or_else(|| self.top_scored_peer_id())
                            .or_else(|| self.any_ready_active_worker_except(""))
                    });
                }
                // W138: idle-floor pin while a mid+ worker exists (often mid-W35 ahead)
                // re-locks mute thrash. Prefer mid+ and release their ahead.
                // Skip when sole_tip forced owner is set (Mode T dual: tip stays on :18333).
                if forced_tip.is_none()
                    && pin_target
                        .as_ref()
                        .is_some_and(|p| self.peer_score_of(p) <= Self::TIP_OWNER_MID_SCORE)
                {
                    if let Some(mid) =
                        self.active_ready_worker_above(Self::TIP_OWNER_MID_SCORE, false)
                    {
                        tracing::warn!(
                            "[IBD_TIP_PIN_PREFER_MID] tip={} floor_cand={} ({:.3}) → mid={} ({:.3})",
                            next_needed,
                            pin_target.as_deref().unwrap_or("-"),
                            pin_target
                                .as_ref()
                                .map(|p| self.peer_score_of(p))
                                .unwrap_or(0.0),
                            mid,
                            self.peer_score_of(&mid)
                        );
                        pin_target = Some(mid);
                    }
                }
                if let Some(ref forced) = forced_tip {
                    if pin_target.as_deref() != Some(forced.as_str()) {
                        tracing::warn!(
                            "[IBD_TIP_PIN_FORCED] tip={} → {} (first BLVM_IBD_PEERS)",
                            next_needed,
                            forced
                        );
                        pin_target = Some(forced.clone());
                    }
                }
                if let Some(ref top_w) = pin_target {
                    let ahead_only = {
                        let g = self.in_flight_per_peer.lock().unwrap();
                        Self::peer_inflight_ahead_only_map(&g, top_w, next_needed)
                    };
                    if ahead_only {
                        tracing::warn!(
                            "[IBD_TIP_PIN_RELEASE_AHEAD] peer={} tip={} — free W35 so tip re-arm is not blocked behind max_in_flight=1",
                            top_w,
                            next_needed
                        );
                        self.force_release_peer_inflight(top_w);
                    }
                }
                let mut g = self.preferred_tip_owner.lock().unwrap();
                if g.is_none() {
                    if let Some(top_w) = pin_target {
                        tracing::warn!(
                            "[IBD_TIP_PIN] covering=0 preferred=None → pin top_w={} score={:.3}",
                            top_w,
                            self.peer_score_of(&top_w)
                        );
                        *g = Some(top_w.clone());
                        drop(g);
                        self.reset_sticky_wan_tenure(&top_w, next_needed);
                    }
                }
            }
            self.clear_all_tip_cover_claims();
            self.log_tip_open_stall_diag(next_needed);
        }
        super::tip_stage::clear_tip_failover();
        self.open_tip_owner_slot();
        super::tip_stage::rearm_tip_sla();
        true
    }

    /// E15: ROTATE also blacklists A for 120s — OPEN pin retry must un-blacklist
    /// prior heroes (keep current OPEN sticky blacklisted).
    fn clear_blacklist_except(&self, keep: &str) -> usize {
        let mut bl = self.blacklisted_until.lock().unwrap();
        let before = bl.len();
        bl.retain(|peer, _| peer == keep);
        before.saturating_sub(bl.len())
    }

    /// Clear blacklists for ACTIVE download workers only (tip-deadlock recovery).
    fn clear_active_worker_blacklists(&self) -> usize {
        let mut bl = self.blacklisted_until.lock().unwrap();
        let mut n = 0usize;
        for p in self.active_download_worker_ids() {
            if bl.remove(&p).is_some() {
                n += 1;
            }
        }
        n
    }

    /// Clear blacklists for IBD-ready peers (even if not currently in `workers`).
    ///
    /// Live 2026-07-16: after tip stall, `ready=16` but `ready_active_ok=0/0` /
    /// `workers` empty or all blacklisted — worker-only clear left covering=0 forever.
    fn clear_ready_peer_blacklists(&self) -> usize {
        let ready: Vec<String> = self
            .ibd_ready_peers
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        let mut bl = self.blacklisted_until.lock().unwrap();
        let mut n = 0usize;
        for p in ready {
            if bl.remove(&p).is_some() {
                n += 1;
            }
        }
        n
    }

    /// W28d: register an explicit tip-cover claim (tip owner / failover / tests).
    pub(crate) fn note_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, s, e)| !(p == peer_id && *s == start && *e == end));
        g.push((peer_id.to_string(), start, end));
    }

    fn clear_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, s, e)| !(p == peer_id && *s == start && *e == end));
    }

    fn clear_tip_cover_claims_for_peer(&self, peer_id: &str) {
        let mut g = self.tip_cover_claims.lock().unwrap();
        g.retain(|(p, _, _)| p != peer_id);
    }

    /// Count in-flight tip-owner/failover claims covering `next_needed` (not ahead walk-ins).
    pub(crate) fn healthy_tip_cover_count(&self, next_needed: u64) -> usize {
        Self::healthy_tip_cover_count_from(&self.tip_cover_claims.lock().unwrap(), next_needed)
    }

    /// W4: count from a claims snapshot (avoids re-locking under `get_work`).
    fn healthy_tip_cover_count_from(claims: &[(String, u64, u64)], next_needed: u64) -> usize {
        claims
            .iter()
            .filter(|(_, s, e)| *s <= next_needed && next_needed <= *e)
            .count()
    }

    /// W4: clone tip-cover claims under the Mutex (one acquire for multiple counts).
    fn snapshot_tip_cover_claims(&self) -> Vec<(String, u64, u64)> {
        self.tip_cover_claims.lock().unwrap().clone()
    }

    /// Minimum remaining tip-cover depth that counts as a real deep tip pipe.
    ///
    /// Env: `BLVM_IBD_TIP_DEEP_COVER_MIN` (default **16**). Shallow walk-promote remnants
    /// (live tip=218: claim 218-224 after promote of ahead 193-224) must not block a new
    /// 128-deep owner through a full soft-retry budget.
    fn tip_deep_cover_min_depth() -> u64 {
        std::env::var("BLVM_IBD_TIP_DEEP_COVER_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16)
            .clamp(4, 128)
    }

    #[inline]
    fn claim_remaining_tip_depth(next_needed: u64, start: u64, end: u64) -> u64 {
        if start > next_needed || next_needed > end || start >= end {
            return 0;
        }
        end.saturating_sub(next_needed).saturating_add(1)
    }

    /// W65: preferred sticky still holds a substantial tip-cover claim.
    fn peer_holds_substantial_tip_cover(&self, peer_id: &str) -> bool {
        let next = self.next_needed_height();
        let min_depth = Self::tip_deep_cover_min_depth();
        self.tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| {
                p == peer_id && Self::claim_remaining_tip_depth(next, *s, *e) >= min_depth
            })
    }

    /// W30/W65: substantial deep tip-owner claims only.
    ///
    /// Ignores `(H,H)` failover micros **and** shallow walk-promote remnants whose
    /// remaining runway is below [`Self::tip_deep_cover_min_depth`]. Live genesis
    /// tip=218: promote of ahead `193-224` → claim `218-224` (depth 7) held tenure
    /// for ~40s of soft-retry while no 128-deep owner was assigned.
    fn deep_tip_cover_count(&self, next_needed: u64) -> usize {
        Self::deep_tip_cover_count_from(&self.tip_cover_claims.lock().unwrap(), next_needed)
    }

    /// W4: deep cover from a claims snapshot.
    fn deep_tip_cover_count_from(claims: &[(String, u64, u64)], next_needed: u64) -> usize {
        let min_depth = Self::tip_deep_cover_min_depth();
        claims
            .iter()
            .filter(|(_, s, e)| Self::claim_remaining_tip_depth(next_needed, *s, *e) >= min_depth)
            .count()
    }

    /// W30: drop all tip-cover claims so WAN gap can re-arm one deep owner.
    pub(crate) fn clear_all_tip_cover_claims(&self) {
        self.tip_cover_claims.lock().unwrap().clear();
    }

    fn is_tip_cover_claim(&self, peer_id: &str, start: u64, end: u64) -> bool {
        self.tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s == start && *e == end)
    }

    /// W49: tip walked into an ahead partition — **promote**, do not abort+reassign.
    ///
    /// Live WAN 564k→574k @ ~13 blk/s: tip-owner span histogram **32×364 / 128×27** with
    /// W28d "after walk-in preempt" spam (same 32-high ranges at tens of Hz). Cause: W43d
    /// aborted walk-ins once `tip_body_landed` while `next_needed` was still inside the
    /// span, then `get_work` re-armed a short tip pipe. That destroyed tip tenure.
    ///
    /// W43 full promote-into-claim blocked sticky re-arm when the walk-in was a zombie.
    /// W49 promotes only while tip is **inside** the span, drops other overlapping deep
    /// claims, and aborts only after tip has walked **past** `end` (true leftover ahead).
    pub(crate) fn should_abort_tip_walk_in(&self, peer_id: &str, start: u64, end: u64) -> bool {
        let next_needed = self.next_needed_height();
        // C1j: tip body missing + range strictly ahead → abort cheese GetData.
        // Prior: `next_needed < start` returned false → ahead kept filling tip+32 while
        // tip empty (C1h ahead_buf_p50=40 / C1i samples still TIP_HOLE_AHEAD).
        if self.tip_gap_missing.load(Ordering::Relaxed) && start > next_needed {
            static C1J_ABORT_LOG: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let prev = C1J_ABORT_LOG.load(Ordering::Relaxed);
            if now.saturating_sub(prev) >= 5
                && C1J_ABORT_LOG
                    .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                tracing::warn!(
                    "[IBD_C1J_ABORT] peer={} span={}-{} tip={} — past-tip GetData aborted while tip missing",
                    peer_id,
                    start,
                    end,
                    next_needed
                );
            }
            return true;
        }
        // Tip still below this span — not a tip walk-in (tip present).
        if next_needed < start {
            return false;
        }
        // Tip walked past the span — free leftover ahead GetData.
        if next_needed > end {
            if self.peer_recently_tip_streaming(
                peer_id,
                Duration::from_secs(Self::TIP_STREAM_HOT_SECS),
            ) {
                return false;
            }
            // Drop stale claims that no longer cover tip (promoted walk-in finished).
            {
                let mut g = self.tip_cover_claims.lock().unwrap();
                g.retain(|(p, _, e)| !(p == peer_id && *e < next_needed));
            }
            return true;
        }
        // Tip inside [start, end] — promote to tip-cover tenure and keep GetData.
        // W111: mute/SLA cooldown peers must not re-sticky via walk-promote.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            return true;
        }
        if self
            .tip_cover_claims
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s <= next_needed && next_needed <= *e)
        {
            return false;
        }
        self.promote_tip_walk_in(peer_id, start, end);
        false
    }

    /// Deep in-flight range (s < e) covering `next_needed` — ahead walk-in or tip pipe.
    /// Find an in-flight range that **substantially** covers tip (deep runway).
    ///
    /// W98: previously matched any `s < e` cover — live W97 freeze @312048 promoted
    /// shallow remnant 312018-312049 (remain=2) every 500ms via
    /// `[IBD_TIP_WALK_PROMOTE_SHALLOW]` while deep tip re-arm lagged ~7s.
    fn find_inflight_deep_covering(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) -> Option<(String, u64, u64)> {
        let min_depth = Self::tip_deep_cover_min_depth();
        let mut best: Option<(String, u64, u64, u64)> = None;
        for (peer, ranges) in in_flight {
            for &(s, e) in ranges {
                if s >= e || s > next_needed || next_needed > e {
                    continue;
                }
                let remain = Self::claim_remaining_tip_depth(next_needed, s, e);
                if remain < min_depth {
                    continue;
                }
                if best.as_ref().is_none_or(|(_, _, _, r)| remain > *r) {
                    best = Some((peer.clone(), s, e, remain));
                }
            }
        }
        best.map(|(p, s, e, _)| (p, s, e))
    }

    /// Convert an ahead walk-in that now covers `next_needed` into the tip-owner pipe.
    fn promote_tip_walk_in(&self, peer_id: &str, start: u64, end: u64) {
        let next_needed = self.next_needed_height();
        if next_needed < start || next_needed > end {
            return;
        }
        // W111: never re-sticky a mute/SLA-cooled peer from residual in-flight.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            tracing::warn!(
                "[IBD_TIP_WALK_PROMOTE_SKIP] peer={} tip={} — tip-owner cooldown (mute/SLA)",
                peer_id,
                next_needed
            );
            return;
        }
        {
            let mut g = self.tip_cover_claims.lock().unwrap();
            // W51/W65: idempotent only when an existing claim still has substantial
            // remaining tip runway. A shallow remnant must not block promote of a
            // longer walk-in (or leave deep_tip_cover stuck at a useless stub).
            let min_depth = Self::tip_deep_cover_min_depth();
            if g.iter()
                .any(|(_, s, e)| Self::claim_remaining_tip_depth(next_needed, *s, *e) >= min_depth)
            {
                return;
            }
            // Drop other peers' tip-covering claims (including shallow stubs).
            g.retain(|(p, s, e)| {
                p == peer_id || !(*s <= next_needed && next_needed <= *e && *s < *e)
            });
            g.retain(|(p, s, e)| !(p == peer_id && *s == next_needed && *e == end));
            g.push((peer_id.to_string(), next_needed, end));
        }
        let remain = Self::claim_remaining_tip_depth(next_needed, next_needed, end);
        let min_depth = Self::tip_deep_cover_min_depth();
        // W65: shallow remnants keep GetData via the claim above, but must NOT pin
        // preferred sticky — that blocked deep owners (live tip=218: preferred=walk
        // with claim 218-224 while soft-retry burned ~40s).
        if remain >= min_depth {
            self.note_tip_owner_assigned(peer_id);
        }
        static LAST_PROMOTE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_PROMOTE_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(prev) >= 500
            && LAST_PROMOTE_MS
                .compare_exchange(
                    prev,
                    now,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            if remain >= min_depth {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE] peer={} span={}-{} → claim={}-{} tip={} — keep GetData as tip owner",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed
                );
            } else {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE_SHALLOW] peer={} span={}-{} → claim={}-{} tip={} remain={} — GetData keep, not sticky (W65)",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed,
                    remain
                );
            }
        }
    }

    /// P5/A4: install peer scores used for gap routing and dual in-flight eligibility.
    pub(crate) fn set_peer_scores(&self, scores: &[(String, f64)]) {
        let mut g = self.peer_scores.lock().unwrap();
        g.clear();
        for (p, s) in scores {
            g.insert(p.clone(), *s);
        }
    }
}
