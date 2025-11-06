//! Storage layer for reference-node
//!
//! This module provides persistent storage for blocks, UTXO set, and chain state.
//! Uses sled for embedded database storage.

pub mod blockstore;
pub mod chainstate;
pub mod hashing;
pub mod txindex;
pub mod utxostore;

use anyhow::Result;
use sled::Db;
use std::path::Path;

/// Storage manager that coordinates all storage operations
pub struct Storage {
    db: Db,
    blockstore: blockstore::BlockStore,
    utxostore: utxostore::UtxoStore,
    chainstate: chainstate::ChainState,
    txindex: txindex::TxIndex,
}

impl Storage {
    /// Create a new storage instance
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Result<Self> {
        let db = sled::open(data_dir)?;

        let blockstore = blockstore::BlockStore::new(db.clone())?;
        let utxostore = utxostore::UtxoStore::new(db.clone())?;
        let chainstate = chainstate::ChainState::new(db.clone())?;
        let txindex = txindex::TxIndex::new(db.clone())?;

        Ok(Self {
            db,
            blockstore,
            utxostore,
            chainstate,
            txindex,
        })
    }

    /// Get the block store
    pub fn blocks(&self) -> &blockstore::BlockStore {
        &self.blockstore
    }

    /// Get the UTXO store
    pub fn utxos(&self) -> &utxostore::UtxoStore {
        &self.utxostore
    }

    /// Get the chain state
    pub fn chain(&self) -> &chainstate::ChainState {
        &self.chainstate
    }

    /// Get the transaction index
    pub fn transactions(&self) -> &txindex::TxIndex {
        &self.txindex
    }

    /// Flush all pending writes to disk
    pub fn flush(&self) -> Result<()> {
        self.db.flush()?;
        Ok(())
    }
}
