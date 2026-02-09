//! Disk-backed UTXO set with bounded in-memory cache
//!
//! Solves the OOM problem during IBD by keeping only a bounded subset of UTXOs
//! in memory and storing the complete set on disk (redb).
//!
//! ## Architecture
//!
//! ```text
//! ┌───────────────────────┐
//! │  In-Memory Cache      │  ← Bounded (e.g., 5M entries ≈ 2.5GB)
//! │  HashMap<OutPoint, U> │
//! └──────────┬────────────┘
//!            │ cache miss → load from disk
//! ┌──────────▼────────────┐
//! │  Disk Store (redb)    │  ← ALL UTXOs, unbounded
//! │  Tree: "ibd_utxos"    │
//! └───────────────────────┘
//! ```
//!
//! ## Performance optimizations (v80)
//!
//! - **Lazy disk sync**: No disk writes until cache reaches 80% capacity.
//!   Early IBD runs at full in-memory speed with zero disk overhead.
//! - **One-time bulk flush**: When cache first reaches 80%, all entries are
//!   flushed to disk in a single batch, enabling safe eviction.
//! - **Pending-only sync**: After bulk flush, sync_block only updates
//!   pending_writes (no redundant cache operations — connect_block already did that).
//! - **Pre-computed tx_ids**: sync_block accepts pre-computed tx_ids to avoid
//!   re-hashing every transaction (already done by connect_block).
//! - **O(1) pending_writes lookup**: HashMap instead of Vec linear scan
//! - **Fixed-size keys**: `[u8; 40]` avoids heap allocation per outpoint
//! - **Batch eviction**: Only evict when 10% over limit, clear 15% headroom

use crate::storage::database::Tree;
use anyhow::Result;
use blvm_consensus::transaction::is_coinbase;
use blvm_consensus::types::{Block, Hash, OutPoint, UTXO, UtxoSet};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Fixed-size outpoint key: 32 bytes txid + 8 bytes index (big-endian)
type OutPointKey = [u8; 40];

/// Serialize an OutPoint to a fixed-size storage key.
/// Zero-allocation: returns a stack-allocated array instead of Vec.
#[inline]
fn outpoint_to_key(outpoint: &OutPoint) -> OutPointKey {
    let mut key = [0u8; 40];
    key[..32].copy_from_slice(&outpoint.hash);
    key[32..40].copy_from_slice(&outpoint.index.to_be_bytes());
    key
}

/// Disk-backed UTXO set with bounded in-memory cache.
///
/// During IBD, the UTXO set grows to tens of millions of entries (>8GB at peak).
/// This struct keeps only a bounded subset in memory and spills the rest to disk.
///
/// ## Two operating modes:
///
/// 1. **In-memory mode** (cache < 80%): Zero disk overhead. connect_block directly
///    modifies the cache. No sync, no prefetch, no eviction.
///
/// 2. **Disk-backed mode** (cache ≥ 80%): Full disk operations.
///    Prefetch → validate → sync → evict cycle per block.
pub struct DiskBackedUtxoSet {
    /// In-memory cache — bounded subset of all UTXOs.
    /// This is passed (via take/restore) to connect_block for validation.
    cache: UtxoSet,

    /// Disk-backed store — ALL UTXOs are persisted here.
    disk: Arc<dyn Tree>,

    /// Maximum number of entries allowed in the in-memory cache.
    max_cache_entries: usize,

    /// Total UTXO count (tracked incrementally to avoid counting disk entries).
    total_utxo_count: usize,

    /// Pending disk writes (batched for performance).
    /// HashMap for O(1) lookup during prefetch instead of O(n) linear scan.
    /// Value: Some(serialized_utxo) for inserts, None for deletes.
    pending_writes: HashMap<OutPointKey, Option<Vec<u8>>>,

    /// Flush threshold — number of pending writes before auto-flushing to disk.
    flush_threshold: usize,

    /// Whether disk operations are needed (transitions from false to true once).
    /// When false: skip sync/prefetch/evict entirely (pure in-memory mode).
    /// When true: full disk-backed mode with prefetch/sync/evict per block.
    needs_disk: bool,

