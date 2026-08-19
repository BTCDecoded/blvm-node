//! Production stub: synthetic WAN harness is compiled out.
//!
//! Real implementation lives in `synthetic_wan.rs` and is only linked when
//! `feature = "ibd-dev"` or `cfg(test)`. Env vars such as `BLVM_IBD_SYNTH_WAN`
//! are ignored in default production binaries.

pub fn enabled() -> bool {
    false
}

pub fn allow_zero_real_peers() -> bool {
    false
}

pub fn body_tip_override() -> Option<u64> {
    None
}

pub fn peer_count() -> usize {
    0
}

pub fn peer_ids() -> Vec<String> {
    Vec::new()
}

pub fn is_synthetic_peer(_peer_id: &str) -> bool {
    false
}

pub fn getdata_delay_ms() -> u64 {
    0
}

pub fn use_fake_download_peers() -> bool {
    false
}

pub fn bulk_local_disk_stream() -> bool {
    false
}

pub fn effective_wan_body_tip(live_tip: u64) -> u64 {
    live_tip
}
