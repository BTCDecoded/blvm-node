//! Type definitions for parallel IBD (chunk work items, byte estimates, shared structs).

use blvm_protocol::{Block, Hash, segwit::Witness};
use std::sync::Arc;

/// Shared block + witnesses used throughout the IBD download→coordinator→prefetch→feeder pipeline.
/// Wrapping at the download layer (before the mpsc send) means all pipeline stages hold a
/// single heap allocation; earlier stages moved Block by value producing per-stage copies.
pub type SharedBlock = Arc<Block>;
pub type SharedWitnesses = Arc<Vec<Vec<Witness>>>;

use crate::storage::disk_utxo::OutPointKey;
use blvm_protocol::types::UTXO;
use rustc_hash::FxHashMap;

#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;

/// Prefetched UTXOs keyed by outpoint key — shared via `Arc` in the pipeline (engine mode uses a static empty sentinel).
pub type PrefetchedUtxoMap = Arc<FxHashMap<OutPointKey, Arc<UTXO>>>;

/// Number of blocks to prefetch ahead
pub const PREFETCH_LOOKAHEAD: usize = 10;

/// Estimate in-memory bytes for a block + witnesses in the feeder buffer.
///
/// Accounts for actual Rust struct layout, not just serialized size:
/// - `Transaction` uses `SmallVec<[TxInput; 2]>` / `SmallVec<[TxOutput; 2]>` inline storage:
///   each inline element is stored directly in the SmallVec without heap allocation for ≤2 items.
///   sizeof(TxInput) ≈ 72B (OutPoint=40 + sequence=8 + script_sig Vec=24).
///   sizeof(TxOutput) ≈ 32B (value=8 + script_pubkey Vec=24).
///   sizeof(Transaction) ≈ 240B inline + 8B version + 8B lock_time.
/// - script_sig / script_pubkey: Vec<u8> data is heap-allocated (separate from SmallVec inline).
///   pre-SegWit: script_sig ≈ 107B per input, script_pubkey ≈ 25B per output.
///   post-SegWit: script_sig is usually empty (data in witness); script_pubkey ≈ 22B.
/// - Witnesses: each element is a Vec<u8> with 24B overhead regardless of data size.
/// - Box<[Transaction]>: one allocation for all transactions in the block (no per-tx heap alloc).
fn estimate_block_bytes_cheap_enabled() -> bool {
    super::latch_env!(bool, {
        matches!(
            std::env::var("BLVM_IBD_ESTIMATE_CHEAP")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1") | Some("true") | Some("yes") | Some("on")
        )
    })
}

/// N16: O(n_tx) estimate — skips per-witness-element `len()` walks.
pub fn estimate_block_bytes_cheap(block: &Block, witnesses: &[Vec<Witness>]) -> usize {
    let n_tx = block.transactions.len();
    let mut n_in = 0usize;
    let mut n_out = 0usize;
    for tx in block.transactions.iter() {
        n_in = n_in.saturating_add(tx.inputs.len());
        n_out = n_out.saturating_add(tx.outputs.len());
    }
    let mut wit = 0usize;
    for tw in witnesses {
        for stack in tw {
            // 24B stack vec + ~64B avg payload per stack item (no elem.len() chase).
            wit = wit
                .saturating_add(24)
                .saturating_add(stack.len().saturating_mul(88));
        }
    }
    (88usize
        .saturating_add(n_tx.saturating_mul(240))
        .saturating_add(n_in.saturating_mul(48))
        .saturating_add(n_out.saturating_mul(40))
        .saturating_add(wit))
    .max(200)
}

