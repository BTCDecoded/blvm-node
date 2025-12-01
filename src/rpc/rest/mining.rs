//! Mining endpoints
//!
//! GET /api/v1/mining/info
//! GET /api/v1/mining/block-template
//! POST /api/v1/mining/blocks

use crate::rpc::mining::MiningRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get mining information
pub async fn get_mining_info(mining: &MiningRpc) -> Result<Value> {
    let info = mining.get_mining_info().await?;
    Ok(info)
}

/// Get block template
pub async fn get_block_template(mining: &MiningRpc, rules: Option<Vec<&str>>, capabilities: Option<Vec<&str>>) -> Result<Value> {
    let mut params = json!([]);
    if let Some(r) = rules {
        params.as_array_mut().unwrap().push(json!(r));
    }
    if let Some(c) = capabilities {
        params.as_array_mut().unwrap().push(json!(c));
    }
    let template = mining.get_block_template(&params).await?;
    Ok(template)
}

/// Submit block
pub async fn submit_block(mining: &MiningRpc, hex: &str) -> Result<Value> {
    let params = json!([hex]);
    let result = mining.submit_block(&params).await?;
    Ok(result)
}

