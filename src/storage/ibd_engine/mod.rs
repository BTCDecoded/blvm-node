//! IBD UTXO Engine — age-tiered in-memory index with disk overflow for Initial Block Download.
//!
//! # Architecture
//!
//! ```text
//! UtxoDatabase
//!   ├── UtxoTable   — flat append-only file + in-memory tail
//!   │                 stores {OutputHeader (16B) || script_bytes} per UTXO
//!   └── UtxoIndex   — 7-age UTXO index (ages[0]=newest, ages[6]=oldest)
//!         ├── MemoryAge[0..2]  — mutable (accepts appends from orchestrator)
//!         ├── MemoryAge[3..6]  — frozen (compacter-only appends)
//!         └── Compacter        — 7 shared threads, one crossbeam channel
//!               each thread: take N runs from one age → merge → push to next age
//! ```
//!
//! # Key sizes
//! - `OutputKey = [u8; 36]` (txid 32B + vout u32 BE 4B) — smaller than legacy [u8; 40]
//! - `OutputKV  = 52 bytes` per index entry (height + id as separate fields)
//! - Bloom filter: ~12 bits/entry, ~1% FPR (7 probes, 64-byte blocked layout)
//! - Directory: prefix-bucket index, ~85 entries/bucket (~4 KB binary search range)
//!
//! # Usage (Phase 2 wire-in)
//! ```rust,ignore
//! // Orchestrator thread (sequential):
//! let pin = db.append(&block, &tx_ids, height)?;
//!
//! // Worker thread (parallel):
//! let session = SpendSession::resolve(&db, &block, &tx_ids, height);
//! let utxo_set = session_to_utxo_set(&session);
//! let result = parallel_ibd.validate_block_only(..., &mut utxo_set, ...);
//! drop(pin); // release height from mutable window
//! ```
//!
//! # Phase 1 scope
//! Module built and tested in isolation. No wire-in to IBD pipeline during Phase 1.
//! Phase 2 adds `SpendSession` and updates `validation_loop.rs`.

pub mod database;
pub mod disk_index;
pub mod disk_segment;
pub mod export;
pub mod import;
pub mod index;
pub mod memory_age;
pub mod memory_run;
pub mod spend_session;
pub mod table;
pub mod types;

pub use database::UtxoDatabase;
pub use export::{
    ckpt_inactive_slot, ckpt_tree_for_slot, run_checkpoint_export_replace, run_watermark_export,
    CKPT_TREE_A, CKPT_TREE_B,
};
pub use import::seed_from_ibd_utxos;
pub use memory_run::set_gc_fence;
pub use spend_session::{
    session_fill_utxo_set, session_to_utxo_set, PartialSpendSession, SpendSession,
};
pub use types::{
    outpoint_to_output_key, output_key_to_outpoint, to_output_key, IdCodec, OutputDetail,
    OutputHeader, OutputId, OutputKV, OutputKey, OUTPUT_ID_DELETED,
};