pub fn estimate_block_bytes(block: &Block, witnesses: &[Vec<Witness>]) -> usize {
    if estimate_block_bytes_cheap_enabled() {
        return estimate_block_bytes_cheap(block, witnesses);
    }
    // Block header (80B) + Box<[Transaction]> pointer (8B).
    let base = 88;
    // Per-transaction: Transaction struct size (inline) + heap for script bytes.
    // SmallVec<[TxInput;2]>: inline stores ≤2 inputs, each 72 bytes (OutPoint=40+seq=8+Vec=24).
    // SmallVec<[TxOutput;2]>: inline stores ≤2 outputs, each 32 bytes (value=8+Vec=24).
    // Transaction struct ≈ 8(ver) + max(2*72, ptr_cap) + max(2*32, ptr_cap) + 8(locktime)
    // Simplified: 240 bytes inline per transaction for typical 1-2 inputs/outputs.
    // script_sig heap: 0 for SegWit inputs; ~107B for pre-SegWit (jemalloc rounds to 128B).
    // script_pubkey heap: ~25B (jemalloc rounds to 32B) per output.
    // Add Vec<u8> 24-byte headers for both even when empty (pointer+len+cap = 24 bytes each).
    let tx_bytes: usize = block
        .transactions
        .iter()
        .map(|tx| {
            // Transaction struct inline cost: ≈240B covers version+SmallVec+lock_time
            let struct_inline = 240usize;
            // script_sig: each input → 24B Vec header + content bytes (0 for SegWit).
            // We can't know script_sig length without deserialization here, so estimate:
            // at post-SegWit heights most inputs are SegWit (script_sig ≈ 0B); at pre-SegWit
            // heights ≈ 107B. The existing wit_bytes below captures witness stack bytes.
            // Conservatively estimate 24B header per input (at least the Vec<u8> struct).
            let script_heap = tx.inputs.len() * 24 + tx.outputs.len() * 32;
            // Heap for overflow inputs/outputs beyond SmallVec inline capacity (>2).
            let overflow = if tx.inputs.len() > 2 {
                (tx.inputs.len() - 2) * 72
            } else { 0 } + if tx.outputs.len() > 2 {
                (tx.outputs.len() - 2) * 32
            } else { 0 };
            struct_inline + script_heap + overflow
        })
        .sum();
    // Witness stack: each element is a Vec<u8>. Count 24B header per element + data bytes.
    let wit_bytes: usize = witnesses
        .iter()
        .flat_map(|tw| tw.iter())
        .map(|stack_item_vec| {
            // Vec<Vec<u8>> = 24B header + per element (24B header + data)
            24 + stack_item_vec.iter().map(|elem| 24 + elem.len()).sum::<usize>()
        })
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
    PrefetchedUtxoMap,
    Vec<Hash>,
    Arc<blvm_consensus::types::UtxoSet>,
);

/// Block feeder buffer: shared between feeder thread (drains ready_rx) and validation thread.
/// Feeder inserts; validation removes next block and reads lookahead for protect_keys.
/// Precomputed tx_ids: may be empty in engine mode (N15 — validation fills before append).
/// Sixth field: precomputed `Arc<UtxoSet>` of this block's speculative outputs (built on the
/// prefetch worker pool — see `ReadyItem`).
/// Last field: estimated bytes for this entry (used by feeder byte cap tracking).
/// Both block and witnesses are Arc-shared with earlier pipeline stages — no deep copy here.
pub type FeederBufferValue = (
    SharedBlock,
    SharedWitnesses,
    Vec<OutPointKey>,
    PrefetchedUtxoMap,
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

/// IBD lifecycle phase — drives export defer, inject policy, and stall behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IbdPhase {
    /// Replaying local bodies from genesis / confirmed watermark.
    LocalReplay,
    /// Sparse local bodies or engine resume gap replay before WAN catch-up.
    Hybrid,
    /// Bodies ended; WAN download + gap fill to header tip.
    WanCatchup,
    /// Within `TIP_FOLLOW_MARGIN` blocks of effective end.
    TipFollow,
}

/// Blocks from effective end at which we treat sync as tip-follow (stall policy relaxes).
pub(crate) const TIP_FOLLOW_MARGIN: u64 = 144;

/// Inputs for [`derive_ibd_phase`] and export-defer policy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IbdPhaseCtx {
    pub validation_h: u64,
    pub start_height: u64,
    pub local_replay_max_height: u64,
    pub confirmed_body_height_at_start: u64,
    pub sparse_local_body_max: u64,
    pub engine_export_height: u64,
    pub effective_end_height: u64,
}

/// Derive the current IBD phase from height probes (replaces scattered boolean gates).
pub(crate) fn derive_ibd_phase(ctx: &IbdPhaseCtx) -> IbdPhase {
    if ctx.validation_h >= ctx.effective_end_height.saturating_sub(TIP_FOLLOW_MARGIN) {
        return IbdPhase::TipFollow;
    }
    if ctx.validation_h > ctx.local_replay_max_height {
        return IbdPhase::WanCatchup;
    }
    let sparse_replay = ctx.confirmed_body_height_at_start == 0 && ctx.sparse_local_body_max > 0;
    let engine_resume_gap = ctx.engine_export_height > 0
        && ctx.start_height > ctx.local_replay_max_height
        && ctx.validation_h < ctx.local_replay_max_height.max(ctx.start_height);
    if sparse_replay || engine_resume_gap {
        return IbdPhase::Hybrid;
    }
    if ctx.start_height <= ctx.local_replay_max_height && ctx.confirmed_body_height_at_start > 0 {
        return IbdPhase::LocalReplay;
    }
    IbdPhase::Hybrid
}

