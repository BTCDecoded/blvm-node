//! Mempool endpoints
//!
//! GET /api/v1/mempool
//! GET /api/v1/mempool/transactions/{txid}
//! GET /api/v1/mempool/stats

use crate::rpc::mempool::MempoolRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get mempool transactions
pub async fn get_mempool(mempool: &MempoolRpc) -> Result<Value> {
    let params = json!([]);
    let txs = mempool.getrawmempool(&params).await?;
    Ok(txs)
}

/// Get mempool transaction by ID
pub async fn get_mempool_transaction(mempool: &MempoolRpc, txid: &str) -> Result<Value> {
    let params = json!([txid]);
    let entry = mempool.getmempoolentry(&params).await?;
    Ok(entry)
}

/// Get mempool statistics
pub async fn get_mempool_stats(mempool: &MempoolRpc) -> Result<Value> {
    let params = json!([]);
    let info = mempool.getmempoolinfo(&params).await?;
    Ok(info)
}
