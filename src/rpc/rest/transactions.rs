//! Transaction endpoints
//!
//! GET  /api/v1/transactions/{txid}
//! GET  /api/v1/transactions/{txid}/confirmations
//! POST /api/v1/transactions (submit transaction)

use crate::rpc::rawtx::RawTxRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get transaction by ID
pub async fn get_transaction(rawtx: &RawTxRpc, txid: &str) -> Result<Value> {
    let params = json!([txid, false]); // false = don't include hex by default
    let details = rawtx.get_transaction_details(&params).await?;
    Ok(details)
}

/// Get transaction confirmations
pub async fn get_transaction_confirmations(rawtx: &RawTxRpc, txid: &str) -> Result<Value> {
    let params = json!([txid, false]);
    let details = rawtx.get_transaction_details(&params).await?;

    // Extract confirmations from the details
    let confirmations = details
        .get("confirmations")
        .and_then(|c| c.as_u64())
        .unwrap_or(0);

    Ok(json!({
        "txid": txid,
        "confirmations": confirmations,
        "blockhash": details.get("blockhash"),
        "blocktime": details.get("blocktime")
    }))
}

/// Submit a raw transaction
pub async fn submit_transaction(rawtx: &RawTxRpc, hex: &str) -> Result<Value> {
    let params = json!([hex]);
    let result = rawtx.sendrawtransaction(&params).await?;
    Ok(result)
}
