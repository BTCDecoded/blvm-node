/// R-28: one-socket line-rate at ≥190k is 60–94 BPS (`win_mbps≈90`).
/// Below this is mute/drip. Do not restore KEEP default 80.
const LINE_RATE_OWNER_BPS: f64 = 60.0;
/// R-58: reserved lookahead past the hero window. Not R-45 flood-skip. Not latch.
const LOOKAHEAD_OFFSET: u64 = 256;
const LOOKAHEAD_WIDTH: u64 = 256;
/// satd far pool is shuffled (still HOL on H).
/// Packed LOOKAHEAD width is **2048**, so the window must be **>width**.
/// **8192** = 4 tiles. Not 50k. Dump LOOKAHEAD stays sequential (`<180k`).
const SWARM_FAR_WINDOW: u64 = 8192;
/// satd near-cursor window (`connect_cursor+256`). LOOKAHEAD starts at +256;
/// this fills the hole with disjoint tiles, not N peers on one stripe (R-27).
const PRIORITY_ZONE: u64 = 256;
const PRIORITY_ZONE_TILE: u64 = 16;
const PRIORITY_ZONE_MAX: usize = 16;
const LOOKAHEAD_MAX: usize = 2;
const LOOKAHEAD_MIN_BPS: f64 = 80.0;
const LOOKAHEAD_FLOOD_HOLD_BPS: f64 = 2000.0;
const LOOKAHEAD_MIN_N: u64 = 2;
/// R-58 dest: n=2 / 1e-3s printed top_bps=2000 and stole a 1839-stream hero.
/// Plan already named 15s. Rank is that clock, not 2 packets.
const LOOKAHEAD_RANK_SECS: u64 = 15;
/// R-62: empty-band sample-then-hold. R-53 flood is ≥2000 or GetData EWMA ≤1.
/// R-63 IA-only hold FAIL (10–50k 724, SAMPLE@tip=1 n=2/bps=2). Reverted. Not R-45.
const EMPTY_BAND_SAMPLE_H: u64 = 50_000;
/// R-142: fat hero window — promote table-top probe once (R-141 await gate blocked trials).
const FAT_PROBE_RETITLE_LO: u64 = 180_000;
const FAT_PROBE_RETITLE_HI: u64 = 210_000;
/// R-161: CRAWL `win<16` desert + sticky_recv 2–14 while lifetime sticky_bps 100–160.
/// Lifetime ≥60 is not healthy when this window is mute. Unknown cache is not mute.
const STICKY_RECV_MUTE_MBPS: f64 = 16.0;
/// Farm contest: CRAWL recv that beats mute sticky. Not probe sojourn.
/// 40 required R-235's 183 Mbps LOOKAHEAD farm (only fired at 327k). Covering-H
/// `46.167` was 10.3 while sticky recv 0. Floor 8 is below that sample and
/// above a 0-stream (R-163). Height gate is FARM_RECV_LO (10k). dest-bc 298 lives.
const FARM_RECV_FAT_MBPS: f64 = 8.0;
/// R-237 dump 30–40k **478**: mute sticky recv 0, warehouse 4096, top 15–20 Mbps.
/// 180k left dump sitting. ≥10k is after dest-bc 0–10k (298 lives). Empty
/// band 901 still silent. Fat probe retitle stays 180k.
const FARM_RECV_LO: u64 = 10_000;
const FARM_RECV_TICK_SECS: u64 = 5;
const FARM_RECV_STREAK: u32 = 3;
/// Gap B: no new far tile at H≥floor while warehouse-full TIP_HOLE_AHEAD.
/// Floor **300k**. Not 50k (R-140 fat **137**). Not 248k
/// (R-136 **193** / R-147 **155**). Not any-ahead (R-153). Ahead ≥ WIDTH.
/// Ahead ≥ WIDTH (2048) stays silent (fat reo p50 **256**).
const GAP_B_LO: u64 = 300_000;
const GAP_B_LOG_SECS: u64 = 5;
const EMPTY_BAND_FLOOD_BPS: f64 = 2000.0;
const EMPTY_BAND_IA_HOLD_MS: u64 = 1;
const EMPTY_BAND_IA_SAMPLE_AFTER_MS: u64 = 2;
/// Before 50k: one disjoint 32-wide stripe (apply will consume). Not R-60 H+256 walk.
/// Not overlapping H sample (duplicate GetData dropped).
const EMPTY_BAND_AHEAD_WIDTH: u64 = 32;
const EMPTY_BAND_AHEAD_MAX: usize = 1;
/// Packed runway: exclusive far lanes at first_missing+LEAD (L1 leapfrog).
/// Not contig+1 64 (R-96 heels). Width is one inflight stripe; fetch walks 64s.
/// R-89: two farm slots; probe table picks who gets them.
///
/// Warehouse WIDTH is 2048 (R-105 10–50k **1538**, reorder p50 **4096**).
/// R-106 set WIDTH=512 globally and deleted that warehouse (10–50k **514**,
/// reorder p50 **513**). Empty WIDTH does not shrink. LEAD=512 shortens the
/// desert. Drop-on-enter puts the stripe in `have_hold`; pack treats
/// hold as occupied and jumps to `e+1` (not GetData inside it).
///
/// L4 fat WIDTH 128 (R-119) optimized `T_farm < T_hero` and deleted the
/// fat have-island (180–200k **51** vs R-118 **312** / R-120 **182**).
/// Fat win is apply eating landed have, not land-in-time of a 128.
/// WIDTH stays 2048 empty and fat. Do not retune to 256.
///
/// R-138: after 50k, desert is one hero batch (64), not 512. R-137 180–200k
/// **127** sat walking `hole+1…+512` with `fa=2`. Empty stays 512 so tip=1
/// `LOOKAHEAD` is still `513-2560`. Not L2c (that was WIDTH 512). Not L4.
pub(crate) const LEAPFROG_LEAD: u64 = 512;
pub(crate) const LEAPFROG_LEAD_AFTER_50K: u64 = 64;

pub(crate) fn leapfrog_lead_at(hole: u64) -> u64 {
    if hole >= EMPTY_BAND_SAMPLE_H {
        LEAPFROG_LEAD_AFTER_50K
    } else {
        LEAPFROG_LEAD
    }
}

pub(crate) const LEAPFROG_WIDTH: u64 = 2048;
/// R-282: far-window extent when `BLVM_IBD_RUNWAY_TILE` is set. Granularity
/// varies; this span stays 8192 (R-281's extent, run safely).
pub(crate) const RUNWAY_SPAN: u64 = 8192;

/// Unset = R-280/R-273 tile 2048. Set T (clamp floor..=2048) for finer tiles.
/// Floor default **128** (`BLVM_IBD_RUNWAY_TILE_MIN`, clamp 8..=128). Unset MIN
/// is byte-identical to the old `t.clamp(128, 2048)`.
pub(crate) fn runway_tile() -> Option<u64> {
    latch_env!(Option<u64>, {
        let floor = std::env::var("BLVM_IBD_RUNWAY_TILE_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128u64)
            .clamp(8, 128);
        std::env::var("BLVM_IBD_RUNWAY_TILE")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(|t: u64| t.clamp(floor, 2048))
    })
}

/// R-314/R-315: height below which `BLVM_IBD_RUNWAY_TILE` is ignored and the
/// farm runs the unset shape (WIDTH 2048 × RUNWAY_MAX 3 — R-115 dump 10–50k
/// **7122**). Stripe width scales inversely with block size: the dump wants
/// three fat tiles, the tip wants 32 thin ones. R-314 tile 16 from genesis:
/// 10–50k **284**. Env `BLVM_IBD_RUNWAY_TILE_FROM`, default **0** (= tile
/// applies everywhere; byte-identical to before this knob).
pub(crate) fn runway_tile_from() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_RUNWAY_TILE_FROM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    })
}

/// Tile in force at height `h`: `None` (unset shape) below
/// [`runway_tile_from`], else [`runway_tile`].
pub(crate) fn runway_tile_at(h: u64) -> Option<u64> {
    if h < runway_tile_from() {
        None
    } else {
        runway_tile()
    }
}

pub(crate) fn leapfrog_width_at(next_needed: u64) -> u64 {
    let w = runway_tile_at(next_needed).unwrap_or(LEAPFROG_WIDTH);
    #[cfg(not(test))]
    {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            tracing::info!(
                "[IBD_RUNWAY_TILE] tile={:?} tile_from={} leapfrog_width@{}={}",
                runway_tile(),
                runway_tile_from(),
                next_needed,
                w
            );
        });
    }
    w
}

/// R-228 dump 10–50k **1323**: farm armed **16897** while hole **12801**.
/// R-230 dump **1645**: at `IBD: 10000` feeder **689**, then 19ms later
/// `feeder=0 armed=14849`. Feeder **<64** was a starve *response* — farms
/// packed far while the warehouse was still full. Gate is height + slid,
/// not feeder. Genesis / dest-bc 0–10k: `next<10k`. r165 180k warehouse
/// still packs. Re-arm and intended LEAD stripe stay. Do **not** rotate H.
/// Gap B owns **300k**.
pub(crate) const STARVE_FAR_MIN_HEIGHT: u64 = 10_000;

pub(crate) fn starve_far_blocks_new_slid(
    next_needed: u64,
    _feeder: u64,
    hole: u64,
    part_start: u64,
    reorder_ahead: u64,
) -> bool {
    if !(STARVE_FAR_MIN_HEIGHT..GAP_B_LO).contains(&next_needed) {
        return false;
    }
    // FAT_HOLD class: warehouse-full at 180k must still pack (r165).
    if next_needed >= H_SLOW_MIN_HEIGHT && reorder_ahead >= LEAPFROG_WIDTH {
        return false;
    }
    // Unset: hole+LEAD+2048 (R-280). Set: hole+LEAD+RUNWAY_SPAN (tiles=peers).
    let span = if runway_tile_at(next_needed).is_some() {
        RUNWAY_SPAN
    } else {
        LEAPFROG_WIDTH
    };
    let slid_from = hole
        .saturating_add(leapfrog_lead_at(hole))
        .saturating_add(span);
    part_start >= slid_from
}
/// R-107 10–50k **1758** warehouse back; per-1k **233–24601**. Two tiles
/// finish and apply walks a hole. Third tile sits on that gap. WIDTH 2048.
pub(crate) const RUNWAY_MAX: usize = 3;

/// Unset = 3 (R-280). Set T: `(RUNWAY_SPAN / T).clamp(3, 32)` so tiles == peers.
pub(crate) fn runway_max() -> usize {
    match runway_tile() {
        None => RUNWAY_MAX,
        Some(t) => ((RUNWAY_SPAN / t.max(1)) as usize).clamp(3, 32),
    }
}

/// Farm slot cap in force at height `h` (see [`runway_tile_at`]).
pub(crate) fn runway_max_at(h: u64) -> usize {
    match runway_tile_at(h) {
        None => RUNWAY_MAX,
        Some(t) => ((RUNWAY_SPAN / t.max(1)) as usize).clamp(3, 32),
    }
}

struct ApplyWinSample {
    h: u64,
    at: Instant,
    bytes: u64,
    bytes_at: Instant,
}

fn apply_win_sample() -> &'static Mutex<ApplyWinSample> {
    static S: std::sync::OnceLock<Mutex<ApplyWinSample>> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let now = Instant::now();
        Mutex::new(ApplyWinSample {
            h: 0,
            at: now,
            bytes: 0,
            bytes_at: now,
        })
    })
}

static TEST_STALL_SEEDED: AtomicBool = AtomicBool::new(false);
static TEST_APPLY_BPS_BITS: AtomicU64 = AtomicU64::new(0);
static TEST_WIN_MBPS_BITS: AtomicU64 = AtomicU64::new(0);
static LAST_APPLY_BPS_BITS: AtomicU64 = AtomicU64::new(0);
static LAST_WIN_MBPS_BITS: AtomicU64 = AtomicU64::new(0);
static LAST_WIN_HAVE: AtomicBool = AtomicBool::new(false);
/// R-245: second pipe after the hero is line-rate (≥60). R-17 was grown=8 bps=0.
/// R-54 used 80 and never armed on the 180–200k band (sticky mbps 28).
const LATCH_AHEAD_STREAM_BPS: f64 = LINE_RATE_OWNER_BPS;
/// R-245: one extra, not R-28's 3. Cheese dests raced the same stripe.
const LATCHED_AHEAD_MAX: usize = 1;
/// R-28: MUTE_DROP same-height cool (ms). R-26 walked 31 peers in 5 ms.
const MUTE_DROP_SAME_H_MS: u64 = 8_000;

impl ChunkAssigner {
    /// R-255: satd far download ⊥ sequential connect. Default **off**.
    /// Env no-ops: `BLVM_IBD_SWARM=1` does not spread LOOKAHEAD.
    /// Does **not** skip `get_work`.
    /// Not N peers on one stripe (R-27). Not covering=0 OPEN.
    fn swarm_far_enabled() -> bool {
        // R-245 restore: R-245 binary has no IBD_SWARM_FAR. Env no-ops.
        false
    }

