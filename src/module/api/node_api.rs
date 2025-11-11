//! Node API implementation for modules
//!
//! Provides a NodeAPI implementation that modules can use to query the node state.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::module::api::events::EventManager;
use crate::module::ipc::protocol::ModuleMessage;
use crate::module::traits::{EventType, ModuleError, NodeAPI};
use crate::storage::Storage;
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Node API implementation for modules
pub struct NodeApiImpl {
    /// Storage reference for querying blockchain data
    storage: Arc<Storage>,
    /// Event manager for event subscriptions
    event_manager: Option<Arc<EventManager>>,
    /// Module ID for this API instance (used for event subscriptions)
    module_id: Option<String>,
}

impl NodeApiImpl {
    /// Create a new Node API implementation
    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            event_manager: None,
            module_id: None,
        }
    }

    /// Create a new Node API implementation with event manager
    pub fn with_event_manager(
        storage: Arc<Storage>,
        event_manager: Arc<EventManager>,
        module_id: String,
    ) -> Self {
        Self {
            storage,
            event_manager: Some(event_manager),
            module_id: Some(module_id),
        }
    }

    /// Set event manager (for late initialization)
    pub fn set_event_manager(&mut self, event_manager: Arc<EventManager>, module_id: String) {
        self.event_manager = Some(event_manager);
        self.module_id = Some(module_id);
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
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
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    async fn subscribe_events(
        &self,
        event_types: Vec<EventType>,
    ) -> Result<mpsc::Receiver<ModuleMessage>, ModuleError> {
        // Create event subscription channel
        let (tx, rx) = mpsc::channel(100);

        // Integrate with event manager if available
        if let (Some(event_manager), Some(module_id)) = (&self.event_manager, &self.module_id) {
            // Register module with event manager
            event_manager
                .subscribe_module(module_id.clone(), event_types, tx)
                .await?;
        } else {
            // Event manager not available - return empty receiver
            // This can happen if NodeAPI is used without event manager setup
            // (e.g., in tests or direct API usage)
            tracing::debug!(
                "Event manager not available for subscribe_events - returning empty receiver"
            );
        }

        Ok(rx)
    }
}
