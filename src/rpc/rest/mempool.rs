//! Mempool endpoints
//!
//! GET /api/v1/mempool
//! GET /api/v1/mempool/transactions/{txid}
//! GET /api/v1/mempool/transactions/{txid}/ancestors
//! GET /api/v1/mempool/transactions/{txid}/descendants
//! GET /api/v1/mempool/stats
//! POST /api/v1/mempool/save
//! POST /api/v1/mempool/transactions/{txid}/priority

use crate::rpc::mempool::MempoolRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get mempool transactions
pub async fn get_mempool(mempool: &MempoolRpc, verbose: bool) -> Result<Value> {
    let params = json!([verbose]);
    let txs = mempool.getrawmempool(&params).await?;
    Ok(txs)
}

/// Get mempool transaction by ID
pub async fn get_mempool_transaction(mempool: &MempoolRpc, txid: &str) -> Result<Value> {
    let params = json!([txid]);
    let entry = mempool.getmempoolentry(&params).await?;
    Ok(entry)
}

/// Get mempool transaction ancestors
pub async fn get_mempool_ancestors(mempool: &MempoolRpc, txid: &str, verbose: bool) -> Result<Value> {
    let params = json!([txid, verbose]);
    let ancestors = mempool.getmempoolancestors(&params).await?;
    Ok(ancestors)
}

/// Get mempool transaction descendants
pub async fn get_mempool_descendants(mempool: &MempoolRpc, txid: &str, verbose: bool) -> Result<Value> {
    let params = json!([txid, verbose]);
    let descendants = mempool.getmempooldescendants(&params).await?;
    Ok(descendants)
}

/// Get mempool statistics
pub async fn get_mempool_stats(mempool: &MempoolRpc) -> Result<Value> {
    let params = json!([]);
    let info = mempool.getmempoolinfo(&params).await?;
    Ok(info)
}

/// Save mempool to disk
pub async fn save_mempool(mempool: &MempoolRpc) -> Result<Value> {
    let params = json!([]);
    let result = mempool.savemempool(&params).await?;
    Ok(result)
}

/// Prioritize transaction in mempool
pub async fn prioritize_transaction(mempool: &MempoolRpc, txid: &str, fee_delta: f64) -> Result<Value> {
    // Note: prioritisetransaction is in mining.rs, but we'll call it through RPC server
    // For now, return error indicating it needs to be implemented
    Err(anyhow::anyhow!("prioritisetransaction not yet exposed via REST API"))
}
