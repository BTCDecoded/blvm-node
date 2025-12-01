//! Block endpoints
//!
//! GET /api/v1/blocks/{hash}
//! GET /api/v1/blocks/{hash}/transactions
//! GET /api/v1/blocks/{hash}/header
//! GET /api/v1/blocks/{hash}/stats
//! GET /api/v1/blocks/{hash}/filter
//! GET /api/v1/blocks/height/{height}
//! POST /api/v1/blocks/{hash}/invalidate
//! POST /api/v1/blocks/{hash}/reconsider

use crate::rpc::blockchain::BlockchainRpc;
use anyhow::Result;
use serde_json::{json, Value};

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

/// Get block header
pub async fn get_block_header(blockchain: &BlockchainRpc, hash: &str, verbose: bool) -> Result<Value> {
    let header = blockchain.get_block_header(hash, verbose).await?;
    Ok(header)
}

/// Get block statistics
pub async fn get_block_stats(blockchain: &BlockchainRpc, hash_or_height: &str) -> Result<Value> {
    let params = json!([hash_or_height]);
    let stats = blockchain.get_block_stats(&params).await?;
    Ok(stats)
}

/// Get block filter (BIP158)
pub async fn get_block_filter(blockchain: &BlockchainRpc, hash: &str, filtertype: Option<&str>) -> Result<Value> {
    let params = if let Some(ft) = filtertype {
        json!([hash, ft])
    } else {
        json!([hash])
    };
    let filter = blockchain.get_block_filter(&params).await?;
    Ok(filter)
}

/// Invalidate block
pub async fn invalidate_block(blockchain: &BlockchainRpc, hash: &str) -> Result<Value> {
    let params = json!([hash]);
    let result = blockchain.invalidate_block(&params).await?;
    Ok(result)
}

/// Reconsider block
pub async fn reconsider_block(blockchain: &BlockchainRpc, hash: &str) -> Result<Value> {
    let params = json!([hash]);
    let result = blockchain.reconsider_block(&params).await?;
    Ok(result)
}