    /// Whether the one-time bulk flush has been done.
    /// The bulk flush writes ALL cache entries to disk so eviction is safe.
    bulk_flushed: bool,

    /// Stats
    pub stats_disk_loads: u64,
    pub stats_cache_hits: u64,
    pub stats_evictions: u64,
    pub stats_pending_hits: u64,
    
    /// Recently accessed outpoints (from last block) - avoid evicting these
    /// This is a simple optimization to prevent evicting UTXOs we just loaded
    recently_accessed: std::collections::HashSet<OutPoint>,
}

impl DiskBackedUtxoSet {
    /// Create a new disk-backed UTXO set.
    pub fn new(
        disk_tree: Arc<dyn Tree>,
        max_cache_entries: usize,
        flush_threshold: usize,
    ) -> Self {
        Self {
            cache: HashMap::with_capacity(max_cache_entries.min(2_000_000)),
            disk: disk_tree,
            max_cache_entries,
            total_utxo_count: 0,
            pending_writes: HashMap::with_capacity(flush_threshold),
            flush_threshold,
            needs_disk: false,
            bulk_flushed: false,
            stats_disk_loads: 0,
            stats_cache_hits: 0,
            stats_evictions: 0,
            stats_pending_hits: 0,
            recently_accessed: std::collections::HashSet::with_capacity(2000), // Typical block has ~2000 inputs
        }
    }

    /// Initialize by counting existing UTXOs on disk (for resuming IBD).
    pub fn initialize_count(&mut self) -> Result<()> {
        self.total_utxo_count = self.disk.len()?;
        if self.total_utxo_count > 0 {
            info!(
                "DiskBackedUtxoSet: Found {} existing UTXOs on disk",
                self.total_utxo_count
            );
            // If there are UTXOs on disk, we're resuming and need disk mode
            self.needs_disk = true;
            self.bulk_flushed = true; // Previous run already flushed
        }
        Ok(())
    }

    /// Check if the cache is approaching capacity and transition to disk mode if needed.
    /// Call this after each block validation.
    ///
    /// Returns true if disk operations are needed (sync/prefetch/evict).
    #[inline]
    pub fn needs_disk_ops(&self) -> bool {
        self.needs_disk
    }

    /// Check cache pressure and transition to disk mode if needed.
    /// Call this every block to detect the 80% threshold.
    pub fn check_pressure(&mut self) -> Result<bool> {
        if self.needs_disk {
            return Ok(true);
        }

        // Transition at 80% capacity
        let threshold = self.max_cache_entries * 4 / 5;
        if self.cache.len() >= threshold {
            info!(
                "DiskBackedUtxoSet: Cache at {}% capacity ({}/{}), transitioning to disk-backed mode",
                self.cache.len() * 100 / self.max_cache_entries,
                self.cache.len(),
                self.max_cache_entries,
            );
            self.needs_disk = true;

            // One-time bulk flush: write all cache entries to disk
            if !self.bulk_flushed {
                self.bulk_flush_cache()?;
                self.bulk_flushed = true;
            }

            return Ok(true);
        }

        // In-memory mode: just track total count from cache size
        self.total_utxo_count = self.cache.len();
        Ok(false)
    }

    /// One-time bulk flush of the entire cache to disk.
    /// This ensures all entries are on disk before eviction begins.
    fn bulk_flush_cache(&mut self) -> Result<()> {
        let count = self.cache.len();
        info!(
            "DiskBackedUtxoSet: Starting one-time bulk flush of {} cache entries to disk...",
            count
        );

        let start = std::time::Instant::now();
        let mut batch = self.disk.batch();

        for (outpoint, utxo) in self.cache.iter() {
            let key = outpoint_to_key(outpoint);
            let value = bincode::serialize(utxo)
                .map_err(|e| anyhow::anyhow!("Failed to serialize UTXO: {}", e))?;
            batch.put(&key, &value);
        }

        batch.commit()?;
        let elapsed = start.elapsed();
        info!(
            "DiskBackedUtxoSet: Bulk flush complete — {} entries in {:.1}s",
            count,
            elapsed.as_secs_f64()
        );

        self.total_utxo_count = count;
        Ok(())
    }

