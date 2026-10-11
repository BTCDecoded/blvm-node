//! Fee estimation endpoints
//!
//! GET /api/v1/fees/estimate

use crate::rpc::mining::MiningRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get fee estimate
pub async fn get_fee_estimate(mining: &MiningRpc, blocks: Option<u64>) -> Result<Value> {
    let params = if let Some(blocks) = blocks {
        json!([blocks])
    } else {
        json!([6]) // Default to 6 blocks
    };
    let estimate = mining.estimate_smart_fee(&params).await?;
    Ok(estimate)
}
