//! Opinionated IBD implementation policy.
//!
//! Three layers — do not add a fourth:
//!
//! 1. **This file** — tip-hole / sole-peer / gap-persist DNA. Not TOML.
//!    KEEP constant is the default; rematch `BLVM_IBD_*` still overrides
//!    (peel contract). Values are the r29od / peel-go KEEP table (S11 240/205.6).
//! 2. **`[ibd]` TOML** — operator surface: peers, mode, chunk_size, engine opt-out,
//!    `max_blocks_in_transit_per_peer`, timeouts, dump_dir.
//! 3. **Emergency / debug env** — `BLVM_IBD_ENGINE=0`, `BLVM_IBD_DUMP_DIR`,
//!    `BLVM_IBD_JEMALLOC_DUMP`, `BLVM_SERVE_ONLY`, `BLVM_IBD_PEERS` pin.
//!
//! Production: KEEP constant is the default; rematch `BLVM_IBD_*` still overrides
//! (peel contract). `#[cfg(test)]` uses historical unset defaults so the assigner
//! suite does not flip.

#[cfg(test)]
use super::latch_env;

/// One GetData in flight to the tip owner. Dual-slot floods Mode T.
pub const TOP_PEER_IN_FLIGHT: usize = 1;
/// Second after-tip stripe while tip is covered. KEEP off.
pub const TIP_FRONTIER_DUAL: bool = false;
/// Release zombie EMPTY_TIP cover. KEEP off (unset in go.sh).
pub const SOLE_EMPTY_RELEASE: bool = false;
/// Tip-glue / sticky flight clamp. KEEP off.
pub const SOLE_TIP_PRIORITY: bool = false;
/// Second download worker per WAN peer so sticky dual-pipe can arm. KEEP on.
pub const STICKY_DUAL_WORKER: bool = true;
/// Grow tip-hole depth as bodies arrive.
pub const TIP_HOLE_GROW: bool = true;
/// Pipe headroom (slots). KEEP exports 128; historical unset was 32.
pub const TIP_HOLE_PIPE: usize = 128;
/// Cold grow cap. KEEP 32.
pub const TIP_HOLE_GROW_CAP: usize = 32;
/// Fast grow cap when getdata→body EWMA is healthy. KEEP 64; historical unset 48.
pub const TIP_HOLE_GROW_FAST_CAP: usize = 64;
/// Initial tip-hole depth under grow-on-delivery.
pub const TIP_HOLE_GROW_START: usize = 8;
/// Slots added per tip-band body.
pub const TIP_HOLE_GROW_STEP: usize = 8;
/// Carry tip-hole depth across chunks for the same peer. KEEP on; historical unset off.
pub const TIP_HOLE_STICKY: bool = true;
/// Elevate past cold cap only while getdata is fast.
pub const TIP_HOLE_GD_FAST: bool = true;
pub const TIP_HOLE_GD_FAST_MS: u64 = 150;
pub const TIP_HOLE_GD_FAST_N: u64 = 16;
pub const TIP_HOLE_GD_FAST_MIN_H: u64 = 0;
/// Clamp deepen while getdata is slow.
pub const TIP_HOLE_GD_SLOW: bool = true;
pub const TIP_HOLE_GD_SLOW_MS: u64 = 800;
pub const TIP_HOLE_GD_SLOW_N: u64 = 16;
pub const TIP_HOLE_GD_SLOW_RATCHET: bool = false;
/// Sole-peer GD_SLOW floor. KEEP unsets the env → 16.
pub const TIP_HOLE_SOLE_FLOOR: usize = 16;
pub const TIP_HOLE_SOLE_NO_FAST_CLEAR_N: u32 = 16;
pub const TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS: u64 = 120_000;
/// Do not arm no-FAST until this height. KEEP 405000; historical unset 0.
pub const TIP_HOLE_SOLE_NO_FAST_ARM_MIN_H: u64 = 405_000;
/// 0 = always apply sole floor. KEEP unsets.
pub const TIP_HOLE_SOLE_FLOOR_MAX_H: u64 = 0;
/// Warm-peer deepen to pipe cap. KEEP off.
pub const TIP_HOLE_WARM: bool = false;
/// Hashes per getdata. KEEP unsets → 64.
pub const GETDATA_BATCH: usize = 64;
/// Gap-persist experiment family. KEEP unset / off.
pub const GAP_PERSIST_OFFLOAD: bool = false;
pub const GAP_PERSIST_TIP_SYNC: bool = false;
pub const GAP_PERSIST_DEFER_FAR: bool = false;
/// covering=0 idle requeue. KEEP unset → 0 (off).
pub const COVERING0_IDLE_REQUEUE_MS: u64 = 0;

fn test_bool(env: &str, default: bool, explicit_on: bool) -> bool {
    let Ok(raw) = std::env::var(env) else {
        return default;
    };
    let t = raw.trim();
    let off = matches!(t, "0" | "false" | "FALSE" | "off" | "OFF" | "no" | "NO");
    let on = matches!(t, "1" | "true" | "TRUE" | "on" | "ON" | "yes" | "YES");
    if explicit_on {
        if on {
            return true;
        }
        if off {
            return false;
        }
        return default;
    }
    // Default-on: only explicit off disables.
    if off { false } else { default }
}