    /// Load a single UTXO from disk by outpoint.
    /// Checks pending_writes first (O(1) HashMap lookup), then disk.
    fn load_from_disk(&mut self, outpoint: &OutPoint) -> Result<Option<UTXO>> {
        let key = outpoint_to_key(outpoint);

        // Check pending writes first — O(1) HashMap lookup
        if let Some(pending_entry) = self.pending_writes.get(&key) {
            self.stats_pending_hits += 1;
            return match pending_entry {
                Some(data) => Ok(Some(bincode::deserialize(data)?)),
                None => Ok(None), // Was deleted in pending
            };
        }

        // Check disk
        match self.disk.get(&key)? {
            Some(data) => Ok(Some(bincode::deserialize(&data)?)),
            None => Ok(None),
        }
    }

    /// Ensure all UTXOs needed by a block's inputs are in the in-memory cache.
    /// Only call when needs_disk_ops() returns true.
    ///
    /// Optimized: Parallelizes disk reads using rayon for better I/O throughput.
    pub fn prefetch_block(&mut self, block: &Block) -> Result<usize> {
        // First pass: collect all outpoints that need loading (not in cache)
        // Optimization: Pre-allocate with capacity hint (estimate inputs per block)
        let est_inputs: usize = block.transactions.iter()
            .filter(|tx| !is_coinbase(tx))
            .map(|tx| tx.inputs.len())
            .sum();
        let mut outpoints_to_load: Vec<OutPoint> = Vec::with_capacity(est_inputs);
        for tx in block.transactions.iter() {
            if is_coinbase(tx) {
                continue;
            }
            for input in tx.inputs.iter() {
                let prevout = &input.prevout;
                // OPTIMIZATION: Compute key once, reuse for both cache and pending_writes checks
                let key = outpoint_to_key(prevout);
                
                // Check cache first
                if self.cache.contains_key(prevout) {
                    self.stats_cache_hits += 1;
                    self.recently_accessed.insert(prevout.clone());
                    continue;
                }
                // Check pending_writes before adding to load list
                if let Some(pending_entry) = self.pending_writes.get(&key) {
                    self.stats_pending_hits += 1;
                    if let Some(data) = pending_entry {
                        if let Ok(utxo) = bincode::deserialize::<UTXO>(data) {
                            self.cache.insert(prevout.clone(), utxo);
                            self.recently_accessed.insert(prevout.clone());
                            self.stats_disk_loads += 1;
                            continue;
                        }
                    }
                    // None = deleted, skip
                    continue;
                }
                outpoints_to_load.push(prevout.clone());
            }
        }

        if outpoints_to_load.is_empty() {
            return Ok(0);
        }

        // OPTIMIZATION: All pending_writes checks already done above, so we only need disk loads
        // This eliminates redundant HashMap lookups and deserialization
        let need_disk_load = outpoints_to_load;
        let mut loaded = 0;

        // Parallel disk reads for better I/O throughput
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            
            // Clone disk reference for parallel access (Arc is thread-safe)
            let disk = Arc::clone(&self.disk);
            
            // Parallel load from disk
            // OPTIMIZATION: Pre-compute keys to avoid redundant outpoint_to_key calls
            let keys_and_outpoints: Vec<(OutPointKey, OutPoint)> = need_disk_load
                .iter()
                .map(|outpoint| (outpoint_to_key(outpoint), outpoint.clone()))
                .collect();
            
            let from_disk: Vec<(OutPoint, UTXO)> = keys_and_outpoints
                .par_iter()
                .filter_map(|(key, outpoint)| {
                    match disk.get(key) {
                        Ok(Some(data)) => {
                            match bincode::deserialize::<UTXO>(&data) {
                                Ok(utxo) => Some((outpoint.clone(), utxo)),
                                Err(_) => None,
                            }
                        }
                        Ok(None) => None,
                        Err(_) => None, // Skip on error
                    }
                })
                .collect();
            
            // Insert all disk-loaded UTXOs into cache and mark as recently accessed
            // OPTIMIZATION: Reuse outpoint after clone (avoid double clone)
            let disk_loaded = from_disk.len();
            for (outpoint, utxo) in from_disk {
                let outpoint_clone = outpoint.clone();
                self.cache.insert(outpoint_clone.clone(), utxo);
                self.recently_accessed.insert(outpoint_clone);
                self.stats_disk_loads += 1;
            }
            
            Ok(disk_loaded)
        }
        
