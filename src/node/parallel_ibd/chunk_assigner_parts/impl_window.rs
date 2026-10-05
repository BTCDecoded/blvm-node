// R-336: window assigner for the WAN crawl.
//
// Included from `chunk_assigner.rs` (private fns shared, like the other parts).
//
// Every run R-273–R-335 had the same crawl shape: one sticky tip owner streams a
// ~256-block runway, ~15 peers patch the holes it leaves, 30 of 46 ready peers
// idle, frontier `bridge_max - next_needed` ~230 regardless of `max_ahead`
// (256 / 1024 / 2048), BPS 30–90 above 200k. The legacy `get_work` reaches
// that shape through ~40 interlocking predicates (C1g freeze, KEEP=0 →
// `wan_hero_hot_for_ahead` false → stripe branch never fires, ahead peer cap,
// hole-first retry queue). Removing correctives one at a time (R-321–R-335)
// cleaned the wire and never moved busy off 16.
//
// This path replaces the crawl policy with the plain moving-window algorithm:
//
// * window = `[next_needed, next_needed + max_ahead]`
// * heights are handed out lowest-first in runs of `WINDOW_TILE` (16) that are
//   neither in flight (any peer) nor already delivered (`window_done`)
// * each peer holds at most `WINDOW_PER_PEER` (1) tiles at once; a fast peer
//   naturally cycles more tiles because it finishes sooner
// * if the front of the window (`next_needed`) has been in flight on one peer
//   for more than `WINDOW_STALL_SECS` (3) and nobody else covers it, the next
//   free peer gets a duplicate request for that tile (wire gate dedups)
// * a peer that failed a range does not get the same heights back for
//   `WINDOW_FAIL_SKIP_SECS` (30); the heights go to the next poller
//
// No sticky owner, no tip owner, no runway, no retry queue. Bounded by
// `max_ahead` (memory) and by per-peer tiles (fairness). Engages only when
// `next_needed >= window_from()` (default 1 since R-355; 120_000 until R-352 —
// the bootstrap chunk keeps the legacy path via `wan_tip_gap_crawl`) and
// `wan_tip_gap_crawl` is true. `BLVM_IBD_ASSIGN=legacy` disables it entirely.

