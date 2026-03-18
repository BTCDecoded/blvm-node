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

/// Rough bytes per block for feeder byte cap. SegWit blocks 1–4MB; use 1.5MB estimate.
pub const FEEDER_BLOCK_BYTES_ESTIMATE: usize = 1_500_000;

/// Ready-queue item: block + pre-loaded UTXOs. Arc avoids clone when sending to validation.
pub type ReadyItem = (
    u64,
    Block,
    Vec<Vec<Witness>>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
);

/// Block feeder buffer: shared between feeder thread (drains ready_rx) and validation thread.
/// Feeder inserts; validation removes next block and reads lookahead for protect_keys.
/// Precomputed tx_ids: feeder computes when inserting to free validation thread from SHA256 work.
pub type FeederBufferValue = (
    Arc<Block>,
    Vec<Vec<Witness>>,
    rustc_hash::FxHashMap<OutPointKey, Arc<UTXO>>,
    Vec<Hash>,
);

/// IBD v2 prefetch work item: (store, keys_raw, height, block, witnesses). Worker filters via store.
#[cfg(feature = "production")]
pub type PrefetchWorkItemV2 = (
    Arc<IbdUtxoStore>,
    Vec<OutPointKey>,
    u64,
    Block,
    Vec<Vec<Witness>>,
);

/// Chunk work item for re-queue on drop. Live log 2026-02-21: workers_in_flight=[], chunks lost every 100 blocks.
pub type ChunkWorkItem = (u64, u64, Option<String>);
