//! Node API implementation for modules
//!
//! Provides a NodeAPI implementation that modules can use to query the node state.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::module::ipc::protocol::ModuleMessage;
use crate::module::traits::{EventType, ModuleError, NodeAPI};
use crate::storage::Storage;
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Node API implementation for modules
pub struct NodeApiImpl {
    /// Storage reference for querying blockchain data
    storage: Arc<Storage>,
}

impl NodeApiImpl {
    /// Create a new Node API implementation
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl NodeAPI for NodeApiImpl {
    async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError> {
        // Query block store (synchronous operation, but we're in async context)
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage
                    .blocks()
                    .get_block(&hash)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get block: {}", e)))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        // Query block store for header
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().get_header(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get block header: {}", e))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        // Query transaction index
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().get_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get transaction: {}", e))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError> {
        // Check if transaction exists in index
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().has_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!(
                        "Failed to check transaction existence: {}",
                        e
                    ))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        // Get block height from chain state
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .chain()
                    .get_height()
                    .map_err(|e| {
                        ModuleError::OperationError(format!("Failed to get block height: {}", e))
                    })?
                    .ok_or_else(|| {
                        ModuleError::OperationError("Chain not yet initialized".to_string())
                    })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        // Get chain tip from chain state
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .chain()
                    .get_tip_hash()
                    .map_err(|e| {
                        ModuleError::OperationError(format!("Failed to get chain tip: {}", e))
                    })?
                    .ok_or_else(|| {
                        ModuleError::OperationError("Chain not yet initialized".to_string())
                    })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        // Query UTXO store (read-only)
        // Note: This is read-only, modules cannot modify UTXO set
        let outpoint_clone = outpoint.clone();
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .utxos()
                    .get_utxo(&outpoint_clone)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get UTXO: {}", e)))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {}", e)))?
    }

    async fn subscribe_events(
        &self,
        _event_types: Vec<EventType>,
    ) -> Result<mpsc::Receiver<ModuleMessage>, ModuleError> {
        // Create event subscription channel
        // TODO: Integrate with actual event system when implemented
        let (_tx, rx) = mpsc::channel(100);

        // For now, return empty receiver
        // In full implementation, this would hook into the node's event system
        Ok(rx)
    }
}
