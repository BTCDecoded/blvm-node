//! Type definitions for parallel IBD.
//!
//! Extracted from parallel_ibd/mod.rs for Phase 2 of complexity remediation.

use blvm_protocol::{segwit::Witness, Block, Hash};
use std::sync::Arc;

use crate::storage::disk_utxo::OutPointKey;
use blvm_consensus::types::UTXO;

#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;

/// Number of blocks to prefetch ahead
pub const PREFETCH_LOOKAHEAD: usize = 10;

/// Estimate in-memory bytes for a block + witnesses in the feeder buffer.
/// Blocks under 100k are ~250 bytes (1 coinbase tx). Using a fixed 1.5MB estimate
/// starved the feeder at early heights — the byte cap hit at ~40 blocks instead of
/// the count cap at 300+.
pub fn estimate_block_bytes(block: &Block, witnesses: &[Vec<Witness>]) -> usize {
    let base = 80;
    let tx_bytes: usize = block
        .transactions
        .iter()
        .map(|tx| 32 + tx.inputs.len() * 48 + tx.outputs.len() * 40)
        .sum();
    let wit_bytes: usize = witnesses
        .iter()
        .flat_map(|tw| tw.iter())
        .flat_map(|w| w.iter())
        .map(|elem| elem.len() + 1)
        .sum();
    (base + tx_bytes + wit_bytes).max(200)
}

/// Ready-queue item: block + pre-loaded UTXOs. Arc avoids clone when sending to validation.
/// `input_keys`: same order as `block_input_keys_into` for this block — validation reuses this
/// instead of re-scanning all inputs on the hot path.
/// `tx_ids`: precomputed transaction hashes (same order as `block.transactions`) — feeder
/// skips a duplicate `compute_block_tx_ids` pass.
pub type ReadyItem = (
    u64,
    Block,
    Vec<Vec<Witness>>,
    Vec<OutPointKey>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
);

/// Block feeder buffer: shared between feeder thread (drains ready_rx) and validation thread.
/// Feeder inserts; validation removes next block and reads lookahead for protect_keys.
/// Precomputed tx_ids: feeder computes when inserting to free validation thread from SHA256 work.
/// Last field: estimated bytes for this entry (used by feeder byte cap tracking).
pub type FeederBufferValue = (
    Arc<Block>,
    Vec<Vec<Witness>>,
    Vec<OutPointKey>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
    usize,
);

/// IBD v2 prefetch work item: (store, keys_raw, tx_ids, height, block, witnesses).
#[cfg(feature = "production")]
pub type PrefetchWorkItemV2 = (
    Arc<IbdUtxoStore>,
    Vec<OutPointKey>,
    Vec<Hash>,
    u64,
    Block,
    Vec<Vec<Witness>>,
);

/// Chunk work item for re-queue on drop. Live log 2026-02-21: workers_in_flight=[], chunks lost every 100 blocks.
pub type ChunkWorkItem = (u64, u64, Option<String>);
