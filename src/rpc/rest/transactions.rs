//! Transaction endpoints
//!
//! GET  /api/v1/transactions/{txid}
//! GET  /api/v1/transactions/{txid}/confirmations
//! GET  /api/v1/transactions/{txid}/outputs/{n}
//! POST /api/v1/transactions (submit transaction)
//! POST /api/v1/transactions/test (test mempool acceptance)
//! POST /api/v1/transactions/decode (decode raw transaction)
//! POST /api/v1/transactions/create (create raw transaction)

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

/// Get transaction output
pub async fn get_transaction_output(
    rawtx: &RawTxRpc,
    txid: &str,
    n: u32,
    include_mempool: bool,
) -> Result<Value> {
    let params = json!([txid, n, include_mempool]);
    let result = rawtx.gettxout(&params).await?;
    Ok(result)
}

/// Submit a raw transaction
pub async fn submit_transaction(rawtx: &RawTxRpc, hex: &str) -> Result<Value> {
    let params = json!([hex]);
    let result = rawtx.sendrawtransaction(&params).await?;
    Ok(result)
}

/// Test if transaction would be accepted to mempool
pub async fn test_transaction(rawtx: &RawTxRpc, hex: &str) -> Result<Value> {
    let params = json!([hex]);
    let result = rawtx.testmempoolaccept(&params).await?;
    Ok(result)
}

/// Decode a raw transaction
pub async fn decode_transaction(rawtx: &RawTxRpc, hex: &str) -> Result<Value> {
    let params = json!([hex]);
    let result = rawtx.decoderawtransaction(&params).await?;
    Ok(result)
}

/// Create a raw transaction
pub async fn create_transaction(
    rawtx: &RawTxRpc,
    inputs: Value,
    outputs: Value,
    locktime: Option<u64>,
    replaceable: Option<bool>,
    version: Option<u32>,
) -> Result<Value> {
    let mut params = json!([inputs, outputs]);
    if let Some(lt) = locktime {
        params.as_array_mut().unwrap().push(json!(lt));
    }
    if let Some(rbf) = replaceable {
        params.as_array_mut().unwrap().push(json!(rbf));
    }
    if let Some(v) = version {
        params.as_array_mut().unwrap().push(json!(v));
    }
    let result = rawtx.createrawtransaction(&params).await?;
    Ok(result)
}
