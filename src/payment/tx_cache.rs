//! Transaction cache for re-broadcast on reorg
//!
//! Stores full transactions for settlement payments so they can be re-broadcast
//! when a block is reorged out and the tx is no longer in mempool.

use crate::{Hash, Transaction};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Max cached txs (LRU eviction by count; simple implementation)
const MAX_CACHED: usize = 10_000;

/// Cache of transactions for re-broadcast when reorged out
#[cfg(feature = "ctv")]
pub struct PaymentTxCache {
    /// tx_hash -> Transaction (bounded by MAX_CACHED)
    cache: Mutex<HashMap<Hash, Transaction>>,
    /// Order of insertion for LRU eviction (tx_hash)
    order: Mutex<Vec<Hash>>,
}

#[cfg(feature = "ctv")]
impl PaymentTxCache {
    /// Create a new tx cache
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
        }
    }

    /// Store a transaction for potential re-broadcast
    pub fn store(&self, tx_hash: Hash, tx: Transaction) {
        let mut cache = self.cache.lock().unwrap();
        let mut order = self.order.lock().unwrap();

        if cache.len() >= MAX_CACHED && !cache.contains_key(&tx_hash) {
            while let Some(old_hash) = order.first().copied() {
                cache.remove(&old_hash);
                order.remove(0);
                if cache.len() < MAX_CACHED {
                    break;
                }
            }
        }

        if !cache.contains_key(&tx_hash) {
            order.push(tx_hash);
        }
        cache.insert(tx_hash, tx);
    }

    /// Get a transaction for re-broadcast
    pub fn get(&self, tx_hash: &Hash) -> Option<Transaction> {
        let cache = self.cache.lock().unwrap();
        cache.get(tx_hash).cloned()
    }
}

#[cfg(feature = "ctv")]
impl Default for PaymentTxCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Stub when CTV disabled
#[cfg(not(feature = "ctv"))]
pub struct PaymentTxCache;

#[cfg(not(feature = "ctv"))]
impl PaymentTxCache {
    pub fn new() -> Self {
        Self
    }
    pub fn store(&self, _tx_hash: Hash, _tx: Transaction) {}
    pub fn get(&self, _tx_hash: &Hash) -> Option<Transaction> {
        None
    }
}

#[cfg(not(feature = "ctv"))]
impl Default for PaymentTxCache {
    fn default() -> Self {
        Self
    }
}
