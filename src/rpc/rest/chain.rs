//! Chain endpoints
//!
//! GET /api/v1/chain/tip
//! GET /api/v1/chain/height
//! GET /api/v1/chain/info

use crate::rpc::blockchain::BlockchainRpc;
use anyhow::Result;
use serde_json::Value;

/// Get chain tip hash
pub async fn get_chain_tip(blockchain: &BlockchainRpc) -> Result<Value> {
    let tip_hash = blockchain.get_best_block_hash().await?;
    Ok(tip_hash)
}

/// Get chain height
pub async fn get_chain_height(blockchain: &BlockchainRpc) -> Result<Value> {
    let height = blockchain.get_block_count().await?;
    Ok(height)
}

/// Get comprehensive chain info
pub async fn get_chain_info(blockchain: &BlockchainRpc) -> Result<Value> {
    let info = blockchain.get_blockchain_state().await?;
    Ok(info)
}
