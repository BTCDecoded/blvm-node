//! Chain endpoints
//!
//! GET /api/v1/chain/tip
//! GET /api/v1/chain/height
//! GET /api/v1/chain/info
//! GET /api/v1/chain/difficulty
//! GET /api/v1/chain/utxo-set
//! GET /api/v1/chain/tips
//! GET /api/v1/chain/tx-stats?nblocks={n}
//! GET /api/v1/chain/prune-info
//! GET /api/v1/indexes
//! POST /api/v1/chain/verify
//! POST /api/v1/chain/prune

use crate::rpc::blockchain::BlockchainRpc;
use anyhow::Result;
use serde_json::{json, Value};

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

/// Get chain difficulty
pub async fn get_chain_difficulty(blockchain: &BlockchainRpc) -> Result<Value> {
    let difficulty = blockchain.get_difficulty().await?;
    Ok(difficulty)
}

/// Get UTXO set info
pub async fn get_utxo_set_info(blockchain: &BlockchainRpc) -> Result<Value> {
    let info = blockchain.get_txoutset_info().await?;
    Ok(info)
}

/// Get chain tips
pub async fn get_chain_tips(blockchain: &BlockchainRpc) -> Result<Value> {
    let tips = blockchain.get_chain_tips().await?;
    Ok(tips)
}

/// Get chain transaction statistics
pub async fn get_chain_tx_stats(blockchain: &BlockchainRpc, nblocks: Option<u64>) -> Result<Value> {
    let params = if let Some(n) = nblocks {
        json!([n])
    } else {
        json!([])
    };
    let stats = blockchain.get_chain_tx_stats(&params).await?;
    Ok(stats)
}

/// Get prune info
pub async fn get_prune_info(blockchain: &BlockchainRpc) -> Result<Value> {
    let params = json!([]);
    let info = blockchain.get_prune_info(&params).await?;
    Ok(info)
}

/// Get index info
pub async fn get_index_info(blockchain: &BlockchainRpc, index_name: Option<&str>) -> Result<Value> {
    let params = if let Some(name) = index_name {
        json!([name])
    } else {
        json!([])
    };
    let info = blockchain.get_index_info(&params).await?;
    Ok(info)
}

/// Verify chain
pub async fn verify_chain(
    blockchain: &BlockchainRpc,
    checklevel: Option<u64>,
    numblocks: Option<u64>,
) -> Result<Value> {
    let params = json!([]);
    let result = blockchain.verify_chain(checklevel, numblocks).await?;
    Ok(result)
}

/// Prune blockchain
pub async fn prune_blockchain(blockchain: &BlockchainRpc, height: u64) -> Result<Value> {
    let params = json!([height]);
    let result = blockchain.prune_blockchain(&params).await?;
    Ok(result)
}