/// Whether periodic checkpoint export should be deferred during gap replay.
pub(crate) fn phase_defers_checkpoint_export(
    phase: IbdPhase,
    ctx: &IbdPhaseCtx,
    gap_export_defer_until: u64,
) -> bool {
    match phase {
        IbdPhase::LocalReplay | IbdPhase::Hybrid => {
            gap_export_defer_until > 0 && ctx.validation_h < gap_export_defer_until
        }
        IbdPhase::WanCatchup | IbdPhase::TipFollow => false,
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    use blvm_protocol::{BlockHeader, Transaction, TransactionOutput};

    #[test]
    fn n16_cheap_estimate_positive_and_micro() {
        let block = Block {
            header: BlockHeader {
                version: 4,
                timestamp: 1_600_000_000,
                ..Default::default()
            },
            transactions: (0..50)
                .map(|i| Transaction {
                    version: 1,
                    inputs: blvm_protocol::tx_inputs![],
                    outputs: blvm_protocol::tx_outputs![TransactionOutput {
                        value: 50 + i,
                        script_pubkey: vec![0x51],
                    }],
                    lock_time: 0,
                })
                .collect::<Vec<_>>()
                .into(),
        };
        let witnesses: Vec<Vec<Witness>> = (0..50)
            .map(|_| vec![vec![vec![0u8; 72], vec![0u8; 33]]])
            .collect();
        let cheap = estimate_block_bytes_cheap(&block, &witnesses);
        assert!(cheap >= 200);
        // Full path (env unset): walk elem lens.
        let t_full = std::time::Instant::now();
        let mut full = 0usize;
        for _ in 0..2_000 {
            full = estimate_block_bytes(&block, &witnesses);
        }
        let full_ns = t_full.elapsed().as_nanos() / 2_000;
        let t_cheap = std::time::Instant::now();
        for _ in 0..2_000 {
            let _ = estimate_block_bytes_cheap(&block, &witnesses);
        }
        let cheap_ns = t_cheap.elapsed().as_nanos() / 2_000;
        eprintln!(
            "[N16 micro] cheap≈{cheap_ns} ns/op full≈{full_ns} ns/op (est cheap={cheap} full={full})"
        );
        assert!(full >= 200);
        assert!(
            cheap_ns <= full_ns.saturating_add(full_ns / 2).max(50),
            "cheap should be competitive with full"
        );
    }

    #[test]
    fn derive_phase_wan_after_local_replay_cap() {
        let ctx = IbdPhaseCtx {
            validation_h: 700_000,
            start_height: 550_000,
            local_replay_max_height: 657_000,
            confirmed_body_height_at_start: 657_000,
            sparse_local_body_max: 0,
            engine_export_height: 550_000,
            effective_end_height: 957_000,
        };
        assert_eq!(derive_ibd_phase(&ctx), IbdPhase::WanCatchup);
    }

    #[test]
    fn derive_phase_tip_follow_near_end() {
        let ctx = IbdPhaseCtx {
            validation_h: 957_200,
            start_height: 550_000,
            local_replay_max_height: 657_000,
            confirmed_body_height_at_start: 657_000,
            sparse_local_body_max: 0,
            engine_export_height: 550_000,
            effective_end_height: 957_278,
        };
        assert_eq!(derive_ibd_phase(&ctx), IbdPhase::TipFollow);
    }

    #[test]
    fn wan_phase_never_defers_export() {
        let ctx = IbdPhaseCtx {
            validation_h: 700_000,
            start_height: 550_000,
            local_replay_max_height: 172_791,
            confirmed_body_height_at_start: 657_000,
            sparse_local_body_max: 0,
            engine_export_height: 550_000,
            effective_end_height: 957_000,
        };
        assert!(!phase_defers_checkpoint_export(
            IbdPhase::WanCatchup,
            &ctx,
            957_000
        ));
    }
}
