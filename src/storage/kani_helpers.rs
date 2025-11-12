//! Kani proof helpers for storage layer verification
//!
//! Provides mock database implementations and helpers for formal verification
//! of storage operations using Kani model checking.

#[cfg(kani)]
pub mod kani_mocks {
    use super::super::database::{Database, Tree};
    use anyhow::Result;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Mock database implementation for Kani proofs
    ///
    /// Uses in-memory HashMap for verification, avoiding actual database operations.
    pub struct MockDatabase {
        trees: Arc<Mutex<HashMap<String, Arc<MockTree>>>>,
    }

    impl MockDatabase {
        pub fn new() -> Self {
            Self {
                trees: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    impl Database for MockDatabase {
        fn open_tree(&self, name: &str) -> Result<Box<dyn Tree>> {
            let mut trees = self.trees.lock().unwrap();
            let tree = trees
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(MockTree::new()))
                .clone();
            Ok(Box::new(MockTreeWrapper { inner: tree }))
        }

        fn flush(&self) -> Result<()> {
            // No-op for mock
            Ok(())
        }
    }

    /// Mock tree implementation using HashMap
    struct MockTree {
        data: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    }

    impl MockTree {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    /// Wrapper to make MockTree implement Tree trait
    struct MockTreeWrapper {
        inner: Arc<MockTree>,
    }

    impl Tree for MockTreeWrapper {
        fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
            self.inner
                .data
                .lock()
                .unwrap()
                .insert(key.to_vec(), value.to_vec());
            Ok(())
        }

        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            Ok(self.inner.data.lock().unwrap().get(key).cloned())
        }

        fn remove(&self, key: &[u8]) -> Result<()> {
            self.inner.data.lock().unwrap().remove(key);
            Ok(())
        }

        fn contains_key(&self, key: &[u8]) -> Result<bool> {
            Ok(self.inner.data.lock().unwrap().contains_key(key))
        }

        fn clear(&self) -> Result<()> {
            self.inner.data.lock().unwrap().clear();
            Ok(())
        }

        fn len(&self) -> Result<usize> {
            Ok(self.inner.data.lock().unwrap().len())
        }

        fn iter(&self) -> Box<dyn Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_> {
            let data = self.inner.data.lock().unwrap();
            let items: Vec<_> = data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Box::new(items.into_iter().map(|(k, v)| Ok((k, v))))
        }
    }
}

/// Storage proof limits for Kani
///
/// These limits bound proof input sizes for tractability.
pub mod proof_limits {
    /// Maximum UTXOs in a set for proof tractability
    pub const MAX_UTXO_COUNT_FOR_PROOF: usize = 10;

    /// Maximum outpoint index value
    pub const MAX_OUTPOINT_INDEX: u32 = 1000;
}

/// Standard unwind bounds for storage operations
pub mod unwind_bounds {
    /// Simple UTXO operations (single lookup/insert)
    pub const SIMPLE_UTXO: u32 = 3;

    /// Complex UTXO operations (iterations, bulk operations)
    pub const COMPLEX_UTXO: u32 = 10;

    /// UTXO set operations (full set iteration)
    pub const UTXO_SET: u32 = 15;
}

#[cfg(kani)]
pub use kani_mocks::MockDatabase;
