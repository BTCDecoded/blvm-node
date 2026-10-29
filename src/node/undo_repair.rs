//! Rebuild connect-undo when the tip moved without one.
//!
//! The validation skip path used to advance `chain_info` and drop the undo log.
//! A later reorg then cannot disconnect that block, and pruning has often deleted
//! the block that created the spent output. This walks the active tail, downloads
//! any missing creator bodies from peers, and stores the undo a connect would have.

use crate::network::NetworkManager;
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use anyhow::{Context, Result, bail};
use blvm_consensus::block::calculate_tx_id;
use blvm_consensus::reorganization::undo_log_from_connected_block;
use blvm_consensus::transaction::is_coinbase;
use blvm_protocol::{Block, Hash, OutPoint, UTXO};
use std::collections::{HashMap, HashSet};
use tracing::info;

/// How far behind the tip a skipped flush can have dropped undo.
/// `should_advance_tip_on_skip_path` advances every block in the last 1024.
const REPAIR_TAIL: u64 = 1024;

/// Heights of block bodies to fetch while resolving one tail's prevouts.
const REPAIR_FETCH: u64 = 10_000;

/// Store undo for active blocks at or below `tip_height` that do not have one.
///
/// Returns how many undo logs were written. Blocks that already have undo stop
/// the walk: the flush path below that point wrote undo with the body.
pub async fn repair_missing_active_undo(
    network: &NetworkManager,
    storage: &Storage,
    tip_height: u64,
    disconnect_from: u64,
) -> Result<usize> {
    let blocks = storage.blocks();
    let missing_blocks =
        active_blocks_missing_undo(blocks.as_ref(), tip_height, disconnect_from)?;
    if missing_blocks.is_empty() {
        return Ok(0);
    }

    let mut wanted: HashSet<Hash> = HashSet::new();
    for (_, _, block) in &missing_blocks {
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                wanted.insert(input.prevout.hash);
            }
        }
    }

    let mut prevouts: HashMap<OutPoint, UTXO> = HashMap::new();
    if !wanted.is_empty() {
        for (height, _, block) in &missing_blocks {
            note_outputs(block, *height, &wanted, &mut prevouts);
        }
        let oldest = missing_blocks.last().map(|(h, _, _)| *h).unwrap_or(tip_height);
        remember_local_prevouts(blocks.as_ref(), oldest, &wanted, &mut prevouts)?;
        fetch_prevouts(network, blocks.as_ref(), oldest, &wanted, &mut prevouts).await?;
    }

    let mut written = 0;
    for (height, hash, block) in &missing_blocks {
        let undo = undo_log_from_connected_block(block, *height, |outpoint| {
            prevouts.get(outpoint).cloned().ok_or_else(|| {
                blvm_consensus::error::ConsensusError::BlockValidation(
                    format!(
                        "undo repair missing prevout {}:{}",
                        hex::encode(outpoint.hash),
                        outpoint.index
                    )
                    .into(),
                )
            })
        })
        .with_context(|| format!("undo repair at height {height}"))?;
        if blocks.store_undo_log_if_absent(hash, &undo)? {
            written += 1;
            info!(
                "[UNDO_REPAIR] stored undo for height {height} prefix {:02x}{:02x}{:02x}{:02x}",
                hash[0], hash[1], hash[2], hash[3]
            );
        }
    }
    Ok(written)
}

