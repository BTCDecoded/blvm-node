//! Synthetic WAN IBD harness — snapshot bodies, fake peers, no real P2P.
//!
//! Lab/dens synth only; Mode T rematch must leave unset.
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

/// Simulated getdata→body latency per block (0 = instant disk load).
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

/// Whether download workers should use fake WAN peer ids (assigner multi-peer / tip-crawl).
///
/// Bulk baseline (`delay=0`, single peer, no force): use `local-disk` stream path — same
/// bodies, without WAN tip-owner / ahead-flood that caps wall BPS at ~6–8 (2026-07-23).
/// Tip-crawl soak: `BLVM_IBD_SYNTH_WAN_FORCE_PEERS=1`, `PEER_COUNT>=2`, or `GETDATA_DELAY_MS>0`.
pub fn use_fake_download_peers() -> bool {
    if !enabled() {
        return false;
    }
    if getdata_delay_ms() > 0 {
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

    static SYNTH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn synth_peer_ids_are_non_lan_wan_addrs() {
        let _lock = SYNTH_ENV_LOCK.lock().unwrap();
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

    #[test]
    fn bulk_delay_zero_single_peer_uses_local_disk_stream() {
        // Serialize env mutations — parallel synth tests share process env.
        let _lock = SYNTH_ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN", "1") };
        unsafe { std::env::set_var("BLVM_IBD_SYNTH_WAN_PEER_COUNT", "1") };
        unsafe { std::env::remove_var("BLVM_IBD_SYNTH_GETDATA_DELAY_MS") };
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
    }
}