#[cfg(test)]
thread_local! {
    // Thread-local so a forced-window test cannot leak into legacy tests running in parallel.
    static WINDOW_TEST_FORCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl ChunkAssigner {
    /// `BLVM_IBD_ASSIGN`: `window` (default) or `legacy`.
    ///
    /// Unit tests default to `legacy` (the ~430 assigner tests assert the legacy
    /// tip-owner policy); the window tests opt in through `window_test_force()`.
    pub(crate) fn window_assign_enabled() -> bool {
        #[cfg(test)]
        {
            if WINDOW_TEST_FORCE.with(|f| f.get()) {
                return true;
            }
            return false;
        }
        #[cfg(not(test))]
        {
            latch_env!(bool, {
                !matches!(
                    std::env::var("BLVM_IBD_ASSIGN")
                        .ok()
                        .as_deref()
                        .map(str::trim),
                    Some("legacy") | Some("0") | Some("off")
                )
            })
        }
    }

    #[cfg(test)]
    pub(crate) fn window_test_force(on: bool) {
        WINDOW_TEST_FORCE.with(|f| f.set(on));
    }

    /// Height from which the window path takes over (`BLVM_IBD_WINDOW_FROM`).
    ///
    /// R-355: default **1** (was 120_000). The pre-window legacy crawl parked 7000→8200 for
    /// 4.8 min in R-352 (`duty_ia_ms=60000`, sole-tip-owner hold, 58–77 ready peers idle);
    /// R-353 and R-354 ran the window path from the end of the bootstrap chunk (`=1`) and did
    /// 1→120k in 80.0 / 78.9 s with zero park lines. The dump path is unaffected:
    /// `window_mode_active` also requires `wan_tip_gap_crawl`, false until bootstrap completes.
    fn window_from() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_FROM")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
        })
    }

    /// True when the window path governs `next_needed` by height alone (coordinator admit
    /// sizing reads this; the legacy assigner below `WINDOW_FROM` keeps its 1024 admit).
    pub(crate) fn window_path_height(next_needed: u64) -> bool {
        Self::window_assign_enabled() && next_needed >= Self::window_from()
    }

    /// R-344: seconds a struck-out front holder is kept off the front reserve when it cannot
    /// be benched (`BLVM_IBD_WINDOW_FRONT_COOL_SECS`, default **0** = off).
    ///
    /// R-345: default off. R-344 ran 30 s as the *replacement* for bench and STALL_DUP age
    /// medians stayed 9–24 s: the 256-height reserve is < 1 s deep at 300 BPS, so a cooled
    /// peer's far tile is the front again almost at once (172.233.49.69 cooled 7× in one
    /// band, 353 dups). Off-window (bench) is the only placement that holds.
    fn window_front_cool_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_FRONT_COOL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                .min(3600)
        })
    }

    /// R-345: bench length by offense count within the last 10 min: `BENCH_SECS/4`,
    /// `BENCH_SECS`, `BENCH_SECS × 5` (15 / 60 / 300 s at the default 60). A transient stall
    /// costs a peer 15 s; a repeat front-staller ends up mostly off the window, which is what
    /// R-341's flat 60 s did for the tail (age 4–7 s) without holding every first offender
    /// for a minute (R-341 340–370k: 16 of 38 benched, busy 22).
    #[inline(never)]
    pub(crate) fn window_bench_secs_for(offenses: u32) -> u64 {
        let base = Self::window_bench_secs();
        match offenses {
            0 | 1 => (base / 4).max(5),
            2 => base,
            _ => base.saturating_mul(5).min(3600),
        }
    }

    /// Record a strike-out for `peer` and return the bench length it earns.
    fn window_note_offense(&self, peer: &str, now: Instant) -> u64 {
        let mut o = self.window_offense.lock().unwrap();
        let e = o.entry(peer.to_string()).or_insert((0, now));
        if now.saturating_duration_since(e.1).as_secs() > 600 {
            e.0 = 0;
        }
        e.0 = e.0.saturating_add(1);
        e.1 = now;
        Self::window_bench_secs_for(e.0)
    }

    /// Blocks per tile (`BLVM_IBD_WINDOW_TILE`, default 16, clamp 1–128).
    fn window_tile() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_TILE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
                .clamp(1, 128)
        })
    }

    /// Tiles a peer may hold at once (`BLVM_IBD_WINDOW_PER_PEER`, default 1, clamp 1–16).
    ///
    /// R-336 ran 2: 46 peers × 32 = 1472 slots > 1024 window, so the whole window
    /// was assigned at once and the front tiles sat on whichever slow peers held
    /// them (reorder 752 blocks, contig 0, one peer stalled the front 1139×). With
    /// 1 tile (16 = the per-peer GetData pipe) the window is never fully assigned
    /// and the front is always what the next free peer requests.
    fn window_per_peer() -> usize {
        latch_env!(usize, {
            std::env::var("BLVM_IBD_WINDOW_PER_PEER")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .clamp(1, 16)
        })
    }

    /// R-342: bytes a peer may have in flight (`BLVM_IBD_WINDOW_PEER_BYTES`, default 8 MB,
    /// clamp 1–64 MB). Only used when `BLVM_IBD_WINDOW_PER_PEER` is unset.
    ///
    /// R-341 cut tiles to 1 MB and kept per-peer 2 → 2 MB in flight per peer; per-peer
    /// throughput fell to ~1.1 MB/s (`bulk_gd_body_avg` 892 ms per 1 MB tile) from
    /// ~2.7 MB/s with 2 × 4 MB tiles in R-340, and NIC stayed at 60 MB/s with 22 busy peers.
    /// Small tiles keep the front tail short; the byte budget restores the pipe depth.
    fn window_peer_bytes() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_PEER_BYTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8_000_000)
                .clamp(1_000_000, 64_000_000)
        })
    }

    /// Tiles this peer may hold: explicit `WINDOW_PER_PEER` when set, else
    /// `PEER_BYTES / (tile × EMA block bytes)` clamped to `[1, 8]`. Slow peers hold ≤ 2.
    fn window_peer_cap(&self, tile: u64, slow: bool) -> usize {
        let cap = if std::env::var_os("BLVM_IBD_WINDOW_PER_PEER").is_some() {
            Self::window_per_peer()
        } else {
            #[cfg(test)]
            {
                Self::window_per_peer()
            }
            #[cfg(not(test))]
            {
                let est = crate::node::parallel_ibd::download::download_est_block_bytes();
                Self::window_peer_cap_for(est, tile, Self::window_peer_bytes())
            }
        };
        if slow {
            cap.min(2)
        } else {
            cap
        }
    }

    /// Pure: `peer_bytes / (tile × est_block_bytes)`, clamped to `[1, 8]`.
    #[inline(never)]
    pub(crate) fn window_peer_cap_for(est_block_bytes: u64, tile: u64, peer_bytes: u64) -> usize {
        let tile_bytes = tile.max(1).saturating_mul(est_block_bytes.max(1));
        (peer_bytes / tile_bytes.max(1)).clamp(1, 8) as usize
    }

    /// Front-of-window stall before a duplicate is issued (`BLVM_IBD_WINDOW_STALL_SECS`, default 3, clamp 1–120).
    /// R-336: tip GetData→body p50 was 1.1 s; a 10 s threshold wasted ~9 s per stalled front tile.
    fn window_stall_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_STALL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
                .clamp(1, 120)
        })
    }

    /// R-356: front-of-window age before a *duplicate* is issued (`BLVM_IBD_WINDOW_DUP_SECS`,
    /// default 1, clamp 1–120). Decoupled from the strike/bench threshold above: R-355 first
    /// dups per stuck tip resolved in p50 0.00–0.03 s / p90 ≤ 0.9 s (half all-local, half a
    /// ≤ 30 ms wire top-up) while the front had already waited the 3 s floor (age med 4–7 s,
    /// 96–313 tips per band, 120–200k `wall_wait_feeder` 76 % of wall with NIC at 23 MB/s).
    /// The dup is cheap; the wait for it was the cost. Strikes and the bench keep 3 s.
    fn window_dup_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_DUP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .clamp(1, 120)
        })
    }

    /// Front stalls by one holder before it is released + evicted (`BLVM_IBD_WINDOW_STALL_STRIKES`, default 2).
    fn window_stall_strikes() -> u32 {
        latch_env!(u32, {
            std::env::var("BLVM_IBD_WINDOW_STALL_STRIKES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2)
                .clamp(1, 20)
        })
    }

    /// Minimum seconds between window evictions (`BLVM_IBD_WINDOW_EVICT_INTERVAL_SECS`, default 15).
    fn window_evict_interval_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_EVICT_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(15)
                .clamp(0, 600)
        })
    }

    /// Never evict below this many IBD-ready peers (`BLVM_IBD_WINDOW_EVICT_MIN_READY`, default 12).
    fn window_evict_min_ready() -> usize {
        latch_env!(usize, {
            std::env::var("BLVM_IBD_WINDOW_EVICT_MIN_READY")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(12)
                .clamp(1, 200)
        })
    }

    /// R-337: disconnect front-stallers instead of benching them (`BLVM_IBD_WINDOW_EVICT=1`).
    /// Default off — eviction drained the roster (no reliable outbound refill).
    fn window_evict_enabled() -> bool {
        latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_WINDOW_EVICT").ok().as_deref(),
                Some("1") | Some("true") | Some("on") | Some("yes")
            )
        })
    }

    /// Bench length for a peer that struck out at the front (`BLVM_IBD_WINDOW_BENCH_SECS`, default 60).
    #[inline(never)]
    fn window_bench_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_BENCH_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60)
                .clamp(5, 3600)
        })
    }

    /// R-338: target bytes per tile (`BLVM_IBD_WINDOW_TILE_BYTES`, default 4 MB; 0 = fixed
    /// `WINDOW_TILE`). A 16-block tile is 2.4 MB at 200k and 8 MB at 340k; sizing tiles in
    /// bytes keeps per-tile latency — and therefore the front tail — flat across heights.
    fn window_tile_bytes() -> u64 {
        latch_env!(u64, {
            match std::env::var("BLVM_IBD_WINDOW_TILE_BYTES")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                Some(0) => 0,
                Some(v) => v.clamp(256_000, 64_000_000),
                // R-346 lock-in: every PASS/HOLD since R-341 ran 1 MB; with the wire-size
                // estimator (R-346) that is a true 1 MB tile (tile 16 / 7 / 5 / 4 / 4).
                None => 1_000_000,
            }
        })
    }

    /// Tile for the current block-size regime: `TILE_BYTES / EMA(block bytes)`, clamped to
    /// `[4, WINDOW_TILE]`. Uses the download layer's per-block EMA.
    fn window_tile_dyn(&self) -> u64 {
        let max_tile = Self::window_tile();
        // Unit tests share the process-wide block-bytes EMA with unrelated tests; keep the
        // fixed tile there and test the arithmetic through `window_tile_for`.
        #[cfg(test)]
        {
            return max_tile;
        }
        #[cfg(not(test))]
        {
            let target = Self::window_tile_bytes();
            if target == 0 {
                return max_tile;
            }
            let est = crate::node::parallel_ibd::download::download_est_block_bytes();
            Self::window_tile_for(est, target, max_tile)
        }
    }

    /// Pure: `target_bytes / est_block_bytes`, clamped to `[min(4, max_tile), max_tile]`.
    #[inline(never)]
    pub(crate) fn window_tile_for(est_block_bytes: u64, target_bytes: u64, max_tile: u64) -> u64 {
        (target_bytes / est_block_bytes.max(1)).clamp(4.min(max_tile), max_tile)
    }

    /// R-358: milliseconds of a peer's *own* measured throughput one tile should cover
    /// (`BLVM_IBD_WINDOW_TILE_MS`, default 600, clamp 100–5000; 0 = off, byte tile only).
    /// R-357 300–340k: one peer served 34 % of the wire at 38 ms per 4-block tile
    /// (busy-frac 0.96), the next three ran 212–234 ms per tile at 0.99 busy — every one
    /// drains its pipe at each tile boundary and waits a round trip for the next getdata,
    /// so a fast peer is capped at ≈ 1 MB / RTT. Top-10 peers carried 64–72 % of the wire
    /// from 250k. Sizing the tile to the peer's rate keeps its pipe full for `TILE_MS`
    /// instead of one RTT; slow peers keep the byte tile (this only grows a tile).
    fn window_tile_ms() -> u64 {
        latch_env!(u64, {
            match std::env::var("BLVM_IBD_WINDOW_TILE_MS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                Some(0) => 0,
                Some(v) => v.clamp(100, 5000),
                None => 600,
            }
        })
    }

    /// Pure: tile for a peer with `n` timed completions at `peer_blk_ms` per wire block.
    /// `target_ms / peer_blk_ms`, never below `base_tile` (the byte tile), never above
    /// `max_tile`. Fewer than 3 samples, or `target_ms == 0`, → `base_tile`.
    #[inline(never)]
    pub(crate) fn window_tile_for_peer_rate(
        base_tile: u64,
        peer_blk_ms: u64,
        n: u32,
        target_ms: u64,
        max_tile: u64,
    ) -> u64 {
        let base = base_tile.min(max_tile.max(1));
        if target_ms == 0 || n < 3 {
            return base;
        }
        (target_ms / peer_blk_ms.max(1)).clamp(base, max_tile.max(base))
    }

    /// R-358: per-peer tile — the byte tile grown to `TILE_MS` of this peer's measured rate.
    fn window_tile_for_peer(&self, peer_id: &str, base_tile: u64) -> u64 {
        let target_ms = Self::window_tile_ms();
        if target_ms == 0 {
            return base_tile;
        }
        let (ms, n) = {
            let per = self.window_peer_blk_ms.lock().unwrap();
            per.get(peer_id).copied().unwrap_or((0, 0))
        };
        Self::window_tile_for_peer_rate(base_tile, ms, n, target_ms, Self::window_tile())
    }

    /// R-338: stall threshold calibrated to observed tile latency: `max(WINDOW_STALL_SECS,
    /// ceil(1.5 × EMA(tile ms)))`, capped at 30 s. Floor applies while no tile has completed.
    #[inline(never)]
    fn window_stall_dyn(&self) -> u64 {
        let floor = Self::window_stall_secs();
        let ema_ms = self.window_tile_ms_ema.load(Ordering::Relaxed);
        if ema_ms == 0 {
            return floor;
        }
        // R-340: 2.5× drifted stall_s to 6 s at 340k (dup age median 7 s vs 3 s in R-338);
        // 1.5× keeps the floor in charge until tiles genuinely slow down.
        let calibrated = (ema_ms * 3 / 2).div_ceil(1000);
        calibrated.clamp(floor, 30)
    }

    /// R-356: age at which the front holder gets a duplicate: `round(1.5 × EMA(tile ms))`
    /// clamped to `[WINDOW_DUP_SECS, stall]`. Never later than the strike threshold, so the
    /// dup always precedes (or coincides with) the bench decision.
    #[inline(never)]
    pub(crate) fn window_dup_after(dup_floor: u64, tile_ms_ema: u64, stall: u64) -> u64 {
        let floor = dup_floor.min(stall);
        if tile_ms_ema == 0 {
            return floor;
        }
        let calibrated = (tile_ms_ema * 3 / 2 + 500) / 1000;
        calibrated.clamp(floor, stall)
    }

    #[inline(never)]
    fn window_dup_dyn(&self, stall: u64) -> u64 {
        Self::window_dup_after(
            Self::window_dup_secs(),
            self.window_tile_ms_ema.load(Ordering::Relaxed),
            stall,
        )
    }

    /// R-354: who may take the front stall dup. A getdata is served in order behind
    /// everything the peer already has queued, so a dup handed to a poller with
    /// `cap − 1` tiles in flight (≈ 7 MB at 200–250k, ~1 MB/s per peer) lands ~7 s later —
    /// R-353 `stall_s=3` yet STALL_DUP age median 7 / 9 / 8 / 12 s and 1959 dups over
    /// ~350 tips (5–7 re-dups per stalled front). Hand the dup only to a poller with an
    /// empty or one-deep pipe (idle fast peers exist whenever the window is fully covered);
    /// past `2 × stall` any non-slow poller may take it so a thin roster cannot hold the
    /// front forever.
    #[inline(never)]
    pub(crate) fn window_dup_taker_ok(taker_inflight: usize, oldest_age: u64, stall: u64) -> bool {
        taker_inflight <= 1 || oldest_age >= stall.saturating_mul(2)
    }

    /// Whether this poller should duplicate the front tile.
    ///
    /// A peer with an empty pipe duplicates at `dup_floor` (default 1s), including a peer
    /// the EMA has marked slow. Resume13: `dup_s=30` but the duplicate age was 54s median
    /// and 173s p90, because every non-slow peer was inside a download and the only
    /// pollers were the slow ones, who were barred by `!slow`. The empty-pipe request is
    /// the one that is not queued behind that peer's current tile.
    pub(crate) fn window_front_dup_ok(
        idle: bool,
        slow: bool,
        oldest_age: u64,
        dup_floor: u64,
        dup_after: u64,
        taker_inflight: usize,
        stall: u64,
    ) -> bool {
        if oldest_age < dup_floor {
            return false;
        }
        // Resume26: 2867707 sat on one peer for 287s (getdata EWMA ~22s). Every
        // other peer was marked slow and already held a tile, so this predicate
        // refused them. A different peer then delivered that block in 772ms.
        // The second cover is the one-block tip; the slow flag does not bar it.
        let _ = (dup_after, slow);
        if idle {
            return true;
        }
        Self::window_dup_taker_ok(taker_inflight, oldest_age, stall)
    }

    /// Resume23: two peers already covered 2861258 and neither had the block.
    /// `front_covers < 2` kept every idle peer off that height for 109s.
    /// One more idle peer may take the single height once the oldest cover
    /// is past the dup floor. A fourth peer does not.
    pub(crate) fn uncovered_tip_third_cover(
        uncovered: bool,
        idle: bool,
        covers: usize,
        oldest_age: u64,
        dup_floor: u64,
    ) -> bool {
        uncovered && idle && covers == 2 && oldest_age >= dup_floor
    }

    /// Resume27: three 4-block tiles already covered the tip, so `covers == 2`
    /// never matched and no peer was asked for that one height. The tiles stayed
    /// 45–90s because a later block in the tile kept the chunk alive.
    ///
    /// An idle peer may take `(next, next)` once every cover is at least
    /// `dup_floor` old. A missing stamp counts as fresh. At most three of those
    /// one-block requests exist; a wide tile does not consume that cap and is
    /// not cancelled. A cover younger than the floor — the fast stretch, where
    /// the body is back in well under a second — adds nothing.
    pub(crate) fn tip_needs_fresh_cover(
        uncovered: bool,
        holds: bool,
        cooled: bool,
        idle: bool,
        any_young: bool,
        dedicated: usize,
    ) -> bool {
        uncovered && !holds && !cooled && idle && !any_young && dedicated < 3
    }

    /// A `window_done` mark hides that height from `get_work`.
    ///
    /// While the tip body is missing, the cursor itself stays assignable even
    /// if a late completion re-marked it. Resume21 otherwise handed the next
    /// tile 400 above the hole. Every other delivered height stays hidden.
    /// Treating the whole hole as free reloaded those blocks from disk:
    /// R-379 completed 1.46M local blocks in 200–250k against 50k wire blocks.
    /// Hiding them is not an assignment cap. A height with no mark, including
    /// past the buffered body, is still free, and `past_hole_should_release`
    /// stays false.
    fn window_done_covers_height(&self, h: u64, next: u64) -> bool {
        if self.window_tip_uncovered.load(Ordering::Relaxed) && h == next {
            return false;
        }
        true
    }

    /// Ahead of the buffered body, keep the tile.
    ///
    /// R-377 aborted these and also refused to assign past `first_ahead`.
    /// Busy peers fell (67 → 42 at 200–250k, 50 → 35 at 370–400k) and the
    /// frontier stopped at the hole, so the cursor met the next missing
    /// height with no runway. `tip_need_body` was 36s → 121s on that band.
    /// The next idle peer still takes the hole first. Aborting this tile
    /// drops the bodies already in it.
    pub(crate) fn past_hole_should_release(&self, _chunk_start: u64) -> bool {
        false
    }

    /// R-341: heights at the front of the window reserved for peers that are not slow
    /// (`BLVM_IBD_WINDOW_FRONT_RESERVE`, default 256; 0 disables placement).
    fn window_front_reserve() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_FRONT_RESERVE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(256)
                .min(8192)
        })
    }

    /// R-341: a peer is slow when its per-block tile time exceeds this many percent of the
    /// roster EMA (`BLVM_IBD_WINDOW_SLOW_PCT`, default 200 = 2×). Needs ≥ 3 completions.
    fn window_slow_pct() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_SLOW_PCT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(200)
                .clamp(110, 2000)
        })
    }

    /// R-341: slow-peer predicate from per-peer vs roster per-block tile time.
    /// R-340 340–370k: STALL_DUP age median 13 s with stall_s 3 — the front tile kept
    /// landing on 0.5 MB/s peers because lowest-first hands it to whoever polls.
    #[inline(never)]
    fn window_peer_is_slow(&self, peer_id: &str) -> bool {
        let global = self.window_blk_ms_ema.load(Ordering::Relaxed);
        if global == 0 {
            return false;
        }
        let per = self.window_peer_blk_ms.lock().unwrap();
        match per.get(peer_id) {
            Some(&(ms, n)) if n >= 3 => ms * 100 > global * Self::window_slow_pct(),
            _ => false,
        }
    }

    /// Peers the window path wants disconnected (drained by the worker loop, which holds the network).
    pub(crate) fn window_take_evictions(&self) -> Vec<String> {
        std::mem::take(&mut *self.window_evict.lock().unwrap())
    }

    /// How long a failing peer is kept off the heights it failed (`BLVM_IBD_WINDOW_FAIL_SKIP_SECS`, default 30).
    fn window_fail_skip_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_FAIL_SKIP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30)
                .clamp(0, 600)
        })
    }

    /// True when the window path owns `get_work` for this `next_needed`.
    pub(crate) fn window_mode_active(&self, next_needed: u64) -> bool {
        Self::window_assign_enabled()
            && next_needed >= Self::window_from()
            && self.wan_tip_gap_crawl(next_needed)
    }

    /// Window-path `get_work`. Caller has already handled shutdown / blacklist.
    pub(crate) fn window_get_work(&self, peer_id: &str, max_ahead: u64) -> Option<(u64, u64)> {
        let next_needed = self.next_needed_height();
        let mut window_hi = next_needed.saturating_add(max_ahead.max(Self::window_tile()));
        // Never past the stored headers or the IBD end height.
        let header_tip = self.header_tip();
        if header_tip > 0 {
            window_hi = window_hi.min(header_tip);
        }
        let end = self.ibd_end_height.load(Ordering::Relaxed);
        if end > 0 {
            window_hi = window_hi.min(end);
        }
        if window_hi < next_needed {
            return None;
        }
        // R-358: byte tile grown to TILE_MS of this peer's own measured rate (fast peers
        // take bigger tiles so their pipe does not drain at every tile boundary).
        let tile = self.window_tile_for_peer(peer_id, self.window_tile_dyn());
        let now = Instant::now();

        // Benched (front-staller) peers get no window work until their bench expires.
        {
            let b = self.window_bench.lock().unwrap();
            if let Some(until) = b.get(peer_id) {
                if *until > now {
                    return None;
                }
            }
        }

        // R-376: do not hand a tile to a peer that has not VerAck'd.
        // The download path waits up to 15s inside the chunk (`handshake not complete`)
        // and that chunk is often the validation tip. An empty ready snapshot used to
        // mean "allow everyone", so the first tiles went out before the coordinator's
        // first refresh — including to mainnet :8333. Empty means nobody is ready,
        // except window unit tests that never populate the set.
        let tests_without_ready_snapshot = self.ibd_ready_peer_count() == 0 && {
            #[cfg(test)]
            {
                WINDOW_TEST_FORCE.with(|f| f.get())
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        if !tests_without_ready_snapshot && !self.peer_is_ibd_ready(peer_id) {
            return None;
        }

        // R-341: slow peers keep off the front of the window and never take the stall dup.
        // R-344: so do holders released from the front by a strike-out, for FRONT_COOL_SECS.
        let cooled = {
            let mut c = self.window_front_cool.lock().unwrap();
            c.retain(|_, until| *until > now);
            c.contains_key(peer_id)
        };
        let slow = Self::window_front_reserve() > 0 && (cooled || self.window_peer_is_slow(peer_id));
        let walk_from = if slow {
            next_needed.saturating_add(Self::window_front_reserve())
        } else {
            next_needed
        };

        let mut guard = self.in_flight_per_peer.lock().unwrap();

        // Per-peer tile cap (R-342: byte budget unless WINDOW_PER_PEER is set).
        let mine = guard.get(peer_id).map(|v| v.len()).unwrap_or(0);
        if mine >= self.window_peer_cap(tile, slow) {
            // A slow peer at the byte cap used to return here, so the sole tip
            // holder stayed alone. Resume26: that holder was 287s old and the
            // next peer delivered the block in 772ms.
            if !cooled {
                let floor = Self::window_dup_secs().max(1);
                let mut covers = 0usize;
                let mut oldest = 0u64;
                let mut holds = false;
                {
                    let started = self.window_started.lock().unwrap();
                    for (p, ranges) in guard.iter() {
                        for &(s, e) in ranges {
                            if s <= next_needed && next_needed <= e {
                                covers += 1;
                                if p == peer_id {
                                    holds = true;
                                }
                                if let Some(t0) = started.get(&(p.clone(), s, e)) {
                                    oldest = oldest.max(now.saturating_duration_since(*t0).as_secs());
                                }
                            }
                        }
                    }
                }
                if !holds && covers == 1 && oldest >= floor {
                    Self::insert_in_flight(&mut guard, peer_id, next_needed, next_needed);
                    self.window_started.lock().unwrap().insert(
                        (peer_id.to_string(), next_needed, next_needed),
                        now,
                    );
                    self.note_tip_cover_claim(peer_id, next_needed, next_needed);
                    crate::node::parallel_ibd::ms_breakdown::note_peers_inflight(
                        guard.values().filter(|v| !v.is_empty()).count(),
                    );
                    tracing::warn!(
                        "[IBD_TIP_SECOND] tip={} age_s={} dup_to={} — one block, holder kept, taker was at cap",
                        next_needed,
                        oldest,
                        peer_id
                    );
                    return Some((next_needed, next_needed));
                }
            }
            return None;
        }

        // Coverage bitmap over the window: in flight (any peer) or delivered.
        let span = (window_hi - next_needed + 1) as usize;
        let mut covered = vec![false; span];
        let idx = |h: u64| (h - next_needed) as usize;
        // (peer, range end, issue time) for every range covering next_needed.
        let mut front_holders: Vec<(String, u64, Option<Instant>)> = Vec::new();
        let mut front_covers = 0usize;
        let mut dedicated = 0usize;
        let mut any_young = false;
        let dup_floor = Self::window_dup_secs();
        for (p, ranges) in guard.iter() {
            for &(s, e) in ranges {
                if e < next_needed || s > window_hi {
                    continue;
                }
                let lo = s.max(next_needed);
                let hi = e.min(window_hi);
                for h in lo..=hi {
                    covered[idx(h)] = true;
                }
                if s <= next_needed && next_needed <= e {
                    front_covers += 1;
                    if s == next_needed && e == next_needed {
                        dedicated += 1;
                    }
                    let t0 = self
                        .window_started
                        .lock()
                        .unwrap()
                        .get(&(p.clone(), s, e))
                        .copied();
                    match t0 {
                        None => any_young = true,
                        Some(t) if now.saturating_duration_since(t).as_secs() < dup_floor => {
                            any_young = true;
                        }
                        Some(_) => {}
                    }
                    front_holders.push((p.clone(), e, t0));
                }
            }
        }
        {
            let mut done = self.window_done.lock().unwrap();
            // Drop delivered heights the validator has passed.
            while let Some((&h, _)) = done.iter().next() {
                if h < next_needed {
                    done.remove(&h);
                } else {
                    break;
                }
            }
            for (&h, _) in done.range(next_needed..=window_hi) {
                if self.window_done_covers_height(h, next_needed) {
                    covered[idx(h)] = true;
                }
            }
        };
        // Drop issue stamps for ranges no longer in flight (chunk_fail with no peer).
        self.window_started.lock().unwrap().retain(|(p, s, e), _| {
            guard.get(p).is_some_and(|v| v.contains(&(*s, *e)))
        });

        // Front-of-window stall. Every holder of `next_needed` past the threshold
        // takes a strike (one per threshold interval); on the second strike its
        // tiles are released and it is queued for eviction (Core disconnects a
        // peer that stalls the window). While fewer than two peers cover the
        // front and the oldest is past the threshold, this peer gets a duplicate.
        //
        // Resume7: done-ahead (28–68) made the old 3 s steal fire while the gauge
        // still said dup_s=30 (tile EMA 72 s). The tip tile moved every 3–4 s and
        // the GetData that finally soft-retried had waited 86 s against a 5 s
        // limit. A finished higher tile is not a reason to ignore the EMA cap.
        let mut oldest_age = 0u64;
        let mut oldest_holder: Option<(String, u64)> = None;
        for (holder, hold_end, t0) in front_holders.iter() {
            let Some(t0) = t0 else { continue };
            let age = now.saturating_duration_since(*t0).as_secs();
            if age > oldest_age || oldest_holder.is_none() {
                oldest_age = age;
                oldest_holder = Some((holder.clone(), *hold_end));
            }
        }
        let holds_front = front_holders.iter().any(|(p, _, _)| p == peer_id);
        if front_covers > 0 && !holds_front {
            let stall = self.window_stall_dyn();
            // R-356: dup fires at `dup_after` (≤ stall); strikes/bench stay on `stall`.
            let dup_after = self.window_dup_dyn(stall);
            let mut evicted_any = false;
            for (holder, hold_end, t0) in front_holders.iter() {
                let Some(t0) = t0 else { continue };
                let age = now.saturating_duration_since(*t0).as_secs();
                if age > oldest_age || oldest_holder.is_none() {
                    oldest_age = age;
                    oldest_holder = Some((holder.clone(), *hold_end));
                }
                if age < stall {
                    continue;
                }
                let strikes = {
                    let mut st = self.window_strikes.lock().unwrap();
                    let e = st.entry(holder.clone()).or_insert((0, now - Duration::from_secs(3600)));
                    if now.saturating_duration_since(e.1).as_secs() > 120 {
                        e.0 = 0;
                    }
                    if now.saturating_duration_since(e.1).as_secs() >= stall {
                        e.0 += 1;
                        e.1 = now;
                    }
                    e.0
                };
                if strikes >= Self::window_stall_strikes() {
                    let ready = self.ibd_ready_peer_count();
                    if Self::window_evict_enabled() {
                        let mut last = self.window_last_evict.lock().unwrap();
                        let interval_ok = last
                            .map(|t| {
                                now.saturating_duration_since(t).as_secs()
                                    >= Self::window_evict_interval_secs()
                            })
                            .unwrap_or(true);
                        if ready > Self::window_evict_min_ready() && interval_ok {
                            let released = guard.remove(holder).map(|v| v.len()).unwrap_or(0);
                            self.window_started
                                .lock()
                                .unwrap()
                                .retain(|(p, _, _), _| p != holder);
                            self.window_evict.lock().unwrap().push(holder.clone());
                            *last = Some(now);
                            self.window_strikes.lock().unwrap().remove(holder);
                            evicted_any = true;
                            tracing::warn!(
                                "[IBD_WINDOW_EVICT] holder={} stalled front {}× (tip={} age_s={}) — released {} tile(s), evicting (ready={})",
                                holder,
                                strikes,
                                next_needed,
                                age,
                                released,
                                ready
                            );
                        }
                    } else {
                        // R-337: bench instead of evict. Eviction drained the roster
                        // (ready 48 → 27 in 10 min; 11 of 24 replacements connected, none
                        // became ready) and the 15 s rate limit let holders pile up 5–10
                        // strikes. Benching keeps the connection, takes the peer off the
                        // window for `window_bench_secs`, and needs no rate limit.
                        let benched_now = {
                            let mut b = self.window_bench.lock().unwrap();
                            b.retain(|_, until| *until > now);
                            b.len()
                        };
                        // R-345: release the front tile(s) and bench the holder off the window
                        // (escalating 15 / 60 / 300 s), never more than half the roster.
                        // R-343 (bench only slow) and R-344 (release + front cooldown) both
                        // left non-delivering holders on the front: age medians 13–24 s vs
                        // R-341's 4–7 s with bench-all.
                        let bench = benched_now < ready / 2;
                        let bench_secs = if bench { self.window_note_offense(holder, now) } else { 0 };
                        let released = guard.remove(holder).map(|v| v.len()).unwrap_or(0);
                        self.window_started
                            .lock()
                            .unwrap()
                            .retain(|(p, _, _), _| p != holder);
                        if bench {
                            self.window_bench
                                .lock()
                                .unwrap()
                                .insert(holder.clone(), now + Duration::from_secs(bench_secs));
                        } else if Self::window_front_cool_secs() > 0 {
                            self.window_front_cool.lock().unwrap().insert(
                                holder.clone(),
                                now + Duration::from_secs(Self::window_front_cool_secs()),
                            );
                        }
                        self.window_strikes.lock().unwrap().remove(holder);
                        evicted_any = true;
                        tracing::warn!(
                            "[IBD_WINDOW_BENCH] holder={} stalled front {}× (tip={} age_s={}) — released {} tile(s), {} (benched={} ready={})",
                            holder,
                            strikes,
                            next_needed,
                            age,
                            released,
                            if bench {
                                format!("benched {bench_secs}s")
                            } else {
                                format!("front cooldown {}s", Self::window_front_cool_secs())
                            },
                            benched_now + usize::from(bench),
                            ready
                        );
                    }
                }
            }
            if evicted_any {
                // Coverage changed; fall through to the plain walk, which now sees
                // the released heights as free (lowest-first hands them out).
                for (holder, _, _) in front_holders.iter() {
                    if !guard.contains_key(holder) {
                        self.clear_tip_cover_claims_for_peer(holder);
                    }
                }
                covered = vec![false; span];
                for ranges in guard.values() {
                    for &(s, e) in ranges {
                        if e < next_needed || s > window_hi {
                            continue;
                        }
                        for h in s.max(next_needed)..=e.min(window_hi) {
                            covered[idx(h)] = true;
                        }
                    }
                }
                let done = self.window_done.lock().unwrap();
                for (&h, _) in done.range(next_needed..=window_hi) {
                    if self.window_done_covers_height(h, next_needed) {
                        covered[idx(h)] = true;
                    }
                }
            } else if Self::uncovered_tip_third_cover(
                self.window_tip_uncovered.load(Ordering::Relaxed),
                mine == 0,
                front_covers,
                oldest_age,
                Self::window_dup_secs(),
            ) {
                Self::insert_in_flight(&mut guard, peer_id, next_needed, next_needed);
                self.window_started.lock().unwrap().insert(
                    (peer_id.to_string(), next_needed, next_needed),
                    now,
                );
                self.note_tip_cover_claim(peer_id, next_needed, next_needed);
                crate::node::parallel_ibd::ms_breakdown::note_peers_inflight(
                    guard.values().filter(|v| !v.is_empty()).count(),
                );
                tracing::warn!(
                    "[IBD_TIP_THIRD] tip={} covers={} age_s={} dup_to={} — one block, holders kept",
                    next_needed,
                    front_covers,
                    oldest_age,
                    peer_id
                );
                return Some((next_needed, next_needed));
            } else if front_covers < 2
                && Self::window_front_dup_ok(
                    mine == 0,
                    slow,
                    oldest_age,
                    Self::window_dup_secs(),
                    dup_after,
                    mine,
                    stall,
                )
            {
                if let Some((holder, hold_end)) = oldest_holder {
                    let end = hold_end.min(next_needed + tile - 1);
                    Self::insert_in_flight(&mut guard, peer_id, next_needed, end);
                    self.window_started
                        .lock()
                        .unwrap()
                        .insert((peer_id.to_string(), next_needed, end), now);
                    crate::node::parallel_ibd::ms_breakdown::note_peers_inflight(
                        guard.values().filter(|v| !v.is_empty()).count(),
                    );
                    tracing::warn!(
                        "[IBD_WINDOW_STALL_DUP] tip={} holder={} age_s={} dup_to={} {}-{} taker_inflight={}",
                        next_needed,
                        holder,
                        oldest_age,
                        peer_id,
                        next_needed,
                        end,
                        mine
                    );
                    return Some((next_needed, end));
                }
            } else if Self::tip_needs_fresh_cover(
                self.window_tip_uncovered.load(Ordering::Relaxed),
                holds_front,
                cooled,
                mine == 0,
                any_young,
                dedicated,
            ) {
                Self::insert_in_flight(&mut guard, peer_id, next_needed, next_needed);
                self.window_started.lock().unwrap().insert(
                    (peer_id.to_string(), next_needed, next_needed),
                    now,
                );
                self.note_tip_cover_claim(peer_id, next_needed, next_needed);
                crate::node::parallel_ibd::ms_breakdown::note_peers_inflight(
                    guard.values().filter(|v| !v.is_empty()).count(),
                );
                tracing::warn!(
                    "[IBD_TIP_FRESH] tip={} wide={} dedicated={} age_s={} dup_to={} — one block, wide holders kept",
                    next_needed,
                    front_covers.saturating_sub(dedicated),
                    dedicated,
                    oldest_age,
                    peer_id
                );
                return Some((next_needed, next_needed));
            }
        }

        // Recently failed heights for this peer.
        let skip_secs = Self::window_fail_skip_secs();
        let my_fails: Vec<(u64, u64)> = {
            let mut f = self.window_fail.lock().unwrap();
            f.retain(|_, (_, t)| now.saturating_duration_since(*t).as_secs() < skip_secs.max(1));
            f.iter()
                .filter(|(_, (p, _))| p == peer_id)
                .map(|(&(s, e), _)| (s, e))
                .collect()
        };
        let failed_by_me = |h: u64| my_fails.iter().any(|&(s, e)| s <= h && h <= e);

        // Lowest-first free run of up to `tile` heights (slow peers start past the reserve).
        // The hole is the first free height. Do not stop at the buffered body:
        // R-377 left idle peers with nothing once that span was in flight, and
        // the runway behind it never refilled. `window_done` still hides a
        // body that is already in reorder.
        let assign_hi = window_hi;
        let mut h = walk_from;
        while h <= assign_hi {
            if covered[idx(h)] || failed_by_me(h) {
                h += 1;
                continue;
            }
            let start = h;
            let mut end = h;
            while end < assign_hi
                && end + 1 - start < tile
                && !covered[idx(end + 1)]
                && !failed_by_me(end + 1)
            {
                end += 1;
            }
            Self::insert_in_flight(&mut guard, peer_id, start, end);
            self.window_started
                .lock()
                .unwrap()
                .insert((peer_id.to_string(), start, end), now);
            crate::node::parallel_ibd::ms_breakdown::note_peers_inflight(
                guard.values().filter(|v| !v.is_empty()).count(),
            );
            if start <= next_needed && next_needed <= end {
                // Tip-covering work is a tip claim for the gauges that read claims.
                self.note_tip_cover_claim(peer_id, start, end);
            }
            return Some((start, end));
        }
        None
    }

    /// Ok completion: heights delivered (network or local). Called from the worker.
    ///
    /// R-355: `net_blocks` is how many of the heights came off the wire. All-local
    /// completions (`net=0`, ~2 ms for a 4-block tile) used to feed the tile / per-block
    /// EMAs: at 340–370k 21 % of completions were all-local and a stall dup that finds its
    /// blocks in the store re-completes 40–70× in 150 ms, so the roster EMA sat at 34–58 ms
    /// per block while real tiles took 250–500 ms → `slow=79` of `ready=79`, every wire
    /// peer past the front reserve on cap ≤ 2, `stall_s` pinned at the 3 s floor, bench
    /// saturated at ready/2 (40), frontier +900–1900 with the window starving. Timing
    /// samples now come only from wire blocks; local completions still clear bookkeeping.
    pub(crate) fn window_note_complete(
        &self,
        peer_id: &str,
        start: u64,
        end: u64,
        net_blocks: u64,
    ) {
        if !Self::window_assign_enabled() {
            return;
        }
        let started = self
            .window_started
            .lock()
            .unwrap()
            .remove(&(peer_id.to_string(), start, end));
        let now = Instant::now();
        if let (Some(t0), true) = (started, net_blocks > 0) {
            // R-338: EMA (7/8) of tile wall time drives the stall threshold.
            let ms = now.saturating_duration_since(t0).as_millis() as u64;
            let old = self.window_tile_ms_ema.load(Ordering::Relaxed);
            let next_ema = if old == 0 { ms } else { (old * 7 + ms) / 8 };
            self.window_tile_ms_ema.store(next_ema, Ordering::Relaxed);
            // R-341: per-block time, roster-wide and per peer, for slow-peer placement.
            // R-355: per *wire* block — a half-local tile is not twice as fast.
            let per_blk = ms / net_blocks;
            let g = self.window_blk_ms_ema.load(Ordering::Relaxed);
            let g_next = if g == 0 { per_blk } else { (g * 7 + per_blk) / 8 };
            self.window_blk_ms_ema.store(g_next, Ordering::Relaxed);
            let mut per = self.window_peer_blk_ms.lock().unwrap();
            let e = per.entry(peer_id.to_string()).or_insert((0, 0));
            e.0 = if e.1 == 0 { per_blk } else { (e.0 * 3 + per_blk) / 4 };
            e.1 = e.1.saturating_add(1);
        }
        let next = self.next_needed_height();
        if end < next {
            return;
        }
        let mut done = self.window_done.lock().unwrap();
        for h in start.max(next)..=end {
            done.insert(h, now);
        }
    }

    fn window_unmark_secs() -> u64 {
        latch_env!(u64, {
            std::env::var("BLVM_IBD_WINDOW_UNMARK_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5)
                .clamp(1, 120)
        })
    }

    /// Coordinator says `height` is missing at the tip. If we marked it delivered more
    /// than `WINDOW_UNMARK_SECS` (5) ago, it was lost downstream (bridge evict, reorder
    /// pressure, reject) — un-mark so the next poller re-issues it. Younger marks are
    /// blocks still in transit reorder → bridge; leave them.
    pub(crate) fn window_note_missing(&self, height: u64) {
        let until = self.window_hole_until.load(Ordering::Relaxed);
        // `first_ahead` is exclusive: those bodies are already in reorder.
        // Absent (`0`) or already at the cursor stays a one-height unmark so
        // the frontier past the buffer is not re-opened.
        let end = if until > height.saturating_add(1) {
            until
        } else {
            height.saturating_add(1)
        };
        self.window_note_missing_span(height, end);
    }

    /// Un-mark every delivered height in `[from, until)` that is older than the unmark
    /// threshold and not currently in flight.
    ///
    /// R-376 resume: stall recovery only unmarked `next_needed`. The next ~60 heights
    /// stayed in `window_done` after the bodies left reorder, so idle peers kept taking
    /// frontier+2000 while validation re-fetched one hole height per stall (~0.2 BPS).
    pub(crate) fn window_note_missing_span(&self, from: u64, until: u64) {
        if !Self::window_assign_enabled() || until <= from {
            return;
        }
        let unmark_secs = Self::window_unmark_secs();
        let now = Instant::now();
        // Same lock order as `window_get_work`: in-flight, then done.
        let mut flying = Vec::new();
        {
            let guard = self.in_flight_per_peer.lock().unwrap();
            for ranges in guard.values() {
                for &(s, e) in ranges {
                    if e < from || s >= until {
                        continue;
                    }
                    let lo = s.max(from);
                    let hi = e.min(until.saturating_sub(1));
                    for h in lo..=hi {
                        flying.push(h);
                    }
                }
            }
        }
        flying.sort_unstable();
        flying.dedup();
        let mut done = self.window_done.lock().unwrap();
        let stale: Vec<u64> = done
            .range(from..until)
            .filter(|(h, t)| {
                now.saturating_duration_since(**t).as_secs() >= unmark_secs
                    && flying.binary_search(h).is_err()
            })
            .map(|(h, _)| *h)
            .collect();
        let n = stale.len();
        if n == 0 {
            return;
        }
        let first = stale[0];
        let last = stale[n - 1];
        for h in stale {
            done.remove(&h);
        }
        tracing::warn!(
            "[IBD_WINDOW_UNMARK] span {}..{} re-issue {} height(s) {}..={}",
            from,
            until,
            n,
            first,
            last
        );
    }

    /// Drop `window_done` marks in `[from, until)` that are not in flight, with no age grace.
    ///
    /// Resume17: the validation tip sat in `window_done` for minutes after the body
    /// left reorder. `window_note_missing` waits 5s and only runs from stall requeue,
    /// so each absent tip cost ~11s (`wait_ms` 10–14s, wire 83ms once re-issued).
    /// The coordinator calls this on the gap poll, while the body is in neither
    /// reorder, the feeder, nor the bridge.
    pub(crate) fn window_reissue_absent(&self, from: u64, until: u64) {
        if !Self::window_assign_enabled() || until <= from {
            return;
        }
        let mut flying = Vec::new();
        {
            let guard = self.in_flight_per_peer.lock().unwrap();
            for ranges in guard.values() {
                for &(s, e) in ranges {
                    if e < from || s >= until {
                        continue;
                    }
                    let lo = s.max(from);
                    let hi = e.min(until.saturating_sub(1));
                    for h in lo..=hi {
                        flying.push(h);
                    }
                }
            }
        }
        flying.sort_unstable();
        flying.dedup();
        let mut done = self.window_done.lock().unwrap();
        let stale: Vec<u64> = done
            .range(from..until)
            .filter(|(h, _)| flying.binary_search(h).is_err())
            .map(|(h, _)| *h)
            .collect();
        let n = stale.len();
        if n == 0 {
            return;
        }
        let first = stale[0];
        let last = stale[n - 1];
        for h in stale {
            done.remove(&h);
        }
        tracing::warn!(
            "[IBD_WINDOW_REISSUE] span {}..{} re-issue {} absent height(s) {}..={}",
            from,
            until,
            n,
            first,
            last
        );
    }

    /// Failure / guard drop: release bookkeeping and keep this peer off these heights briefly.
    /// Returns true when the window path consumed the event (no retry-queue push).
    fn window_note_fail(&self, start: u64, end: u64, peer: Option<&str>) -> bool {
        if !Self::window_assign_enabled() || start < Self::window_from() {
            return false;
        }
        if let Some(p) = peer {
            self.window_started
                .lock()
                .unwrap()
                .remove(&(p.to_string(), start, end));
            self.window_fail
                .lock()
                .unwrap()
                .insert((start, end), (p.to_string(), Instant::now()));
        }
        true
    }

    /// Gauge line for the note: window fill and seat count.
    pub(crate) fn window_gauge(&self) -> Option<String> {
        let next = self.next_needed_height();
        if !self.window_mode_active(next) {
            return None;
        }
        let guard = self.in_flight_per_peer.lock().unwrap();
        let busy = guard.values().filter(|v| !v.is_empty()).count();
        let tiles: usize = guard.values().map(|v| v.len()).sum();
        let frontier = guard
            .values()
            .flat_map(|v| v.iter().map(|&(_, e)| e))
            .max()
            .unwrap_or(next);
        let done = self.window_done.lock().unwrap().len();
        let now = Instant::now();
        let benched = self
            .window_bench
            .lock()
            .unwrap()
            .values()
            .filter(|u| **u > now)
            .count();
        let slow = {
            let g = self.window_blk_ms_ema.load(Ordering::Relaxed);
            let pct = Self::window_slow_pct();
            self.window_peer_blk_ms
                .lock()
                .unwrap()
                .values()
                .filter(|&&(ms, n)| g > 0 && n >= 3 && ms * 100 > g * pct)
                .count()
        };
        Some(format!(
            "[IBD_WINDOW] next={} busy={} tiles={} frontier=+{} done_ahead={} benched={} ready={} tile={} fast_tile={}/{} blk_kb={} tile_ms={} stall_s={} dup_s={} blk_ms={} slow={}",
            next,
            busy,
            tiles,
            frontier.saturating_sub(next),
            done,
            benched,
            self.ibd_ready_peer_count(),
            self.window_tile_dyn(),
            // R-358: largest per-peer tile in force and how many peers are above the byte tile.
            {
                let base = self.window_tile_dyn();
                let target = Self::window_tile_ms();
                let max_tile = Self::window_tile();
                let per = self.window_peer_blk_ms.lock().unwrap();
                per.values()
                    .map(|&(ms, n)| Self::window_tile_for_peer_rate(base, ms, n, target, max_tile))
                    .max()
                    .unwrap_or(base)
            },
            {
                let base = self.window_tile_dyn();
                let target = Self::window_tile_ms();
                let max_tile = Self::window_tile();
                let per = self.window_peer_blk_ms.lock().unwrap();
                per.values()
                    .filter(|&&(ms, n)| {
                        Self::window_tile_for_peer_rate(base, ms, n, target, max_tile) > base
                    })
                    .count()
            },
            crate::node::parallel_ibd::download::download_est_block_bytes() / 1000,
            self.window_tile_ms_ema.load(Ordering::Relaxed),
            self.window_stall_dyn(),
            self.window_dup_dyn(self.window_stall_dyn()),
            self.window_blk_ms_ema.load(Ordering::Relaxed),
            slow
        ))
    }
}
