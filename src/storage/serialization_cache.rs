//! Serialization caches for performance optimization
//!
//! Provides LRU caches for frequently serialized data structures to avoid
//! redundant serialization operations during IBD.

use std::sync::{Arc, Mutex};
use std::sync::OnceLock;
use lru::LruCache;
use blvm_consensus::types::Hash;

/// Header serialization cache
///
/// Caches serialized block headers to avoid re-serializing the same header
/// multiple times during block storage operations.
///
/// Cache size: 1000 entries (covers ~4 flush batches of 250 blocks each)
static HEADER_SERIALIZE_CACHE: OnceLock<Mutex<LruCache<Hash, Arc<Vec<u8>>>>> = OnceLock::new();

/// Get or initialize the header serialization cache
fn get_header_cache() -> &'static Mutex<LruCache<Hash, Arc<Vec<u8>>>> {
    HEADER_SERIALIZE_CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(1000.try_into().unwrap()))
    })
}

/// Get cached serialized header, or None if not in cache
pub fn get_cached_serialized_header(block_hash: &Hash) -> Option<Arc<Vec<u8>>> {
    let cache = get_header_cache();
    let mut guard = cache.lock().unwrap();
    guard.get(block_hash).map(|arc| Arc::clone(arc))
}

/// Cache a serialized header
pub fn cache_serialized_header(block_hash: Hash, serialized: Vec<u8>) {
    let cache = get_header_cache();
    let mut guard = cache.lock().unwrap();
    guard.put(block_hash, Arc::new(serialized));
}

/// Transaction serialization cache
///
/// Caches serialized transactions to avoid re-serializing the same transaction
/// multiple times (e.g., for merkle root calculation and storage).
///
/// Cache size: 50,000 entries (covers ~25 blocks worth of transactions)
static TX_SERIALIZE_CACHE: OnceLock<Mutex<LruCache<Hash, Arc<Vec<u8>>>>> = OnceLock::new();

/// Get or initialize the transaction serialization cache
fn get_tx_cache() -> &'static Mutex<LruCache<Hash, Arc<Vec<u8>>>> {
    TX_SERIALIZE_CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(50_000.try_into().unwrap()))
    })
}

/// Get cached serialized transaction, or None if not in cache
pub fn get_cached_serialized_tx(tx_hash: &Hash) -> Option<Arc<Vec<u8>>> {
    let cache = get_tx_cache();
    let mut guard = cache.lock().unwrap();
    guard.get(tx_hash).map(|arc| Arc::clone(arc))
}

/// Cache a serialized transaction
pub fn cache_serialized_tx(tx_hash: Hash, serialized: Vec<u8>) {
    let cache = get_tx_cache();
    let mut guard = cache.lock().unwrap();
    guard.put(tx_hash, Arc::new(serialized));
}

/// Clear all caches (useful for testing or memory pressure situations)
pub fn clear_all_caches() {
    if let Some(cache) = HEADER_SERIALIZE_CACHE.get() {
        let mut guard = cache.lock().unwrap();
        guard.clear();
    }
    if let Some(cache) = TX_SERIALIZE_CACHE.get() {
        let mut guard = cache.lock().unwrap();
        guard.clear();
    }
}




