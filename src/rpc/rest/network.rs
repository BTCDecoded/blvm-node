//! Network endpoints
//!
//! GET /api/v1/network/info
//! GET /api/v1/network/peers
//! GET /api/v1/network/connections/count
//! GET /api/v1/network/totals
//! GET /api/v1/network/nodes
//! GET /api/v1/network/nodes/addresses
//! GET /api/v1/network/bans
//! POST /api/v1/network/ping
//! POST /api/v1/network/nodes
//! POST /api/v1/network/active
//! POST /api/v1/network/bans
//! DELETE /api/v1/network/nodes/{address}
//! DELETE /api/v1/network/bans

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

/// Get connection count
pub async fn get_connection_count(network: &NetworkRpc) -> Result<Value> {
    let params = json!([]);
    let count = network.get_connection_count(&params).await?;
    Ok(count)
}

/// Get network totals
pub async fn get_net_totals(network: &NetworkRpc) -> Result<Value> {
    let params = json!([]);
    let totals = network.get_net_totals(&params).await?;
    Ok(totals)
}

/// Get added node info
pub async fn get_added_node_info(network: &NetworkRpc, node: Option<&str>, dns: bool) -> Result<Value> {
    let params = if let Some(n) = node {
        json!([n, dns])
    } else {
        json!([dns])
    };
    let info = network.getaddednodeinfo(&params).await?;
    Ok(info)
}

/// Get node addresses
pub async fn get_node_addresses(network: &NetworkRpc, count: Option<u32>) -> Result<Value> {
    let params = if let Some(c) = count {
        json!([c])
    } else {
        json!([])
    };
    let addresses = network.getnodeaddresses(&params).await?;
    Ok(addresses)
}

/// List banned nodes
pub async fn list_banned(network: &NetworkRpc) -> Result<Value> {
    let params = json!([]);
    let banned = network.list_banned(&params).await?;
    Ok(banned)
}

/// Ping network
pub async fn ping_network(network: &NetworkRpc) -> Result<Value> {
    let params = json!([]);
    let result = network.ping(&params).await?;
    Ok(result)
}

/// Add node
pub async fn add_node(network: &NetworkRpc, address: &str, command: &str) -> Result<Value> {
    let params = json!([address, command]);
    let result = network.add_node(&params).await?;
    Ok(result)
}

/// Disconnect node
pub async fn disconnect_node(network: &NetworkRpc, address: &str) -> Result<Value> {
    let params = json!([address]);
    let result = network.disconnect_node(&params).await?;
    Ok(result)
}

/// Set network active
pub async fn set_network_active(network: &NetworkRpc, state: bool) -> Result<Value> {
    let params = json!([state]);
    let result = network.setnetworkactive(&params).await?;
    Ok(result)
}

/// Set ban
pub async fn set_ban(network: &NetworkRpc, subnet: &str, command: &str, bantime: Option<u64>, absolute: Option<bool>) -> Result<Value> {
    let mut params = json!([subnet, command]);
    if let Some(bt) = bantime {
        params.as_array_mut().unwrap().push(json!(bt));
    }
    if let Some(abs) = absolute {
        params.as_array_mut().unwrap().push(json!(abs));
    }
    let result = network.set_ban(&params).await?;
    Ok(result)
}

/// Clear banned nodes
pub async fn clear_banned(network: &NetworkRpc) -> Result<Value> {
    let params = json!([]);
    let result = network.clear_banned(&params).await?;
    Ok(result)
}