        #[cfg(not(feature = "rayon"))]
        {
            // Sequential fallback
            for outpoint in need_disk_load {
                if let Some(utxo) = self.load_from_disk(&outpoint)? {
                    self.cache.insert(outpoint.clone(), utxo);
                    self.recently_accessed.insert(outpoint);
                    loaded += 1;
                    self.stats_disk_loads += 1;
                }
            }
            Ok(loaded)
        }
    }

    /// Get a mutable reference to the in-memory cache.
    #[inline]
    pub fn cache_mut(&mut self) -> &mut UtxoSet {
        &mut self.cache
    }

    /// Get the number of entries currently in the in-memory cache.
    #[inline]
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Get the total UTXO count (cache + disk-only).
    #[inline]
    pub fn total_len(&self) -> usize {
        if self.needs_disk {
            self.total_utxo_count
        } else {
            self.cache.len()
        }
    }

    /// After block validation, record the block's UTXO changes for disk persistence.
    /// Only updates pending_writes — does NOT touch the cache (connect_block already did that).
    ///
    /// Only call when needs_disk_ops() returns true.
    ///
    /// Uses pre-computed tx_ids to avoid re-hashing every transaction.
    /// Optimized: Parallelizes UTXO serialization for better CPU utilization.
    pub fn sync_block_with_txids(&mut self, block: &Block, height: u64, tx_ids: &[Hash]) -> Result<()> {
        // Collect all operations first
        // Optimization: Pre-allocate with capacity hints to avoid reallocations
        let est_inputs: usize = block.transactions.iter()
            .filter(|tx| !is_coinbase(tx))
            .map(|tx| tx.inputs.len())
            .sum();
        let est_outputs: usize = block.transactions.iter().map(|tx| tx.outputs.len()).sum();
        let mut deletes: Vec<OutPointKey> = Vec::with_capacity(est_inputs);
        let mut inserts: Vec<(OutPointKey, UTXO)> = Vec::with_capacity(est_outputs);
        
        for (tx_idx, tx) in block.transactions.iter().enumerate() {
            let tx_id = tx_ids[tx_idx];
            let is_cb = is_coinbase(tx);

            // Record spent inputs as deletes
            if !is_cb {
                for input in tx.inputs.iter() {
                    deletes.push(outpoint_to_key(&input.prevout));
                    self.total_utxo_count = self.total_utxo_count.saturating_sub(1);
                }
            }

            // Record new outputs — create UTXO directly from block output
            // OPTIMIZATION: Compute key directly without creating OutPoint struct
            for (vout, output) in tx.outputs.iter().enumerate() {
                let mut key = [0u8; 40];
                key[..32].copy_from_slice(&tx_id);
                key[32..].copy_from_slice(&(vout as u64).to_be_bytes());
                
                let utxo = UTXO {
                    value: output.value,
                    script_pubkey: output.script_pubkey.clone(),
                    height,
                    is_coinbase: is_cb,
                };
                inserts.push((key, utxo));
                self.total_utxo_count += 1;
            }
        }

        // Parallel serialize UTXOs for inserts
        #[cfg(feature = "rayon")]
        {
            use rayon::prelude::*;
            
            let serialized_inserts: Vec<(OutPointKey, Vec<u8>)> = inserts
                .par_iter()
                .map(|(key, utxo)| {
                    let value = bincode::serialize(utxo)
                        .map_err(|e| anyhow::anyhow!("Failed to serialize UTXO: {}", e))?;
                    Ok((*key, value))
                })
                .collect::<Result<Vec<_>>>()?;
            
            // Insert deletes
            for key in deletes {
                self.pending_writes.insert(key, None);
            }
            
            // Insert serialized UTXOs
            for (key, value) in serialized_inserts {
                self.pending_writes.insert(key, Some(value));
            }
        }
        
        #[cfg(not(feature = "rayon"))]
        {
            // Insert deletes
            for key in deletes {
                self.pending_writes.insert(key, None);
            }
            
            // Serialize and insert UTXOs sequentially
            for (key, utxo) in inserts {
                let value = bincode::serialize(&utxo)
                    .map_err(|e| anyhow::anyhow!("Failed to serialize UTXO: {}", e))?;
                self.pending_writes.insert(key, Some(value));
            }
        }

        // Auto-flush if threshold reached
        if self.pending_writes.len() >= self.flush_threshold {
            self.flush()?;
        }

        Ok(())
    }

    /// Flush all pending writes to disk in a single batch transaction.
    pub fn flush(&mut self) -> Result<usize> {
        if self.pending_writes.is_empty() {
            return Ok(0);
        }

        let count = self.pending_writes.len();
        let mut batch = self.disk.batch();

        for (key, value_opt) in self.pending_writes.drain() {
            match value_opt {
                Some(value) => batch.put(&key, &value),
                None => batch.delete(&key),
            }
        }

        batch.commit()?;
        debug!("DiskBackedUtxoSet: flushed {} operations to disk", count);
        Ok(count)
    }

    /// Evict entries from the in-memory cache to stay under the memory limit.
    /// Only call when needs_disk_ops() returns true.
    pub fn evict_if_needed(&mut self) -> usize {
        let trigger_threshold = self.max_cache_entries + self.max_cache_entries / 10;
        if self.cache.len() <= trigger_threshold {
            return 0;
        }

        let target = self.max_cache_entries * 9 / 10;
        let to_evict = self.cache.len().saturating_sub(target);

        if to_evict == 0 {
            return 0;
        }

        // Evict entries, but avoid evicting recently accessed UTXOs
        // OPTIMIZATION: Single-pass collection, no redundant HashSet
        let mut keys_to_evict: Vec<OutPoint> = self.cache.keys()
            .filter(|k| !self.recently_accessed.contains(k))
            .take(to_evict)
            .cloned()
            .collect();
        
        // If we couldn't evict enough non-recently-accessed items, evict some recently-accessed ones
        let remaining_to_evict = to_evict.saturating_sub(keys_to_evict.len());
        if remaining_to_evict > 0 {
            let mut additional_keys: Vec<OutPoint> = self.cache.keys()
                .filter(|k| self.recently_accessed.contains(k))
                .take(remaining_to_evict)
                .cloned()
                .collect();
            keys_to_evict.append(&mut additional_keys);
        }
        
        let evicted = keys_to_evict.len();
        for key in &keys_to_evict {
            self.cache.remove(key);
        }
        
        // Clear recently_accessed after eviction (will be repopulated by next prefetch)
        self.recently_accessed.clear();
        self.stats_evictions += evicted as u64;

        debug!(
            "DiskBackedUtxoSet: evicted {} entries (cache: {}/{}, total UTXOs: {})",
            evicted,
            self.cache.len(),
            self.max_cache_entries,
            self.total_utxo_count,
        );

        evicted
    }

    /// Log statistics for monitoring.
    pub fn log_stats(&self, height: u64) {
        info!(
            "UTXO-DISK at height {}: cache={}/{}, total={}, disk_loads={}, cache_hits={}, pending_hits={}, evictions={}, pending_writes={}, mode={}",
            height,
            self.cache.len(),
            self.max_cache_entries,
            self.total_len(),
            self.stats_disk_loads,
            self.stats_cache_hits,
            self.stats_pending_hits,
            self.stats_evictions,
            self.pending_writes.len(),
            if self.needs_disk { "disk" } else { "memory" },
        );
    }
}

impl Drop for DiskBackedUtxoSet {
    fn drop(&mut self) {
        if !self.pending_writes.is_empty() {
            if let Err(e) = self.flush() {
                warn!("DiskBackedUtxoSet: failed to flush on drop: {}", e);
            }
        }
    }
}
