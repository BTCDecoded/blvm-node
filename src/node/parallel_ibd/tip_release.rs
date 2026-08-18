//! W6/N14: release-side tip latch (Libbitcoin `chase::windowed` analogue).
//!
//! Tip `GAP_STREAM` must not park on a saturated admission `block_tx` (`send.await`).
//! When the channel is full, latch the tip body so the coordinator can take it before
//! bulk admit — strand/scheduling for admission only, never for tip release.

use super::types::{SharedBlock, SharedWitnesses};
use std::sync::Mutex;

/// One-slot tip release latch. Replaces stale heights with the newest tip offer.
static TIP_RELEASE_LATCH: Mutex<Option<(u64, SharedBlock, SharedWitnesses)>> = Mutex::new(None);

/// Opt-in W6/N14 release-side tip drain (`BLVM_IBD_RELEASE_SIDE_DRAIN=1`).
pub(crate) fn release_side_drain_enabled() -> bool {
    matches!(
        std::env::var("BLVM_IBD_RELEASE_SIDE_DRAIN")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Latch tip body when `block_tx` is full. Returns previous latched height if replaced.
pub(crate) fn offer_tip_release(
    height: u64,
    block: SharedBlock,
    witnesses: SharedWitnesses,
) -> Option<u64> {
    let mut g = TIP_RELEASE_LATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = g.as_ref().map(|(h, _, _)| *h);
    *g = Some((height, block, witnesses));
    prev
}

/// Take latched tip for coordinator admit (before bulk `recv_many`).
pub(crate) fn take_tip_release() -> Option<(u64, SharedBlock, SharedWitnesses)> {
    TIP_RELEASE_LATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// True when the release-side latch already holds `height` (coordinator will admit it).
pub(crate) fn tip_release_holds(height: u64) -> bool {
    TIP_RELEASE_LATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|(h, _, _)| *h == height)
}

/// Test/helpers: clear latch between unit cases.
#[cfg(test)]
pub(crate) fn clear_tip_release_for_test() {
    *TIP_RELEASE_LATCH
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{Block, BlockHeader, Transaction, TransactionOutput};
    use std::sync::Arc;

    fn dummy_block() -> SharedBlock {
        Arc::new(Block {
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
        })
    }

    #[test]
    fn w6_tip_release_latch_offer_take_and_replace() {
        clear_tip_release_for_test();
        let b = dummy_block();
        let w: SharedWitnesses = Arc::new(vec![vec![]]);
        assert!(take_tip_release().is_none());
        assert!(!tip_release_holds(100));
        assert!(offer_tip_release(100, Arc::clone(&b), Arc::clone(&w)).is_none());
        assert!(tip_release_holds(100));
        assert!(!tip_release_holds(101));
        let prev = offer_tip_release(101, Arc::clone(&b), Arc::clone(&w));
        assert_eq!(prev, Some(100));
        assert!(tip_release_holds(101));
        let (h, _, _) = take_tip_release().expect("latched");
        assert_eq!(h, 101);
        assert!(!tip_release_holds(101));
        assert!(take_tip_release().is_none());
        clear_tip_release_for_test();
    }
}
