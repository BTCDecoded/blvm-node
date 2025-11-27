//! Blockchain read-only API for modules
//!
//! Provides a convenient, read-only interface for modules to query blockchain data.
//! All operations are read-only and cannot modify consensus state.

use std::sync::Arc;

use crate::module::traits::ModuleError;
use crate::storage::chainstate::{ChainInfo, ChainParams};
use crate::storage::{blockstore::BlockMetadata, Storage};
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Blockchain API for modules
///
/// Provides read-only access to blockchain data including blocks, transactions,
/// chain state, and UTXO information. All operations are safe for modules as
/// they cannot modify consensus state.
pub struct BlockchainApi {
    /// Storage reference for querying blockchain data
    storage: Arc<Storage>,
}

impl BlockchainApi {
    /// Create a new blockchain API
    pub fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }

    /// Get a block by hash
    pub async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage
                    .blocks()
                    .get_block(&hash)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get block: {e}")))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get a block header by hash
    pub async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().get_header(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get block header: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get a block by height
    pub async fn get_block_by_height(&self, height: u64) -> Result<Option<Block>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                // First get hash by height
                let hash = storage.blocks().get_hash_by_height(height).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get hash by height: {e}"))
                })?;

                if let Some(hash) = hash {
                    // Then get block by hash
                    storage.blocks().get_block(&hash).map_err(|e| {
                        ModuleError::OperationError(format!("Failed to get block: {e}"))
                    })
                } else {
                    Ok(None)
                }
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get block hash by height
    pub async fn get_hash_by_height(&self, height: u64) -> Result<Option<Hash>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage.blocks().get_hash_by_height(height).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get hash by height: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get block height by hash
    pub async fn get_height_by_hash(&self, hash: &Hash) -> Result<Option<u64>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().get_height_by_hash(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get height by hash: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get blocks in a height range
    pub async fn get_blocks_by_height_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<Block>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage
                    .blocks()
                    .get_blocks_by_height_range(start, end)
                    .map_err(|e| {
                        ModuleError::OperationError(format!(
                            "Failed to get blocks by height range: {e}"
                        ))
                    })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get block metadata (TX count, etc.) without loading full block
    pub async fn get_block_metadata(
        &self,
        hash: &Hash,
    ) -> Result<Option<BlockMetadata>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().get_block_metadata(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get block metadata: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Check if a block exists
    pub async fn has_block(&self, hash: &Hash) -> Result<bool, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.blocks().has_block(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to check block existence: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get total number of blocks stored
    pub async fn block_count(&self) -> Result<usize, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage.blocks().block_count().map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get block count: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get recent headers for median time-past calculation
    pub async fn get_recent_headers(&self, count: usize) -> Result<Vec<BlockHeader>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage.blocks().get_recent_headers(count).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get recent headers: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get a transaction by hash
    pub async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().get_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get transaction: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Check if a transaction exists
    pub async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let hash = *hash;
            move || {
                storage.transactions().has_transaction(&hash).map_err(|e| {
                    ModuleError::OperationError(format!(
                        "Failed to check transaction existence: {e}"
                    ))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get UTXO by outpoint
    pub async fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let outpoint = outpoint.clone();
            move || {
                storage
                    .utxos()
                    .get_utxo(&outpoint)
                    .map_err(|e| ModuleError::OperationError(format!("Failed to get UTXO: {e}")))
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Check if a UTXO exists
    pub async fn has_utxo(&self, outpoint: &OutPoint) -> Result<bool, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            let outpoint = outpoint.clone();
            move || {
                storage.utxos().has_utxo(&outpoint).map_err(|e| {
                    ModuleError::OperationError(format!("Failed to check UTXO existence: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get current chain information
    pub async fn get_chain_info(&self) -> Result<Option<ChainInfo>, ModuleError> {
        tokio::task::spawn_blocking({
            let storage = Arc::clone(&self.storage);
            move || {
                storage.chain().load_chain_info().map_err(|e| {
                    ModuleError::OperationError(format!("Failed to get chain info: {e}"))
                })
            }
        })
        .await
        .map_err(|e| ModuleError::OperationError(format!("Task join error: {e}")))?
    }

    /// Get current chain tip (highest block hash)
    pub async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        let chain_info = self.get_chain_info().await?;
        chain_info
            .map(|info| info.tip_hash)
            .ok_or_else(|| ModuleError::OperationError("Chain not initialized".to_string()))
    }

    /// Get current block height
    pub async fn get_block_height(&self) -> Result<u64, ModuleError> {
        let chain_info = self.get_chain_info().await?;
        chain_info
            .map(|info| info.height)
            .ok_or_else(|| ModuleError::OperationError("Chain not initialized".to_string()))
    }

    /// Get chain parameters
    pub async fn get_chain_params(&self) -> Result<Option<ChainParams>, ModuleError> {
        let chain_info = self.get_chain_info().await?;
        Ok(chain_info.map(|info| info.chain_params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use tempfile::TempDir;

    fn create_test_storage() -> (TempDir, Arc<Storage>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
        (temp_dir, storage)
    }

    #[tokio::test]
    async fn test_blockchain_api_creation() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        // Should create successfully
        assert!(true);
    }

    #[tokio::test]
    async fn test_get_block_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.get_block(&hash).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_block_header_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.get_block_header(&hash).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_block_by_height_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_block_by_height(0).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_hash_by_height_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_hash_by_height(0).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_height_by_hash_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.get_height_by_hash(&hash).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_blocks_by_height_range_empty() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_blocks_by_height_range(0, 10).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_get_block_metadata_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.get_block_metadata(&hash).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_has_block_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.has_block(&hash).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_block_count_empty() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.block_count().await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_get_recent_headers_empty() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_recent_headers(10).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_get_transaction_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.get_transaction(&hash).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_has_transaction_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let hash = [0u8; 32];

        let result = api.has_transaction(&hash).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_get_utxo_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let outpoint = OutPoint {
            hash: [0u8; 32],
            index: 0,
        };

        let result = api.get_utxo(&outpoint).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_has_utxo_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);
        let outpoint = OutPoint {
            hash: [0u8; 32],
            index: 0,
        };

        let result = api.has_utxo(&outpoint).await;
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_get_chain_info_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_chain_info().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_get_chain_tip_not_initialized() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_chain_tip().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::OperationError(msg) => {
                assert!(msg.contains("Chain not initialized"));
            }
            _ => panic!("Expected OperationError"),
        }
    }

    #[tokio::test]
    async fn test_get_block_height_not_initialized() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_block_height().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            ModuleError::OperationError(msg) => {
                assert!(msg.contains("Chain not initialized"));
            }
            _ => panic!("Expected OperationError"),
        }
    }

    #[tokio::test]
    async fn test_get_chain_params_nonexistent() {
        let (_temp_dir, storage) = create_test_storage();
        let api = BlockchainApi::new(storage);

        let result = api.get_chain_params().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
