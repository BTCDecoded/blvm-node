//! Serialization caches for performance optimization
//!
//! Provides LRU caches for frequently serialized data structures to avoid
//! redundant serialization operations during IBD.

use blvm_consensus::types::Hash;
use blvm_protocol::lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

const HEADER_CACHE_SHARDS: usize = 16;
const TX_CACHE_SHARDS: usize = 64;

#[inline]
fn header_shard(hash: &Hash) -> usize {
    (hash[0] as usize) & (HEADER_CACHE_SHARDS - 1)
}

#[inline]
fn tx_shard(hash: &Hash) -> usize {
    (hash[0] as usize) & (TX_CACHE_SHARDS - 1)
}

/// When true, header get/put are no-ops — avoids 16 mutex rounds per block during IBD flush
/// (sequential heights ⇒ ~0% LRU hit rate on that path).
static HEADER_SERIALIZE_CACHE_BYPASS: AtomicBool = AtomicBool::new(false);

/// Set by `do_flush_to_storage` around the parallel header serialize phase.
#[inline]
pub fn set_ibd_header_serialize_cache_bypass(on: bool) {
    HEADER_SERIALIZE_CACHE_BYPASS.store(on, Ordering::Relaxed);
}

#[inline]
pub fn ibd_header_serialize_cache_bypassed() -> bool {
    HEADER_SERIALIZE_CACHE_BYPASS.load(Ordering::Relaxed)
}

/// Enables header serialize cache bypass until dropped (IBD blockstore flush path).
pub struct IbdHeaderSerializeCacheBypassGuard(());

impl IbdHeaderSerializeCacheBypassGuard {
    #[inline]
    pub fn enter() -> Self {
        set_ibd_header_serialize_cache_bypass(true);
        Self(())
    }
}

impl Drop for IbdHeaderSerializeCacheBypassGuard {
    fn drop(&mut self) {
        set_ibd_header_serialize_cache_bypass(false);
    }
}

/// Sharded header serialization cache (reduces Mutex contention vs one global LRU under rayon).
static HEADER_SERIALIZE_CACHE: OnceLock<[Mutex<LruCache<Hash, Arc<Vec<u8>>>>; HEADER_CACHE_SHARDS]> =
    OnceLock::new();

fn header_caches(
) -> &'static [Mutex<LruCache<Hash, Arc<Vec<u8>>>>; HEADER_CACHE_SHARDS] {
    HEADER_SERIALIZE_CACHE.get_or_init(|| {
        let cap = NonZeroUsize::new(64).expect("nonzero");
        std::array::from_fn(|_| Mutex::new(LruCache::new(cap)))
    })
}

/// Get cached serialized header, or None if not in cache
pub fn get_cached_serialized_header(block_hash: &Hash) -> Option<Arc<Vec<u8>>> {
    if ibd_header_serialize_cache_bypassed() {
        return None;
    }
    let shard = header_shard(block_hash);
    let mut guard = header_caches()[shard].lock().unwrap();
    guard.get(block_hash).map(Arc::clone)
}

/// Cache a serialized header
pub fn cache_serialized_header(block_hash: Hash, serialized: Vec<u8>) {
    if ibd_header_serialize_cache_bypassed() {
        return;
    }
    let shard = header_shard(&block_hash);
    let mut guard = header_caches()[shard].lock().unwrap();
    guard.put(block_hash, Arc::new(serialized));
}

/// Sharded transaction serialization cache.
static TX_SERIALIZE_CACHE: OnceLock<[Mutex<LruCache<Hash, Arc<Vec<u8>>>>; TX_CACHE_SHARDS]> =
    OnceLock::new();

fn tx_caches() -> &'static [Mutex<LruCache<Hash, Arc<Vec<u8>>>>; TX_CACHE_SHARDS] {
    TX_SERIALIZE_CACHE.get_or_init(|| {
        let cap = NonZeroUsize::new(800).expect("nonzero");
        std::array::from_fn(|_| Mutex::new(LruCache::new(cap)))
    })
}

/// Get cached serialized transaction, or None if not in cache
pub fn get_cached_serialized_tx(tx_hash: &Hash) -> Option<Arc<Vec<u8>>> {
    let shard = tx_shard(tx_hash);
    let mut guard = tx_caches()[shard].lock().unwrap();
    guard.get(tx_hash).map(Arc::clone)
}

/// Cache a serialized transaction
pub fn cache_serialized_tx(tx_hash: Hash, serialized: Vec<u8>) {
    let shard = tx_shard(&tx_hash);
    let mut guard = tx_caches()[shard].lock().unwrap();
    guard.put(tx_hash, Arc::new(serialized));
}

/// Clear all caches (useful for testing or memory pressure situations)
pub fn clear_all_caches() {
    if let Some(caches) = HEADER_SERIALIZE_CACHE.get() {
        for m in caches {
            let mut guard = m.lock().unwrap();
            guard.clear();
        }
    }
    if let Some(caches) = TX_SERIALIZE_CACHE.get() {
        for m in caches {
            let mut guard = m.lock().unwrap();
            guard.clear();
        }
    }
}
