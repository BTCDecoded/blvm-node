//! R-326: a body that arrives after its GetData receiver was dropped is
//! admitted into reorder when the hash is in the header index and the
//! height is still inside `[next_needed, next_needed + max_ahead]`.
//!
//! Default **on**. `BLVM_IBD_LATE_BODY_ADMIT=0|false|off|no` restores discard.
//! Network → IBD: unknown hashes are never admitted. The engine still
//! validates block content.

use super::insert_reorder_gap_aware;
use super::latch_env;
use super::types::{SharedBlock, SharedWitnesses};
use super::wire_hash_gate;
use blvm_protocol::segwit::Witness;
use blvm_protocol::{Block, Hash};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::info;

const QUEUE_CAP: usize = 8192;

static QUEUE: Mutex<VecDeque<(u64, SharedBlock, SharedWitnesses)>> = Mutex::new(VecDeque::new());
static MAX_AHEAD: OnceLock<Arc<AtomicU64>> = OnceLock::new();

pub(crate) fn bind_max_ahead(ahead: Arc<AtomicU64>) {
    let _ = MAX_AHEAD.set(ahead);
}

fn max_ahead() -> u64 {
    if let Some(a) = MAX_AHEAD.get() {
        let v = a.load(Ordering::Relaxed);
        if v > 0 {
            return v;
        }
    }
    super::wan_bulk_tip_gap_ahead_cap()
}

/// Default on. `0|false|off|no` disables.
pub(crate) fn enabled() -> bool {
    let on = latch_env!(bool, {
        match std::env::var("BLVM_IBD_LATE_BODY_ADMIT") {
            Ok(v) => {
                let t = v.trim();
                !(t == "0"
                    || t.eq_ignore_ascii_case("false")
                    || t.eq_ignore_ascii_case("off")
                    || t.eq_ignore_ascii_case("no"))
            }
            Err(_) => true,
        }
    });
    #[cfg(not(test))]
    {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            info!("[IBD_LATE_BODY_ADMIT] enabled={on}");
        });
    }
    on
}

fn log_admit(h: u64, peer: SocketAddr, next_needed: u64) {
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let prev = LAST_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 2000 {
        return;
    }
    if LAST_MS
        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    info!("[IBD_LATE_BODY_ADMIT] h={h} peer={peer} next_needed={next_needed} (late arrival used)");
}

/// Admit a late body. `true` means it is queued for the same
/// `insert_reorder_gap_aware` path a normal receive uses.
pub(crate) fn try_admit(
    peer_addr: SocketAddr,
    block_hash: Hash,
    block: Block,
    witnesses: Vec<Vec<Witness>>,
    _wire_payload: Option<Vec<u8>>,
) -> bool {
    if !enabled() {
        return false;
    }
    let Some(h) = wire_hash_gate::lookup_want(block_hash) else {
        return false;
    };
    let Some(next_needed) = wire_hash_gate::current_next_needed() else {
        return false;
    };
    if h < next_needed {
        return false;
    }
    if h > next_needed.saturating_add(max_ahead()) {
        return false;
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= QUEUE_CAP {
        return false;
    }
    q.push_back((h, Arc::new(block), Arc::new(witnesses)));
    drop(q);
    log_admit(h, peer_addr, next_needed);
    true
}

/// Coordinator drain. Present/evict accounting stays in `insert_reorder_gap_aware`.
pub(crate) fn drain_late_admits(
    reorder_buffer: &mut std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_needed: u64,
    buffer_limit: usize,
    window: u64,
    bridge_pending_max: usize,
) {
    let batch: Vec<_> = {
        let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).collect()
    };
    for (h, block, witnesses) in batch {
        let _ = insert_reorder_gap_aware(
            reorder_buffer,
            h,
            block,
            witnesses,
            next_needed,
            buffer_limit,
            window,
            bridge_pending_max,
        );
    }
}

#[cfg(test)]
pub(crate) fn clear_queue_for_test() {
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
pub(crate) fn set_max_ahead_for_test(n: u64) {
    if let Some(a) = MAX_AHEAD.get() {
        a.store(n, Ordering::Relaxed);
    } else {
        let _ = MAX_AHEAD.set(Arc::new(AtomicU64::new(n)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{BlockHeader, Transaction, TransactionOutput};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn dummy_block() -> Block {
        Block {
            header: BlockHeader {
                version: 1,
                timestamp: 1,
                ..Default::default()
            },
            transactions: vec![Transaction {
                version: 1,
                inputs: blvm_protocol::tx_inputs![],
                outputs: blvm_protocol::tx_outputs![TransactionOutput {
                    value: 50,
                    script_pubkey: vec![0x51],
                }],
                lock_time: 0,
            }]
            .into(),
        }
    }

    fn peer() -> SocketAddr {
        "127.0.0.1:8333".parse().unwrap()
    }

    fn arm(hash: Hash, height: u64, synced_tip: u64, ahead: u64) {
        clear_queue_for_test();
        unsafe { std::env::remove_var("BLVM_IBD_LATE_BODY_ADMIT") };
        wire_hash_gate::note_want(hash, height);
        wire_hash_gate::test_set_synced_tip(synced_tip);
        set_max_ahead_for_test(ahead);
    }

    #[test]
    fn r326_needed_height_in_window_is_admitted() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let hash = [0xA1u8; 32];
        arm(hash, 110, 99, 64);
        assert!(try_admit(peer(), hash, dummy_block(), vec![], None));
        let mut reorder = BTreeMap::new();
        drain_late_admits(&mut reorder, 100, 4096, 64, 0);
        assert!(reorder.contains_key(&110), "reorder has h");
        wire_hash_gate::forget_want(hash);
        clear_queue_for_test();
    }

    #[test]
    fn r326_below_next_needed_is_false() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let hash = [0xA2u8; 32];
        arm(hash, 50, 99, 64);
        assert!(!try_admit(peer(), hash, dummy_block(), vec![], None));
        wire_hash_gate::forget_want(hash);
    }

    #[test]
    fn r326_unknown_hash_is_false() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_queue_for_test();
        unsafe { std::env::remove_var("BLVM_IBD_LATE_BODY_ADMIT") };
        wire_hash_gate::test_set_synced_tip(99);
        set_max_ahead_for_test(64);
        let hash = [0xA3u8; 32];
        wire_hash_gate::forget_want(hash);
        assert!(!try_admit(peer(), hash, dummy_block(), vec![], None));
    }

    #[test]
    fn r326_env_zero_is_false() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let hash = [0xA4u8; 32];
        arm(hash, 110, 99, 64);
        unsafe { std::env::set_var("BLVM_IBD_LATE_BODY_ADMIT", "0") };
        assert!(!try_admit(peer(), hash, dummy_block(), vec![], None));
        unsafe { std::env::remove_var("BLVM_IBD_LATE_BODY_ADMIT") };
        wire_hash_gate::forget_want(hash);
    }
}
