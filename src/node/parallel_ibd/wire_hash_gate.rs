//! R-304: hash the 80-byte header before `deserialize_block_with_witnesses`.
//!
//! Default **off** (`BLVM_IBD_WIRE_HASH_GATE` unset) = byte-for-byte current
//! parse. When on, skip deserialize only when this hash is an IBD GetData we
//! issued **and** its height is already below `next_needed` (the
//! `already_validated` stream). Unknown / relay hashes always parse.

use crate::node::parallel_ibd::latch_env;
use crate::storage::hashing::double_sha256;
use dashmap::DashMap;
use dashmap::DashSet;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

struct Acc {
    n: AtomicU64,
    bytes: AtomicU64,
}

impl Acc {
    const fn new() -> Self {
        Self {
            n: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
    fn add(&self, bytes: u64) {
        self.n.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    fn get(&self) -> (u64, u64) {
        (
            self.n.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

struct State {
    want: DashMap<[u8; 32], u64>,
    skipped: DashSet<[u8; 32]>,
    skip: Acc,
    unknown: Acc,
    still_needed: Acc,
}

impl State {
    fn new() -> Self {
        Self {
            want: DashMap::new(),
            skipped: DashSet::new(),
            skip: Acc::new(),
            unknown: Acc::new(),
            still_needed: Acc::new(),
        }
    }
}

fn state() -> &'static State {
    static S: OnceLock<State> = OnceLock::new();
    S.get_or_init(State::new)
}

static NEXT: OnceLock<Arc<AtomicU64>> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_WIRE_HASH_GATE") {
            Ok(v) => {
                let t = v.trim();
                t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on")
            }
            Err(_) => false,
        }
    })
}

/// IBD start: live `validation_height` so the gate can read `next_needed`.
pub(crate) fn bind_validation_height(h: Arc<AtomicU64>) {
    let _ = NEXT.set(h);
    if enabled() {
        info!(
            "[IBD_WIRE_HASH_GATE] on — skip deserialize when IBD GetData height < next_needed"
        );
    }
}

fn next_needed() -> Option<u64> {
    NEXT.get()
        .map(|a| a.load(Ordering::Relaxed).saturating_add(1))
}

/// Record an IBD GetData (hash → height) so a late frame can be recognized.
pub(crate) fn note_want(hash: [u8; 32], height: u64) {
    state().want.insert(hash, height);
}

pub(crate) fn forget_want(hash: [u8; 32]) {
    state().want.remove(&hash);
}

/// Height recorded for an IBD GetData hash. `None` = not in our header index.
pub(crate) fn lookup_want(hash: [u8; 32]) -> Option<u64> {
    state().want.get(&hash).map(|e| *e)
}

pub(crate) fn current_next_needed() -> Option<u64> {
    next_needed()
}

#[cfg(test)]
pub(crate) fn test_set_synced_tip(tip: u64) {
    if let Some(a) = NEXT.get() {
        a.store(tip, Ordering::Relaxed);
    } else {
        let _ = NEXT.set(Arc::new(AtomicU64::new(tip)));
    }
}

/// True if the hash-gate closed this oneshot on purpose (fetch must not abort).
pub(crate) fn was_skipped(hash: [u8; 32]) -> bool {
    state().skipped.contains(&hash)
}

pub(crate) fn skip_n() -> u64 {
    state().skip.get().0
}

/// Bitcoin frame: magic 4 + cmd 12 + len 4 + checksum 4 = 24, then payload.
/// Payload starts with the 80-byte header; hash is double-SHA256 of those 80 bytes.
pub(crate) fn try_skip_obsolete_block_frame(data: &[u8]) -> Option<[u8; 32]> {
    if !enabled() {
        return None;
    }
    if data.len() < 24 + 80 {
        return None;
    }
    let payload_len = u32::from_le_bytes([data[16], data[17], data[18], data[19]]) as usize;
    if payload_len < 80 {
        return None;
    }
    let header = &data[24..24 + 80];
    let hash = double_sha256(header);
    let frame_b = data.len() as u64;
    let Some(h) = state().want.get(&hash).map(|e| *e) else {
        // Unsolicited / relay / unmatched: must parse. Count, do not drop.
        state().unknown.add(frame_b);
        return None;
    };
    let Some(need) = next_needed() else {
        // Gate on but IBD not bound: count-and-parse, never drop.
        state().unknown.add(frame_b);
        return None;
    };
    if h >= need {
        state().still_needed.add(frame_b);
        return None;
    }
    let st = state();
    st.skip.add(frame_b);
    st.skipped.insert(hash);
    st.want.remove(&hash);
    Some(hash)
}

#[derive(Clone, Copy, Default)]
struct Snap {
    skip_n: u64,
    skip_b: u64,
    unknown_n: u64,
    unknown_b: u64,
    still_n: u64,
    still_b: u64,
}

fn snap_now() -> Snap {
    let st = state();
    let (skip_n, skip_b) = st.skip.get();
    let (unknown_n, unknown_b) = st.unknown.get();
    let (still_n, still_b) = st.still_needed.get();
    Snap {
        skip_n,
        skip_b,
        unknown_n,
        unknown_b,
        still_n,
        still_b,
    }
}

fn emit_one(tag: &str, h: u64, s: Snap) {
    info!(
        "[IBD_WIRE_HASH_GATE] {} h={} skip_n={} skip_B={} unknown_n={} unknown_B={} still_needed_n={} still_needed_B={}",
        tag, h, s.skip_n, s.skip_b, s.unknown_n, s.unknown_b, s.still_n, s.still_b,
    );
}

pub(crate) fn emit(h: u64) {
    static LAST: OnceLock<std::sync::Mutex<Snap>> = OnceLock::new();
    let last = LAST.get_or_init(|| std::sync::Mutex::new(Snap::default()));
    let now = snap_now();
    let win = {
        let mut g = last.lock().unwrap_or_else(|e| e.into_inner());
        let w = Snap {
            skip_n: now.skip_n.saturating_sub(g.skip_n),
            skip_b: now.skip_b.saturating_sub(g.skip_b),
            unknown_n: now.unknown_n.saturating_sub(g.unknown_n),
            unknown_b: now.unknown_b.saturating_sub(g.unknown_b),
            still_n: now.still_n.saturating_sub(g.still_n),
            still_b: now.still_b.saturating_sub(g.still_b),
        };
        *g = now;
        w
    };
    emit_one("win", h, win);
    emit_one("cum", h, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn block_frame(hdr80: [u8; 80]) -> Vec<u8> {
        let mut v = vec![0u8; 24];
        v[4..9].copy_from_slice(b"block");
        v[16..20].copy_from_slice(&80u32.to_le_bytes());
        v.extend_from_slice(&hdr80);
        v
    }

    fn tip() -> Arc<AtomicU64> {
        static T: OnceLock<Arc<AtomicU64>> = OnceLock::new();
        T.get_or_init(|| {
            let a = Arc::new(AtomicU64::new(0));
            bind_validation_height(Arc::clone(&a));
            a
        })
        .clone()
    }

    #[test]
    fn r304_hash_gate_defaults_off_so_unset_is_current_parse() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("BLVM_IBD_WIRE_HASH_GATE") };
        assert!(
            !enabled(),
            "R-304: BLVM_IBD_WIRE_HASH_GATE unset must parse every frame; got on"
        );
        let mut hdr = [0u8; 80];
        hdr[0] = 0xA4;
        let hash = double_sha256(&hdr);
        let _ = tip();
        note_want(hash, 1);
        tip().store(10_000, Ordering::Relaxed);
        assert!(
            try_skip_obsolete_block_frame(&block_frame(hdr)).is_none(),
            "default-off must not skip even an obsolete pending hash"
        );
        forget_want(hash);
    }