/// Tip-first blocks on the active chain that have a body and no undo.
///
/// A pruned body ends the walk. The reorg only disconnects blocks whose undo
/// we can rebuild, and a missing body further down is not a reason to refuse
/// the tip.
pub(crate) fn active_blocks_missing_undo(
    blocks: &BlockStore,
    tip_height: u64,
    disconnect_from: u64,
) -> Result<Vec<(u64, Hash, Block)>> {
    // Only blocks the heavier chain will disconnect. Rebuilding the whole
    // prune window asks for prevouts that are older than the fetch limit and
    // are not required to leave the fork.
    let floor = disconnect_from.min(tip_height).max(tip_height.saturating_sub(REPAIR_TAIL));
    let mut missing_blocks = Vec::new();
    for height in (floor..=tip_height).rev() {
        let Some(hash) = blocks.get_hash_by_height(height)? else {
            break;
        };
        if blocks.get_undo_log(&hash)?.is_some() {
            break;
        }
        let Some(block) = blocks.get_block(&hash)? else {
            info!(
                "[UNDO_REPAIR] height {height} body is pruned; repairing undo above it"
            );
            break;
        };
        missing_blocks.push((height, hash, block));
    }
    Ok(missing_blocks)
}

fn remember_local_prevouts(
    blocks: &BlockStore,
    from_height: u64,
    wanted: &HashSet<Hash>,
    found: &mut HashMap<OutPoint, UTXO>,
) -> Result<()> {
    let mut height = from_height;
    let stop = from_height.saturating_sub(REPAIR_FETCH);
    while height > stop {
        height -= 1;
        if wanted.iter().all(|txid| found.keys().any(|op| op.hash == *txid)) {
            break;
        }
        let Some(hash) = blocks.get_hash_by_height(height)? else {
            continue;
        };
        let Some(block) = blocks.get_block(&hash)? else {
            continue;
        };
        note_outputs(&block, height, wanted, found);
    }
    Ok(())
}

async fn fetch_prevouts(
    network: &NetworkManager,
    blocks: &BlockStore,
    from_height: u64,
    wanted: &HashSet<Hash>,
    found: &mut HashMap<OutPoint, UTXO>,
) -> Result<()> {
    let mut height = from_height;
    let stop = from_height.saturating_sub(REPAIR_FETCH);
    let mut fetched = 0u64;
    while height > stop {
        if wanted.iter().all(|txid| found.keys().any(|op| op.hash == *txid)) {
            return Ok(());
        }
        height -= 1;
        let Some(hash) = blocks.get_hash_by_height(height)? else {
            continue;
        };
        if blocks.has_block_body(&hash)? {
            if let Some(block) = blocks.get_block(&hash)? {
                note_outputs(&block, height, wanted, found);
            }
            continue;
        }
        let Some((block, _)) = network.request_block(hash).await else {
            continue;
        };
        if blocks.get_block_hash(&block) != hash {
            continue;
        }
        blocks.store_block(&block)?;
        note_outputs(&block, height, wanted, found);
        fetched += 1;
        if fetched % 500 == 0 {
            info!("[UNDO_REPAIR] fetched {fetched} pruned bodies, still resolving prevouts");
        }
    }
    let unresolved: Vec<String> = wanted
        .iter()
        .filter(|txid| found.keys().all(|op| op.hash != **txid))
        .map(hex::encode)
        .collect();
    if unresolved.is_empty() {
        Ok(())
    } else {
        bail!(
            "undo repair could not find {} prevout transaction(s) within {REPAIR_FETCH} blocks: {}",
            unresolved.len(),
            unresolved.join(", ")
        )
    }
}

