//! UTXO set storage implementation
//!
//! Stores and manages the UTXO set for efficient transaction validation.

use crate::storage::database::{Database, Tree};
use anyhow::Result;
use bllvm_protocol::{OutPoint, UtxoSet, UTXO};
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "production")]
use std::sync::{OnceLock, RwLock};

/// UTXO serialization cache (production feature only)
///
/// Caches serialized UTXO bytes to avoid re-serializing the same UTXO.
/// Cache key is the OutPoint hash.
#[cfg(feature = "production")]
static SERIALIZATION_CACHE: OnceLock<RwLock<lru::LruCache<[u8; 32], Vec<u8>>>> = OnceLock::new();

#[cfg(feature = "production")]
fn get_serialization_cache() -> &'static RwLock<lru::LruCache<[u8; 32], Vec<u8>>> {
    SERIALIZATION_CACHE.get_or_init(|| {
        use lru::LruCache;
        use std::num::NonZeroUsize;
        // Cache 20,000 serialized UTXOs (balance between memory and hit rate)
        // Each entry is ~100-200 bytes average, so ~2-4MB total
        RwLock::new(LruCache::new(NonZeroUsize::new(20_000).unwrap()))
    })
}

/// Calculate cache key from OutPoint
#[cfg(feature = "production")]
fn outpoint_cache_key(outpoint: &OutPoint) -> [u8; 32] {
    // Use OutPoint hash directly as cache key (already 32 bytes)
    outpoint.hash
}

/// UTXO set storage manager
pub struct UtxoStore {
    #[allow(dead_code)]
    db: Arc<dyn Database>,
    utxos: Arc<dyn Tree>,
    spent_outputs: Arc<dyn Tree>,
}

impl UtxoStore {
    /// Create a new UTXO store
    pub fn new(db: Arc<dyn Database>) -> Result<Self> {
        let utxos = Arc::from(db.open_tree("utxos")?);
        let spent_outputs = Arc::from(db.open_tree("spent_outputs")?);

        Ok(Self {
            db,
            utxos,
            spent_outputs,
        })
    }

    /// Store the entire UTXO set
    ///
    /// Performance optimization: Caches serialized UTXO bytes to avoid re-serialization
    /// Performance optimization: Parallelizes serialization and caching for large UTXO sets
    pub fn store_utxo_set(&self, utxo_set: &UtxoSet) -> Result<()> {
        // Clear existing UTXOs
        self.utxos.clear()?;

        // Optimization: For large UTXO sets, parallelize serialization
        #[cfg(all(feature = "production", feature = "rayon"))]
        {
            use rayon::prelude::*;

            // Collect all UTXOs into vector for parallel processing
            let utxos: Vec<_> = utxo_set.iter().collect();

            // Parallelize serialization (but not database writes - they must be sequential)
            let serialized_utxos: Vec<_> = utxos
                .par_iter()
                .map(|(outpoint, utxo)| {
                    let key = self.outpoint_key(outpoint);

                    // Check cache first
                    let cache = get_serialization_cache();
                    let cache_key = outpoint_cache_key(outpoint);

                    // Try to get from cache
                    let value = if let Ok(cached) = cache.read() {
                        if let Some(serialized) = cached.peek(&cache_key) {
                            serialized.clone() // Clone cached result
                        } else {
                            // Cache miss - serialize and cache
                            let serialized = bincode::serialize(utxo)
                                .map_err(|e| anyhow::anyhow!("Serialization failed: {}", e))?;

                            // Store in cache
                            if let Ok(mut cache) = cache.write() {
                                cache.put(cache_key, serialized.clone());
                            }

                            serialized
                        }
                    } else {
                        // Cache lock failed - serialize without caching
                        bincode::serialize(utxo)
                            .map_err(|e| anyhow::anyhow!("Serialization failed: {}", e))?
                    };

                    Ok((key, value))
                })
                .collect::<Result<Vec<_>>>()?;

            // Sequential database writes (database operations must be sequential)
            for (key, value) in serialized_utxos {
                self.utxos.insert(&key, &value)?;
            }
        }

        #[cfg(not(all(feature = "production", feature = "rayon")))]
        {
            // Store each UTXO sequentially
            for (outpoint, utxo) in utxo_set {
                let key = self.outpoint_key(outpoint);

                #[cfg(feature = "production")]
                let value = {
                    // Check cache first
                    let cache = get_serialization_cache();
                    let cache_key = outpoint_cache_key(outpoint);

                    // Try to get from cache
                    if let Ok(cached) = cache.read() {
                        if let Some(serialized) = cached.peek(&cache_key) {
                            serialized.clone() // Clone cached result
                        } else {
                            // Cache miss - serialize and cache
                            let serialized = bincode::serialize(utxo)?;

                            // Store in cache
                            if let Ok(mut cache) = cache.write() {
                                cache.put(cache_key, serialized.clone());
                            }

                            serialized
                        }
                    } else {
                        // Cache lock failed - serialize without caching
                        bincode::serialize(utxo)?
                    }
                };

                #[cfg(not(feature = "production"))]
                let value = bincode::serialize(utxo)?;

                self.utxos.insert(&key, &value)?;
            }
        }

        Ok(())
    }

