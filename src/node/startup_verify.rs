//! Startup check of the last [`STARTUP_VERIFY_DEPTH`] blocks.
//!
//! The check runs only when each of those bodies and its undo record is present.
//! A store that no longer has them still starts. A present body whose undo does
//! not match that body is reported. This function does not write a replacement.

use anyhow::{Result, bail};
use blvm_consensus::reorganization::BlockUndoLog;
use blvm_protocol::{Block, OutPoint};

use crate::storage::Storage;

pub const STARTUP_VERIFY_DEPTH: u64 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupVerifyOutcome {
    Passed { blocks: u64 },
    SkippedNoBlockData { height: u64 },
}

/// Check the tip window. Missing bodies or undo records skip. A mismatch is an error.
pub fn verify_startup_blocks(storage: &Storage) -> Result<StartupVerifyOutcome> {
    #[cfg(not(feature = "production"))]
    {
        let _ = storage;
        return Ok(StartupVerifyOutcome::Passed { blocks: 0 });
    }
    #[cfg(feature = "production")]
    {
        verify_production(storage)
    }
}

#[cfg(feature = "production")]
fn verify_production(storage: &Storage) -> Result<StartupVerifyOutcome> {
    let Some(tip) = storage.chain().get_height()? else {
        return Ok(StartupVerifyOutcome::Passed { blocks: 0 });
    };
    let start = tip.saturating_sub(STARTUP_VERIFY_DEPTH - 1);
    let blocks = storage.blocks();
    let mut window = Vec::new();
    for height in start..=tip {
        let Some(hash) = blocks.get_hash_by_height(height)? else {
            return skip(height);
        };
        let Some(block) = blocks.get_block(&hash)? else {
            return skip(height);
        };
        let Some(undo) = blocks.get_undo_log(&hash)? else {
            return skip(height);
        };
        window.push((height, hash, block, undo));
    }
    for (height, hash, block, undo) in &window {
        if !undo_applies(block, *height, undo) {
            bail!(
                "undo for block {} at height {height} does not apply",
                hex::encode(hash)
            );
        }
    }
    Ok(StartupVerifyOutcome::Passed {
        blocks: window.len() as u64,
    })
}

fn skip(height: u64) -> Result<StartupVerifyOutcome> {
    tracing::info!(
        "startup verification skipped blocks this node does not hold at height {height}"
    );
    Ok(StartupVerifyOutcome::SkippedNoBlockData { height })
}

fn undo_applies(block: &Block, height: u64, undo: &BlockUndoLog) -> bool {
    let mut expected = Vec::new();
    for tx in block.transactions.iter() {
        let tx_id = blvm_consensus::block::calculate_tx_id(tx);
        let coinbase = blvm_consensus::transaction::is_coinbase(tx);
        if !coinbase {
            for input in tx.inputs.iter() {
                expected.push(Expect::Spend(input.prevout));
            }
        }
        for (index, output) in tx.outputs.iter().enumerate() {
            expected.push(Expect::Create {
                outpoint: OutPoint {
                    hash: tx_id,
                    index: index as u32,
                },
                value: output.value,
                script: output.script_pubkey.as_slice().to_vec(),
                coinbase,
            });
        }
    }
    if undo.entries.len() != expected.len() {
        return false;
    }
    for (entry, expect) in undo.entries.iter().rev().zip(expected.iter()) {
        match expect {
            Expect::Spend(outpoint) => {
                if &entry.outpoint != outpoint
                    || entry.previous_utxo.is_none()
                    || entry.new_utxo.is_some()
                {
                    return false;
                }
            }
            Expect::Create {
                outpoint,
                value,
                script,
                coinbase,
            } => {
                let Some(created) = entry.new_utxo.as_ref() else {
                    return false;
                };
                if &entry.outpoint != outpoint
                    || entry.previous_utxo.is_some()
                    || created.value != *value
                    || created.height != height
                    || created.is_coinbase != *coinbase
                    || created.script_pubkey.as_ref() != script.as_slice()
                {
                    return false;
                }
            }
        }
    }
    true
}

enum Expect {
    Spend(OutPoint),
    Create {
        outpoint: OutPoint,
        value: i64,
        script: Vec<u8>,
        coinbase: bool,
    },
}

#[cfg(all(test, feature = "production"))]
mod tests {
    use super::{STARTUP_VERIFY_DEPTH, StartupVerifyOutcome, verify_startup_blocks};
    use crate::storage::Storage;
    use crate::storage::database::default_backend;
    use blvm_consensus::reorganization::{BlockUndoLog, undo_log_from_connected_block};

    fn empty_store() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::with_backend(dir.path(), default_backend()).unwrap();
        (dir, storage)
    }

    fn genesis() -> blvm_protocol::Block {
        blvm_consensus::block::genesis_block(blvm_protocol::types::Network::Regtest)
    }

    #[test]
    fn depth_is_six() {
        assert_eq!(STARTUP_VERIFY_DEPTH, 6);
    }

    #[test]
    fn an_empty_store_passes() {
        let (_dir, storage) = empty_store();
        assert_eq!(
            verify_startup_blocks(&storage).unwrap(),
            StartupVerifyOutcome::Passed { blocks: 0 }
        );
    }

    #[test]
    fn a_tip_without_bodies_still_starts() {
        let (_dir, storage) = empty_store();
        let genesis = genesis();
        let hash = storage.blocks().get_block_hash(&genesis);
        storage
            .chain()
            .update_tip(&hash, &genesis.header, 0)
            .unwrap();
        assert_eq!(
            verify_startup_blocks(&storage).unwrap(),
            StartupVerifyOutcome::SkippedNoBlockData { height: 0 }
        );
    }

    #[test]
    fn a_body_without_undo_still_starts() {
        let (_dir, storage) = empty_store();
        let genesis = genesis();
        let hash = storage.blocks().get_block_hash(&genesis);
        storage.blocks().store_block(&genesis).unwrap();
        storage.blocks().store_height(0, &hash).unwrap();
        storage
            .chain()
            .update_tip(&hash, &genesis.header, 0)
            .unwrap();
        assert_eq!(
            verify_startup_blocks(&storage).unwrap(),
            StartupVerifyOutcome::SkippedNoBlockData { height: 0 }
        );
    }

    #[test]
    fn a_matching_undo_passes_and_a_mismatch_is_left_in_place() {
        let (_dir, storage) = empty_store();
        let genesis = genesis();
        let hash = storage.blocks().get_block_hash(&genesis);
        storage.blocks().store_block(&genesis).unwrap();
        storage.blocks().store_height(0, &hash).unwrap();
        storage
            .chain()
            .update_tip(&hash, &genesis.header, 0)
            .unwrap();
        let undo = undo_log_from_connected_block(&genesis, 0, |_| {
            Err(blvm_consensus::error::ConsensusError::BlockValidation(
                "genesis has no spends".into(),
            ))
        })
        .unwrap();
        storage.blocks().store_undo_log(&hash, &undo).unwrap();
        assert_eq!(
            verify_startup_blocks(&storage).unwrap(),
            StartupVerifyOutcome::Passed { blocks: 1 }
        );

        storage
            .blocks()
            .store_undo_log(&hash, &BlockUndoLog::new())
            .unwrap();
        let error = verify_startup_blocks(&storage).unwrap_err();
        assert!(format!("{error:#}").contains("does not apply"));
        let stored = storage.blocks().get_undo_log(&hash).unwrap().unwrap();
        assert!(stored.entries.is_empty());
    }
}