fn note_outputs(
    block: &Block,
    height: u64,
    wanted: &HashSet<Hash>,
    found: &mut HashMap<OutPoint, UTXO>,
) {
    for (tx_index, tx) in block.transactions.iter().enumerate() {
        let txid = calculate_tx_id(tx);
        if !wanted.contains(&txid) {
            continue;
        }
        let coinbase = tx_index == 0 && is_coinbase(tx);
        for (index, output) in tx.outputs.iter().enumerate() {
            found.insert(
                OutPoint {
                    hash: txid,
                    index: index as u32,
                },
                UTXO {
                    value: output.value,
                    script_pubkey: output.script_pubkey.as_slice().into(),
                    height,
                    is_coinbase: coinbase,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blvm_protocol::{BlockHeader, Transaction, TransactionInput, TransactionOutput};

    fn coinbase(value: i64) -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: [0; 32],
                    index: 0xffff_ffff,
                },
                script_sig: vec![4].into(),
                sequence: 0xffff_ffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value,
                script_pubkey: vec![0x51],
            }]
            .into(),
            lock_time: 0,
        }
    }

    #[test]
    fn a_local_creator_block_supplies_the_spent_output() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        let parent_tx = coinbase(50);
        let parent_id = calculate_tx_id(&parent_tx);
        let parent = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: [0; 32],
                merkle_root: [1; 32],
                timestamp: 1,
                bits: 0x207f_ffff,
                nonce: 1,
            },
            transactions: vec![parent_tx].into_boxed_slice(),
        };
        let parent_hash = storage.blocks().get_block_hash(&parent);
        storage.blocks().store_block(&parent).unwrap();
        storage.blocks().store_height(4, &parent_hash).unwrap();

        let spender = Transaction {
            version: 1,
            inputs: vec![TransactionInput {
                prevout: OutPoint {
                    hash: parent_id,
                    index: 0,
                },
                script_sig: vec![].into(),
                sequence: 0xffff_ffff,
            }]
            .into(),
            outputs: vec![TransactionOutput {
                value: 40,
                script_pubkey: vec![0x52],
            }]
            .into(),
            lock_time: 0,
        };
        let child = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: parent_hash,
                merkle_root: [2; 32],
                timestamp: 2,
                bits: 0x207f_ffff,
                nonce: 2,
            },
            transactions: vec![coinbase(10), spender].into_boxed_slice(),
        };
        let child_hash = storage.blocks().get_block_hash(&child);
        storage.blocks().store_block(&child).unwrap();
        storage.blocks().store_height(5, &child_hash).unwrap();
        assert!(storage.blocks().get_undo_log(&child_hash).unwrap().is_none());

        let mut found = HashMap::new();
        let wanted = HashSet::from([parent_id]);
        remember_local_prevouts(storage.blocks().as_ref(), 5, &wanted, &mut found).unwrap();
        let spent = OutPoint {
            hash: parent_id,
            index: 0,
        };
        let utxo = found.get(&spent).expect("prevout from the stored parent");
        assert_eq!(utxo.value, 50);
        assert_eq!(utxo.height, 4);
        assert!(utxo.is_coinbase);

        let undo = undo_log_from_connected_block(&child, 5, |op| {
            found.get(op).cloned().ok_or_else(|| {
                blvm_consensus::error::ConsensusError::BlockValidation("missing".into())
            })
        })
        .unwrap();
        assert!(storage
            .blocks()
            .store_undo_log_if_absent(&child_hash, &undo)
            .unwrap());
        assert!(storage.blocks().get_undo_log(&child_hash).unwrap().is_some());
    }

    #[test]
    fn a_pruned_block_below_the_tip_does_not_drop_the_tip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::new(dir.path()).unwrap();
        let pruned = [9u8; 32];
        storage.blocks().store_height(4, &pruned).unwrap();
        let tip = Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: pruned,
                merkle_root: [2; 32],
                timestamp: 2,
                bits: 0x207f_ffff,
                nonce: 2,
            },
            transactions: vec![coinbase(10)].into_boxed_slice(),
        };
        let tip_hash = storage.blocks().get_block_hash(&tip);
        storage.blocks().store_block(&tip).unwrap();
        storage.blocks().store_height(5, &tip_hash).unwrap();

        let missing = active_blocks_missing_undo(storage.blocks().as_ref(), 5, 4).unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, 5);
        assert_eq!(missing[0].1, tip_hash);
        let only_the_fork = active_blocks_missing_undo(storage.blocks().as_ref(), 5, 5).unwrap();
        assert_eq!(only_the_fork.len(), 1);
        assert_eq!(only_the_fork[0].0, 5);
    }
}
