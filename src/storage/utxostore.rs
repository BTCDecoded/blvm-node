//! UTXO set storage implementation
//!
//! Stores and manages the UTXO set for efficient transaction validation.

use anyhow::Result;
use protocol_engine::{OutPoint, UtxoSet, UTXO};
use sled::Db;
use std::collections::HashMap;

/// UTXO set storage manager
pub struct UtxoStore {
    #[allow(dead_code)]
    db: Db,
    utxos: sled::Tree,
    spent_outputs: sled::Tree,
}

impl UtxoStore {
    /// Create a new UTXO store
    pub fn new(db: Db) -> Result<Self> {
        let utxos = db.open_tree("utxos")?;
        let spent_outputs = db.open_tree("spent_outputs")?;

        Ok(Self {
            db,
            utxos,
            spent_outputs,
        })
    }

    /// Store the entire UTXO set
    pub fn store_utxo_set(&self, utxo_set: &UtxoSet) -> Result<()> {
        // Clear existing UTXOs
        self.utxos.clear()?;

        // Store each UTXO
        for (outpoint, utxo) in utxo_set {
            let key = self.outpoint_key(outpoint);
            let value = bincode::serialize(utxo)?;
            self.utxos.insert(key, value)?;
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
    pub fn add_utxo(&self, outpoint: &OutPoint, utxo: &UTXO) -> Result<()> {
        let key = self.outpoint_key(outpoint);
        let value = bincode::serialize(utxo)?;
        self.utxos.insert(key, value)?;
        Ok(())
    }

    /// Remove a UTXO from the set
    pub fn remove_utxo(&self, outpoint: &OutPoint) -> Result<()> {
        let key = self.outpoint_key(outpoint);
        self.utxos.remove(key)?;
        Ok(())
    }

    /// Get a UTXO by outpoint
    pub fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>> {
        let key = self.outpoint_key(outpoint);
        if let Some(data) = self.utxos.get(key)? {
            let utxo: UTXO = bincode::deserialize(&data)?;
            Ok(Some(utxo))
        } else {
            Ok(None)
        }
    }

    /// Check if a UTXO exists
    pub fn has_utxo(&self, outpoint: &OutPoint) -> Result<bool> {
        let key = self.outpoint_key(outpoint);
        Ok(self.utxos.contains_key(key)?)
    }

    /// Mark an output as spent
    pub fn mark_spent(&self, outpoint: &OutPoint) -> Result<()> {
        let key = self.outpoint_key(outpoint);
        self.spent_outputs.insert(key, &[])?;
        Ok(())
    }

    /// Get all UTXOs in the set
    pub fn get_all_utxos(&self) -> Result<UtxoSet> {
        self.load_utxo_set()
    }

    /// Check if an output is spent
    pub fn is_spent(&self, outpoint: &OutPoint) -> Result<bool> {
        let key = self.outpoint_key(outpoint);
        Ok(self.spent_outputs.contains_key(key)?)
    }

    /// Get total number of UTXOs
    pub fn utxo_count(&self) -> Result<usize> {
        Ok(self.utxos.len())
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