    /// R-313: disable September farm / lookahead. Default **off**.
    /// `BLVM_IBD_NO_FARM=1` serves only the sticky tip stripe.
    /// Unset is byte-identical to today's allow_ahead + LOOKAHEAD pack.
    pub(crate) fn no_farm_enabled() -> bool {
        let v = latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_NO_FARM")
                    .ok()
                    .as_deref()
                    .map(str::trim),
                Some("1") | Some("true") | Some("on") | Some("yes")
            )
        });
        #[cfg(not(test))]
        {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                tracing::info!("[IBD_NO_FARM] enabled={}", v);
            });
        }
        v
    }

    /// R-322: do not abort in-flight start>H GetData while tip is missing.
    /// Default **on**. `BLVM_IBD_NO_TIP_ABORT=0` restores the three C1J aborts.
    /// Holds (LOOKAHEAD/LATCH/PRIORITY/C1J_KEEP) and the W49 tip-past-end
    /// abort are unchanged.
    pub(crate) fn no_tip_abort_enabled() -> bool {
        let v = latch_env!(bool, {
            !matches!(
                std::env::var("BLVM_IBD_NO_TIP_ABORT")
                    .ok()
                    .as_deref()
                    .map(str::trim),
                Some("0") | Some("false") | Some("off") | Some("no")
            )
        });
        #[cfg(not(test))]
        {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| {
                tracing::info!("[IBD_NO_TIP_ABORT] enabled={}", v);
            });
        }
        v
    }

    fn c1j_noabort_log(peer_id: &str, start: u64, end: u64, tip: u64, which: u8) {
        static LAST_S: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let prev = LAST_S.load(Ordering::Relaxed);
        if now.saturating_sub(prev) >= 5
            && LAST_S
                .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                "[IBD_C1J_NOABORT] peer={} span={}-{} tip={} which={}",
                peer_id,
                start,
                end,
                tip,
                which
            );
        }
    }

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

    /// PIPE_FILL `received=0` streak that may arm C1t without gd-fast.
    ///
    /// Deep mute cover keeps `effective_healthy≥1`, so mute_reopen never opens `(H,H)`.
    /// Grow-on-delivery stays at START=8 while `received=0`, so the C1n gd-fast gate
    /// never elevates. Leftover soak 2026-08-22: C1T_COLD_CLOCK=0, grown=8, covering=1.
    fn c1t_recv0_mute_race() -> bool {
        super::tip_stage::pipe_fill_recv0_streak() >= 2
            || super::IBD_EMPTY_TIP.load(Ordering::Relaxed)
    }

    /// Preferred ≥80 exists while cheese is showing (H missing, ahead sitting).
    /// r 37k: lottery FORCE then C1t piled covering=3 while the hero was already
    /// sticky — mute racers do not fetch H, they steal the pipe.
    fn cheese_hero_blocks_c1t_race(&self) -> bool {
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let ahead = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed);
        let holes = super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        if ahead < 8 && holes < 5 {
            return false;
        }
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        self.tip_sticky_usable(&pref)
            && Self::line_rate_or_keep(self.wan_tip_stream_bps(&pref))
    }

    /// CHEESE / TIP_HOLE_AHEAD hide `IBD_EMPTY_TIP` (`tip_runway_mode` returns CHEESE
    /// first when `holes≥5`). Live genesis-b 91698: holes=22 covering=2 feeder=0 —
    /// C1t never raced and CRAWL died after the ahead dump.
    ///
    /// Only covering≥2: covering=1 + holes≥5 is healthy 150ms RTT cheese, not a freeze.
    fn c1t_cheese_hidden_tip_hole(&self) -> bool {
        if self.cheese_hero_blocks_c1t_race() {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        if super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) != 0 {
            return false;
        }
        let holes = super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        // CHEESE only (`holes≥5`). `ahead>0` with holes<5 is healthy TIP_HOLE_AHEAD;
        // genesis-c 50–150k: 131/369 C1T_CHEESE had holes<5 during 1762 window p50.
        if holes < 5 {
            return false;
        }
        self.healthy_tip_cover_count(self.next_needed_height()) >= 2
    }

    /// C1t: open one tip-height racer (no past-tip ahead). W88 episode still applies.
    fn c1t_tip_height_race(&self) -> bool {
        if self.cheese_hero_blocks_c1t_race() {
            return false;
        }
        let ms = Self::c1t_tip_race_ms();
        if ms == 0 {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        let gd_fast =
            super::download::tip_hole_grow_cap_effective() > super::download::tip_hole_grow_cap();
        let recv0_mute = Self::c1t_recv0_mute_race();
        let cheese_hidden = self.c1t_cheese_hidden_tip_hole();
        // Mute peerday guard — same gate as C1n grow-fast (slow EWMA → no race),
        // unless PIPE_FILL proves the deep owner is recv=0 (then we must race).
        if !gd_fast && !recv0_mute && !cheese_hidden {
            return false;
        }
        let awaiting = super::tip_stage::tip_awaiting_ms_for_cap();
        if (gd_fast || recv0_mute) && awaiting >= ms {
            return true;
        }
        if cheese_hidden {
            static LAST_CHEESE: Mutex<Option<Instant>> = Mutex::new(None);
            let mut g = LAST_CHEESE.lock().unwrap();
            if let Some(t) = *g {
                if t.elapsed() < Duration::from_millis(80) {
                    return false;
                }
            }
            *g = Some(Instant::now());
            tracing::warn!(
                "[IBD_C1T_CHEESE] tip={} covering={} holes={} ahead={} feeder=0 — tip-height race",
                self.next_needed_height(),
                self.healthy_tip_cover_count(self.next_needed_height()),
                super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed),
                super::IBD_REORDER_AHEAD.load(Ordering::Relaxed)
            );
            return true;
        }
        // Phase 2 mid-gap: covering≥1 + feeder=0 but await clock stuck at 0 (stamp reset
        // while tip never lands). Live EMPTY_TIP SLOW_STRETCH had covering=1 await_ms=0
        // gd_ewma~180 — classic C1t never armed. Debounce ~80ms to avoid W172 storms.
        //
        // Genesis TRUE WAN 2026-08-22 @187–191k: 90 BPS drip resets await to 1–119ms
        // (dead zone: not 0, not ≥120) and feeder=2–3 of *non-tip* bodies. EMPTY_TIP
        // + covering=1 + ready=66 must still race — those feeders are not the tip.
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed);
        if !recv0_mute && (feeder > 0 || awaiting > 0) {
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
            "[IBD_C1T_COLD_CLOCK] tip={} covering={} await_ms=0 feeder=0 recv0={} — tip-height race",
            tip,
            covering,
            super::tip_stage::pipe_fill_recv0_streak()
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
        if self.cheese_hero_blocks_c1t_race() {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed)
            && !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed)
        {
            return false;
        }
        // Genesis-b 91698: CHEESE covering=2 + await clock stuck at 0 (ahead PIPE
        // resets it). C1t alone only raises fetchers_cap to 2 — need 3 to assign.
        if self.c1t_cheese_hidden_tip_hole() {
            return true;
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

    /// R-16: mute sit (bps≈0 / no body for mute_reopen secs) → drop cover so
    /// another ready peer takes exclusive H. Existing `note_tip_owner_failed_mute`
    /// / force_release. Not a second GetData on H. No `BLVM_IBD_MUTE_*`.
    ///
    /// R-22: drop only when this `get_work` caller is a ready replacement who can
    /// take exclusive H now. R-18 left covering=2 EMPTY_TIP; R-21 left covering=1
    /// with no live GetData (force_release re-locked `in_flight`). Never covering=0
    /// with no owner. Never covering=2 on H. 120s mute cooldown stays.
    fn maybe_drop_mute_tip_cover(
        &self,
        raw_covering: usize,
        caller: &str,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
    ) -> Option<(u64, u64)> {
        if !self.mute_single_cover_reopen(raw_covering) {
            return None;
        }
        let pref = self.preferred_tip_owner()?;
        let bps = self.wan_tip_stream_bps(&pref);
        let recv0 = super::tip_stage::pipe_fill_recv0_streak() > 0;
        // R-30 dest @224k: MUTE_DROP evicted a 20.6 BPS replacement because
        // recv0 was a GetData gap. Mute is bps≈0. recv0 alone is not mute.
        if bps >= 1.0 {
            return None;
        }
        let repl = self.any_ready_active_worker_except(&pref)?;
        // Caller is the mute (or a third peer) → wait. Do not drop into covering=0
        // or insert a zombie (H,H) the replacement never GetData'd.
        if caller != repl.as_str() {
            return None;
        }
        let h = self.next_needed_height();
        let now_ms = Self::unix_now_ms();
        let last_h = self.last_mute_drop_h.load(Ordering::Relaxed);
        let last_at = self.last_mute_drop_at_ms.load(Ordering::Relaxed);
        if last_h == h && now_ms.saturating_sub(last_at) < MUTE_DROP_SAME_H_MS {
            return None;
        }
        tracing::warn!(
            "[IBD_MUTE_DROP] sticky={} tip={} bps={:.1} recv0={} covering={} — drop cover, exclusive H peer={} (not dual GetData)",
            pref,
            h,
            bps,
            recv0,
            raw_covering,
            repl
        );
        self.last_mute_drop_h.store(h, Ordering::Relaxed);
        self.last_mute_drop_at_ms.store(now_ms, Ordering::Relaxed);
        guard.remove(&pref);
        self.note_tip_owner_failed_mute(&pref);
        Self::insert_in_flight(guard, &repl, h, h);
        self.note_tip_cover_claim(&repl, h, h);
        self.note_tip_owner_assigned(&repl);
        Some((h, h))
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
        // Called under `get_work`'s `in_flight` guard — hole-band must not re-lock it.
        self.tip_ahead_hole_band_update(feeder_len);
        if self.tip_ahead_hole_freeze.load(Ordering::Relaxed) {
            return false;
        }
        // dest-ar @294k: covering=1 mute still farmed flight_ahead=7–8.
        // C1g is tip-missing, not tip-mute. Ignition (no STREAM yet) must not
        // look like mute — W47 ahead-after-tip-lands has bps=0 / grown=8.
        if !self.wan_hero_hot_for_ahead() {
            return false;
        }
        true
    }

    /// WAN ahead only while sticky is KEEP and grown≥32. No samples yet = ignition.
    fn wan_hero_hot_for_ahead(&self) -> bool {
        if !self.wan_tip_gap_crawl(self.next_needed_height()) {
            return true;
        }
        // R-336 opt-in: `BLVM_IBD_WAN_AHEAD_FORCE_HOT=1` treats the hero as hot so
        // the multi-peer ahead stripe branch can open in the crawl. With KEEP=0
        // (`preferred_meets_keep_bps` always false) this fn is false whenever the
        // sticky owner has streamed, so `[IBD_PIPE_F] reason=stripe` is 0 in every
        // crawl R-273–R-335 and `TIP_AHEAD_PEERS` is never evaluated. Default off.
        if latch_env!(bool, {
            matches!(
                std::env::var("BLVM_IBD_WAN_AHEAD_FORCE_HOT").ok().as_deref(),
                Some("1") | Some("true") | Some("on") | Some("yes")
            )
        }) {
            return true;
        }
        let Some(pref) = self.preferred_tip_owner() else {
            return true;
        };
        if self.tip_stream_count(&pref) == 0 && self.wan_tip_stream_bps(&pref) <= 0.0 {
            return true;
        }
        if !self.preferred_meets_keep_bps() {
            return false;
        }
        // Tests latch TIP_HOLE_STICKY=false (historical unset). Production KEEP is on;
        // dest-ar CRAWL grown= is tip_hole_depth_for.
        if Self::tip_hole_sticky_enabled() && self.tip_hole_depth_for(&pref) < 32 {
            return false;
        }
        true
    }

    /// Non-owner C1g runway stripe: exclude sticky; after first probe OK, n≥1 only.
    fn wan_peer_may_take_ahead_stripe(&self, peer_id: &str) -> bool {
        if self.preferred_tip_owner().as_deref() == Some(peer_id) {
            return false;
        }
        if !super::tip_probe::enabled() || super::tip_probe::probe_ok_count() == 0 {
            return true;
        }
        super::tip_probe::peer_probed(peer_id)
    }

    /// CRAWL `grown=` map. `tip_hole_depth_for` returns grow_start when
    /// TIP_HOLE_STICKY is off (tests + KEEP=0 ship). Latch must read the map.
    fn mapped_tip_hole_depth(&self, peer_id: &str) -> usize {
        self.tip_hole_depth
            .lock()
            .unwrap()
            .get(peer_id)
            .copied()
            .unwrap_or(0)
    }

    fn ahead_inflight_count(
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) -> usize {
        in_flight
            .values()
            .flatten()
            .filter(|(s, _)| *s > next_needed)
            .count()
    }

    fn prune_latched_ahead(&self, next_needed: u64) {
        self.latched_ahead
            .lock()
            .unwrap()
            .retain(|(_, _, e)| *e >= next_needed);
    }

    fn latched_ahead_holds(&self, peer_id: &str, start: u64, end: u64) -> bool {
        self.latched_ahead
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s == start && *e == end)
    }

    fn latched_ahead_overlaps(&self, start: u64, end: u64) -> bool {
        self.latched_ahead
            .lock()
            .unwrap()
            .iter()
            .any(|(_, s, e)| *s <= end && start <= *e)
    }

    fn prune_priority_zone(&self, next_needed: u64) {
        self.priority_zone
            .lock()
            .unwrap()
            .retain(|(_, _, e)| *e >= next_needed);
    }

    fn priority_zone_holds(&self, peer_id: &str, start: u64, end: u64) -> bool {
        self.priority_zone
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s == start && *e == end)
    }

    fn priority_zone_overlaps(&self, start: u64, end: u64) -> bool {
        self.priority_zone
            .lock()
            .unwrap()
            .iter()
            .any(|(_, s, e)| *s <= end && start <= *e)
    }

    /// satd priority zone: disjoint tiles in (H, H+256]. Covering=0 must not
    /// take H. Preferred never takes start>H here. Not R-27 same-stripe.
    /// Not 50k shuffle. C1j holds via `priority_zone_holds`.
    ///
    /// R-249: do **not** start at `tip_contiguous_assign_frontier` (R-248
    /// jumped to owner_end+1 = 33 while 2–32 sat on one TCP → CHEESE @212
    /// holes=45 first_ahead=241, 0–10k 108). Scan from H+1. Duplicate the
    /// preferred owner's covering for start>H; still exclusive on H.
    fn try_assign_priority_zone(
        &self,
        peer_id: &str,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        raw_covering: usize,
    ) -> Option<(u64, u64)> {
        if raw_covering == 0 {
            return None;
        }
        let pref = self.preferred_tip_owner()?;
        if pref == peer_id {
            return None;
        }
        self.prune_priority_zone(next_needed);
        if let Some((rs, re)) = self
            .priority_zone
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| p == peer_id)
            .map(|(_, s, e)| (*s, *e))
        {
            if !guard
                .get(peer_id)
                .is_some_and(|r| r.iter().any(|&(s, e)| s == rs && e == re))
            {
                if !self.try_insert_dynamic(guard, peer_id, rs, re) {
                    return None;
                }
                static LAST_PZ_REARM_MS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let prev = LAST_PZ_REARM_MS.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 500
                    && LAST_PZ_REARM_MS
                        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    tracing::warn!(
                        "[IBD_PRIORITY_ZONE] peer={} {}-{} tip={} — re-arm near-cursor tile",
                        peer_id,
                        rs,
                        re,
                        next_needed
                    );
                }
                return Some((rs, re));
            }
            return None;
        }
        if self.priority_zone.lock().unwrap().len() >= PRIORITY_ZONE_MAX {
            return None;
        }
        if guard
            .get(peer_id)
            .is_some_and(|r| r.iter().any(|&(s, _)| s > next_needed))
        {
            return None;
        }
        // Same door as LOOKAHEAD: a new near-cursor tile is a new far tile.
        // Re-arm above still returns the reserved tile. R-165 warehouse at ≥300k.
        if self.gap_b_blocks_new_far_tile(guard, &pref, next_needed) {
            self.note_gap_b(next_needed);
            return None;
        }
        let zone_end = next_needed.saturating_add(PRIORITY_ZONE);
        let mut h = next_needed.saturating_add(1);
        while h <= zone_end {
            if h <= next_needed {
                return None;
            }
            let tile_end_raw = h
                .saturating_add(PRIORITY_ZONE_TILE.saturating_sub(1))
                .min(zone_end);
            let Some((_, tile_end)) = self.clip_end_to_headers(h, tile_end_raw) else {
                break;
            };
            if tile_end < h {
                break;
            }
            let blocked =
                Self::range_overlaps_inflight_except_h_coverers(guard, next_needed, h, tile_end)
                    || self.priority_zone_overlaps(h, tile_end)
                    || self.latched_ahead_overlaps(h, tile_end);
            if !blocked {
                if !self.try_insert_dynamic(guard, peer_id, h, tile_end) {
                    return None;
                }
                self.priority_zone
                    .lock()
                    .unwrap()
                    .push((peer_id.to_string(), h, tile_end));
                tracing::warn!(
                    "[IBD_PRIORITY_ZONE] peer={} {}-{} tip={} owner={} — duplicate H+1 over H-coverers",
                    peer_id,
                    h,
                    tile_end,
                    next_needed,
                    pref
                );
                return Some((h, tile_end));
            }
            h = tile_end.saturating_add(1);
        }
        None
    }

    /// R-245: one other peer, one extra stripe, insert under the get_work lock.
    /// Dump-height extras cheese'd (R-27 @4586 / R-28 @2366 / R-33 @1978 / R-34 349).
    /// R-54 hooked with no height gate and armed only @398k (`flight_ahead=0` on fat).
    /// Preferred never takes start>H here. Ignition (grown<32 / bps<60) stays frozen.
    /// Stripe ≤ WAN admit (default 64) so GAP_ADMIT cannot drop the second pipe.
    /// R-252: start at H+1 (duplicate H-coverers), not owner_end+1. Live R-251
    /// latched 190776-190967 at tip=188732; apply walked into an empty stripe
    /// (17.5s / 57 BPS 190–191k, TIP_HOLE_AHEAD first_ahead=190968). Same jump
    /// R-248/R-249 convicted on the priority zone. Wall A: never a second TCP on H.
    fn try_assign_latched_ahead(
        &self,
        peer_id: &str,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        raw_covering: usize,
    ) -> Option<(u64, u64)> {
        if raw_covering == 0 {
            return None;
        }
        // R-34 dump 349 / R-27–R-33 cheese were below fat. R-54's 3 extras at
        // 398k never moved 180–200k. One extra only after FAT_PROBE_RETITLE_LO.
        if next_needed < FAT_PROBE_RETITLE_LO {
            return None;
        }
        // R-33: extras at first_ahead=2042 while H=1978 empty. Require a live
        // GetData that contains H, not the CRAWL covering=1 walk-in lie.
        // R-34 AND'd IBD_TIP_IN_REORDER and got flight_ahead=0: body landing
        // clears inflight, so AND never holds. Cover of H is the door.
        // R-245 kept c1i contig≥8 here; live C1G_FREEZE @180102 covering=1
        // delayed first [IBD_C1G_LATCH_AHEAD] to tip=200000 — missed fat.
        // Dump cheese stays behind FAT_PROBE_RETITLE_LO. Contig freeze stays
        // on LOOKAHEAD/MQ, not on this one extra.
        let pref = self.preferred_tip_owner()?;
        if pref == peer_id {
            return None;
        }
        if self.mapped_tip_hole_depth(&pref) < 32 {
            return None;
        }
        if self.wan_tip_stream_bps(&pref) < LATCH_AHEAD_STREAM_BPS {
            return None;
        }
        if !guard
            .values()
            .flatten()
            .any(|&(s, e)| s <= next_needed && next_needed <= e)
        {
            return None;
        }
        self.prune_latched_ahead(next_needed);
        // Same peer retry after C1j abort released inflight. Do not hand the
        // reserved stripe to anyone else (R-28 2364–2427 ×2 in 5 ms).
        if let Some((rs, re)) = self
            .latched_ahead
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| p == peer_id)
            .map(|(_, s, e)| (*s, *e))
        {
            if !guard
                .get(peer_id)
                .is_some_and(|r| r.iter().any(|&(s, e)| s == rs && e == re))
            {
                if !self.try_insert_dynamic(guard, peer_id, rs, re) {
                    return None;
                }
                tracing::warn!(
                    "[IBD_C1G_LATCH_RETRY] peer={} {}-{} tip={} — re-arm reserved extra after abort",
                    peer_id,
                    rs,
                    re,
                    next_needed
                );
                return Some((rs, re));
            }
            return None;
        }
        if self.latched_ahead.lock().unwrap().len() >= LATCHED_AHEAD_MAX {
            return None;
        }
        if guard
            .get(peer_id)
            .is_some_and(|r| r.iter().any(|&(s, _)| s > next_needed))
        {
            return None;
        }
        // New latch is a new far tile. Reserved retry above still re-arms.
        if self.gap_b_blocks_new_far_tile(guard, &pref, next_needed) {
            self.note_gap_b(next_needed);
            return None;
        }
        // Log-only: live R-251 owner_end was 2k past apply. Do not assign there.
        let runway = next_needed.saturating_add(2048);
        let frontier = Self::tip_contiguous_assign_frontier(guard, next_needed, runway);
        let part_start = next_needed.saturating_add(1);
        if part_start <= next_needed {
            return None;
        }
        let stripe = super::wan_gap_admit_window().max(32);
        let part_end_raw = part_start.saturating_add(stripe.saturating_sub(1));
        let (_, part_end) = self.clip_end_to_headers(part_start, part_end_raw)?;
        if part_end < part_start {
            return None;
        }
        if Self::range_overlaps_inflight_except_h_coverers(guard, next_needed, part_start, part_end)
            || self.latched_ahead_overlaps(part_start, part_end)
            || self.priority_zone_overlaps(part_start, part_end)
        {
            return None;
        }
        if !self.try_insert_dynamic(guard, peer_id, part_start, part_end) {
            return None;
        }
        self.latched_ahead
            .lock()
            .unwrap()
            .push((peer_id.to_string(), part_start, part_end));
        tracing::warn!(
            "[IBD_C1G_LATCH_AHEAD] peer={} {}-{} tip={} owner={} owner_end={} slots={}/{} — duplicate H+1 over H-coverers, not owner_end+1",
            peer_id,
            part_start,
            part_end,
            next_needed,
            pref,
            frontier,
            self.latched_ahead.lock().unwrap().len(),
            LATCHED_AHEAD_MAX
        );
        Some((part_start, part_end))
    }

    /// Have-enter drops + hold only when the warehouse is **landed**
    /// (`reorder >= WIDTH`, r98). Thin have (`reorder=1`) keeps the stripe —
    /// R-135 200–248k dropped an unlanded 2048 at tip=233541 / 234385 /
    /// 235516 (`reorder=1`, `flight_ahead=0`) and sat **730s**. Empty enter
    /// still keeps (R-115). Do not rewrite `s` to `H+1` (R-113). Fully
    /// behind (`e <= H`) still drops. 15s farm clock stays (R-92).
    fn drop_stale_lookahead_stripes(
        &self,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
    ) {
        let have = Self::h_body_present();
        let landed = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) as u64 >= LEAPFROG_WIDTH;
        let mut gone: Vec<(String, u64, u64)> = Vec::new();
        {
            let mut stripes = self.lookahead_stripes.lock().unwrap();
            stripes.retain(|(p, s, e)| {
                if *e <= next_needed || (*s <= next_needed && have && landed) {
                    gone.push((p.clone(), *s, *e));
                    false
                } else {
                    true
                }
            });
        }
        if gone.is_empty() {
            let mut hold = self.lookahead_have_hold.lock().unwrap();
            let n0 = hold.len();
            hold.retain(|&(_, e)| e > next_needed);
            if hold.len() != n0 {
                drop(hold);
                self.publish_lookahead_reserved_from_stripes();
            }
            return;
        }
        for (p, s, e) in gone {
            if let Some(ranges) = guard.get_mut(&p) {
                ranges.retain(|&(rs, re)| rs != s || re != e);
                if ranges.is_empty() {
                    guard.remove(&p);
                }
            }
            // R-92: keep the 15s / n≥8 farm clock across a finished 64.
            // Wipe-on-complete made OUTRANK=0 on R-91. Do not rematch R-58.
            if s <= next_needed && e > next_needed {
                self.lookahead_have_hold.lock().unwrap().push((s, e));
                tracing::warn!(
                    "[IBD_LOOKAHEAD] peer={} {}-{} tip={} — drop on enter; leapfrog",
                    p,
                    s,
                    e,
                    next_needed
                );
            } else {
                tracing::warn!(
                    "[IBD_LOOKAHEAD] peer={} {}-{} tip={} — drop stale (e<=H); arm live stripe",
                    p,
                    s,
                    e,
                    next_needed
                );
            }
        }
        self.lookahead_have_hold
            .lock()
            .unwrap()
            .retain(|&(_, e)| e > next_needed);
        self.publish_lookahead_reserved_from_stripes();
    }

    fn publish_lookahead_reserved_from_stripes(&self) {
        let mut ranges: Vec<(u64, u64)> = self
            .lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s, e)| (*s, *e))
            .collect();
        ranges.extend(
            self.lookahead_have_hold
                .lock()
                .unwrap()
                .iter()
                .copied(),
        );
        super::publish_lookahead_reserved(ranges);
    }

    /// One packed runway stripe at owner_end+1 after the hero covers the hole.
    /// Have (`IN_REORDER`) still packs (fat / apply lag). Hero inflight on
    /// first_missing also packs (R-91 empty seat). Covering≥1 without the hero
    /// on the hole is not pack — R-84 @186k packed past an in-flight 64,
    /// `bridge_min=H+19`, holes=50, `KILL_HOLES`.
    /// R-69 H+256 leftover on the sticky is closed. Hero never. Latch is live (R-245).
    fn try_assign_lookahead_stripe(
        &self,
        peer_id: &str,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        next_needed: u64,
        raw_covering: usize,
    ) -> Option<(u64, u64)> {
        if Self::no_farm_enabled() {
            return None;
        }
        self.drop_stale_lookahead_stripes(guard, next_needed);
        let pref = self.preferred_tip_owner()?;
        let packed = self.may_pack_runway(guard, &pref);
        let empty_ahead = false;
        if !packed {
            let _ = raw_covering;
            return None;
        }
        if pref == peer_id {
            self.vacate_sticky_farm_for_h(guard, peer_id, next_needed);
            return None;
        }
        // R-139: 3 slid tiles at +4096 fill RUNWAY_MAX so intended
        // hole+LEAD never arms (R-133/R-138 lead==512/64 ≈ 0). Far hold
        // keeps reserved-admit (island). Not leftover-have. Not uncover.
        // R-154/R-155 180k hold REVERTED (fat 167/166). Gap B is 300k +
        // warehouse-full only — not R-153 any-ahead.
        if packed {
            let hole = self.first_missing_height();
            // R-145: release only when every runway farm is slid past
            // hole+LEAD+WIDTH (+4096 chain). R-144 dropped lead_window_free
            // and released on LEAD_PACK (513/2561/4609) — 20+ churn hits
            // by 80k, 10–50k ts ~691 vs R-143 3266. Still no lead_window_free
            // so hold/inflight-stuck LEAD can release once all farms slid.
            // Gap B: do not slide a new far tile into a full warehouse.
            if !self.gap_b_blocks_new_far_tile(guard, &pref, next_needed)
                && self.packed_stripe_owners() >= runway_max_at(next_needed)
                && self.all_runway_farms_slid(hole)
            {
                self.release_farthest_slid_farm(guard, hole);
            }
        }
        // Re-arm this peer's reserved stripe after C1j / complete (one owner).
        // L2: s<=H already dropped the whole stripe (no trim-hold remainder).
        if let Some((rs, re)) = self
            .lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _, _)| p == peer_id)
            .map(|(_, s, e)| (*s, *e))
        {
            if !guard
                .get(peer_id)
                .is_some_and(|r| r.iter().any(|&(s, e)| s == rs && e == re))
            {
                if !self.try_insert_dynamic(guard, peer_id, rs, re) {
                    return None;
                }
                tracing::warn!(
                    "[IBD_LOOKAHEAD] peer={} {}-{} tip={} — re-arm reserved stripe",
                    peer_id,
                    rs,
                    re,
                    next_needed
                );
                return Some((rs, re));
            }
            return None;
        }
        if self.gap_b_blocks_new_far_tile(guard, &pref, next_needed) {
            self.note_gap_b(next_needed);
            return None;
        }
        if super::tip_probe::enabled() && super::tip_probe::probe_ok_count() > 0 {
            let top: Vec<String> = super::tip_probe::ranked_probes(Some(&pref))
                .into_iter()
                .take(runway_max_at(next_needed))
                .map(|(p, _)| p)
                .collect();
            if !top.iter().any(|p| p == peer_id) {
                return None;
            }
        }
        let owners = {
            let g = self.lookahead_stripes.lock().unwrap();
            g.iter()
                .map(|(p, _, _)| p.clone())
                .collect::<std::collections::HashSet<_>>()
                .len()
        };
        let max_slots = if empty_ahead {
            EMPTY_BAND_AHEAD_MAX
        } else if packed {
            runway_max_at(next_needed)
        } else {
            LOOKAHEAD_MAX
        };
        if owners >= max_slots {
            return None;
        }
        if guard
            .get(peer_id)
            .is_some_and(|r| r.iter().any(|&(s, _)| s > next_needed))
        {
            return None;
        }
        let width = if packed {
            leapfrog_width_at(next_needed)
        } else if empty_ahead {
            EMPTY_BAND_AHEAD_WIDTH
        } else {
            LOOKAHEAD_WIDTH
        };
        let intended = if packed {
            let hole = self.first_missing_height();
            hole.saturating_add(leapfrog_lead_at(hole))
        } else {
            0
        };
        let mut part_start = if packed {
            intended
        } else if empty_ahead {
            next_needed.saturating_add(EMPTY_BAND_AHEAD_WIDTH)
        } else {
            next_needed.saturating_add(LOOKAHEAD_OFFSET)
        };
        // Hero owns first_hole. Pack waits until hero inflight covers it —
        // do not skip to hole+64 (warehouse) and do not cheese the hole.
        if packed {
            let hole = self.first_missing_height();
            let hero_covers = guard.get(&pref).is_some_and(|r| {
                r.iter().any(|&(s, e)| s <= hole && hole <= e)
            });
            if part_start == hole && !hero_covers {
                return None;
            }
        }
        if packed
            && next_needed >= FAT_PROBE_RETITLE_LO
            && Self::swarm_far_enabled()
            && width > 0
        {
            let slots = (SWARM_FAR_WINDOW / width).max(1);
            let mut h = next_needed;
            for b in peer_id.as_bytes() {
                h = h.wrapping_mul(31).wrapping_add(*b as u64);
            }
            let spread = intended.saturating_add((h % slots).saturating_mul(width));
            if spread > next_needed {
                part_start = spread;
            }
        }
        let mut part_end = 0u64;
        let mut found = false;
        let mut ignored_behind = 0u32;
        for _ in 0..max_slots {
            if part_start <= next_needed {
                break;
            }
            let part_end_raw = part_start.saturating_add(width.saturating_sub(1));
            let Some((_, e)) = self.clip_end_to_headers(part_start, part_end_raw) else {
                break;
            };
            if e < part_start {
                break;
            }
            if packed {
                let (occ, ign) = self.packed_overlap_end(guard, part_start, e);
                ignored_behind = ignored_behind.saturating_add(ign);
                if let Some(occ) = occ {
                    let next = occ.saturating_add(1);
                    if next <= part_start {
                        break;
                    }
                    part_start = next;
                    continue;
                }
            } else if Self::range_overlaps_inflight(guard, part_start, e)
                || self.lookahead_overlaps(part_start, e)
            {
                part_start = part_start.saturating_add(width);
                continue;
            }
            part_end = e;
            found = true;
            break;
        }
        if !found {
            return None;
        }
        if packed {
            let hole = self.first_missing_height();
            let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) as u64;
            let reorder = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) as u64;
            if starve_far_blocks_new_slid(next_needed, feeder, hole, part_start, reorder) {
                tracing::warn!(
                    "[IBD_STARVE_FAR] tip={} hole={} feeder={} reorder={} armed={} — no new slid tile",
                    next_needed,
                    hole,
                    feeder,
                    reorder,
                    part_start
                );
                return None;
            }
        }
        if !self.try_insert_dynamic(guard, peer_id, part_start, part_end) {
            return None;
        }
        self.lookahead_stripes
            .lock()
            .unwrap()
            .push((peer_id.to_string(), part_start, part_end));
        self.publish_lookahead_reserved_from_stripes();
        tracing::warn!(
            "[IBD_LOOKAHEAD] peer={} {}-{} tip={} owner={} slots={}/{} — reserved disjoint GetData",
            peer_id,
            part_start,
            part_end,
            next_needed,
            pref,
            self.lookahead_stripes.lock().unwrap().len(),
            max_slots
        );
        if packed && (ignored_behind > 0 || part_start != intended) {
            if Self::swarm_far_enabled() && next_needed >= FAT_PROBE_RETITLE_LO {
                tracing::warn!(
                    "[IBD_SWARM_FAR] intended={} armed={} {}-{} tip={} — shuffled far tile, not skip-H",
                    intended,
                    part_start,
                    part_start,
                    part_end,
                    next_needed
                );
            }
            tracing::warn!(
                "[IBD_LEAD_PACK] intended={} armed={} {}-{} tip={} ignored_behind={}",
                intended,
                part_start,
                part_start,
                part_end,
                next_needed,
                ignored_behind
            );
        }
        Some((part_start, part_end))
    }

    /// First farm slot `[hole+LEAD, hole+LEAD+WIDTH)`. Under-apply
    /// (`s<=hole`) does not occupy it — R-115/R-136 keep those.
    fn lead_window_free(&self, hole: u64) -> bool {
        let ws = hole.saturating_add(leapfrog_lead_at(hole));
        let we = ws.saturating_add(leapfrog_width_at(hole).saturating_sub(1));
        let occupies = |s: u64, e: u64| s > hole && s <= we && ws <= e;
        if self
            .lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .any(|(_, s, e)| occupies(*s, *e))
        {
            return false;
        }
        !self
            .lookahead_have_hold
            .lock()
            .unwrap()
            .iter()
            .any(|&(s, e)| occupies(s, e))
    }

    fn packed_stripe_owners(&self) -> usize {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _, _)| p.clone())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    /// True when every lookahead stripe starts at or after the +4096 slide
    /// floor (R-139). LEAD_PACK arms 513/2561/4609 — not slid, no release.
    fn all_runway_farms_slid(&self, hole: u64) -> bool {
        if self.packed_stripe_owners() < runway_max_at(hole) {
            return false;
        }
        let slid_from = hole
            .saturating_add(leapfrog_lead_at(hole))
            .saturating_add(leapfrog_width_at(hole));
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .all(|(_, s, _)| *s >= slid_from)
    }

    /// Drop the farthest farm with `s >= hole+LEAD+WIDTH` into have_hold.
    /// Admit still sees the island. Next pack arms intended LEAD.
    fn release_farthest_slid_farm(
        &self,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        hole: u64,
    ) -> bool {
        let slid_from = hole
            .saturating_add(leapfrog_lead_at(hole))
            .saturating_add(leapfrog_width_at(hole));
        let mut pick: Option<(String, u64, u64)> = None;
        {
            let stripes = self.lookahead_stripes.lock().unwrap();
            for (p, s, e) in stripes.iter() {
                if *s >= slid_from && pick.as_ref().is_none_or(|(_, ps, _)| *s > *ps) {
                    pick = Some((p.clone(), *s, *e));
                }
            }
        }
        let Some((p, s, e)) = pick else {
            return false;
        };
        self.lookahead_stripes
            .lock()
            .unwrap()
            .retain(|(pp, ss, ee)| !(pp == &p && *ss == s && *ee == e));
        if let Some(ranges) = guard.get_mut(&p) {
            ranges.retain(|&(rs, re)| rs != s || re != e);
            if ranges.is_empty() {
                guard.remove(&p);
            }
        }
        self.lookahead_have_hold.lock().unwrap().push((s, e));
        self.publish_lookahead_reserved_from_stripes();
        tracing::warn!(
            "[IBD_LEAD_SLIDE_RELEASE] peer={} {}-{} hole={} lead={} — farthest slid; pack LEAD",
            p,
            s,
            e,
            hole,
            leapfrog_lead_at(hole)
        );
        true
    }

    fn lookahead_overlaps(&self, start: u64, end: u64) -> bool {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .any(|(_, s, e)| start <= *e && end >= *s)
    }

    /// Live stripes, enter-hold, and inflight that **start at or after**
    /// `first_missing` are occupied warehouse. Pack jumps to that `e+1`
    /// (R-107: do not shrink WIDTH / do not assign inside a live farm).
    /// Ranges with `s <= first_missing` (under-apply / enter-hold) do **not**
    /// slide the desert — R-132 fat `lead==512` was 0 because those skipped
    /// `hole+LEAD`. Returns `(occ_end, ignored_behind)`.
    fn packed_overlap_end(
        &self,
        guard: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> (Option<u64>, u32) {
        let hole = self.first_missing_height();
        let mut occ: Option<u64> = None;
        let mut ignored_behind = 0u32;
        let bump = |occ: &mut Option<u64>, e: u64| {
            *occ = Some(occ.map_or(e, |x| x.max(e)));
        };
        let consider = |s: u64, e: u64, occ: &mut Option<u64>, ign: &mut u32| {
            if s <= end && start <= e {
                if s <= hole {
                    *ign = ign.saturating_add(1);
                    return;
                }
                bump(occ, e);
            }
        };
        for &(s, e) in guard.values().flatten() {
            consider(s, e, &mut occ, &mut ignored_behind);
        }
        for &(_, s, e) in self.lookahead_stripes.lock().unwrap().iter() {
            consider(s, e, &mut occ, &mut ignored_behind);
        }
        for &(s, e) in self.lookahead_have_hold.lock().unwrap().iter() {
            consider(s, e, &mut occ, &mut ignored_behind);
        }
        (occ, ignored_behind)
    }

    /// Next reserved farm/hold start strictly after `after`.
    /// Hero GetData clips to `start-1` so a 256-batch cannot overlap the
    /// warehouse (R-108 @47489: batch 47489-47744 vs farm 47617-49664
    /// refused the assign → `flight_tip=0` → GAP_STREAM the remnant).
    /// A live/hold stripe that *covers* `after` counts as start `after+1`
    /// so empty-enter keep still lets the hero own H (R-112 left farm on H).
    fn first_reserved_start_after(&self, after: u64) -> Option<u64> {
        let mut min_s: Option<u64> = None;
        let bump = |min_s: &mut Option<u64>, s: u64| {
            if s > after {
                *min_s = Some(min_s.map_or(s, |x| x.min(s)));
            }
        };
        for &(_, s, e) in self.lookahead_stripes.lock().unwrap().iter() {
            if s <= after && after <= e {
                bump(&mut min_s, after.saturating_add(1));
            } else {
                bump(&mut min_s, s);
            }
        }
        for &(s, e) in self.lookahead_have_hold.lock().unwrap().iter() {
            if s <= after && after <= e {
                bump(&mut min_s, after.saturating_add(1));
            } else {
                bump(&mut min_s, s);
            }
        }
        min_s
    }

    /// Exact live farm GetData covering `hole`. Hero may take H without
    /// treating that warehouse inflight as a competing tip pipe.
    fn farm_stripe_covers_hole(&self, peer: &str, s: u64, e: u64, hole: u64) -> bool {
        self.lookahead_stripes.lock().unwrap().iter().any(|(p, fs, fe)| {
            p == peer && *fs == s && *fe == e && *fs <= hole && hole <= *fe
        })
    }

    fn range_overlaps_inflight_except_farm_on_hole(
        &self,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        start: u64,
        end: u64,
    ) -> bool {
        in_flight.iter().any(|(peer, ranges)| {
            ranges.iter().any(|&(s, e)| {
                s <= end && start <= e && !self.farm_stripe_covers_hole(peer, s, e, start)
            })
        })
    }

    fn inflight_cover_is_only_farm(
        &self,
        in_flight: &HashMap<String, Vec<(u64, u64)>>,
        hole: u64,
    ) -> bool {
        let mut any = false;
        for (peer, ranges) in in_flight {
            for &(s, e) in ranges {
                if s <= hole && hole <= e {
                    any = true;
                    if !self.farm_stripe_covers_hole(peer, s, e, hole) {
                        return false;
                    }
                }
            }
        }
        any
    }

    /// H body is in reorder. Contig-only is not "have" — R-82 `flight_tip=0`
    /// while `CONTIG>=1` packed farmers past a missing H.
    fn h_body_present() -> bool {
        super::IBD_TIP_IN_REORDER.load(Ordering::Relaxed)
    }

    /// R-91: pack if have *or* the hero's inflight covers first_missing.
    /// Empty apply keeps `IN_REORDER=0`; waiting for have left reserved=0
    /// before 50k (R-90). Covering-only without the hero on the hole stays
    /// closed (R-84).
    fn may_pack_runway(
        &self,
        guard: &HashMap<String, Vec<(u64, u64)>>,
        pref: &str,
    ) -> bool {
        if Self::h_body_present() {
            return true;
        }
        let hole = self.first_missing_height();
        guard.get(pref).is_some_and(|r| {
            r.iter().any(|&(s, e)| s <= hole && hole <= e)
        })
    }

    /// R-136 / R-147 leftover: H missing, feeder empty, ahead ≥ WIDTH.
    /// R-153 any-ahead (`ahead>0`) fired on EMPTY_TIP (HOLE **2/78**).
    fn gap_b_tip_hole_ahead_warehouse() -> bool {
        if Self::h_body_present() {
            return false;
        }
        if super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) != 0 {
            return false;
        }
        let ahead = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) as u64;
        ahead >= LEAPFROG_WIDTH
    }

    fn gap_b_lo() -> u64 {
        GAP_B_LO
    }

    /// New far tile only. Re-arm of an already-reserved stripe stays.
    fn gap_b_blocks_new_far_tile(
        &self,
        guard: &HashMap<String, Vec<(u64, u64)>>,
        pref: &str,
        next_needed: u64,
    ) -> bool {
        if next_needed < Self::gap_b_lo() {
            return false;
        }
        if !Self::gap_b_tip_hole_ahead_warehouse() {
            return false;
        }
        let hole = self.first_missing_height();
        guard
            .get(pref)
            .is_some_and(|r| r.iter().any(|&(s, e)| s <= hole && hole <= e))
    }

    fn note_gap_b(&self, next_needed: u64) {
        static LAST: AtomicU64 = AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let prev = LAST.load(Ordering::Relaxed);
        if now.saturating_sub(prev) < GAP_B_LOG_SECS {
            return;
        }
        if LAST
            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        tracing::warn!(
            "[IBD_GAP_B] tip={} ahead={} — no new far tile (HOLE_AHEAD warehouse, hero covers H)",
            next_needed,
            super::IBD_REORDER_AHEAD.load(Ordering::Relaxed)
        );
    }

    fn first_missing_height(&self) -> u64 {
        let h = self.next_needed_height();
        let at = super::IBD_FIRST_HOLE_AT.load(Ordering::Relaxed);
        let hole = super::IBD_FIRST_HOLE.load(Ordering::Relaxed);
        // Published run is [at, hole). Trust it only while H is still in
        // reorder (R-103 L2b sprint: enter 8193, skip 8229). `at == h`
        // without have is R-255 stale hole: apply drained H, HERO_SKIP
        // still GetData's hole, HOLE_ANY silent, covering=0. R-82/R-83:
        // H missing ⇒ do not skip a missing body.
        if Self::h_body_present() {
            if hole > h && at <= h {
                return hole;
            }
            return h.saturating_add(1);
        }
        h
    }

    /// End of the contiguous-from-H run (reorder/contig + hero inflight that
    /// touches H). Disjoint leftover / farmer stripes do not extend this.
    fn contiguous_from_h_end(
        &self,
        guard: &HashMap<String, Vec<(u64, u64)>>,
        pref: &str,
        next_needed: u64,
    ) -> u64 {
        let mut end = next_needed;
        let hole = self.first_missing_height();
        if hole > next_needed {
            end = hole.saturating_sub(1);
        }
        let mut grew = true;
        while grew {
            grew = false;
            if let Some(ranges) = guard.get(pref) {
                for &(s, e) in ranges {
                    if e >= next_needed && s <= end.saturating_add(1) && e > end {
                        end = e;
                        grew = true;
                    }
                }
            }
            if let Ok(claims) = self.tip_cover_claims.lock() {
                for (p, s, e) in claims.iter() {
                    if p == pref && *e >= next_needed && *s <= end.saturating_add(1) && *e > end
                    {
                        end = *e;
                        grew = true;
                    }
                }
            }
        }
        end
    }

    fn leftover_force_blocks_preferred_ahead(
        &self,
        peer_id: &str,
        start: u64,
        next_needed: u64,
    ) -> bool {
        if Self::h_body_present() && start == self.first_missing_height() {
            return false;
        }
        self.leftover_force_getdata.load(Ordering::Relaxed)
            && self.preferred_tip_owner().as_deref() == Some(peer_id)
            && start > next_needed
    }

    pub(crate) fn peer_lookahead_covers(&self, peer_id: &str, height: u64) -> bool {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s <= height && height <= *e)
    }

    pub(crate) fn note_lookahead_stream(&self, peer_id: &str) {
        let mut g = self.lookahead_streams.lock().unwrap();
        let now = Instant::now();
        if let Some(entry) = g.get_mut(peer_id) {
            entry.streams = entry.streams.saturating_add(1);
            entry.last_stream = now;
        } else {
            g.insert(
                peer_id.to_string(),
                TipStreamWindow {
                    streams: 1,
                    started: now,
                    last_stream: now,
                },
            );
        }
    }

    fn lookahead_stream_bps(&self, peer_id: &str) -> (f64, u64, f64) {
        let g = self.lookahead_streams.lock().unwrap();
        let Some(e) = g.get(peer_id) else {
            return (0.0, 0, 0.0);
        };
        let secs = e.started.elapsed().as_secs_f64();
        if secs < LOOKAHEAD_RANK_SECS as f64 {
            return (0.0, e.streams, secs);
        }
        (e.streams as f64 / secs, e.streams, secs)
    }

    fn best_lookahead_ready(&self, exclude: Option<&str>) -> Option<(String, f64, u64)> {
        {
            let mut guard = self.in_flight_per_peer.lock().unwrap();
            self.drop_stale_lookahead_stripes(&mut guard, self.next_needed_height());
        }
        let stripes = self.lookahead_stripes.lock().unwrap().clone();
        let mut best: Option<(String, f64, u64)> = None;
        for (p, _, _) in stripes {
            if exclude == Some(p.as_str()) {
                continue;
            }
            if !self.peer_is_ibd_ready(&p)
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
            {
                continue;
            }
            let (bps, n, _secs) = self.lookahead_stream_bps(&p);
            if n < LOOKAHEAD_MIN_N || bps < LOOKAHEAD_MIN_BPS {
                continue;
            }
            if best.as_ref().map(|(_, b, _)| bps > *b).unwrap_or(true) {
                best = Some((p, bps, n));
            }
        }
        best
    }

    /// Table-top lookahead ≥2× sticky on the lookahead clock. Hold flood ≥2000.
    fn lookahead_outrank_bypasses_healthy(&self, sticky: &str) -> bool {
        let sticky_bps = self.wan_tip_stream_bps(sticky);
        if sticky_bps >= LOOKAHEAD_FLOOD_HOLD_BPS {
            return false;
        }
        let Some((top, top_bps, n)) = self.best_lookahead_ready(Some(sticky)) else {
            return false;
        };
        if n < LOOKAHEAD_MIN_N || top_bps < LOOKAHEAD_MIN_BPS || top_bps < sticky_bps * 2.0 {
            return false;
        }
        tracing::warn!(
            "[IBD_LOOKAHEAD_OUTRANK] sticky={} sticky_bps={:.1} top={} top_bps={:.1} n={} — bypass healthy_tip_bps",
            sticky,
            sticky_bps,
            top,
            top_bps,
            n
        );
        true
    }

    fn best_reserved_stream(&self, exclude: Option<&str>) -> Option<(String, f64, u64)> {
        {
            let mut guard = self.in_flight_per_peer.lock().unwrap();
            self.drop_stale_lookahead_stripes(&mut guard, self.next_needed_height());
        }
        let stripes = self.lookahead_stripes.lock().unwrap().clone();
        let mut best: Option<(String, f64, u64)> = None;
        for (p, _, _) in stripes {
            if exclude == Some(p.as_str()) {
                continue;
            }
            if !self.peer_is_ibd_ready(&p)
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
            {
                continue;
            }
            let (bps, n, secs) = self.lookahead_stream_bps(&p);
            if n < 8 || secs < LOOKAHEAD_RANK_SECS as f64 || bps <= 0.0 {
                continue;
            }
            if best.as_ref().map(|(_, b, _)| bps > *b).unwrap_or(true) {
                best = Some((p, bps, n));
            }
        }
        best
    }

    /// Faster reserved farmer (15s, n≥8) vs sticky on **live GetData EWMA 2×**
    /// **and** reserved BPS 2× sticky H-stream (logged pair).
    /// R-93: R-92 KEEP 486→30.5 (ewma 120 vs 51) must not fire.
    /// Dying-hero 6.8→31.6 still may. Same 2× ratio as `lookahead_outrank_bypasses_healthy`.
    /// Not `win_mbps`. Not best_lookahead_ready (MIN_N=2).
    pub(crate) fn runway_sojourn_outranks(&self, sticky: &str, _next_needed: u64) -> bool {
        self.runway_outrank_challenger(sticky).is_some()
    }

    /// R-123 97k: `HERO_SKIP +1`, reorder ≥ WIDTH, apply sat 44s. `sticky_bps`
    /// stayed 149–341 so R-118 dying (`<80`) never fired. Not `gd_slow` (assigner
    /// does not emit it).
    ///
    /// R-126: warehouse + hole+1 is also healthy fat. R-115 235–236k was **33**
    /// BPS at `win_mbps` 229–280 (line rate). R-124 213–215k was **6.8** BPS
    /// for 294s. Hop only when apply is below half of that 33 and receive is
    /// still alive (`win ≥ LOOKAHEAD_MIN_BPS`). Desert (`reorder=0`) stays
    /// closed. No window yet → do not hop (R-115 false fire).
    fn apply_parked_on_plus1_hole(&self) -> bool {
        let h = self.next_needed_height();
        if h < EMPTY_BAND_SAMPLE_H {
            return false;
        }
        if super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) < LEAPFROG_WIDTH as usize {
            return false;
        }
        if self.first_missing_height().saturating_sub(h) > 1 {
            return false;
        }
        self.apply_stalled_vs_win()
    }

    /// R-115 235–236k timestamp **33** BPS. Half is the stall line so that
    /// dest's slowest fat 1k never hops. R-124 **6.8** is under it.
    const R115_FAT_LINE_BPS: f64 = 33.0;
    const APPLY_STALL_FRAC: f64 = 0.5;
    const APPLY_WIN_SECS: f64 = 8.0;

    fn apply_stalled_vs_win(&self) -> bool {
        let (Some(apply), Some(win)) = self.note_and_read_apply_win() else {
            return false;
        };
        LAST_APPLY_BPS_BITS.store(apply.to_bits(), Ordering::Relaxed);
        LAST_WIN_MBPS_BITS.store(win.to_bits(), Ordering::Relaxed);
        LAST_WIN_HAVE.store(true, Ordering::Relaxed);
        if win < LOOKAHEAD_MIN_BPS {
            return false;
        }
        apply < Self::R115_FAT_LINE_BPS * Self::APPLY_STALL_FRAC
    }

    fn note_and_read_apply_win(&self) -> (Option<f64>, Option<f64>) {
        if TEST_STALL_SEEDED.load(Ordering::Relaxed) {
            return (
                Some(f64::from_bits(TEST_APPLY_BPS_BITS.load(Ordering::Relaxed))),
                Some(f64::from_bits(TEST_WIN_MBPS_BITS.load(Ordering::Relaxed))),
            );
        }
        let h = self.next_needed_height();
        let bytes = super::download::download_bytes_total();
        let now = Instant::now();
        let mut g = apply_win_sample().lock().unwrap_or_else(|e| e.into_inner());
        let dt_a = now.saturating_duration_since(g.at).as_secs_f64();
        let dt_w = now.saturating_duration_since(g.bytes_at).as_secs_f64();
        let apply = if dt_a >= Self::APPLY_WIN_SECS {
            let bps = h.saturating_sub(g.h) as f64 / dt_a;
            g.h = h;
            g.at = now;
            LAST_APPLY_BPS_BITS.store(bps.to_bits(), Ordering::Relaxed);
            Some(bps)
        } else {
            None
        };
        let win = if dt_w >= Self::APPLY_WIN_SECS {
            let db = bytes.saturating_sub(g.bytes) as f64;
            let mbps = (db / (1024.0 * 1024.0)) * 8.0 / dt_w.max(0.001);
            g.bytes = bytes;
            g.bytes_at = now;
            LAST_WIN_MBPS_BITS.store(mbps.to_bits(), Ordering::Relaxed);
            LAST_WIN_HAVE.store(true, Ordering::Relaxed);
            Some(mbps)
        } else {
            None
        };
        (apply, win)
    }

    #[cfg(test)]
    pub(crate) fn test_seed_apply_win_stall(apply_bps: f64, win_mbps: f64) {
        TEST_APPLY_BPS_BITS.store(apply_bps.to_bits(), Ordering::Relaxed);
        TEST_WIN_MBPS_BITS.store(win_mbps.to_bits(), Ordering::Relaxed);
        TEST_STALL_SEEDED.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn test_reset_apply_win_stall() {
        TEST_STALL_SEEDED.store(false, Ordering::Relaxed);
        TEST_APPLY_BPS_BITS.store(0, Ordering::Relaxed);
        TEST_WIN_MBPS_BITS.store(0, Ordering::Relaxed);
        LAST_APPLY_BPS_BITS.store(0, Ordering::Relaxed);
        LAST_WIN_MBPS_BITS.store(0, Ordering::Relaxed);
        LAST_WIN_HAVE.store(false, Ordering::Relaxed);
        let now = Instant::now();
        *apply_win_sample().lock().unwrap_or_else(|e| e.into_inner()) = ApplyWinSample {
            h: 0,
            at: now,
            bytes: 0,
            bytes_at: now,
        };
    }

    /// R-124 fat sit: warehouse full, no second H≥80. Ready worker may take
    /// the hole. Not reserved-rank / 15s (flight_ahead=0 @213k). Floor lottery
    /// still refused (`peer_ok_for_gap_race`). Prefer higher H-stream, then score.
    fn best_parked_ready_challenger(&self, sticky: &str) -> Option<String> {
        let mut best: Option<(String, f64, f64)> = None;
        for p in self.active_download_worker_ids() {
            if p == sticky
                || !self.peer_is_ibd_ready(&p)
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
                || !self.peer_ok_for_gap_race(&p)
            {
                continue;
            }
            let h_bps = self.wan_tip_stream_bps(&p);
            let score = self.peer_score_of(&p);
            let better = match &best {
                None => true,
                Some((_, bh, bs)) => h_bps > *bh || (h_bps == *bh && score > *bs),
            };
            if better {
                best = Some((p, h_bps, score));
            }
        }
        best.map(|(p, _, _)| p)
    }

    /// Proven H (≥80), not reserved warehouse BPS (R-117 28 hops / R-118).
    fn best_proven_h_challenger(&self, sticky: &str) -> Option<String> {
        let peers: Vec<String> = self.peer_scores.lock().unwrap().keys().cloned().collect();
        let mut best: Option<(String, f64)> = None;
        for p in peers {
            if p == sticky
                || !self.peer_is_ibd_ready(&p)
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
            {
                continue;
            }
            let bps = self.wan_tip_stream_bps(&p);
            if bps < LOOKAHEAD_MIN_BPS {
                continue;
            }
            if best.as_ref().map(|(_, b)| bps > *b).unwrap_or(true) {
                best = Some((p, bps));
            }
        }
        best.map(|(p, _)| p)
    }

    /// Drop only the H cover so the new hero can GetData the hole. Ahead
    /// warehouse stays (R-122 UNCOVER_REFILL cancelled it → 180–200k **42**).
    fn vacate_h_cover_keep_ahead(&self, peer: &str, next_needed: u64) {
        if let Ok(mut g) = self.in_flight_per_peer.lock() {
            if let Some(ranges) = g.get_mut(peer) {
                ranges.retain(|&(s, e)| !(s <= next_needed && next_needed <= e));
                if ranges.is_empty() {
                    g.remove(peer);
                }
            }
        }
        self.tip_cover_claims.lock().unwrap().retain(|(p, s, e)| {
            !(p == peer && *s <= next_needed && next_needed <= *e)
        });
    }

    fn runway_outrank_challenger(&self, sticky: &str) -> Option<String> {
        if self.hero_is_flood_class(sticky) {
            return None;
        }
        if self.apply_parked_on_plus1_hole() {
            if let Some(ch) = self.best_proven_h_challenger(sticky) {
                return Some(ch);
            }
            if let Some(ch) = self.best_parked_ready_challenger(sticky) {
                return Some(ch);
            }
        }
        let (sticky_ms, sticky_n) =
            super::tip_stage::getdata_body_ewma_ms_for_peer(sticky, 8)?;
        if sticky_n < 8 {
            return None;
        }
        let (top, top_bps, n) = self.best_reserved_stream(Some(sticky))?;
        if n < 8 {
            return None;
        }
        let sticky_bps = self.wan_tip_stream_bps(sticky);
        if top_bps < sticky_bps * 2.0 {
            return None;
        }
        // Reserved warehouse BPS is not H BPS. R-100 @38k: reserved 1333 vs
        // sticky H 123 KEEP, then H sat at 13. R-116 @179k: reserved 817 vs
        // sticky 87 KEEP, then 22s hole + 19 steals (180–200k 148 vs R-115 232).
        // Healthy sticky (≥80 H-stream) only yields if the challenger also
        // 2× on H — empty and fat. Farms have no H-stream.
        // Fat dying door (R-117: 28 hops): reserved 2× still stole. Challenger
        // must have proven H (≥80). Empty keeps R-93 (6.8→31.6 reserved).
        // Flood hold ≥2000 stays.
        let top_h = self.wan_tip_stream_bps(&top);
        if sticky_bps >= LOOKAHEAD_MIN_BPS && top_h < sticky_bps * 2.0 {
            return None;
        }
        if self.next_needed_height() >= EMPTY_BAND_SAMPLE_H
            && sticky_bps < LOOKAHEAD_MIN_BPS
            && top_h < LOOKAHEAD_MIN_BPS
        {
            return None;
        }
        let (top_ms, top_n) = super::tip_stage::getdata_body_ewma_ms_for_peer(&top, 8)?;
        if top_n < 8 {
            return None;
        }
        if top_ms.saturating_mul(2) > sticky_ms {
            return None;
        }
        Some(top)
    }

    pub(crate) fn maybe_keep_runway_retitle(&self, next_needed: u64) -> bool {
        let Some(sticky) = self.preferred_tip_owner() else {
            return false;
        };
        if let Some(last) = *self.last_tip_trial_at.lock().unwrap() {
            if last.elapsed() < Duration::from_secs(Self::tip_trial_cooldown_secs()) {
                return false;
            }
        }
        let Some(challenger) = self.runway_outrank_challenger(&sticky) else {
            return false;
        };
        if challenger == sticky {
            return false;
        }
        let parked = self.apply_parked_on_plus1_hole();
        if parked {
            self.vacate_h_cover_keep_ahead(&sticky, next_needed);
        } else if !self.hold_covering_getdata(&sticky, next_needed) {
            self.force_release_peer_inflight(&sticky);
        }
        *self.preferred_tip_owner.lock().unwrap() = Some(challenger.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&challenger, next_needed);
        *self.last_tip_trial_at.lock().unwrap() = Some(Instant::now());
        self.lookahead_handoff_on_promote(&sticky, &challenger, next_needed);
        let (top_bps, _, _) = self.lookahead_stream_bps(&challenger);
        let sticky_ewma = super::tip_stage::getdata_body_ewma_ms_for_peer(&sticky, 8)
            .map(|(ms, _)| ms);
        let top_ewma = super::tip_stage::getdata_body_ewma_ms_for_peer(&challenger, 8)
            .map(|(ms, _)| ms);
        if parked {
            tracing::warn!(
                "[IBD_PARKED_YIELD] sticky={} sticky_bps={:.1} top={} top_h={:.1} apply_bps={:.1} win_mbps={:.1} reorder_ahead={} hole={} — skip+1 warehouse, apply stalled vs win, ahead inflight stays",
                sticky,
                self.wan_tip_stream_bps(&sticky),
                challenger,
                self.wan_tip_stream_bps(&challenger),
                f64::from_bits(LAST_APPLY_BPS_BITS.load(Ordering::Relaxed)),
                f64::from_bits(LAST_WIN_MBPS_BITS.load(Ordering::Relaxed)),
                super::IBD_REORDER_AHEAD.load(Ordering::Relaxed),
                self.first_missing_height()
            );
        } else {
            tracing::warn!(
                "[IBD_RUNWAY_OUTRANK] sticky={} sticky_bps={:.1} sticky_ewma_ms={:?} top={} top_bps={:.1} top_ewma_ms={:?} — KEEP retitle, live EWMA 2x + reserved BPS 2x + H-stream 2x if sticky≥80, no abort",
                sticky,
                self.wan_tip_stream_bps(&sticky),
                sticky_ewma,
                challenger,
                top_bps,
                top_ewma
            );
        }
        true
    }

    /// R-146: at 180–210k, retitle only to a flood-class challenger
    /// (stream ≥2000). Probe fallback stole H on R-143/R-145 (`72.90` /
    /// `154.53`) and sat 20–30 BPS. No flood peer → keep sticky. One
    /// successful flood retitle per dest.
    pub(crate) fn maybe_fat_probe_retitle(&self, next_needed: u64) -> bool {
        if !(FAT_PROBE_RETITLE_LO..FAT_PROBE_RETITLE_HI).contains(&next_needed) {
            return false;
        }
        if self.fat_probe_retitle_done.load(Ordering::Relaxed) {
            return false;
        }
        if !super::tip_probe::enabled() || super::tip_probe::probe_ok_count() == 0 {
            return false;
        }
        let Some(sticky) = self.preferred_tip_owner() else {
            return false;
        };
        if crate::node::parallel_ibd::export_owner_hold_protects(&sticky) {
            return false;
        }
        if self.wan_tip_stream_bps(&sticky) >= EMPTY_BAND_FLOOD_BPS {
            self.fat_probe_retitle_done.store(true, Ordering::Relaxed);
            return false;
        }
        let Some(challenger) = self.ready_flood_challenger(&sticky) else {
            return false;
        };
        if !self.hold_covering_getdata(&sticky, next_needed) {
            self.force_release_peer_inflight(&sticky);
        }
        let sticky_bps = self.wan_tip_stream_bps(&sticky);
        *self.preferred_tip_owner.lock().unwrap() = Some(challenger.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&challenger, next_needed);
        *self.last_tip_trial_at.lock().unwrap() = Some(Instant::now());
        self.lookahead_handoff_on_promote(&sticky, &challenger, next_needed);
        self.fat_probe_retitle_done.store(true, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_FAT_PROBE_FLOOD] sticky={} sticky_bps={:.1} top={} top_bps={:.1} tip={} — flood challenger takes H",
            sticky,
            sticky_bps,
            challenger,
            self.wan_tip_stream_bps(&challenger),
            next_needed
        );
        true
    }

    /// R-53 / R-18 hold. List-head pin at score=0.002 is not flood-class.
    /// GetData EWMA, not apply `owner_body_ia_median` (R-63 IA-only / R-70 WIN burst).
    fn hero_is_flood_class(&self, sticky: &str) -> bool {
        if self.wan_tip_stream_bps(sticky) >= EMPTY_BAND_FLOOD_BPS {
            return true;
        }
        matches!(
            super::tip_stage::getdata_body_ewma_ms_for_peer(sticky, 16),
            Some((ms, _)) if ms <= EMPTY_BAND_IA_HOLD_MS
        )
    }

    fn preferred_is_flood_class(&self) -> bool {
        self.preferred_tip_owner()
            .is_some_and(|p| self.hero_is_flood_class(&p))
    }

    /// Fat mute cache is not title. R-162 pierced lifetime ≥60; R-163 then
    /// seated a 0-stream probe (200–248k **99**). R-165 logged `IBD_RECV_MUTE`
    /// 357× and trialed a lifetime-83 sticky at 219k. Hold stays.
    fn fat_sticky_recv_mute(&self, sticky: &str, next_needed: u64) -> bool {
        let _ = (sticky, next_needed);
        false
    }

    fn farm_recv_note_mute(&self, mute: bool) -> u32 {
        if !mute {
            self.farm_recv_mute_streak.store(0, Ordering::Relaxed);
            return 0;
        }
        let mut last = self.farm_recv_last_tick.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < Duration::from_secs(FARM_RECV_TICK_SECS) {
                return self.farm_recv_mute_streak.load(Ordering::Relaxed);
            }
        }
        *last = Some(Instant::now());
        self.farm_recv_mute_streak.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Best farm by current CRAWL recv. LOOKAHEAD stripe (R-163) **or**
    /// inflight covering H / ready worker (R-235 `46.167` was not a stripe)
    /// **or** CRAWL recv-cache top (R-239 46k: `188.214` **181.7** with
    /// `in_flight_ranges=0` / `busy_peers=0` — not a worker on H).
    /// Not probe rank.
    fn fattest_farm_recv(&self, exclude: &str) -> Option<(String, f64)> {
        let mut names: Vec<String> = Vec::new();
        {
            let stripes = self.lookahead_stripes.lock().unwrap();
            for (p, _, _) in stripes.iter() {
                names.push(p.clone());
            }
        }
        if let Ok(g) = self.in_flight_per_peer.lock() {
            names.extend(g.keys().cloned());
        }
        names.extend(self.active_download_worker_ids());
        if let Some((p, _)) = super::download::download_cached_fattest_other(exclude) {
            names.push(p);
        }
        names.sort();
        names.dedup();
        let mut best: Option<(String, f64)> = None;
        for p in names {
            if p == exclude {
                continue;
            }
            if !self.peer_is_ibd_ready(&p)
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
            {
                continue;
            }
            let Some(mbps) = super::download::download_cached_recv_mbps(&p) else {
                continue;
            };
            if best.as_ref().map(|(_, b)| mbps > *b).unwrap_or(true) {
                best = Some((p, mbps));
            }
        }
        best
    }

    /// 15s (3×5s CRAWL) of mute sticky + farm recv ≥8 → one promote onto H.
    /// ≥10k (R-237 dump 32k mute sit). Not empty band. Not fat-probe 180k.
    /// Live sticky recv (R-139 / R-150 / R-158) never mutes → no promote.
    /// 5s flicker (R-162 5.6→339) resets the streak.
    pub(crate) fn maybe_farm_recv_promote(&self, next_needed: u64) -> bool {
        if next_needed < FARM_RECV_LO {
            return false;
        }
        // R-241: dump one-shot @113630; fat open covering=0 sticky recv 0.7
        // top 1021. Dump shot does not spend the fat shot.
        if next_needed >= FAT_PROBE_RETITLE_LO
            && self.farm_recv_promote_done.load(Ordering::Relaxed)
            && !self.farm_recv_fat_rearmed.swap(true, Ordering::Relaxed)
        {
            self.farm_recv_promote_done.store(false, Ordering::Relaxed);
            self.farm_recv_mute_streak.store(0, Ordering::Relaxed);
            *self.farm_recv_last_tick.lock().unwrap() = None;
        }
        if self.farm_recv_promote_done.load(Ordering::Relaxed) {
            return false;
        }
        let Some(sticky) = self.preferred_tip_owner() else {
            return false;
        };
        if crate::node::parallel_ibd::export_owner_hold_protects(&sticky) {
            return false;
        }
        let Some(sticky_recv) = super::download::download_cached_recv_mbps(&sticky) else {
            self.farm_recv_note_mute(false);
            return false;
        };
        let Some((farm, farm_recv)) = self.fattest_farm_recv(&sticky) else {
            self.farm_recv_note_mute(false);
            return false;
        };
        let mute = sticky_recv < STICKY_RECV_MUTE_MBPS && farm_recv >= FARM_RECV_FAT_MBPS;
        let streak = self.farm_recv_note_mute(mute);
        if streak < FARM_RECV_STREAK {
            return false;
        }
        if farm == sticky {
            return false;
        }
        if !self.hold_covering_getdata(&sticky, next_needed) {
            self.force_release_peer_inflight(&sticky);
        }
        *self.preferred_tip_owner.lock().unwrap() = Some(farm.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&farm, next_needed);
        *self.last_tip_trial_at.lock().unwrap() = Some(Instant::now());
        self.lookahead_handoff_on_promote(&sticky, &farm, next_needed);
        self.farm_recv_promote_done.store(true, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_FARM_RECV_PROMOTE] sticky={} sticky_recv_mbps={:.1} farm={} farm_recv_mbps={:.1} streak={} tip={} — farm takes H, one shot",
            sticky,
            sticky_recv,
            farm,
            farm_recv,
            streak,
            next_needed
        );
        true
    }

    #[cfg(test)]
    pub(crate) fn test_backdate_farm_recv_tick(&self) {
        *self.farm_recv_last_tick.lock().unwrap() =
            Some(Instant::now() - Duration::from_secs(FARM_RECV_TICK_SECS + 1));
    }

    /// Ready non-hero that already looks flood. Used to pierce ≥60 / gd_wait
    /// so a late `164.152` can take H from mesh (R-85 locked `14.137` at 318).
    /// Not a mesh lottery. Flood sticky is excluded by the caller.
    fn ready_flood_challenger(&self, exclude: &str) -> Option<String> {
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
            if !self.hero_is_flood_class(&peer_id) {
                continue;
            }
            let bps = self.wan_tip_stream_bps(&peer_id);
            if best.as_ref().map(|(_, s)| bps > *s).unwrap_or(true) {
                best = Some((peer_id, bps));
            }
        }
        best.map(|(p, _)| p)
    }

    /// Hunt: one disjoint-ahead stripe while H<50k and hero is not flood-class.
    /// Hold ≥2000 / IA≤1. R-64: IA `None` does not arm (tip=1 n=2/bps=2).
    /// Not overlapping H. Not R-45. Not R-63 IA-only hold.
    fn empty_band_should_sample(&self, sticky: &str) -> bool {
        if self.next_needed_height() >= EMPTY_BAND_SAMPLE_H {
            return false;
        }
        if super::tip_stage::empty_band_sample_trials() >= 1 {
            return false;
        }
        if self.hero_is_flood_class(sticky) {
            return false;
        }
        if self.wan_tip_stream_bps(sticky) >= EMPTY_BAND_FLOOD_BPS {
            return false;
        }
        matches!(
            super::tip_stage::owner_body_ia_median(),
            Some((ia, _)) if ia > EMPTY_BAND_IA_SAMPLE_AFTER_MS
        )
    }

    fn empty_band_sample_outranks(&self, sticky: &str) -> bool {
        // R-87: hard-false left 146.70 IA17 skip-locked (10–50k 450).
        // One H trial. Not H+32 (R-78). Not R-45 (cap 1 via should_sample).
        // R-94: do not pierce line_rate_or_keep (≥60 covering).
        // R-90 @17543 1344 / R-91 @2229 380 / R-93 @1125 581.
        // Sample still arms when sticky_bps < 60 (R-92 @193 18.2).
        if Self::line_rate_or_keep(self.wan_tip_stream_bps(sticky)) {
            return false;
        }
        self.empty_band_should_sample(sticky)
    }

    fn lookahead_handoff_on_promote(&self, old_hero: &str, new_hero: &str, next_needed: u64) {
        let mut stripes = self.lookahead_stripes.lock().unwrap();
        if let Some(slot) = stripes.iter_mut().find(|(p, _, _)| p == new_hero) {
            let (s, e) = (slot.1, slot.2);
            slot.0 = old_hero.to_string();
            drop(stripes);
            let mut guard = self.in_flight_per_peer.lock().unwrap();
            if let Some(ranges) = guard.get_mut(new_hero) {
                ranges.retain(|&(rs, re)| rs != s || re != e);
                if ranges.is_empty() {
                    guard.remove(new_hero);
                }
            }
            Self::insert_in_flight(&mut guard, old_hero, s, e);
            tracing::warn!(
                "[IBD_LOOKAHEAD] peer={} {}-{} tip={} — vacated stripe to demoted hero",
                old_hero,
                s,
                e,
                next_needed
            );
        }
    }

    /// Sticky's one slot is H when H is missing. A leftover farm stripe must
    /// not `STICKY_CAP` the hero (R-83 KEEP retitle left the new sticky at 1/1).
    /// R-96: drop **reserved** farm even when the hero already covers H.
    /// Do not strip H-pipe growth (next 64 while H still in the first 64) —
    /// R-95 retain-only-covering-H deleted WIN refill (38→3 fills, 2510→592).
    fn vacate_sticky_farm_for_h(
        &self,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        next_needed: u64,
    ) -> bool {
        let farm: Vec<(u64, u64)> = self
            .lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _, _)| p == peer_id)
            .map(|(_, s, e)| (*s, *e))
            .collect();
        if farm.is_empty() {
            return false;
        }
        if let Some(ranges) = guard.get_mut(peer_id) {
            ranges.retain(|&(s, e)| !farm.iter().any(|&(fs, fe)| fs == s && fe == e));
            if ranges.is_empty() {
                guard.remove(peer_id);
            }
        }
        self.lookahead_stripes
            .lock()
            .unwrap()
            .retain(|(p, _, _)| p != peer_id);
        self.publish_lookahead_reserved_from_stripes();
        tracing::warn!(
            "[IBD_LOOKAHEAD] peer={} tip={} — vacate farm, hero takes H",
            peer_id,
            next_needed
        );
        true
    }

    /// R-132: sticky cap is 1 (or TOP 2). A 64-walk **inside a farm**
    /// (`s >= hole+LEAD`) is not the reserved `(s,e)`, so
    /// `vacate_sticky_farm_for_h` leaves it and `STICKY_CAP` blocks H.
    /// Drop only that farm-interior walk. Leftover `(s==e)` stays (R-134).
    /// Desert-fill `hole+1…hole+LEAD` stays (R-134 fat dropped those 12/13
    /// and the 512 never became have). Covering-H 64 stays (R-115 235k).
    /// Farm-peer ahead stays. Not uncover. Not `H_COVER`.
    fn vacate_sticky_non_h_walks(
        &self,
        guard: &mut HashMap<String, Vec<(u64, u64)>>,
        peer_id: &str,
        hole: u64,
    ) -> bool {
        let Some(ranges) = guard.get(peer_id) else {
            return false;
        };
        if ranges.iter().any(|&(s, e)| s <= hole && hole <= e) {
            return false;
        }
        if ranges.is_empty() {
            return false;
        }
        let pack_from = hole.saturating_add(leapfrog_lead_at(hole));
        let walk_from = hole.saturating_add(LEAPFROG_LEAD);
        let drop_walk = |s: u64, e: u64| {
            let len = e.saturating_sub(s).saturating_add(1);
            (s >= pack_from && len >= LEAPFROG_WIDTH)
                || (s >= walk_from && len >= 64)
        };
        if !ranges.iter().any(|&(s, e)| drop_walk(s, e)) {
            return false;
        }
        let Some(ranges) = guard.get_mut(peer_id) else {
            return false;
        };
        let dropped: Vec<(u64, u64)> = ranges
            .iter()
            .copied()
            .filter(|&(s, e)| drop_walk(s, e))
            .collect();
        ranges.retain(|&(s, e)| !drop_walk(s, e));
        if ranges.is_empty() {
            guard.remove(peer_id);
        }
        tracing::warn!(
            "[IBD_HERO_REARM] sticky={} hole={} dropped={:?} — hero takes H",
            peer_id,
            hole,
            dropped
        );
        true
    }

    fn lookahead_range_reserved(&self, peer_id: &str, start: u64, end: u64) -> bool {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .any(|(p, s, e)| p == peer_id && *s == start && *e == end)
    }

    #[cfg(test)]
    pub(crate) fn test_seed_tip_stream_rank(&self, peer: &str, streams: u64, elapsed_secs: u64) {
        let now = Instant::now();
        let started = now
            .checked_sub(Duration::from_secs(elapsed_secs.max(1)))
            .unwrap_or(now);
        self.peer_tip_streams.lock().unwrap().insert(
            peer.to_string(),
            TipStreamWindow {
                streams,
                started,
                last_stream: now,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn test_set_validation_height(&self, h: u64) {
        self.validation_height.store(h, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn test_drop_stale_lookahead(&self, next_needed: u64) {
        let mut guard = self.in_flight_per_peer.lock().unwrap();
        self.drop_stale_lookahead_stripes(&mut guard, next_needed);
    }

    #[cfg(test)]
    pub(crate) fn test_seed_lookahead_stripe(&self, peer: &str, start: u64, end: u64) {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .push((peer.to_string(), start, end));
        self.publish_lookahead_reserved_from_stripes();
    }

    #[cfg(test)]
    pub(crate) fn test_push_lookahead_hold(&self, start: u64, end: u64) {
        self.lookahead_have_hold.lock().unwrap().push((start, end));
        self.publish_lookahead_reserved_from_stripes();
    }

    #[cfg(test)]
    pub(crate) fn test_have_hold_contains(&self, start: u64, end: u64) -> bool {
        self.lookahead_have_hold
            .lock()
            .unwrap()
            .iter()
            .any(|&(s, e)| s == start && e == end)
    }

    #[cfg(test)]
    pub(crate) fn test_lead_window_free(&self, hole: u64) -> bool {
        self.lead_window_free(hole)
    }

    /// R-145: release when RUNWAY_MAX full and all farms slid (+4096 chain).
    #[cfg(test)]
    pub(crate) fn test_try_slide_release(&self, hole: u64) -> bool {
        let mut guard = self.in_flight_per_peer.lock().unwrap();
        if self.packed_stripe_owners() >= runway_max_at(hole) && self.all_runway_farms_slid(hole) {
            self.release_farthest_slid_farm(&mut guard, hole)
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn test_all_runway_farms_slid(&self, hole: u64) -> bool {
        self.all_runway_farms_slid(hole)
    }

    #[cfg(test)]
    pub(crate) fn test_first_missing_height(&self) -> u64 {
        self.first_missing_height()
    }

    #[cfg(test)]
    pub(crate) fn test_lookahead_stream(&self, peer: &str) -> (f64, u64, f64) {
        self.lookahead_stream_bps(peer)
    }

    #[cfg(test)]
    pub(crate) fn test_seed_lookahead_rank(&self, peer: &str, streams: u64, elapsed_secs: u64) {
        let now = Instant::now();
        let started = now
            .checked_sub(Duration::from_secs(elapsed_secs.max(1)))
            .unwrap_or(now);
        self.lookahead_streams.lock().unwrap().insert(
            peer.to_string(),
            TipStreamWindow {
                streams,
                started,
                last_stream: now,
            },
        );
        let start = self.next_needed_height().saturating_add(LOOKAHEAD_OFFSET);
        let end = start.saturating_add(LOOKAHEAD_WIDTH.saturating_sub(1));
        let mut g = self.lookahead_stripes.lock().unwrap();
        if !g.iter().any(|(p, _, _)| p == peer) {
            g.push((peer.to_string(), start, end));
        }
    }

    /// dest-q / dest-at 180k: stripe past H with KEEP hero attached (ahead=192–232)
    /// cheese-sits the empty-block and scored bands. Ahead and sticky dual-pipe
    /// start at H+grown (floor 32), not frontier+1.
    fn wan_ahead_stripe_floor(&self, next_needed: u64) -> u64 {
        // KEEP-only. Do not use preferred_meets_keep_bps — E LINE_RATE would
        // make this H+grown, and R-35 grown is 128 (dest-au first−H=64 cheese).
        let keep = a6m_gd_slow_tip_bps_keep();
        let Some(pref) = self.preferred_tip_owner() else {
            return next_needed;
        };
        if keep <= 0.0
            || !self.tip_sticky_usable(&pref)
            || self.wan_tip_stream_bps(&pref) < keep
        {
            return next_needed;
        }
        let grown = self
            .preferred_tip_owner()
            .map(|p| self.tip_hole_depth_for(&p) as u64)
            .unwrap_or(32)
            .max(32);
        next_needed.saturating_add(grown)
    }

    /// W28c: remember who owns the tip pipeline after a successful assign.
    pub(crate) fn note_tip_owner_assigned(&self, peer_id: &str) {
        // dest-as 66k: gap-preempt / main-queue resticky must not overwrite KEEP.
        // Walk-promote already HOLDs; this is the assign-path door.
        if let Some(pref) = self.preferred_tip_owner() {
            if pref != peer_id && self.preferred_meets_keep_bps() {
                return;
            }
            // R-89: KEEP=0 makes preferred_meets_keep_bps always false, so any
            // tip assign restickied `34.125` at 1000 BPS onto `117.212` at 83.
            // Do not change preferred_meets_keep_bps (that door is extras).
            // Mute (bps<60) still yields. R-87 23 BPS WIN still yields.
            if pref != peer_id && Self::line_rate_or_keep(self.wan_tip_stream_bps(&pref)) {
                return;
            }
            // R-66b: WIN holds ignition (H<64) so list-head cannot steal.
            // R-71: after 64, WIN pins only a flood-class pipe. Mute/fail
            // / STICKY_DROP still clear. Cheap resticky — no trial teardown.
            if pref != peer_id
                && super::tip_stage::tournament_winner().as_deref() == Some(pref.as_str())
                && (self.next_needed_height() < 64 || self.hero_is_flood_class(&pref))
            {
                return;
            }
        }
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

    /// Ready worker with CRAWL recv strictly above `sticky_recv`. Not score.
    /// R-226 successor `18.194` was farm-score; `36.225` recv 0.1 sat H.
    pub(crate) fn h_slow_fatter_recv_successor(
        &self,
        sticky: &str,
        sticky_recv: f64,
    ) -> Option<String> {
        let mut best: Option<(String, f64)> = None;
        for p in self.active_download_worker_ids() {
            if p == sticky
                || self.is_peer_blacklisted(&p)
                || self.tip_owner_in_fail_cooldown(&p)
                || !self.peer_is_ibd_ready(&p)
            {
                continue;
            }
            let Some(r) = super::download::download_cached_recv_mbps(&p) else {
                continue;
            };
            if !h_slow_recv_successor_beats(sticky_recv, r) {
                continue;
            }
            if best.as_ref().map(|(_, br)| r > *br).unwrap_or(true) {
                best = Some((p, r));
            }
        }
        best.map(|(p, _)| p)
    }

    /// After a slow exclusive-H GetData at fat+, cooldown that sticky once
    /// per GetData sample. R-218 dumped occupancy by rotating below 180k and
    /// re-firing the same `tip_gd` 24× (`sticky=-`).
    ///
    /// R-219: 15s fail-cooldown expired and the same peer re-won H. Park
    /// **120s** (mute default) with `cool_healthy` so STREAM ≥60 cannot skip.
    /// CAS the GetData sample so concurrent `get_work` cannot triple-fire.
    /// Pin a live successor — open slot + score lottery reseated `74.167`.
    pub(crate) fn maybe_rotate_slow_h_sticky(&self) {
        let Some(sticky) = self.preferred_tip_owner() else {
            return;
        };
        let last_peer = super::tip_stage::last_getdata_body_peer_id();
        if last_peer.as_deref() != Some(sticky.as_str()) {
            return;
        }
        let next_needed = self
            .validation_height
            .load(Ordering::Relaxed)
            .saturating_add(1);
        let gd_ms = super::tip_stage::last_getdata_body_ms();
        if gd_ms == 0 {
            return;
        }
        let prev = LAST_H_SLOW_GD_MS.load(Ordering::Relaxed);
        let already = gd_ms == prev;
        let feeder = super::IBD_FEEDER_BUFFER_BLOCKS.load(Ordering::Relaxed) as u64;
        let sticky_bps = self.wan_tip_stream_bps(&sticky);
        let reorder_ahead = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) as u64;
        // R-334: fat-RTT rotation opt-in only (see `h_slow_rotate_enabled`).
        let fat_rtt = h_slow_rotate_enabled()
            && body_h_rtt_should_rotate(
                gd_ms as f64,
                sticky_bps,
                feeder,
                next_needed,
                already,
            );
        let dump_wh = body_dump_warehouse_should_rotate(
            sticky_bps,
            feeder,
            next_needed,
            reorder_ahead,
            already,
        );
        if !fat_rtt && !dump_wh {
            return;
        }
        if let Some((sticky_recv, top_recv)) =
            super::download::download_cached_sticky_vs_top(&sticky)
        {
            if h_slow_recv_leader_holds(sticky_recv, top_recv) {
                return;
            }
        }
        if crate::node::parallel_ibd::export_owner_hold_protects(&sticky) {
            return;
        }
        // Scored sticky: only pin a ready worker with recv strictly above
        // sticky (R-226 pin `18.194` score then `36.225` recv 0.1). Unknown
        // cache keeps R-219 score successor (R-217 still rotates).
        let successor = match super::download::download_cached_recv_mbps(&sticky) {
            Some(sticky_recv) => match self.h_slow_fatter_recv_successor(&sticky, sticky_recv) {
                Some(p) => p,
                None => return,
            },
            None => {
                if self.ibd_ready_peer_count() <= 1
                    || self.any_ready_active_worker_except(&sticky).is_none()
                {
                    return;
                }
                match self.any_ready_active_worker_except(&sticky) {
                    Some(p) => p,
                    None => return,
                }
            }
        };
        if self.ibd_ready_peer_count() <= 1 {
            return;
        }
        if LAST_H_SLOW_GD_MS
            .compare_exchange(prev, gd_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        // Cool first so successor pick cannot re-elect this peer (R-219 15s
        // open-slot + score lottery reseated `74.167`).
        self.mark_tip_owner_fail_cooldown_ex(&sticky, H_SLOW_COOLDOWN_SECS, true);
        self.force_release_peer_inflight(&sticky);
        *self.preferred_tip_owner.lock().unwrap() = Some(successor.clone());
        self.tip_owner_open.store(false, Ordering::Relaxed);
        self.reset_sticky_wan_tenure(&successor, next_needed);
        tracing::warn!(
            "[IBD_H_SLOW] peer={} rtt_ms={} sticky_bps={:.1} feeder={} h={} cool={}s successor={} reorder={} why={} — rotate",
            sticky,
            gd_ms,
            sticky_bps,
            feeder,
            next_needed,
            H_SLOW_COOLDOWN_SECS,
            successor,
            reorder_ahead,
            if dump_wh { "dump_warehouse" } else { "fat_rtt" }
        );
    }

    /// dest-ba KEEP=80 doors. Ship KEEP=0. STREAM ≥60 is the same bar as C1u.
    /// Do not restore KEEP default 80 (R-22). Mute (bps≈0) stays mute.
    pub(crate) fn line_rate_or_keep(bps: f64) -> bool {
        if bps >= LINE_RATE_OWNER_BPS {
            return true;
        }
        let keep = a6m_gd_slow_tip_bps_keep();
        keep > 0.0 && bps >= keep
    }

    /// dest-ae: dest-ab @255073 had a ≥80 hero; dest-ac FAIL pinned mutes
    /// when ahead<8. Sparse-cheese timer/pin only for KEEP preferred.
    /// R-43: KEEP=0 → false (R-30 / R-35 extras off). E LINE_RATE opened
    /// `wan_allow_multi_peer_ahead` → R-42 CHEESE ahead=149 holes=32.
    /// dest-ba KEEP=80 still holds. Do not restore KEEP default 80.
    pub(crate) fn preferred_meets_keep_bps(&self) -> bool {
        let keep = a6m_gd_slow_tip_bps_keep();
        if keep <= 0.0 {
            return false;
        }
        let Some(pref) = self.preferred_tip_owner() else {
            return false;
        };
        self.tip_sticky_usable(&pref) && self.wan_tip_stream_bps(&pref) >= keep
    }

    /// W28c: clear sticky owner after tip-covering failure so the next best peer can take over.
    pub(crate) fn note_tip_owner_failed(&self, peer_id: &str) {
        self.note_tip_owner_failed_with_cooldown(peer_id, Self::tip_owner_fail_cooldown_secs());
    }

    /// R-243: TCP RST of the covering hero. Cooldown first so concurrent
    /// `get_work` cannot TIP_WALK_PROMOTE residual inflight (R-242: RST
    /// `3.136.178.225` then promote 186264-187793 300ms later). Not mute
    /// (120s) — reconnect should be allowed after the fail cooldown.
    ///
    /// R-244: `note_tip_owner_failed` skips cooldown for sole-ready / healthy
    /// BPS (still-connected CAP). Live R-243 dump: covering `136.117.5.22`
    /// RST logged `[IBD_TIP_OWNER_COOLDOWN_SKIP] sole ready`, inflight dropped,
    /// then the same IP was re-titled and wiped 9× in 0–10k (371 vs R-242 880).
    /// TCP down always cools; those skips stay for CAP.
    pub(crate) fn note_peer_tcp_gone(&self, peer_id: &str) {
        self.note_tip_owner_failed(peer_id);
        let secs = Self::tip_owner_fail_cooldown_secs();
        let until = Instant::now() + Duration::from_secs(secs);
        self.tip_owner_fail_until
            .lock()
            .unwrap()
            .insert(peer_id.to_string(), until);
        self.force_release_peer_inflight(peer_id);
        tracing::warn!(
            "[IBD_PEER_GONE] peer={} — inflight dropped, tip slot opened (TCP down, cool {}s)",
            peer_id,
            secs
        );
    }

    /// W103: mute tip-gap CAP abort — shorter cooldown + clear W88 failover episode so a
    /// (H,H) racer can arm immediately. Live W102b: 15s cooldown + episode latch left
    /// covering=1 mute peer for 8s×N inside tip60-watch 20s.
    ///
    /// **W111:** also `force_release_peer_inflight` — live W110 @326324 walk-promoted the
    /// mute-failed peer from residual in-flight in the same ms as mute (`TIP_WALK_PROMOTE`
    /// after `TIP_FAILOVER` armed), re-pinning sticky and burning another 5–15s.
    pub(crate) fn note_tip_owner_failed_mute(&self, peer_id: &str) {
        // dest-ap cheese H: a ≥80 that just 5s-timed-out *this* H must not
        // skip cooldown (healthy_tip_bps) and re-arm the same hole until
        // LIMITED. Short tip-only cool so W28c can take **this** H. Not
        // dest-am shadow (second inflight on H) and not P1e's 120s mute ban
        // (that parks the only ≥80 → Genesis-d grown=8).
        let stream_bps = self.wan_tip_stream_bps(peer_id);
        let stream_hero = Self::line_rate_or_keep(stream_bps);
        let window_hero = self.is_last_stream_keep_hero(peer_id);
        let just_pinned = self.is_recent_gd_slow_pin(peer_id);
        // dest-az 154k/179k: last STREAM-window ≥80 decayed below keep, then
        // MUTE_KILL GD_SLOW new= that peer and P1e-120s'd them 3s later.
        // This-H 8s failover, not dest-aq 180s keep-hot.
        let protect_decayed = (window_hero && !stream_hero) || just_pinned;
        let (secs, cool_healthy) = if (stream_hero && self.cheese_tip_sitting()) || protect_decayed {
            let secs = std::env::var("BLVM_IBD_CHEESE_HERO_H_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8)
                .clamp(5, 15);
            if protect_decayed {
                if just_pinned {
                    tracing::warn!(
                        "[IBD_P1E_GD_SLOW_PIN_PROTECT] peer={} stream={:.1} — 8s this-H, not 120s mute ban",
                        peer_id,
                        stream_bps
                    );
                } else {
                    tracing::warn!(
                        "[IBD_P1E_WINDOW_HERO_PROTECT] peer={} stream={:.1} — last STREAM ≥80; 8s this-H, not 120s mute ban",
                        peer_id,
                        stream_bps
                    );
                }
            }
            (secs, true)
        } else {
            // P1e: tip-role ban after mute (PIPE_FILL / CAP). Default **120s**
            // (was 5s) so TIP_PIN cannot re-elect the mute; clamp 60–180.
            let secs = std::env::var("BLVM_IBD_TIP_OWNER_MUTE_COOLDOWN_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120)
                .clamp(60, 180);
            (secs, false)
        };
        // Drop W88 episode so want_tip_owner can assign failover micro on the next poll.
        self.tip_failover_once_h.store(0, Ordering::Relaxed);
        self.tip_failover_once_at_ms.store(0, Ordering::Relaxed);
        self.note_tip_owner_failed_with_cooldown_ex(peer_id, secs, cool_healthy);
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
        self.note_tip_owner_failed_with_cooldown_ex(peer_id, cooldown_secs, false);
    }

    fn note_tip_owner_failed_with_cooldown_ex(
        &self,
        peer_id: &str,
        cooldown_secs: u64,
        cool_healthy: bool,
    ) {
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
        self.mark_tip_owner_fail_cooldown_ex(peer_id, cooldown_secs, cool_healthy);
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
        self.mark_tip_owner_fail_cooldown_ex(peer_id, secs, false);
    }

    fn mark_tip_owner_fail_cooldown_ex(&self, peer_id: &str, secs: u64, cool_healthy: bool) {
        if crate::node::parallel_ibd::export_owner_hold_protects(peer_id) {
            tracing::warn!(
                "[IBD_EXPORT_OWNER_HOLD] skip P1e/cooldown peer={} (was {}s)",
                peer_id,
                secs
            );
            return;
        }
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
        // Genesis-d 180k+: only fast peer (`164.152` lifetime ≥80) sat in 30s
        // trial/A6m cooldown while mutes took grown=8. Same bar as KEEP / C1u-hero.
        // dest-ap exception: cheese `(H,H)` timeout *this* height (`cool_healthy`).
        let bps = self.wan_tip_stream_bps(peer_id);
        if !cool_healthy && Self::line_rate_or_keep(bps) {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN_SKIP] peer={} — healthy_tip_bps={:.1} (was {}s)",
                peer_id,
                bps,
                secs
            );
            return;
        }
        let until = Instant::now() + Duration::from_secs(secs);
        self.tip_owner_fail_until
            .lock()
            .unwrap()
            .insert(peer_id.to_string(), until);
        if cool_healthy {
            tracing::warn!(
                "[IBD_CHEESE_HERO_H_COOLDOWN] peer={} {}s bps={:.1} — this H failover, not LIMITED",
                peer_id,
                secs,
                bps
            );
        } else {
            tracing::warn!(
                "[IBD_TIP_OWNER_COOLDOWN] peer={} {}s — skip tip-owner / TIP_PIN",
                peer_id,
                secs
            );
        }
    }

    /// H missing and bodies already sit in reorder (dest-ap 148449 / dest-an 201447).
    pub(crate) fn cheese_tip_sitting(&self) -> bool {
        self.tip_gap_missing.load(Ordering::Relaxed)
            && super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) > 0
    }

    /// dest-ap: cheese `(H,H)` timeout on a ≥80 is not a prune signal.
    pub(crate) fn cheese_hero_h_timeout_no_strike(
        &self,
        peer_id: &str,
        start: u64,
        end: u64,
        err_str: &str,
    ) -> bool {
        if !super::download::cheese_h_timeout_err(err_str) {
            return false;
        }
        let tip = self.next_needed_height();
        if !(start <= tip && tip <= end) {
            return false;
        }
        if !self.cheese_tip_sitting() {
            return false;
        }
        Self::line_rate_or_keep(self.wan_tip_stream_bps(peer_id))
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
        // R-22: `maybe_drop_mute_tip_cover` already holds `in_flight` (get_work).
        // Re-lock wedged R-21 after MUTE_DROP. try_lock: remove if free, else the
        // caller already dropped this peer from the live guard.
        if let Ok(mut g) = self.in_flight_per_peer.try_lock() {
            g.remove(peer_id);
        }
        self.clear_tip_cover_claims_for_peer(peer_id);
    }

    fn preferred_covers_tip_h(&self, pref: &str, next_needed: u64) -> bool {
        let g = self.in_flight_per_peer.lock().unwrap();
        g.get(pref).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(s, e)| s <= next_needed && next_needed <= e)
        })
    }

    /// Layer A: covering GetData with STREAM ≥1 is not evicted to search.
    /// Same mute line as `MUTE_DROP` (`bps ≥ 1`). Skip/cooldown stay ≥60.
    fn hold_covering_getdata(&self, peer: &str, next_needed: u64) -> bool {
        self.preferred_covers_tip_h(peer, next_needed)
            && self.wan_tip_stream_bps(peer) >= 1.0
    }

    fn cheese_hero_abort_ahead_ms() -> u64 {
        std::env::var("BLVM_IBD_CHEESE_HERO_ABORT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000)
            .clamp(0, 30_000)
    }

    /// Preferred ≥80 is fetching start>H while cheese is armed (FEEDER_STARVE
    /// pin or awaiting≥2s). Override `C1J_KEEP` so the hero drops the stripe
    /// and `get_work` can gap-preempt H.
    fn cheese_hero_must_drop_ahead(&self, peer_id: &str, start: u64) -> bool {
        let next_needed = self.next_needed_height();
        if start <= next_needed {
            return false;
        }
        if !self.tip_gap_missing.load(Ordering::Relaxed) {
            return false;
        }
        if self.preferred_tip_owner().as_deref() != Some(peer_id) {
            return false;
        }
        if !self.tip_sticky_usable(peer_id)
            || !Self::line_rate_or_keep(self.wan_tip_stream_bps(peer_id))
        {
            return false;
        }
        let ahead = super::IBD_REORDER_AHEAD.load(Ordering::Relaxed);
        let holes = super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        // dest-ba C1J_KEEP is the next PIPE_FILL (H+32 / FAST 64). dest-bk
        // live drop was H+96. dest-x pin drops FAR start>H so the hero
        // takes H. R-38 pin still aborted H+32/H+64 (WIN10K 61.8 vs R-37
        // 327). Hold PIPE_FILL before pin / awaiting.
        const PIPE_FILL_HOLD: u64 = 64;
        if start <= next_needed.saturating_add(PIPE_FILL_HOLD) {
            return false;
        }
        // dest-q / dest-x: abort FAR only when H is empty. R-39 dest-bk
        // awaiting + pin aborted H+88 while covering≥1 (WIN10K 95 vs R-30
        // 1415). dest-ba apply/GetData wait is not dest-q.
        if self.healthy_tip_cover_count(next_needed) >= 1 {
            return false;
        }
        // dest-bk 179761: sticky `108.36` @1411 C1J_KEEP span start>H,
        // await_ms=35s, then 48s crawl to 180k inst 20. ahead<8 && holes<5
        // used to return before awaiting, so C1J_KEEP never aborted and
        // dest-x 273 died at 184k ts 56. Awaiting ≥ abort_ms drops start>H
        // even when reorder-ahead is still low. Healthy 150ms holes stay
        // C1J_KEEP (awaiting not armed).
        let awaiting =
            super::tip_stage::tip_awaiting_ms_for_cap() >= Self::cheese_hero_abort_ahead_ms();
        if awaiting {
            return true;
        }
        if ahead < 8 && holes < 5 {
            return false;
        }
        false
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
        // Probe rank first (endgame §1); cheese ≥80 / covering0 / score stay fallback.
        if self.preferred_tip_owner.lock().unwrap().is_none() {
            let pin = self
                .best_probe_ready(None)
                .or_else(|| {
                    super::sole_tip_forced_owner().filter(|p| !self.is_peer_blacklisted(p))
                })
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
        let mut crossed_keep = false;
        // Hot path: tip-adjacent bodies credit every STREAM — avoid `to_string` on hits.
        if let Some(entry) = g.get_mut(peer_id) {
            if entry.started.elapsed() > Duration::from_secs(600) {
                // dest-au 208k: 10min hard reset set streams=1 → bps=1 and
                // tip_gd_force trialled KEEP hero `104.194` (sticky_streams=2
                // vs chall_streams=4365) while CRAWL still 472. Carry the
                // rate into 30s so healthy_tip_bps still holds; mute decays
                // in that window (not dest-aq 180s keep-hot).
                const CARRY_SECS: u64 = 30;
                let secs = entry.started.elapsed().as_secs_f64().max(1.0);
                let bps = entry.streams as f64 / secs;
                let carry = ((bps * CARRY_SECS as f64).round() as u64).max(1);
                *entry = TipStreamWindow {
                    streams: carry.saturating_add(1),
                    started: now
                        .checked_sub(Duration::from_secs(CARRY_SECS))
                        .unwrap_or(now),
                    last_stream: now,
                };
                if Self::line_rate_or_keep(bps) {
                    crossed_keep = true;
                }
            } else {
                let prev_streams = entry.streams;
                entry.streams = entry.streams.saturating_add(1);
                entry.last_stream = now;
                let secs = entry.started.elapsed().as_secs_f64().max(1.0);
                let prev = prev_streams as f64 / secs;
                let bps = entry.streams as f64 / secs;
                if Self::line_rate_or_keep(bps) && !Self::line_rate_or_keep(prev) {
                    crossed_keep = true;
                }
            }
        } else {
            g.insert(
                peer_id.to_string(),
                TipStreamWindow {
                    streams: 1,
                    started: now,
                    last_stream: now,
                },
            );
        }
        drop(g);
        if self.next_needed_height() < 64 && !super::tip_stage::tournament_closed() {
            super::tip_stage::tournament_note_body(peer_id);
            match super::tip_stage::tournament_poll() {
                super::tip_stage::TournamentPoll::Win { ref peer, .. } => {
                    self.note_tip_owner_assigned(peer);
                }
                super::tip_stage::TournamentPoll::Timeout
                | super::tip_stage::TournamentPoll::None => {}
            }
        }
        if crossed_keep {
            // dest-ba 158k: challenger stream-count (28k vs sticky 2.5k) must
            // not steal window-hero identity from the preferred KEEP sticky.
            let pref = self.preferred_tip_owner();
            let mut h = self.last_stream_keep_hero.lock().unwrap();
            if (pref.as_deref() == Some(peer_id) || h.is_none())
                && h.as_deref() != Some(peer_id) {
                    *h = Some(peer_id.to_string());
                }
        }
    }

    fn is_last_stream_keep_hero(&self, peer_id: &str) -> bool {
        self.last_stream_keep_hero
            .lock()
            .unwrap()
            .as_deref()
            == Some(peer_id)
    }

    fn is_recent_gd_slow_pin(&self, peer_id: &str) -> bool {
        let g = self.last_gd_slow_pin.lock().unwrap();
        match g.as_ref() {
            Some((p, t)) if p == peer_id => {
                t.elapsed() <= Duration::from_secs(gd_slow_pin_protect_secs())
            }
            _ => false,
        }
    }

    fn remember_gd_slow_pin(&self, peer_id: &str) {
        *self.last_gd_slow_pin.lock().unwrap() =
            Some((peer_id.to_string(), Instant::now()));
    }

    #[cfg(test)]
    pub(crate) fn test_note_gd_slow_pin(&self, peer_id: &str) {
        self.remember_gd_slow_pin(peer_id);
    }

    #[cfg(test)]
    pub(crate) fn test_age_tip_stream_started(&self, peer_id: &str, started_ago_secs: u64) {
        let mut g = self.peer_tip_streams.lock().unwrap();
        if let Some(e) = g.get_mut(peer_id) {
            if let Some(t) = Instant::now().checked_sub(Duration::from_secs(started_ago_secs)) {
                e.started = t;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_age_tip_stream_last(&self, peer_id: &str, last_ago_secs: u64) {
        let mut g = self.peer_tip_streams.lock().unwrap();
        if let Some(e) = g.get_mut(peer_id) {
            if let Some(t) = Instant::now().checked_sub(Duration::from_secs(last_ago_secs)) {
                e.last_stream = t;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_ahead_stripe_floor(&self, next_needed: u64) -> u64 {
        self.wan_ahead_stripe_floor(next_needed)
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

    /// C1u must not cliff a proven tip owner on *global* mute EWMA.
    /// Genesis-a 148k: 136.33 KEEP'd at 386 BPS then `[IBD_TIP_HOLE_GD_SLOW_HOLD] 8→8`
    /// because `gd_ewma=1003` from the previous sticky.
    ///
    /// R-28: KEEP=0 made this always false, so line-rate 128-block GetData
    /// (1.3 s @ 86 mbps / 112 KiB) hit the 800 ms gate and cliffed 128→8.
    /// That is the 76 vs ~91 gap. Mute (bps≈0) still clamps.
    pub(crate) fn tip_owner_clears_c1u_clamp(&self, peer_id: &str) -> bool {
        let bps = self.wan_tip_stream_bps(peer_id);
        if bps >= LINE_RATE_OWNER_BPS {
            return true;
        }
        let keep = a6m_gd_slow_tip_bps_keep();
        if keep > 0.0 && bps >= keep {
            return true;
        }
        // R-147: fat sticky keeps the grown H pipe.
        // R-148 LEAD-farm clear FAIL: 200–248k 120 sit 45% vs R-147 151 / 14%.
        // Do not rematch farm C1u. Mute / extras stay clamped.
        self.fat_preferred_keeps_c1u_pipe(peer_id)
    }

    /// Fat titled owner with any GAP_STREAM this window keeps the grown pipe.
    fn fat_preferred_keeps_c1u_pipe(&self, peer_id: &str) -> bool {
        let h = self.validation_height.load(Ordering::Relaxed);
        if h < FAT_PROBE_RETITLE_LO {
            return false;
        }
        if self.preferred_tip_owner().as_deref() != Some(peer_id) {
            return false;
        }
        let g = self.peer_tip_streams.lock().unwrap();
        g.get(peer_id).is_some_and(|e| e.streams > 0)
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
        self.export_owner_hold_tick();
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
        if gd_slow {
            let (ms, n) = gd_ewma.unwrap_or((0, 0));
            let feeder_keep = a6m_gd_slow_feeder_keep();
            let tip_keep = a6m_gd_slow_tip_bps_keep();
            let stream_bps = self.wan_tip_stream_bps(&sticky);
            // E16/E16b: runway or strong tip crawl ⇒ do not OPEN/blacklist on EWMA alone.
            let keep_feeder = feeder_keep > 0 && feeder >= feeder_keep && !mute_fast;
            // Genesis TRUE WAN 2026-08-22 @181k: persist-skip keeps feeder=0, so
            // mute_fast=true on any gd_slow. That bypassed tip_bps keep and rotated a
            // 273 BPS sticky → 13 BPS trial → 20s no_preferred → ~90 BPS. E11 illusory
            // health was ~64 BPS (below default keep 80). Keep tip_bps even when mute_fast.
            // dest-ax 226k: cheese sit dropped *height* recent_bps below min so this
            // block never ran, then MUTE_KILL GD_SLOW despite TIP_OWNER_COOLDOWN_SKIP
            // healthy_tip_bps=92. Keep on stream ≥ keep even when H is sitting.
            let keep_tip_recent = Self::line_rate_or_keep(recent_bps);
            let keep_tip_stream = Self::line_rate_or_keep(stream_bps);
            // dest-ba 498k/590k: cheese sit decayed stream under 80 then
            // A6N_OPEN + 180s cooldown of a probe-known ≥80. Skip rotate
            // unless a better-ranked probe is ready (endgame §1).
            let keep_tip_probe = tip_keep > 0.0
                && super::tip_probe::probe_keep_hero(&sticky)
                && !self.challenger_probe_outranks(&sticky);
            let keep_tip = keep_tip_recent || keep_tip_stream || keep_tip_probe;
            if keep_feeder || keep_tip {
                tracing::warn!(
                    "[IBD_A6M_GD_SLOW_KEEP] sticky={} tip_bps={:.1} stream_bps={:.1} tip_keep={:.0} feeder={}≥{} gd_ewma={}ms (n={}) ≥ {} pipe_recv0={} reason={} — skip rotate",
                    sticky,
                    recent_bps,
                    stream_bps,
                    tip_keep,
                    feeder,
                    feeder_keep,
                    ms,
                    n,
                    a6m_max_getdata_ms(),
                    pipe_mute,
                    if keep_tip && keep_feeder {
                        "tip+feeder"
                    } else if keep_tip_probe && !keep_tip_stream && !keep_tip_recent {
                        "probe_rank"
                    } else if keep_tip_stream && !keep_tip_recent {
                        "stream_bps"
                    } else if keep_tip {
                        "tip_bps"
                    } else {
                        "feeder"
                    }
                );
                return false;
            }
            if recent_bps >= min_bps {
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

    /// P2: tip trials. Default **off** since R-332: TRIAL_START killed the tip owner on
    /// slowness (`covering=0`) and left the tip hole ownerless for 62s (R-331 320–330k
    /// 33.4 BPS). Mute kills ≥200k: R-273 9, R-331 57, R-332 (trials off) 2; THE band
    /// 61.2→84.0. Opt in: `BLVM_IBD_TIP_TRIAL=1`.
    fn tip_trial_enabled() -> bool {
        matches!(
            std::env::var("BLVM_IBD_TIP_TRIAL")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("on") | Some("yes")
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
        sticky_bps: f64,
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
            "[IBD_TIP_TRIAL_SKIP] reason={} sticky={} sticky_bps={:.1} await_ms={} need_ms={} tip={}",
            reason,
            sticky,
            sticky_bps,
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

    /// First ready probe-ranked peer (does not set `grown`; owner EWMA stays B1).
    fn best_probe_ready(&self, exclude: Option<&str>) -> Option<String> {
        for (p, _) in super::tip_probe::ranked_probes(exclude) {
            if self.is_peer_blacklisted(&p)
                || !self.peer_is_ibd_ready(&p)
                || self.tip_owner_in_fail_cooldown(&p)
                || self.peer_has_reserved_stripe(&p)
            {
                continue;
            }
            return Some(p);
        }
        None
    }

    fn peer_has_reserved_stripe(&self, peer_id: &str) -> bool {
        self.lookahead_stripes
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _, _)| p == peer_id)
    }

    /// True when a ready non-sticky probe outranks sticky's probe (or sticky is unranked).
    /// dest-ba 158k: `63.254` outranked `80.147` — trial must start.
    fn challenger_probe_outranks(&self, sticky: &str) -> bool {
        let sticky_bps = super::tip_probe::probe_wave_bps(sticky).unwrap_or(0.0);
        match self.best_probe_ready(Some(sticky)) {
            Some(ch) => super::tip_probe::probe_wave_bps(&ch).unwrap_or(0.0) > sticky_bps,
            None => false,
        }
    }

    /// dest-as 190k: `healthy_tip_bps` sat on unranked `32.217` (stream 82–263)
    /// while `35.203.41.106` was table-top at 1231 and taking ahead chunks.
    /// Bypass only when that **table-top** peer (not rank #2) is ready and
    /// outranks by ≥2×. If #1 is not live-ready, sit and log; do not install
    /// mid-rank (`164.152` dest-as REVERT). D-7.1: dest-as 1231 *was* a
    /// worker (126 takes); the ready gate still passes that sit.
    pub(crate) fn probe_outrank_bypasses_healthy(&self, sticky: &str, next_needed: u64) -> bool {
        // Same instrument: probe GetData sojourn vs sticky tip GetData EWMA.
        // Empty-band 10k wave is not tip BPS (R-75 stole `120.159` @2614).
        // Mute still trials without this bypass.
        if next_needed < EMPTY_BAND_SAMPLE_H {
            return false;
        }
        // R-157 fat line-rate probe-hold REVERTED — 180–200k 89
        // (warehouse sit while title held). R-146 flood-only retitle stays.
        if self.hero_is_flood_class(sticky) {
            return false;
        }
        let Some((top, _)) = super::tip_probe::best_probe_rank_skip(Some(sticky), |p| {
            self.is_peer_blacklisted(p)
        }) else {
            return false;
        };
        if super::tip_probe::probe_n(&top) < 2 {
            return false;
        }
        let Some(top_ms) = super::tip_probe::probe_ewma_ms(&top) else {
            return false;
        };
        let Some((sticky_gd_ms, _)) =
            super::tip_stage::getdata_body_ewma_ms_for_peer(sticky, 8)
        else {
            return false;
        };
        if top_ms.saturating_mul(2) > sticky_gd_ms {
            return false;
        }
        let reason = if self.is_peer_blacklisted(&top) {
            "blacklist"
        } else if !self.peer_is_ibd_ready(&top) {
            "not_ready"
        } else if self.tip_owner_in_fail_cooldown(&top) {
            "fail_cooldown"
        } else {
            tracing::warn!(
                "[IBD_PROBE_OUTRANK] sticky={} sticky_gd_ms={} top={} top_probe_ms={} — bypass healthy_tip_bps",
                sticky,
                sticky_gd_ms,
                top,
                top_ms
            );
            return true;
        };
        Self::log_probe_ready_skip(&top, reason, top_ms as f64, sticky);
        false
    }

    fn log_probe_ready_skip(peer: &str, reason: &str, rank_bps: f64, sticky: &str) {
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
            "[IBD_PROBE_READY_SKIP] peer={} reason={} rank_bps={:.1} sticky={}",
            peer,
            reason,
            rank_bps,
            sticky
        );
    }

    /// R-131: 8s last_win collapsed + warehouse. `flight_tip` 0 or ≥1.
    /// Await-gate bypass. Not 5s CRAWL win (R-36: 182872 6.5 then 70).
    /// Not R-115 235k (win 260). Not empty. Not uncover. Not last-completed tip_gd.
    const W2A_WIN_COLLAPSE: f64 = 16.0;

    fn last_win_mbps(&self) -> Option<f64> {
        if TEST_STALL_SEEDED.load(Ordering::Relaxed) {
            return Some(f64::from_bits(TEST_WIN_MBPS_BITS.load(Ordering::Relaxed)));
        }
        let (_, win) = self.note_and_read_apply_win();
        if win.is_some() {
            return win;
        }
        if !LAST_WIN_HAVE.load(Ordering::Relaxed) {
            return None;
        }
        Some(f64::from_bits(LAST_WIN_MBPS_BITS.load(Ordering::Relaxed)))
    }

    fn win_collapse_may_pierce(&self, next_needed: u64) -> bool {
        if next_needed < EMPTY_BAND_SAMPLE_H {
            return false;
        }
        if super::IBD_REORDER_AHEAD.load(Ordering::Relaxed) < LEAPFROG_WIDTH as usize {
            return false;
        }
        let Some(win) = self.last_win_mbps() else {
            return false;
        };
        if win >= Self::W2A_WIN_COLLAPSE {
            return false;
        }
        let Some(sticky) = self.preferred_tip_owner() else {
            return false;
        };
        self.best_proven_h_challenger(&sticky).is_some()
    }

    /// P2: ready challenger for a tip trial (does **not** require prior tip streams).
    fn best_tip_trial_challenger(&self, exclude: &str) -> Option<String> {
        // R-89: reserved farmers do not take H (`72.81` stripe → chall_bps=10).
        // KEEP retitle is the farm→hero door. Probe rank next.
        // Probe rank first — next-best non-owner, not lottery / A6n-only.
        if let Some(p) = self.best_probe_ready(Some(exclude)) {
            return Some(p);
        }
        // dest-as: after first IBD_PROBE_OK, do not fall through to score lottery.
        if super::tip_probe::enabled() && super::tip_probe::probe_ok_count() > 0 {
            return None;
        }
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
        if self.maybe_keep_runway_retitle(next_needed) {
            return true;
        }
        if self.maybe_fat_probe_retitle(next_needed) {
            return true;
        }
        if self.maybe_farm_recv_promote(next_needed) {
            return true;
        }
        if self.maybe_demote_cooled_ia_owner(next_needed) {
            return true;
        }
        self.maybe_start_tip_trial(next_needed)
    }

    /// Live owner body-IA demotion (origin-arm n=5). `sticky_bps ≥80` is not
    /// throughput: slow draws held ≥80 at IA median 19 ms. When trailing IA
    /// median ≥ 10 ms and probe has a ready ≥80 alternate, rotate. Does not
    /// change cheese, pin, or the hold ≥80 bar. EXPORT_OWNER_HOLD still blocks.
    pub(crate) fn maybe_demote_cooled_ia_owner(&self, _next_needed: u64) -> bool {
        self.export_owner_hold_tick();
        // R-81: probe does not take H (R-75). Slot pick only.
        false
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
        self.export_owner_hold_tick();
        // Probe-rank is quality discovery (endgame increment 1). Do not cut this
        // skip-forest as if wait_feeder is always 86–91% — dest-bc collect-bound
        // on a fast owner. dest-bb STREAM-window skip stays dropped. Never git restore.
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
        // R-28: 128-block line-rate GetData is ~1.3 s → gd_slow is a false positive.
        // R-26 then ping-ponged 104.239 ↔ 104.250 every 30 s from 293k (`tip_gd_force`).
        // R-36: `< LINE_RATE` treated a GetData wait (window 25–59) as dead and
        // stole `13.43` / `164.152` at await_ms=0. Mute (bps<1) still force-trials.
        // Await-gate / slow_drip unchanged.
        let tip_gd_force = feeder == 0
            && gd_slow
            && covering >= 1
            && sticky_for_gd.as_ref().is_some_and(|sticky| {
                self.wan_tip_stream_bps(sticky) < 1.0
                    && self.best_tip_trial_challenger(sticky).is_some_and(|ch| {
                        let s = self.tip_stream_count(sticky);
                        let c = self.tip_stream_count(&ch);
                        c > s
                            || self.wan_tip_stream_bps(&ch)
                                > self.wan_tip_stream_bps(sticky) * 1.25
                    })
            });
        // §7.2: feeder==0 ∧ gap_missing ∧ awaiting≥T — or slow-drip / tip-gd force.
        // R-81: KEEP retitle is maybe_keep_runway_retitle (before this). Mute trial
        // stays. Probe / lookahead / runway do not start TipTrial.
        let sticky_early = self.preferred_tip_owner();
        let sample_outrank = sticky_early
            .as_deref()
            .is_some_and(|s| self.empty_band_sample_outranks(s));
        if feeder > 0 && !sample_outrank {
            return false;
        }
        // R-131: compute before await-gate. R-129 172k / R-130 184460 first
        // tick had await < need and never reached the pierce.
        let win_collapse = self.win_collapse_may_pierce(next_needed);
        if !sample_outrank
            && !slow_drip
            && !tip_gd_force
            && !win_collapse
            && (!gap || await_ms < need_ms)
        {
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
                        let pin = self.preferred_tip_owner();
                        let pin_s = pin.as_deref().unwrap_or("-");
                        let pin_bps = pin
                            .as_deref()
                            .map(|p| self.wan_tip_stream_bps(p))
                            .unwrap_or(0.0);
                        Self::log_tip_trial_skip(
                            "post_open_settle",
                            pin_s,
                            await_ms,
                            need_ms,
                            next_needed,
                            pin_bps,
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
                Self::log_tip_trial_skip("no_preferred", "-", await_ms, need_ms, next_needed, 0.0);
                return false;
            }
        };
        if crate::node::parallel_ibd::export_owner_hold_protects(&sticky) {
            Self::log_tip_trial_skip(
                "export_owner_hold",
                &sticky,
                await_ms,
                need_ms,
                next_needed,
                self.wan_tip_stream_bps(&sticky),
            );
            return false;
        }
        // Genesis TRUE WAN 2026-08-22 @280k: 3s await blip MUTE_KILL'd a 594 BPS
        // sticky (TRIAL_START → REVERT trial_bps=0 → 120s cooldown) and dropped
        // the soak 313→22. A6m tip_bps keep already exists; trial start must
        // honor it too. slow_drip is already bps < min, so this only blocks
        // await-gate / tip_gd_force evictions of a healthy crawl.
        let tip_keep = a6m_gd_slow_tip_bps_keep();
        let sticky_bps = self.wan_tip_stream_bps(&sticky);
        // dest-aq 211→280k: 180s last_keep sat on `162.35` after the
        // window dropped under 80 (`grown=8`, ts 72). Skip only while
        // *current* stream BPS is ≥ keep. Cheese-H no-strike + 8s cool
        // still covers the 5s timeout without pinning a mute.
        // dest-ba skipped TRIAL_START when sticky stream ≥ KEEP.
        // KEEP=0 + flood-2000/8s-crawl-only left R-35 TRIAL_START=74 on a
        // 71 BPS hero (dest-1 273→80). E: STREAM ≥60 is the same bar as A6M.
        // Mute (bps≈0) still trials. Probe outrank still may start (dest-as).
        // Do not restore KEEP default 80.
        let recent_crawl = self
            .tip_crawl_recent_bps(next_needed, 8)
            .map(|(b, _)| b)
            .unwrap_or(0.0);
        // R-36: window BPS and 8s crawl both go to 0 while the next GetData
        // is in flight. `13.43` had 15499 streams and still TRIAL_START'd.
        // KEEP=0 only: last_stream <8s on the *preferred* peer is a wait, not
        // a mute. R-41: `is_last_stream_keep_hero` is one global slot (first
        // flood `104.63`). Preferred `217.62` with 12k–31k streams still
        // TRIAL_START'd because it was not that slot. dest-aq KEEP=80 still
        // trials when current window < keep
        // (`p2_tip_trial_starts_when_window_dips_below_keep`). Mute
        // (streams=0 / last_stream ≥8s) still trials. Probe outrank still may.
        let gd_wait_hold = tip_keep <= 0.0
            && self.peer_recently_tip_streaming(&sticky, Duration::from_secs(8));
        // Flood sticky holds (same skip). Mesh sticky + flood challenger
        // pierces — R-85 `TRIAL_SKIP` healthy_tip_bps @82 locked 318 BPS.
        // Not R-45: mesh vs mesh still skips. Challenger must already be flood.
        let flood_challenger = if self.hero_is_flood_class(&sticky) {
            None
        } else {
            self.ready_flood_challenger(&sticky)
        };
        // R-43 baseline. R-45 empty-band flood skip FAIL (10–50k 219 / 190k 99).
        // R-129: R-128 tip_gd stall pierce reverted (dest tag 0; 195k froze at 799).
        let tip_gd_stall = false;
        let recv_mute = self.fat_sticky_recv_mute(&sticky, next_needed);
        if recv_mute {
            tracing::warn!(
                "[IBD_RECV_MUTE] sticky={} sticky_bps={:.1} sticky_recv_mbps={:.1} tip={} — lifetime healthy does not skip trial",
                sticky,
                sticky_bps,
                super::download::download_cached_recv_mbps(&sticky).unwrap_or(0.0),
                next_needed
            );
        }
        if (Self::line_rate_or_keep(sticky_bps)
            || recent_crawl >= LINE_RATE_OWNER_BPS
            || gd_wait_hold)
            && !recv_mute
        {
            if self.empty_band_sample_outranks(&sticky)
                || flood_challenger.is_some()
                || win_collapse
                || self.probe_outrank_bypasses_healthy(&sticky, next_needed)
            {
                // dest-as 190k hole / R-58 lookahead 2× / R-86 flood pierce.
                // R-131: 8s last_win<16 + warehouse + proven-H, ft 0 or ≥1.
                if win_collapse && flood_challenger.is_none() {
                    let flight_tip = {
                        let g = self.in_flight_per_peer.lock().unwrap();
                        Self::covering_next_count(&g, next_needed)
                    };
                    tracing::warn!(
                        "[IBD_WIN_COLLAPSE] sticky={} win_mbps={:.1} reorder={} flight_tip={} sticky_bps={:.1} — pierce healthy_tip_bps",
                        sticky,
                        self.last_win_mbps().unwrap_or(0.0),
                        super::IBD_REORDER_AHEAD.load(Ordering::Relaxed),
                        flight_tip,
                        sticky_bps
                    );
                }
            } else {
                Self::log_tip_trial_skip(
                    if gd_wait_hold && !Self::line_rate_or_keep(sticky_bps) {
                        "gd_wait_recent_stream"
                    } else {
                        "healthy_tip_bps"
                    },
                    &sticky,
                    await_ms,
                    need_ms,
                    next_needed,
                    sticky_bps,
                );
                return false;
            }
        }
        // Endgame §1: trial of a probe-known ≥80 is rare. dest-ba 158k
        // TRIAL_START found `63.254` because they outranked — allow that.
        // dest-bb STREAM-window skip would have blocked that trial.
        if tip_keep > 0.0
            && super::tip_probe::probe_keep_hero(&sticky)
            && !self.challenger_probe_outranks(&sticky)
        {
            Self::log_tip_trial_skip(
                "probe_keep_hero",
                &sticky,
                await_ms,
                need_ms,
                next_needed,
                sticky_bps,
            );
            return false;
        }
        let flood_pierce = flood_challenger.is_some();
        // R-163: recv-mute + best_tip_trial_challenger seated probe `86.249`
        // (0 streams) and skipped reserved farms (R-89). Fat farm recv is the
        // only legal mute challenger. No farm → do not trial.
        let Some(challenger) = (if recv_mute {
            self.fattest_farm_recv(&sticky).map(|(p, _)| p)
        } else {
            flood_challenger
                .or_else(|| {
                    if win_collapse {
                        self.best_proven_h_challenger(&sticky)
                    } else {
                        None
                    }
                })
                .or_else(|| self.best_tip_trial_challenger(&sticky))
        }) else {
            Self::log_tip_trial_skip(
                if recv_mute {
                    "recv_mute_no_farm"
                } else {
                    "no_challenger"
                },
                &sticky,
                await_ms,
                need_ms,
                next_needed,
                self.wan_tip_stream_bps(&sticky),
            );
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
        // R-130 W2a: H is already uncovered. force_release would wipe ahead
        // (R-122). Vacate H cover only.
        if win_collapse {
            self.vacate_h_cover_keep_ahead(&sticky, next_needed);
        } else if !self.hold_covering_getdata(&sticky, next_needed) {
            // Layer A: covering bps≥1 GetData finishes. Mute still releases (R-18 find).
            self.force_release_peer_inflight(&sticky);
        }
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
        if next_needed < EMPTY_BAND_SAMPLE_H && sample_outrank {
            super::tip_stage::note_empty_band_sample_trial();
            tracing::warn!(
                "[IBD_EMPTY_SAMPLE] sticky={} challenger={} tip={} sticky_bps={:.1} — one H trial",
                sticky,
                challenger,
                next_needed,
                sticky_bps
            );
        }
        self.lookahead_handoff_on_promote(&sticky, &challenger, next_needed);
        if flood_pierce {
            tracing::warn!(
                "[IBD_FLOOD_PIERCE] sticky={} challenger={} tip={} sticky_bps={:.1} — flood challenger takes H",
                sticky,
                challenger,
                next_needed,
                sticky_bps
            );
        }
        tracing::warn!(
            "[IBD_TIP_TRIAL_START] sticky={} challenger={} tip={} awaiting={}s trial={}s sticky_streams={} chall_streams={} slow_drip={} tip_gd_force={} tip_gd_stall={} await_ms={} covering={}",
            sticky,
            challenger,
            next_needed,
            awaiting,
            Self::tip_trial_secs(),
            sticky_streams,
            chall_streams,
            slow_drip,
            tip_gd_force,
            tip_gd_stall,
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
        let trial_secs = trial.started.elapsed().as_secs_f64().max(1e-3);
        let trial_bps = height_delta as f64 / trial_secs;
        let gd_still_slow = super::tip_stage::getdata_body_ewma_ms()
            .map(|(ms, _)| ms >= a6m_max_getdata_ms())
            .unwrap_or(false);
        // Keep challenger if they delivered tip streams ≥ sticky×1.25, or sticky had
        // zero tip bodies while tip advanced / challenger streamed.
        //
        // Leftover TRUE WAN 2026-08-22: `maybe_start_tip_trial` cools sticky
        // (`force_release` + fail cooldown) so `sticky_delta` is ~always 0.
        // The cooled-zero arm then KEEP'd every dribble — live hdr-locator soak
        // 7 KEEP / 1 REVERT, KEEP tip_bps 18–58 with gd_ewma 0.9–10s (global
        // EWMA, not per-peer). Height still advances because validation
        // continues. Do not KEEP that unfair zero-compare unless GetData is
        // no longer slow or the challenger cleared the E16b healthy-crawl bar
        // (default 80 BPS).
        let cooled_unfair = sticky_delta == 0;
        let stream_win = chall_delta > 0
            && (chall_delta as f64 >= (sticky_delta as f64) * 1.25
                || (cooled_unfair && height_delta > 0));
        // Genesis TRUE WAN 2026-08-22: sticky_delta=1–4 still lost the cooled_unfair
        // guard and KEEP'd gd_slow challengers at trial_bps 19–48 (stall 221–225k).
        // Live 374k: gd_ewma=766 just under the slow bar → KEEP trial_bps=4.2
        // (`gd_slow=false`) and pinned the soak at 7 BPS. The 80 bar is a KEEP
        // floor, not only a gd_slow qualifier.
        let keep_bar = a6m_gd_slow_tip_bps_keep();
        let chall_bps = self.wan_tip_stream_bps(&trial.challenger);
        // Genesis-c 184421: mute→hero START, CRAWL sticky_bps=394 grown=24, then
        // REVERT height_delta=0 chall_delta=0 trial_bps=0 and 120s-cool the hero.
        // Height-delta is unmeasurable on a single missing tip; lifetime tip-stream
        // ≥80 is the same bar as healthy_tip_bps / KEEP floor. Mute dribbles
        // (18–58) stay under the bar and still REVERT.
        // KEEP=0 used to KEEP any stream_win (R-26 ping-pong 16×). dest-ba
        // only KEEP'd a ≥80 challenger. E: ≥60 challenger, else REVERT drip.
        //
        // R-159 mute_trial_keep REVERTED — 180–200k 90. The KEEP at 180656
        // was existing line-rate (chall_bps=188), not mute (sticky 112).
        // Fat steal was PROBE_OUTRANK. Do not rematch R-157 probe-hold (89).
        let hero_keep = Self::line_rate_or_keep(chall_bps);
        let keep = hero_keep || (stream_win && keep_bar > 0.0 && trial_bps >= keep_bar);
        *self.tip_trial.lock().unwrap() = None;
        if keep {
            // Challenger stays preferred; sticky already cooled for tip-role.
            tracing::warn!(
                "[IBD_TIP_TRIAL_KEEP] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={} trial_bps={:.1} chall_bps={:.1} gd_slow={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta,
                trial_bps,
                chall_bps,
                gd_still_slow
            );
            self.log_mute_kill(
                "TRIAL_KEEP",
                &trial.sticky,
                Some(&trial.challenger),
                next_needed,
                Some(self.wan_tip_stream_bps(&trial.challenger)),
            );
            super::tip_stage::reset_getdata_body_ewma();
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
                "[IBD_TIP_TRIAL_REVERT] sticky={} challenger={} tip={} sticky_delta={} chall_delta={} height_delta={} trial_bps={:.1} chall_bps={:.1} gd_slow={} cooled_unfair={}",
                trial.sticky,
                trial.challenger,
                next_needed,
                sticky_delta,
                chall_delta,
                height_delta,
                trial_bps,
                chall_bps,
                gd_still_slow,
                cooled_unfair
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
        if crate::node::parallel_ibd::export_owner_hold_protects(sticky) {
            tracing::warn!(
                "[IBD_EXPORT_OWNER_HOLD] skip MUTE_KILL/GD_SLOW/OPEN sticky={} tenure_bps={:.2}",
                sticky,
                tenure_bps
            );
            return false;
        }
        let sticky_tip_bps = self.wan_tip_stream_bps(sticky);
        // dest-ax 226k: cheese sit skipped keep_tip (height recent_bps < min) then
        // MUTE_KILL GD_SLOW while mark_tip_owner_fail_cooldown skipped (≥80 stream).
        // Same bar as TIP_OWNER_COOLDOWN_SKIP / trial skip — do not rotate.
        let stream_keep = a6m_gd_slow_tip_bps_keep();
        if gd_slow && Self::line_rate_or_keep(sticky_tip_bps) {
            tracing::warn!(
                "[IBD_A6M_GD_SLOW_KEEP] sticky={} stream_bps={:.1} tenure_bps={:.1} tip_keep={:.0} — skip MUTE_KILL (healthy stream)",
                sticky,
                sticky_tip_bps,
                tenure_bps,
                stream_keep
            );
            return false;
        }
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
                if gd_slow {
                    self.remember_gd_slow_pin(cand_id);
                }
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
            .best_probe_ready(Some(sticky))
            .or_else(|| {
                self.top_scored_active_ready_worker()
                    .filter(|p| p != sticky)
            })
            .or_else(|| self.any_ready_active_worker_except(sticky));
        let mut cleared_cd = 0usize;
        if pinned.is_none() && gd_slow {
            // E15: prior ROTATE cooled+blacklisted the only tip hero → pinned=None×3.
            let cleared_bl = self.clear_blacklist_except(sticky);
            cleared_cd = self.clear_tip_owner_fail_cooldowns_except(sticky);
            pinned = self
                .best_probe_ready(Some(sticky))
                .or_else(|| {
                    self.top_scored_active_ready_worker()
                        .filter(|p| p != sticky)
                })
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
        if gd_slow {
            if let Some(ref p) = pinned {
                self.remember_gd_slow_pin(p);
            }
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
        if self.is_peer_blacklisted(pref) || !self.is_active_download_worker(pref) {
            return false;
        }
        if self.peer_is_ibd_ready(pref) {
            return true;
        }
        // R-66b: WIN then STICKY_DROP 1ms later. Ignition racers have
        // ibd_ready=false. Product B: declared winner owns H.
        // R-68: TIMEOUT has no winner → same drop of list-head (0–10k 263).
        if super::tip_stage::tournament_winner().as_deref() == Some(pref) {
            return true;
        }
        super::tip_stage::tournament_timed_out()
            && self.preferred_tip_owner().as_deref() == Some(pref)
    }

    /// R-66 / R-69: a not-ready winner or TIMEOUT preferred owns H before any
    /// cover exists. A ready list-head must not take that span.
    fn unready_tournament_sticky_owns_h(&self, pref: &str) -> bool {
        !self.peer_is_ibd_ready(pref) && self.tip_sticky_usable(pref)
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
    ///
    /// **Must not `lock()` `in_flight_per_peer`.** `get_work` holds that mutex for the
    /// whole assign, then calls [`Self::wan_allow_multi_peer_ahead`] → hole-band →
    /// [`Self::tip_owner_credible_for_hole_freeze`] → here. Genesis-c 186043: a
    /// blocking lock here wedged `get_work` (`held_ms` 45s→450s+), CRAWL died, and
    /// leftover_force `arm_try_lock_fail` could not insert `(H,H)`.
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
        let Ok(g) = self.in_flight_per_peer.try_lock() else {
            return false;
        };
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
            // W137: mid+ mute-CAP cooldown clear only when no pin candidate exists.
            // Live R-51 @21025: MID_CLEAR uncooled MUTE_DROP'd 113.30, then W138
            // PREFER_MID stole pin from floor 112.157. Floor/idle candidate wins;
            // E15b lockout (every scored ready cooled) still clears below.
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
                if pin_target.is_none() {
                    self.maybe_clear_mid_plus_fail_cooldowns_covering0(next_needed);
                    pin_target = forced_tip.clone().or_else(|| {
                        self.best_covering0_tip_pin_candidate(next_needed)
                            .or_else(|| self.top_scored_peer_id())
                    });
                }
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

    /// Live GetData covering H. Claims can lag CHUNK_OBSOLETE (R-252 OPEN_SKIP sit).
    fn inflight_covers_height(&self, height: u64) -> bool {
        self.in_flight_per_peer
            .lock()
            .unwrap()
            .values()
            .flatten()
            .any(|&(s, e)| s <= height && height <= e)
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
        let no_tip_abort = Self::no_tip_abort_enabled();
        // C1j: tip body missing + range strictly ahead → abort cheese GetData.
        // Prior: `next_needed < start` returned false → ahead kept filling tip+32 while
        // tip empty (C1h ahead_buf_p50=40 / C1i samples still TIP_HOLE_AHEAD).
        if self.tip_gap_missing.load(Ordering::Relaxed) && start > next_needed {
            // R-255: revert R-253/R-254 COVERING0_ABORT. Fat abort **813**
            // churned the hero (R-254 **102** vs R-252 **160**). R-58 HOLD
            // reserved. H in reorder still KEEP. Wall A: not a second TCP on H.
            // R-58: HOLD reserved first. FAR abort used to run before this and
            // drop stripe 2 (H+128) when covering=0 (hero skip / H in reorder).
            if self.lookahead_range_reserved(peer_id, start, end) {
                tracing::warn!(
                    "[IBD_C1J_LOOKAHEAD_HOLD] peer={} {}-{} tip={} — reserved stripe held",
                    peer_id,
                    start,
                    end,
                    next_needed
                );
                return false;
            }
            // R-28: C1j aborted start>H then reissued 2364–2427 ×2. Reservation
            // must survive abort. Dump-height latch stays off (next_needed gate).
            if self.latched_ahead_holds(peer_id, start, end) {
                tracing::warn!(
                    "[IBD_C1J_LATCH_HOLD] peer={} {}-{} tip={} — reserved extra held",
                    peer_id,
                    start,
                    end,
                    next_needed
                );
                return false;
            }
            if self.priority_zone_holds(peer_id, start, end) {
                static LAST_PZ_HOLD_MS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let prev = LAST_PZ_HOLD_MS.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 500
                    && LAST_PZ_HOLD_MS
                        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    tracing::warn!(
                        "[IBD_C1J_PRIORITY_HOLD] peer={} {}-{} tip={} — satd near-cursor tile held",
                        peer_id,
                        start,
                        end,
                        next_needed
                    );
                }
                return false;
            }
            // R-69 @341k: leftover 343180-343307 while applied=341529 covering=0.
            // PIPE_FILL (start≤H+64) still KEEP. Do not FAR-abort reserved stripes.
            const FAR_AHEAD_ABORT: u64 = 64;
            let h_buffered = super::IBD_TIP_IN_REORDER.load(Ordering::Relaxed)
                || super::IBD_TIP_CONTIG_RUNWAY.load(Ordering::Relaxed) >= 1;
            if self.healthy_tip_cover_count(next_needed) == 0
                && !h_buffered
                && start > next_needed.saturating_add(FAR_AHEAD_ABORT)
            {
                if no_tip_abort {
                    Self::c1j_noabort_log(peer_id, start, end, next_needed, 1);
                    return false;
                }
                tracing::warn!(
                    "[IBD_C1J_LOOKAHEAD_ABORT] peer={} span={}-{} tip={} covering=0 — drop far reserved stripe",
                    peer_id,
                    start,
                    end,
                    next_needed
                );
                return true;
            }
            // q 175k / r 37k: C1J_KEEP let the ≥80 preferred keep a stripe
            // starting after H for 50–73s. Once cheese is armed, drop that
            // ahead GetData so the hero can take H. Healthy 150ms hole still
            // keeps C1J_KEEP (pin/awaiting not armed).
            if self.cheese_hero_must_drop_ahead(peer_id, start) {
                if no_tip_abort {
                    Self::c1j_noabort_log(peer_id, start, end, next_needed, 2);
                    return false;
                }
                static CHEESE_ABORT_LOG: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let prev = CHEESE_ABORT_LOG.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 5
                    && CHEESE_ABORT_LOG
                        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    tracing::warn!(
                        "[IBD_CHEESE_HERO_ABORT] peer={} span={}-{} tip={} ahead={} holes={} — drop preferred start>H (C1J_KEEP overridden dest-bk awaiting-low-ahead)",
                        peer_id,
                        start,
                        end,
                        next_needed,
                        super::IBD_REORDER_AHEAD.load(Ordering::Relaxed),
                        super::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed)
                    );
                }
                return true;
            }
            // R-54 dest FAIL — C1j hold for latched extras is OFF again.
            // R-28 dest cheese'd @2366 because extras start>H and C1j aborted
            // then reissued. Wall B is off; C1j stays absolute for start>H.
            // Genesis TRUE WAN 2026-08-22 @280–294k: gap_missing is permanently
            // true (EMPTY_TIP every height). C1j then aborts the hero's next
            // stripe (PIPE_FILL 32) as "cheese", walk-in 3/3 blacklists them,
            // and the soak sits at 11–44 BPS. Keep a hot tip-STREAM peer's
            // pipe — same exception as tip-past-end below.
            let last_ago_s = {
                let g = self.peer_tip_streams.lock().unwrap();
                g.get(peer_id).map(|e| e.last_stream.elapsed().as_secs())
            };
            if self.peer_recently_tip_streaming(
                peer_id,
                Duration::from_secs(Self::TIP_STREAM_HOT_SECS),
            ) {
                let keep = a6m_gd_slow_tip_bps_keep();
                let stream_bps = self.wan_tip_stream_bps(peer_id);
                // dest-be 05:23: C1J_KEEP held 17 BPS drip stripes 1k ahead
                // (95.216 361623 / 74.131 361111) while covering=0 at H=360635.
                // KEEP is the ≥80 hero's next PIPE_FILL, not any peer that
                // streamed a body in the last 15s. E: STREAM ≥60 when KEEP=0.
                if Self::line_rate_or_keep(stream_bps) {
                    static C1J_KEEP_LOG: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let prev = C1J_KEEP_LOG.load(Ordering::Relaxed);
                    if now.saturating_sub(prev) >= 5
                        && C1J_KEEP_LOG
                            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        tracing::warn!(
                            "[IBD_C1J_KEEP] peer={} span={}-{} tip={} last_stream_ago_s={:?} hot_secs={} stream_bps={:.1} keep={:.0} — skip abort, hot tip-STREAM ≥keep",
                            peer_id,
                            start,
                            end,
                            next_needed,
                            last_ago_s,
                            Self::TIP_STREAM_HOT_SECS,
                            stream_bps,
                            keep
                        );
                    }
                    return false;
                }
            }
            // C: issued extra stays while hero is line-rate and H is covered.
            // dest-ba C1J_ABORT 495. Silent ≥15s still dies (cold leftover).
            if self.preferred_tip_owner().as_deref() != Some(peer_id) {
                if let Some(pref) = self.preferred_tip_owner() {
                    let extra_fresh = last_ago_s.map(|s| s < 15).unwrap_or(true);
                    if extra_fresh
                        && Self::line_rate_or_keep(self.wan_tip_stream_bps(&pref))
                        && self.healthy_tip_cover_count(next_needed) >= 1
                    {
                        return false;
                    }
                }
            }
            if no_tip_abort {
                Self::c1j_noabort_log(peer_id, start, end, next_needed, 3);
                return false;
            }
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
                    "[IBD_C1J_ABORT] peer={} span={}-{} tip={} last_stream_ago_s={:?} hot_secs={} — past-tip GetData aborted while tip missing",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    last_ago_s,
                    Self::TIP_STREAM_HOT_SECS
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
        // R-233: do **not** abort farm-behind-H here — LOOKAHEAD stripes start
        // before current H once tip walks into them (dest-bc 513-2560). Abort
        // killed dump occupancy. Resticky skip lives in `promote_tip_walk_in`.
        // R-323: cooled peer keeps in-flight (incl. 2048 farm stripes). Default
        // on with NO_TIP_ABORT; do **not** promote (W111). `=0` restores abort.
        if self.tip_owner_in_fail_cooldown(peer_id) {
            if no_tip_abort {
                Self::c1j_noabort_log(peer_id, start, end, next_needed, 4);
                return false;
            }
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

    /// R-232: a farm stripe packed before H (`start < next_needed`) with deep
    /// remaining runway is not a tip walk-in **while a ≥60 sticky holds H**.
    /// Live: `34.106` span 179631-181678 → claim 180215-181678 same-ms as
    /// H_SLOW successor `46.28`. W65 shallow remain < min_depth still promotes.
    /// Walk-in ≥60 still promotes.
    /// R-246: skip also fired when sticky recv=0.0; covering `24.113`
    /// 189996-190187 at H=190000 sat until farm-recv @191556 of a 30 mbps
    /// farm. Do not skip when preferred is missing or not line-rate.
    pub(crate) fn farm_behind_h_blocks_walk_promote(
        &self,
        peer_id: &str,
        start: u64,
        end: u64,
        next_needed: u64,
    ) -> bool {
        if start >= next_needed || next_needed > end {
            return false;
        }
        let min_depth = Self::tip_deep_cover_min_depth();
        let remain = Self::claim_remaining_tip_depth(next_needed, next_needed, end);
        if remain < min_depth {
            return false;
        }
        if Self::line_rate_or_keep(self.wan_tip_stream_bps(peer_id)) {
            return false;
        }
        matches!(
            self.preferred_tip_owner(),
            Some(pref)
                if pref != peer_id && Self::line_rate_or_keep(self.wan_tip_stream_bps(&pref))
        )
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
        if self.farm_behind_h_blocks_walk_promote(peer_id, start, end, next_needed) {
            tracing::warn!(
                "[IBD_TIP_WALK_PROMOTE_SKIP] peer={} span={}-{} tip={} — farm-behind-H (hold ≥60 sticky)",
                peer_id,
                start,
                end,
                next_needed
            );
            return;
        }
        // genesis-o 80–147k: hero `174.93` @200–300 lost preferred every 2–4k to
        // ahead mutes. W51 only no-ops while a deep claim still exists; after the
        // hero pipe completes, walk-promote restickies the mute (243× on o).
        // Hold a usable ≥80 preferred — not steal, not DEAD_OWNER.
        if let Some(pref) = self.preferred_tip_owner() {
            if pref != peer_id
                && self.tip_sticky_usable(&pref)
                && Self::line_rate_or_keep(self.wan_tip_stream_bps(&pref))
            {
                // p @32k: 1.0M unrate-limited SKIPs / 2 min (250 MB log). Hold stays;
                // log at most once per 500ms like WALK_PROMOTE.
                static LAST_HERO_SKIP_MS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let prev = LAST_HERO_SKIP_MS.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 500
                    && LAST_HERO_SKIP_MS
                        .compare_exchange(
                            prev,
                            now,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    tracing::warn!(
                        "[IBD_TIP_WALK_PROMOTE_SKIP] peer={} tip={} — hold ≥80 sticky={} bps={:.1}",
                        peer_id,
                        next_needed,
                        pref,
                        self.wan_tip_stream_bps(&pref)
                    );
                }
                return;
            }
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
        // Genesis-d 180k+: `164.152` shallow-promoted (remain=2–11) with
        // sticky_bps 500–1100 and was *not* restickied → mute lottery + grown=8.
        // A ≥80 tip-streamer is not the W65 mute remnant.
        let hero = Self::line_rate_or_keep(self.wan_tip_stream_bps(peer_id));
        // dest-ap: W65 resticky after cheese H timeout put 107.194 back on
        // the same hole (cooldown was skipped) until LIMITED.
        let cooled = self.tip_owner_in_fail_cooldown(peer_id);
        if (remain >= min_depth || hero) && !cooled {
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
            if remain >= min_depth && !cooled {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE] peer={} span={}-{} → claim={}-{} tip={} — keep GetData as tip owner",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed
                );
            } else if hero && cooled {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE_SHALLOW] peer={} span={}-{} → claim={}-{} tip={} remain={} — skip resticky (cheese H cooldown)",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed,
                    remain
                );
            } else if hero {
                tracing::warn!(
                    "[IBD_TIP_WALK_PROMOTE_SHALLOW] peer={} span={}-{} → claim={}-{} tip={} remain={} — resticky healthy_tip_bps (W65 hero)",
                    peer_id,
                    start,
                    end,
                    next_needed,
                    end,
                    next_needed,
                    remain
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
