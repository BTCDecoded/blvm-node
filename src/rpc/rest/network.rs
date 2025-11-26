//! Network endpoints
//!
//! GET /api/v1/network/info
//! GET /api/v1/network/peers

use crate::rpc::network::NetworkRpc;
use anyhow::Result;
use serde_json::{json, Value};

/// Get network information
pub async fn get_network_info(network: &NetworkRpc) -> Result<Value> {
    let info = network.get_network_info().await?;
    Ok(info)
}

/// Get peer information
pub async fn get_network_peers(network: &NetworkRpc) -> Result<Value> {
    let peers = network.get_peer_info().await?;
    Ok(peers)
}
