//! UTXO prefetch workers for parallel IBD.
//!
//! Workers load UTXOs for upcoming blocks while validation runs, hiding disk latency.

#[cfg(feature = "production")]
use std::sync::Arc;

#[cfg(feature = "production")]
use blvm_consensus::types::UTXO;
#[cfg(feature = "production")]
use crossbeam_channel::{Receiver, Sender};
#[cfg(feature = "production")]
use rustc_hash::FxHashMap;

#[cfg(feature = "production")]
use crate::storage::disk_utxo::{load_keys_from_disk, OutPointKey};
#[cfg(feature = "production")]
use crate::storage::ibd_utxo_store::IbdUtxoStore;

use super::types::{PrefetchWorkItemV2, ReadyItem};

/// Single-pass: cache lookup + disk load + map build.
/// Used by prefetch workers to build UTXO map for a block.
#[cfg(feature = "production")]
pub(crate) fn prefetch_build_utxo_map(
    store: &IbdUtxoStore,
    keys: &[OutPointKey],
) -> FxHashMap<OutPointKey, Arc<UTXO>> {
    let mut full_map =
        FxHashMap::with_capacity_and_hasher(keys.len(), Default::default());
    let mut to_load: Vec<OutPointKey> = Vec::new();
    for key in keys {
        if let Some(ref r) = store.cache_get(key) {
            if let Some(arc) = r.as_ref() {
                full_map.insert(*key, Arc::clone(arc));
                continue;
            }
        }
        to_load.push(*key);
    }
    if !to_load.is_empty() && !store.memory_only() {
        if let Ok(loaded) = load_keys_from_disk(store.disk_clone(), to_load) {
            for (key, utxo) in loaded {
                let arc = Arc::new(utxo);
                full_map.insert(key, Arc::clone(&arc));
                store.cache_insert_and_track(key, arc);
            }
        }
    }
    full_map
}

/// Run a single prefetch worker. Receives work items, builds UTXO map, sends to ready queue.
#[cfg(feature = "production")]
pub(crate) fn run_prefetch_worker(
    rx: Receiver<PrefetchWorkItemV2>,
    tx: Sender<ReadyItem>,
    store: Arc<IbdUtxoStore>,
) {
    while let Ok((s, keys, h, block, witnesses)) = rx.recv() {
        let full_map = prefetch_build_utxo_map(&s, &keys);
        let _ = tx.send((h, block, witnesses, full_map));
    }
}
