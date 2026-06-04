//! Type definitions for parallel IBD (chunk work items, byte estimates, shared structs).

use blvm_protocol::{segwit::Witness, Block, Hash};
use std::sync::Arc;

/// Shared block + witnesses used throughout the IBD download→coordinator→prefetch→feeder pipeline.
/// Wrapping at the download layer (before the mpsc send) means all pipeline stages hold a
/// single heap allocation; earlier stages moved Block by value producing per-stage copies.
pub type SharedBlock = Arc<Block>;
pub type SharedWitnesses = Arc<Vec<Vec<Witness>>>;

use crate::storage::disk_utxo::OutPointKey;
use blvm_protocol::types::UTXO;

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
/// `spec_adds`: this block's speculative outputs, precomputed on the prefetch worker pool so
/// the validation dispatcher (single-threaded) does **not** rebuild a per-block `UtxoSet`
/// (~O(outputs) HashMap inserts + Arc allocations) on its hot path — pre-append outputs on
/// the prefetch pool before validation starts (same role as a spend-prep worker thread).
/// Block and witnesses are `Arc`-wrapped so this item (and all earlier pipeline stages)
/// share a single heap allocation rather than duplicating block bytes at every stage.
pub type ReadyItem = (
    u64,
    SharedBlock,
    SharedWitnesses,
    Vec<OutPointKey>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
    Arc<blvm_consensus::types::UtxoSet>,
);

/// Block feeder buffer: shared between feeder thread (drains ready_rx) and validation thread.
/// Feeder inserts; validation removes next block and reads lookahead for protect_keys.
/// Precomputed tx_ids: feeder computes when inserting to free validation thread from SHA256 work.
/// Sixth field: precomputed `Arc<UtxoSet>` of this block's speculative outputs (built on the
/// prefetch worker pool — see `ReadyItem`).
/// Last field: estimated bytes for this entry (used by feeder byte cap tracking).
/// Both block and witnesses are Arc-shared with earlier pipeline stages — no deep copy here.
pub type FeederBufferValue = (
    SharedBlock,
    SharedWitnesses,
    Vec<OutPointKey>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
    Arc<blvm_consensus::types::UtxoSet>,
    usize,
);

/// IBD v2 prefetch work item: (store, keys_raw, tx_ids, height, block, witnesses, engine_mode).
/// Block and witnesses are Arc-shared from the download layer — no per-stage deep copies.
/// `engine_mode`: when true the age-tiered engine owns UTXO resolution; the prefetch worker
/// skips `prefetch_build_utxo_map` and `build_spec_adds` (their outputs are never consumed by
/// the engine validation path), saving ~440 cache lookups and ~2000 Arc allocs per block.
#[cfg(feature = "production")]
pub type PrefetchWorkItemV2 = (
    Arc<IbdUtxoStore>,
    Vec<OutPointKey>,
    Vec<Hash>,
    u64,
    SharedBlock,
    SharedWitnesses,
    bool, // engine_mode
);

/// Chunk work item for re-queue on drop. Live log 2026-02-21: workers_in_flight=[], chunks lost every 100 blocks.
pub type ChunkWorkItem = (u64, u64, Option<String>);