    #[test]
    fn r304_skips_when_pending_getdata_height_already_below_next_needed() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("BLVM_IBD_WIRE_HASH_GATE", "1") };
        let mut hdr = [0u8; 80];
        hdr[0] = 0xB4;
        let hash = double_sha256(&hdr);
        let _ = tip();
        note_want(hash, 50);
        tip().store(100, Ordering::Relaxed);
        let got = try_skip_obsolete_block_frame(&block_frame(hdr));
        unsafe { std::env::remove_var("BLVM_IBD_WIRE_HASH_GATE") };
        assert_eq!(got, Some(hash), "obsolete pending GetData must skip parse");
        assert!(was_skipped(hash), "fetch RecvError must see the skip");
        forget_want(hash);
    }

    #[test]
    fn r304_does_not_skip_pending_hash_whose_height_is_still_needed() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("BLVM_IBD_WIRE_HASH_GATE", "1") };
        let mut hdr = [0u8; 80];
        hdr[0] = 0xD4;
        let hash = double_sha256(&hdr);
        let _ = tip();
        note_want(hash, 200);
        tip().store(50, Ordering::Relaxed);
        let got = try_skip_obsolete_block_frame(&block_frame(hdr));
        unsafe { std::env::remove_var("BLVM_IBD_WIRE_HASH_GATE") };
        assert!(
            got.is_none(),
            "still-needed pending GetData must parse"
        );
        forget_want(hash);
    }

    #[test]
    fn r304_does_not_skip_unknown_hash_relay_still_parses() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("BLVM_IBD_WIRE_HASH_GATE", "1") };
        let mut hdr = [0u8; 80];
        hdr[0] = 0xC4;
        let _ = tip();
        tip().store(100, Ordering::Relaxed);
        let got = try_skip_obsolete_block_frame(&block_frame(hdr));
        unsafe { std::env::remove_var("BLVM_IBD_WIRE_HASH_GATE") };
        assert!(
            got.is_none(),
            "unknown/relay hash must still parse — a wrong drop stalls IBD"
        );
    }

    #[test]
    fn r304_want_map_from_batch_getdata_is_what_the_gate_looks_up() {
        // IBD fill uses enqueue_network_block_batch, not register_and_request_block.
        // The gate only sees hashes note_want recorded. The batch path must call it.
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("BLVM_IBD_WIRE_HASH_GATE", "1") };
        let mut hdr = [0u8; 80];
        hdr[0] = 0xE4;
        let hash = double_sha256(&hdr);
        let _ = tip();
        note_want(hash, 80);
        tip().store(200, Ordering::Relaxed);
        let got = try_skip_obsolete_block_frame(&block_frame(hdr));
        unsafe { std::env::remove_var("BLVM_IBD_WIRE_HASH_GATE") };
        assert_eq!(got, Some(hash));
        forget_want(hash);
    }
}
