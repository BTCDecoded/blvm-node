//! IBD validation loop — runs on a dedicated `std::thread`.
//!
//! Each block: **connect** (build a `UtxoSet` view from the engine spend session, run
//! `validate_block_only` and BIP30 on that view). The age-tiered engine holds the UTXO set.
//! Retire publishes the height. Checkpoint export writes the heed snapshot.

use super::feeder::FeederState;
use super::latch_env;
use super::memory::{self, MemoryGuard, PressureLevel};
use super::types::PrefetchedUtxoMap;
use crate::storage::Storage;
use crate::storage::blockstore::BlockStore;
use crate::storage::disk_utxo::{
    OutPointKey, block_input_keys_batch_into_arc, block_input_keys_into_filtered,
    block_input_keys_into_filtered_with_tx_ids, key_to_outpoint, outpoint_to_key,
};
use crate::storage::ibd_engine::{
    PartialSpendSession, SpendSession, UtxoDatabase, hotpath_timer_sample,
    session_fill_coinbase_utxos, session_fill_utxo_set,
};
use crate::storage::ibd_utxo_store::{IbdUtxoStore, PendingFlushPackage};
use crate::utils::time::current_timestamp;
use anyhow::Result;
use blvm_protocol::bip_validation::Bip30Index;
use blvm_protocol::{
    BitcoinProtocolEngine, Block, BlockHeader, Hash, UTXO, UtxoSet, segwit::Witness,
};
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Reuse `Arc<Vec<Vec<Witness>>>` of empty stacks for pre-segwit blocks (same `n` as tx count).
/// Validation runs on one thread — `thread_local` avoids a global mutex on this hot path.
thread_local! {
    static EMPTY_WITNESS_STACKS: RefCell<FxHashMap<usize, Arc<Vec<Vec<Witness>>>>> =
        RefCell::new(FxHashMap::default());
}

/// Weak reference to a sample Arc<Block> captured at h=SAMPLE_HEIGHT.
/// Checked in MEM_REPORT to see if that block has been freed (weak upgrade fails)
/// or is still alive (upgrade succeeds = memory leak).
static SAMPLE_BLOCK_WEAK: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Weak<Block>>>> =
    std::sync::OnceLock::new();
const BLOCK_SAMPLE_HEIGHT: u64 = 150_000;

include!("validation_loop_parts/binder.rs");
include!("validation_loop_parts/scoreboard.rs");
include!("validation_loop_parts/apply.rs");

#[cfg(test)]
#[path = "validation_loop_tests.rs"]
mod tests;
