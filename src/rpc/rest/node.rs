//! Node control endpoints
//!
//! GET /api/v1/node/uptime
//! GET /api/v1/node/memory
//! GET /api/v1/node/rpc-info
//! GET /api/v1/node/help
//! GET /api/v1/node/logging
//! POST /api/v1/node/stop
//! POST /api/v1/node/logging

use crate::rpc::control::ControlRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get node uptime
pub async fn get_uptime(control: &ControlRpc) -> Result<Value> {
    let params = json!([]);
    let uptime = control.uptime(&params).await?;
    Ok(uptime)
}

/// Get memory info
pub async fn get_memory_info(control: &ControlRpc, mode: Option<&str>) -> Result<Value> {
    let params = if let Some(m) = mode {
        json!([m])
    } else {
        json!([])
    };
    let info = control.getmemoryinfo(&params).await?;
    Ok(info)
}

/// Get RPC info
pub async fn get_rpc_info(control: &ControlRpc) -> Result<Value> {
    let params = json!([]);
    let info = control.getrpcinfo(&params).await?;
    Ok(info)
}

/// Get help
pub async fn get_help(control: &ControlRpc, command: Option<&str>) -> Result<Value> {
    let params = if let Some(cmd) = command {
        json!([cmd])
    } else {
        json!([])
    };
    let help = control.help(&params).await?;
    Ok(help)
}

/// Get logging status
pub async fn get_logging(control: &ControlRpc) -> Result<Value> {
    let params = json!([]);
    let logging = control.logging(&params).await?;
    Ok(logging)
}

/// Set logging
pub async fn set_logging(control: &ControlRpc, include: Option<Vec<String>>, exclude: Option<Vec<String>>) -> Result<Value> {
    let mut params = json!([]);
    if let Some(inc) = include {
        params.as_array_mut().unwrap().push(json!(inc));
    }
    if let Some(exc) = exclude {
        params.as_array_mut().unwrap().push(json!(exc));
    }
    let logging = control.logging(&params).await?;
    Ok(logging)
}

/// Stop node
pub async fn stop_node(control: &ControlRpc) -> Result<Value> {
    let params = json!([]);
    let result = control.stop(&params).await?;
    Ok(result)
}