fn test_usize(env: &str, default: usize, lo: usize, hi: usize) -> usize {
    std::env::var(env)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
        .clamp(lo, hi)
}

fn test_u64(env: &str, default: u64, lo: u64, hi: u64) -> u64 {
    std::env::var(env)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
        .clamp(lo, hi)
}

fn test_u32(env: &str, default: u32, lo: u32, hi: u32) -> u32 {
    std::env::var(env)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
        .clamp(lo, hi)
}

macro_rules! keep_bool {
    ($keep:expr, $test_default:expr, $env:expr, $explicit_on:expr) => {{
        #[cfg(not(test))]
        {
            let _ = $test_default;
            test_bool($env, $keep, $explicit_on)
        }
        #[cfg(test)]
        {
            latch_env!(bool, { test_bool($env, $test_default, $explicit_on) })
        }
    }};
}

macro_rules! keep_usize {
    ($keep:expr, $test_default:expr, $env:expr, $lo:expr, $hi:expr) => {{
        #[cfg(not(test))]
        {
            let _ = $test_default;
            test_usize($env, $keep, $lo, $hi)
        }
        #[cfg(test)]
        {
            latch_env!(usize, { test_usize($env, $test_default, $lo, $hi) })
        }
    }};
}

macro_rules! keep_u64 {
    ($keep:expr, $test_default:expr, $env:expr, $lo:expr, $hi:expr) => {{
        #[cfg(not(test))]
        {
            let _ = $test_default;
            test_u64($env, $keep, $lo, $hi)
        }
        #[cfg(test)]
        {
            latch_env!(u64, { test_u64($env, $test_default, $lo, $hi) })
        }
    }};
}

pub(crate) fn sticky_dual_worker() -> bool {
    keep_bool!(
        STICKY_DUAL_WORKER,
        false,
        "BLVM_IBD_STICKY_DUAL_WORKER",
        true
    )
}

pub(crate) fn sole_tip_priority() -> bool {
    keep_bool!(SOLE_TIP_PRIORITY, false, "BLVM_IBD_SOLE_TIP_PRIORITY", true)
}

pub(crate) fn top_peer_in_flight() -> usize {
    keep_usize!(TOP_PEER_IN_FLIGHT, 2, "BLVM_IBD_TOP_PEER_IN_FLIGHT", 1, 4)
}

pub(crate) fn tip_hole_sticky() -> bool {
    keep_bool!(TIP_HOLE_STICKY, false, "BLVM_IBD_TIP_HOLE_STICKY", true)
}

pub(crate) fn getdata_batch() -> usize {
    keep_usize!(GETDATA_BATCH, 64, "BLVM_IBD_GETDATA_BATCH", 1, 64)
}

pub(crate) fn tip_hole_grow() -> bool {
    keep_bool!(TIP_HOLE_GROW, true, "BLVM_IBD_TIP_HOLE_GROW", false)
}

pub(crate) fn tip_hole_pipe() -> usize {
    keep_usize!(TIP_HOLE_PIPE, 32, "BLVM_IBD_TIP_HOLE_PIPE", 2, 128)
}

pub(crate) fn tip_hole_grow_cap_raw() -> usize {
    keep_usize!(TIP_HOLE_GROW_CAP, 32, "BLVM_IBD_TIP_HOLE_GROW_CAP", 2, 128)
}

pub(crate) fn tip_hole_gd_fast() -> bool {
    keep_bool!(TIP_HOLE_GD_FAST, true, "BLVM_IBD_TIP_HOLE_GD_FAST", false)
}

pub(crate) fn tip_hole_gd_fast_ms() -> u64 {
    keep_u64!(
        TIP_HOLE_GD_FAST_MS,
        150,
        "BLVM_IBD_TIP_HOLE_GD_FAST_MS",
        50,
        800
    )
}

pub(crate) fn tip_hole_gd_fast_n() -> u64 {
    keep_u64!(TIP_HOLE_GD_FAST_N, 16, "BLVM_IBD_TIP_HOLE_GD_FAST_N", 4, 64)
}

pub(crate) fn tip_hole_grow_fast_cap_raw() -> usize {
    keep_usize!(
        TIP_HOLE_GROW_FAST_CAP,
        48,
        "BLVM_IBD_TIP_HOLE_GROW_FAST_CAP",
        2,
        96
    )
}

pub(crate) fn tip_hole_gd_fast_min_h() -> u64 {
    keep_u64!(
        TIP_HOLE_GD_FAST_MIN_H,
        0,
        "BLVM_IBD_TIP_HOLE_GD_FAST_MIN_H",
        0,
        u64::MAX
    )
}

pub(crate) fn tip_hole_gd_slow() -> bool {
    keep_bool!(TIP_HOLE_GD_SLOW, true, "BLVM_IBD_TIP_HOLE_GD_SLOW", false)
}

pub(crate) fn tip_hole_gd_slow_ms() -> u64 {
    keep_u64!(
        TIP_HOLE_GD_SLOW_MS,
        800,
        "BLVM_IBD_TIP_HOLE_GD_SLOW_MS",
        200,
        5_000
    )
}

