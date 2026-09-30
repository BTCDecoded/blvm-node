//! Synthetic WAN IBD harness — snapshot bodies, fake peers, no real P2P.
//!
//! **Compile gate:** linked only with `feature = "ibd-dev"` or `cfg(test)`.
//! Production binaries compile `synthetic_wan_stub.rs` under the same module name;
//! `BLVM_IBD_SYNTH_WAN` is then a no-op.
//!
//! Lab/dens synth only; Mode T rematch: `--features ibd-dev` and leave env unset
//! unless the cell is a synth soak.
//!
//! Enable with `BLVM_IBD_SYNTH_WAN=1`. Bodies load from disk (like `local-disk`) but
//! `wan_body_tip` can be pinned below stored bodies so assigner tip-crawl / multi-peer
//! paths run as in WAN soak.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

pub fn enabled() -> bool {
    std::env::var("BLVM_IBD_SYNTH_WAN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Synthetic harness implies zero real peers unless explicitly disabled.
pub fn allow_zero_real_peers() -> bool {
    enabled()
        || std::env::var("BLVM_IBD_ALLOW_ZERO_PEERS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Pin WAN body tip below on-disk bodies so `wan_tip_gap_crawl` activates for the band.
pub fn body_tip_override() -> Option<u64> {
    if !enabled() {
        return None;
    }
    std::env::var("BLVM_IBD_SYNTH_WAN_BODY_TIP")
        .ok()
        .and_then(|s| s.parse().ok())
}

pub fn peer_count() -> usize {
    if !enabled() {
        return 0;
    }
    std::env::var("BLVM_IBD_SYNTH_WAN_PEER_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(1, 16)
}

/// RFC 5737 TEST-NET-3 addresses — parse as normal peer SocketAddrs (WAN multi-peer).
pub fn peer_ids() -> Vec<String> {
    if !enabled() {
        return Vec::new();
    }
    (0..peer_count())
        .map(|i| {
            let octet = (i + 1).min(254);
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, octet as u8)), 8333).to_string()
        })
        .collect()
}

pub fn is_synthetic_peer(peer_id: &str) -> bool {
    if !enabled() {
        return false;
    }
    let Ok(addr) = peer_id.parse::<SocketAddr>() else {
        return false;
    };
    matches!(
        addr.ip(),
        IpAddr::V4(v4) if v4.octets()[0] == 203 && v4.octets()[1] == 0 && v4.octets()[2] == 113
    )
}

/// dest-1 / dest-ak class: 2–3 ms body IA → ~305–370 wall BPS.
pub const REPLAY_180_220K_HERO: &str = "203.0.113.1:8333";
pub const REPLAY_180_220K_HERO_MS: u64 = 3;
/// Drip class: 23–27 ms IA → ~25–67 wall BPS. Probe wave is still ≥80 (`32/0.025s`).
pub const REPLAY_180_220K_DRIP: &str = "203.0.113.2:8333";
pub const REPLAY_180_220K_DRIP_MS: u64 = 25;
/// Stall: 800 ms sojourn → wave 40 ≺ 80 (not `probe_keep_hero`).
pub const REPLAY_180_220K_STALL: &str = "203.0.113.3:8333";
pub const REPLAY_180_220K_STALL_MS: u64 = 800;

/// Named replay fixture (`BLVM_IBD_SYNTH_REPLAY=180-220k`).
pub fn replay_fixture() -> Option<&'static str> {
    if !enabled() {
        return None;
    }
    let v = std::env::var("BLVM_IBD_SYNTH_REPLAY").ok()?;
    match v.trim() {
        "180-220k" | "180_220k" | "180220k" => Some("180-220k"),
        _ => None,
    }
}

fn peer_delay_overrides() -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    let Ok(s) = std::env::var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS") else {
        return out;
    };
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((peer, ms)) = part.rsplit_once('=') else {
            continue;
        };
        let Ok(ms) = ms.trim().parse::<u64>() else {
            continue;
        };
        out.insert(peer.trim().to_string(), ms.min(30_000));
    }
    out
}

/// Simulated getdata→body latency per block (0 = instant disk load). Global default.
pub fn getdata_delay_ms() -> u64 {
    if !enabled() {
        return 0;
    }
    std::env::var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        .min(30_000)
}

/// Per-peer GetData IA. Overlay env beats the named 180–220k fixture; else global.
pub fn getdata_delay_ms_for_peer(peer_id: &str) -> u64 {
    if !enabled() {
        return 0;
    }
    if let Some(&ms) = peer_delay_overrides().get(peer_id) {
        return ms;
    }
    if replay_fixture() == Some("180-220k") {
        return match peer_id {
            REPLAY_180_220K_HERO => REPLAY_180_220K_HERO_MS,
            REPLAY_180_220K_DRIP => REPLAY_180_220K_DRIP_MS,
            REPLAY_180_220K_STALL => REPLAY_180_220K_STALL_MS,
            _ => getdata_delay_ms().max(50),
        };
    }
    getdata_delay_ms()
}

