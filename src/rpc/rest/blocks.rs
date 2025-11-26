//! Block endpoints
//!
//! GET /api/v1/blocks/{hash}
//! GET /api/v1/blocks/{hash}/transactions
//! GET /api/v1/blocks/height/{height}

use crate::rpc::blockchain::BlockchainRpc;
use anyhow::Result;
use serde_json::Value;

/// Get block by hash
pub async fn get_block_by_hash(blockchain: &BlockchainRpc, hash: &str) -> Result<Value> {
    let block = blockchain.get_block(hash).await?;
    Ok(block)
}

/// Get block by height
pub async fn get_block_by_height(blockchain: &BlockchainRpc, height: u64) -> Result<Value> {
    // First get the block hash for this height
    let block_hash = blockchain.get_block_hash(height).await?;
    let hash_str = block_hash.as_str().unwrap_or("");

    // Then get the block
    let block = blockchain.get_block(hash_str).await?;
    Ok(block)
}

/// Get block transactions
pub async fn get_block_transactions(blockchain: &BlockchainRpc, hash: &str) -> Result<Value> {
    // Get the full block which includes transactions
    let block = blockchain.get_block(hash).await?;

    // Extract transactions from block
    if let Some(tx_array) = block.get("tx") {
        Ok(tx_array.clone())
    } else {
        Ok(Value::Array(vec![]))
    }
}