pub(crate) fn tip_hole_gd_slow_n() -> u64 {
    keep_u64!(TIP_HOLE_GD_SLOW_N, 16, "BLVM_IBD_TIP_HOLE_GD_SLOW_N", 4, 64)
}

pub(crate) fn tip_hole_slow_fill_cap_raw() -> Option<usize> {
    #[cfg(not(test))]
    {
        None
    }
    #[cfg(test)]
    {
        latch_env!(Option<usize>, {
            std::env::var("BLVM_IBD_TIP_HOLE_SLOW_FILL_CAP")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }
}

pub(crate) fn tip_hole_gd_slow_ratchet() -> bool {
    keep_bool!(
        TIP_HOLE_GD_SLOW_RATCHET,
        false,
        "BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET",
        true
    )
}

pub(crate) fn tip_hole_sole_floor() -> usize {
    keep_usize!(
        TIP_HOLE_SOLE_FLOOR,
        16,
        "BLVM_IBD_TIP_HOLE_SOLE_FLOOR",
        2,
        64
    )
}

pub(crate) fn tip_hole_sole_floor_recover_ms_raw() -> Option<u64> {
    #[cfg(not(test))]
    {
        None
    }
    #[cfg(test)]
    {
        latch_env!(Option<u64>, {
            std::env::var("BLVM_IBD_TIP_HOLE_SOLE_FLOOR_RECOVER_MS")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }
}

pub(crate) fn tip_hole_sole_no_fast_clear_n() -> u32 {
    #[cfg(not(test))]
    {
        TIP_HOLE_SOLE_NO_FAST_CLEAR_N
    }
    #[cfg(test)]
    {
        latch_env!(u32, {
            test_u32("BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_CLEAR_N", 16, 4, 64)
        })
    }
}

pub(crate) fn tip_hole_sole_no_fast_min_hold_ms() -> u64 {
    keep_u64!(
        TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS,
        120_000,
        "BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS",
        1_000,
        600_000
    )
}

pub(crate) fn tip_hole_sole_no_fast_arm_min_h() -> u64 {
    keep_u64!(
        TIP_HOLE_SOLE_NO_FAST_ARM_MIN_H,
        0,
        "BLVM_IBD_TIP_HOLE_SOLE_NO_FAST_ARM_MIN_H",
        0,
        u64::MAX
    )
}

pub(crate) fn tip_hole_sole_floor_max_h() -> u64 {
    keep_u64!(
        TIP_HOLE_SOLE_FLOOR_MAX_H,
        0,
        "BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H",
        0,
        u64::MAX
    )
}

pub(crate) fn tip_hole_warm() -> bool {
    keep_bool!(TIP_HOLE_WARM, false, "BLVM_IBD_TIP_HOLE_WARM", true)
}

pub(crate) fn tip_hole_warm_cap_raw() -> Option<usize> {
    #[cfg(not(test))]
    {
        None
    }
    #[cfg(test)]
    {
        latch_env!(Option<usize>, {
            std::env::var("BLVM_IBD_TIP_HOLE_WARM_CAP")
                .ok()
                .and_then(|s| s.parse().ok())
        })
    }
}

pub(crate) fn tip_hole_grow_start() -> usize {
    keep_usize!(
        TIP_HOLE_GROW_START,
        8,
        "BLVM_IBD_TIP_HOLE_GROW_START",
        2,
        32
    )
}

pub(crate) fn tip_hole_grow_step() -> usize {
    keep_usize!(TIP_HOLE_GROW_STEP, 8, "BLVM_IBD_TIP_HOLE_GROW_STEP", 2, 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_table_is_r29od_peel() {
        assert_eq!(TOP_PEER_IN_FLIGHT, 1);
        assert!(!TIP_FRONTIER_DUAL);
        assert!(!SOLE_EMPTY_RELEASE);
        assert!(!SOLE_TIP_PRIORITY);
        assert!(STICKY_DUAL_WORKER);
        assert_eq!(TIP_HOLE_PIPE, 128);
        assert_eq!(TIP_HOLE_GROW_CAP, 32);
        assert_eq!(TIP_HOLE_GROW_FAST_CAP, 64);
        assert_eq!(TIP_HOLE_GROW_START, 8);
        assert!(TIP_HOLE_STICKY);
        assert!(TIP_HOLE_GD_FAST);
        assert_eq!(TIP_HOLE_GD_FAST_MS, 150);
        assert_eq!(TIP_HOLE_GD_FAST_N, 16);
        assert_eq!(TIP_HOLE_SOLE_NO_FAST_MIN_HOLD_MS, 120_000);
        assert_eq!(TIP_HOLE_SOLE_NO_FAST_ARM_MIN_H, 405_000);
        assert!(!GAP_PERSIST_OFFLOAD);
        assert!(!GAP_PERSIST_TIP_SYNC);
        assert!(!GAP_PERSIST_DEFER_FAR);
        assert_eq!(COVERING0_IDLE_REQUEUE_MS, 0);
        assert_eq!(GETDATA_BATCH, 64);
    }
}
