//! Address endpoints
//!
//! GET /api/v1/addresses/{address}/balance
//! GET /api/v1/addresses/{address}/transactions
//! GET /api/v1/addresses/{address}/utxos

use crate::rpc::blockchain::BlockchainRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get address balance
pub async fn get_address_balance(blockchain: &BlockchainRpc, address: &str) -> Result<Value> {
    let params = json!([address]);
    let balance = blockchain
        .getaddressbalance(&params)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get address balance: {}", e))?;
    Ok(balance)
}

/// Get address transactions
pub async fn get_address_transactions(blockchain: &BlockchainRpc, address: &str) -> Result<Value> {
    let params = json!([address]);
    let txids = blockchain
        .getaddresstxids(&params)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get address transactions: {}", e))?;
    Ok(txids)
}

/// Get address UTXOs
pub async fn get_address_utxos(blockchain: &BlockchainRpc, address: &str) -> Result<Value> {
    // Get address info which includes UTXO count
    let params = json!([address]);
    let info = blockchain.get_address_info(&params).await?;

    // Extract relevant fields
    Ok(json!({
        "address": address,
        "balance": info.get("balance"),
        "utxo_count": info.get("utxo_count"),
        "tx_count": info.get("tx_count")
    }))
}