/// True when synth injects GetData IA (global, per-peer overlay, or named replay).
pub fn injected_ia() -> bool {
    if !enabled() {
        return false;
    }
    getdata_delay_ms() > 0 || replay_fixture().is_some() || !peer_delay_overrides().is_empty()
}

/// Whether download workers should use fake WAN peer ids (assigner multi-peer / tip-crawl).
///
/// Bulk baseline (`delay=0`, single peer, no force): use `local-disk` stream path — same
/// bodies, without WAN tip-owner / ahead-flood that caps wall BPS at ~6–8 (2026-07-23).
/// Tip-crawl soak: `BLVM_IBD_SYNTH_WAN_FORCE_PEERS=1`, `PEER_COUNT>=2`, or `GETDATA_DELAY_MS>0`.
pub fn use_fake_download_peers() -> bool {
    if !enabled() {
        return false;
    }
    if injected_ia() {
        return true;
    }
    if std::env::var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return true;
    }
    peer_count() >= 2
}

/// Bulk synth on the `local-disk` stream path (not fake multi-peer tip-crawl).
///
/// When true: obsolete clears sticky without `TIP_OWNER_OPEN`. Tip-only LOCAL_GAP,
/// `LOCAL_GAP_FILL=0`, tip-owner cooldown, and keep-claim-after-complete all tip-crawled
/// or hard-stalled ~3–9 wall BPS. Best complete band remains no-OPEN + full LOCAL_GAP
/// (300→350 wall ~371 / 350→400 ~178).
pub fn bulk_local_disk_stream() -> bool {
    enabled() && !use_fake_download_peers()
}

/// Resolve live WAN body tip for assigner/coordinator (override wins when set).
pub fn effective_wan_body_tip(live_tip: u64) -> u64 {
    body_tip_override().unwrap_or(live_tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::peer_scoring::is_lan_peer;

    #[serial_test::serial(ibd)]
    #[test]
    fn synth_peer_ids_are_non_lan_wan_addrs() {
        let _lock = crate::ibd_test_lock::guard();
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN", "1") };
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "3") };
        let peers = peer_ids();
        assert_eq!(peers.len(), 3);
        for p in &peers {
            assert!(is_synthetic_peer(p));
            let addr: SocketAddr = p.parse().expect("parse");
            assert!(!is_lan_peer(&addr));
        }
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT") };
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn bulk_delay_zero_single_peer_uses_local_disk_stream() {
        // Serialize env mutations — parallel synth tests share process env.
        let _lock = crate::ibd_test_lock::guard();
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN", "1") };
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_REPLAY") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS") };
        assert!(!use_fake_download_peers());
        assert!(bulk_local_disk_stream());
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "4") };
        assert!(use_fake_download_peers());
        assert!(!bulk_local_disk_stream());
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1") };
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS", "1") };
        assert!(use_fake_download_peers());
        assert!(!bulk_local_disk_stream());
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN_FORCE_PEERS") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_REPLAY") };
    }

    #[serial_test::serial(ibd)]
    #[test]
    fn replay_180_220k_injects_per_peer_ia() {
        // Per-peer GetData IA only. Checkpoint export at 180000 is
        // dest_bl_180k_sit_must_not_clear_w75_burst_ema +
        // dest_bl_180k_export_hold_restore_must_keep_preferred_hh.
        // Do not `wan-bench-local-replay.sh restore` while /mnt/data is 15G free.
        let _lock = crate::ibd_test_lock::guard();
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN", "1") };
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_REPLAY", "180-220k") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS") };
        assert_eq!(replay_fixture(), Some("180-220k"));
        assert!(injected_ia());
        assert!(use_fake_download_peers());
        assert!(!bulk_local_disk_stream());
        assert_eq!(
            getdata_delay_ms_for_peer(REPLAY_180_220K_HERO),
            REPLAY_180_220K_HERO_MS
        );
        assert_eq!(
            getdata_delay_ms_for_peer(REPLAY_180_220K_DRIP),
            REPLAY_180_220K_DRIP_MS
        );
        assert_eq!(
            getdata_delay_ms_for_peer(REPLAY_180_220K_STALL),
            REPLAY_180_220K_STALL_MS
        );
        // Overlay beats the named fixture (dest-x cheese pin vs drip).
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS", "203.0.113.2:8333=2") };
        assert_eq!(getdata_delay_ms_for_peer(REPLAY_180_220K_DRIP), 2);
        assert_eq!(
            getdata_delay_ms_for_peer(REPLAY_180_220K_HERO),
            REPLAY_180_220K_HERO_MS
        );
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_WAN") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_REPLAY") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_PEER_MS") };
    }
}