    /// Load the entire UTXO set
    pub fn load_utxo_set(&self) -> Result<UtxoSet> {
        let mut utxo_set = HashMap::new();

        for result in self.utxos.iter() {
            let (key, value) = result?;
            let outpoint = self.outpoint_from_key(&key)?;
            let utxo: UTXO = bincode::deserialize(&value)?;
            utxo_set.insert(outpoint, utxo);
        }

        Ok(utxo_set)
    }

    /// Add a UTXO to the set
    ///
    /// Performance optimization: Caches serialized UTXO bytes
    pub fn add_utxo(&self, outpoint: &OutPoint, utxo: &UTXO) -> Result<()> {
        let key = self.outpoint_key(outpoint);

        #[cfg(feature = "production")]
        let value = {
            // Check cache first
            let cache = get_serialization_cache();
            let cache_key = outpoint_cache_key(outpoint);

            // Try to get from cache
            if let Ok(cached) = cache.read() {
                if let Some(serialized) = cached.peek(&cache_key) {
                    serialized.clone() // Clone cached result
                } else {
                    // Cache miss - serialize and cache
                    let serialized = bincode::serialize(utxo)?;

                    // Store in cache
                    if let Ok(mut cache) = cache.write() {
                        cache.put(cache_key, serialized.clone());
                    }

                    serialized
                }
            } else {
                // Cache lock failed - serialize without caching
                bincode::serialize(utxo)?
            }
        };

        #[cfg(not(feature = "production"))]
        let value = bincode::serialize(utxo)?;

        self.utxos.insert(&key, &value)?;
        Ok(())
    }

    /// Remove a UTXO from the set
    pub fn remove_utxo(&self, outpoint: &OutPoint) -> Result<()> {
        let key = self.outpoint_key(outpoint);
        self.utxos.remove(&key)?;
        Ok(())
    }

    /// Get a UTXO by outpoint
    pub fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>> {
        let key = self.outpoint_key(outpoint);
        if let Some(data) = self.utxos.get(&key)? {
            let utxo: UTXO = bincode::deserialize(&data)?;
            Ok(Some(utxo))
        } else {
            Ok(None)
        }
    }

    /// Check if a UTXO exists
    pub fn has_utxo(&self, outpoint: &OutPoint) -> Result<bool> {
        let key = self.outpoint_key(outpoint);
        self.utxos.contains_key(&key)
    }

    /// Mark an output as spent
    pub fn mark_spent(&self, outpoint: &OutPoint) -> Result<()> {
        let key = self.outpoint_key(outpoint);
        self.spent_outputs.insert(&key, &[])?;
        Ok(())
    }

    /// Get all UTXOs in the set
    pub fn get_all_utxos(&self) -> Result<UtxoSet> {
        self.load_utxo_set()
    }

    /// Check if an output is spent
    pub fn is_spent(&self, outpoint: &OutPoint) -> Result<bool> {
        let key = self.outpoint_key(outpoint);
        self.spent_outputs.contains_key(&key)
    }

    /// Get total number of UTXOs
    pub fn utxo_count(&self) -> Result<usize> {
        self.utxos.len()
    }

    /// Get total UTXO value
    pub fn total_value(&self) -> Result<u64> {
        let mut total = 0u64;

        for result in self.utxos.iter() {
            let (_, value) = result?;
            let utxo: UTXO = bincode::deserialize(&value)?;
            total += utxo.value as u64;
        }

        Ok(total)
    }

    /// Convert outpoint to storage key
    fn outpoint_key(&self, outpoint: &OutPoint) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(&outpoint.hash);
        key.extend_from_slice(&outpoint.index.to_be_bytes());
        key
    }

    /// Convert storage key to outpoint
    fn outpoint_from_key(&self, key: &[u8]) -> Result<OutPoint> {
        if key.len() < 32 + 8 {
            return Err(anyhow::anyhow!("Invalid outpoint key length"));
        }

        let mut hash = [0u8; 32];
        hash.copy_from_slice(&key[0..32]);
        let index = u64::from_be_bytes([
            key[32], key[33], key[34], key[35], key[36], key[37], key[38], key[39],
        ]);

        Ok(OutPoint { hash, index })
    }
}
