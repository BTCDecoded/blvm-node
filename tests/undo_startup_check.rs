//! Nodes synced before the parallel-IBD undo fix stored empty undo logs. Startup must say
//! so (with what to do), and a reorg that would disconnect such a block must be refused
//! rather than silently corrupting the UTXO set.
#![cfg(feature = "production")]

use blvm_consensus::reorganization::{BlockUndoLog, UndoEntry};
use blvm_node::node::undo_check::{
    STARTUP_UNDO_CHECK_DEPTH, check_recent_undo_at_startup, ensure_disconnect_undo_usable,
    recent_heights_with_unusable_undo, startup_message,
};
use blvm_node::storage::Storage;
use blvm_protocol::{BlockHeader, Hash, OutPoint, UTXO};
use std::sync::Arc;
use tempfile::TempDir;

fn hash_at(height: u64) -> Hash {
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&height.to_le_bytes());
    h[31] = 0xab;
    h
}

/// What the sequential path writes for a coinbase-only block: one created output.
fn good_undo(height: u64) -> BlockUndoLog {
    BlockUndoLog {
        entries: vec![UndoEntry {
            outpoint: OutPoint {
                hash: hash_at(height),
                index: 0,
            },
            previous_utxo: None,
            new_utxo: Some(Arc::new(UTXO {
                value: 50_0000_0000,
                height,
                is_coinbase: true,
                ..Default::default()
            })),
        }],
    }
}

enum Undo {
    Good,
    Empty,
    Missing,
}

/// Chain 0..=tip with the given undo state per height (genesis has no undo).
fn chain(dir: &TempDir, tip: u64, undo: impl Fn(u64) -> Undo) -> Storage {
    let storage = Storage::new(dir.path()).unwrap();
    let blocks = storage.blocks();
    let header = |h: u64| BlockHeader {
        version: 4,
        prev_block_hash: if h == 0 { [0; 32] } else { hash_at(h - 1) },
        merkle_root: [0; 32],
        timestamp: 1_300_000_000 + h,
        bits: 0x207fffff,
        nonce: 0,
    };
    let headers: Vec<_> = (0..=tip).map(|h| (hash_at(h), header(h), h)).collect();
    blocks.store_headers_batch(&headers).unwrap();
    for h in 0..=tip {
        storage
            .chain()
            .update_tip(&hash_at(h), &header(h), h)
            .unwrap();
    }
    for h in 1..=tip {
        match undo(h) {
            Undo::Good => blocks.store_undo_log(&hash_at(h), &good_undo(h)).unwrap(),
            Undo::Empty => blocks
                .store_undo_log(&hash_at(h), &BlockUndoLog::new())
                .unwrap(),
            Undo::Missing => {}
        }
    }
    storage
}

#[test]
fn healthy_node_passes_startup_check() {
    let dir = TempDir::new().unwrap();
    let storage = chain(&dir, 120, |_| Undo::Good);
    assert!(check_recent_undo_at_startup(&storage).is_empty());
}

/// Synced by parallel IBD on the old code: every undo log is empty.
#[test]
fn old_parallel_ibd_node_is_flagged_at_startup() {
    let dir = TempDir::new().unwrap();
    let storage = chain(&dir, 120, |_| Undo::Empty);
    let bad = check_recent_undo_at_startup(&storage);
    assert_eq!(bad.len() as u64, STARTUP_UNDO_CHECK_DEPTH);
    assert_eq!(bad.first(), Some(&120));
    assert_eq!(bad.last(), Some(&21));
}

/// Old IBD, then a few blocks connected at the tip by the (fixed) sequential path.
#[test]
fn reports_only_the_affected_heights() {
    let dir = TempDir::new().unwrap();
    let storage = chain(&dir, 30, |h| match h {
        26..=30 => Undo::Good,
        25 => Undo::Missing,
        _ => Undo::Empty,
    });
    let bad = recent_heights_with_unusable_undo(&storage.blocks(), 30, 10);
    assert_eq!(bad, vec![25, 24, 23, 22, 21]);
    // Genesis is never checked; a short chain checks what it has.
    let all = recent_heights_with_unusable_undo(&storage.blocks(), 30, 1000);
    assert_eq!(all.len(), 25);
    assert!(!all.contains(&0));
}

#[test]
fn operator_message_says_what_happened_and_what_to_do() {
    let msg = startup_message(&[25, 24, 21], 30, 100);
    for needle in [
        "missing or empty for 3 of the last 30 blocks",
        "heights 21..=25",
        "refuse any reorg",
        "resync",
        "data directory",
    ] {
        assert!(msg.contains(needle), "message lacks {needle:?}: {msg}");
    }
}

#[test]
fn reorg_over_empty_undo_is_refused() {
    let dir = TempDir::new().unwrap();
    let storage = chain(&dir, 30, |h| if h >= 29 { Undo::Good } else { Undo::Empty });
    let blocks = storage.blocks();
    // Disconnecting 29..=30 is fine.
    ensure_disconnect_undo_usable(&blocks, &[(29, hash_at(29)), (30, hash_at(30))]).unwrap();
    // Disconnecting 28..=30 hits an empty undo log at 28.
    let err = ensure_disconnect_undo_usable(
        &blocks,
        &[(28, hash_at(28)), (29, hash_at(29)), (30, hash_at(30))],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("Refusing reorg of depth 3"), "{err}");
    assert!(err.contains("height 28"), "{err}");
    assert!(err.contains("resync"), "{err}");
}

#[test]
fn reorg_over_missing_undo_is_refused() {
    let dir = TempDir::new().unwrap();
    let storage = chain(&dir, 5, |h| if h == 5 { Undo::Missing } else { Undo::Good });
    assert!(ensure_disconnect_undo_usable(&storage.blocks(), &[(5, hash_at(5))]).is_err());
}
