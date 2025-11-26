//! Network RPC methods
//!
//! Implements network-related JSON-RPC methods for querying and managing network state.

use crate::network::NetworkManager;
use crate::rpc::errors::{RpcError, RpcResult};
use crate::utils::current_timestamp;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::debug;

/// Network RPC methods
#[derive(Clone)]
pub struct NetworkRpc {
    network_manager: Option<Arc<NetworkManager>>,
}

impl NetworkRpc {
    /// Create a new network RPC handler
    pub fn new() -> Self {
        Self {
            network_manager: None,
        }
    }

    /// Create with dependencies
    pub fn with_dependencies(network_manager: Arc<NetworkManager>) -> Self {
        Self {
            network_manager: Some(network_manager),
        }
    }

    /// Get network information
    pub async fn get_network_info(&self) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getnetworkinfo");

        use std::sync::OnceLock;

        static CACHED_NETWORK_INFO: OnceLock<Value> = OnceLock::new();

        if let Some(ref network) = self.network_manager {
            let peer_count = network.peer_count();

            // Build static template once
            let base_info = CACHED_NETWORK_INFO.get_or_init(|| {
                json!({
                    "version": 70015,
                    "subversion": "/reference-node:0.1.0/",
                    "protocolversion": 70015,
                    "localservices": "0000000000000001",
                    "localrelay": true,
                    "timeoffset": 0,
                    "networkactive": true,
                    "connections": 0,
                    "networks": [
                        {
                            "name": "ipv4",
                            "limited": false,
                            "reachable": true,
                            "proxy": "",
                            "proxy_randomize_credentials": false
                        },
                        {
                            "name": "ipv6",
                            "limited": false,
                            "reachable": true,
                            "proxy": "",
                            "proxy_randomize_credentials": false
                        }
                    ],
                    "relayfee": 0.00001000,
                    "incrementalfee": 0.00001000,
                    "localaddresses": [],
                    "warnings": ""
                })
            });

            // Clone and update only the dynamic field
            let mut result = base_info.clone();
            result["connections"] = json!(peer_count);
            Ok(result)
        } else {
            Ok(json!({
                "version": 70015,
                "subversion": "/reference-node:0.1.0/",
                "protocolversion": 70015,
                "localservices": "0000000000000001",
                "localrelay": true,
                "timeoffset": 0,
                "networkactive": true,
                "connections": 0,
                "networks": [
                    {
                        "name": "ipv4",
                        "limited": false,
                        "reachable": true,
                        "proxy": "",
                        "proxy_randomize_credentials": false
                    },
                    {
                        "name": "ipv6",
                        "limited": false,
                        "reachable": true,
                        "proxy": "",
                        "proxy_randomize_credentials": false
                    }
                ],
                "relayfee": 0.00001000,
                "incrementalfee": 0.00001000,
                "localaddresses": [],
                "warnings": ""
            }))
        }
    }

    /// Get peer information
    pub async fn get_peer_info(&self) -> RpcResult<Value> {
        debug!("RPC: getpeerinfo");

        if let Some(ref network) = self.network_manager {
            let peer_manager = network.peer_manager().await;

            // This avoids: 1) cloning all addresses, 2) looking up each peer again
            let mut peers = Vec::new();
            for addr in peer_manager.peer_addresses() {
                if let Some(peer) = peer_manager.get_peer(&addr) {
                    peers.push(json!({
                        "id": match addr {
                            crate::network::transport::TransportAddr::Tcp(sock) => sock.port() as u64,
                            #[cfg(feature = "quinn")]
                            crate::network::transport::TransportAddr::Quinn(sock) => sock.port() as u64,
                            #[cfg(feature = "iroh")]
                            crate::network::transport::TransportAddr::Iroh(_) => 0u64,
                        },
                        "addr": addr.to_string(),
                        "addrlocal": "",
                        "services": "0000000000000001",
                        "relaytxes": true,
                        "lastsend": peer.last_send(),
                        "lastrecv": peer.last_recv(),
                        "bytessent": peer.bytes_sent(),
                        "bytesrecv": peer.bytes_recv(),
                        "conntime": peer.conntime(),
                        "timeoffset": 0,
                        "pingtime": 0.0,
                        "minping": 0.0,
                        "version": 70015,
                        "subver": "/reference-node:0.1.0/",
                        "inbound": false,
                        "addnode": false,
                        "startingheight": 0,
                        "synced_headers": -1,
                        "synced_blocks": -1,
                        "inflight": [],
                        "whitelisted": false,
                        "minfeefilter": 0.00001000,
                        "bytessent_per_msg": {},
                        "bytesrecv_per_msg": {}
                    }));
                }
            }
            Ok(json!(peers))
        } else {
            Ok(json!([]))
        }
    }

    /// Get connection count
    ///
    /// Params: []
    pub async fn get_connection_count(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getconnectioncount");

        if let Some(ref network) = self.network_manager {
            Ok(Value::Number(serde_json::Number::from(
                network.peer_count(),
            )))
        } else {
            Ok(Value::Number(serde_json::Number::from(0)))
        }
    }

    /// Ping connected peers
    ///
    /// Params: []
    pub async fn ping(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: ping");

        // Core's ping RPC just sets a flag, actual ping happens in network thread
        // Network manager should handle ping in background task if needed

        Ok(Value::Null)
    }

    /// Add a node to connect to
    ///
    /// Params: ["node", "command"]
    /// command can be: "add", "remove", "onetry"
    pub async fn add_node(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: addnode");

        let node = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("node", Some("string (IP:port)")))?;

        let command = params.get(1).and_then(|p| p.as_str()).unwrap_or("add");

        // Parse node address
        let addr: SocketAddr = node.parse().map_err(|_| {
            RpcError::invalid_params_with_fields(
                format!("Invalid node address: {node}"),
                vec![("node", "Must be in format IP:port (e.g., 192.168.1.1:8333)")],
                Some(json!([
                    "Format: IPv4:port or [IPv6]:port",
                    "Example: 192.168.1.1:8333 or [2001:db8::1]:8333"
                ])),
            )
        })?;

        if let Some(ref mut network) = self.network_manager.as_ref() {
            match command {
                "add" => {
                    network.add_persistent_peer(addr);
                    debug!("Added node {} to persistent peer list", addr);
                    Ok(Value::Null)
                }
                "remove" => {
                    network.remove_persistent_peer(addr);
                    debug!("Removed node {} from persistent peer list", addr);
                    Ok(Value::Null)
                }
                "onetry" => {
                    // Try to connect to node once
                    if let Err(e) = network.connect_to_peer(addr).await {
                        return Err(RpcError::internal_error(format!(
                            "Failed to connect to {addr}: {e}"
                        )));
                    }
                    debug!("Connected to node {} (onetry)", addr);
                    Ok(Value::Null)
                }
                _ => Err(RpcError::invalid_params(format!(
                    "Invalid command: {command}. Must be 'add', 'remove', or 'onetry'"
                ))),
            }
        } else {
            match command {
                "add" | "remove" | "onetry" => Ok(Value::Null),
                _ => Err(RpcError::invalid_params(format!(
                    "Invalid command: {command}. Must be 'add', 'remove', or 'onetry'"
                ))),
            }
        }
    }

    /// Disconnect a specific node
    ///
    /// Params: ["address"]
    pub async fn disconnect_node(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: disconnectnode");

        let address = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("address", Some("string (IP:port)")))?;

        let addr: SocketAddr = address.parse().map_err(|_| {
            RpcError::invalid_params_with_fields(
                format!("Invalid address: {address}"),
                vec![(
                    "address",
                    "Must be in format IP:port (e.g., 192.168.1.1:8333)",
                )],
                Some(json!([
                    "Format: IPv4:port or [IPv6]:port",
                    "Example: 192.168.1.1:8333 or [2001:db8::1]:8333"
                ])),
            )
        })?;

        if let Some(ref network) = self.network_manager {
            // Send disconnect message to network manager
            // The network manager will handle peer removal via PeerDisconnected message
            let peer_manager = network.peer_manager().await;
            use crate::network::transport::TransportAddr;
            let transport_addr = TransportAddr::Tcp(addr);
            if peer_manager.get_peer(&transport_addr).is_some() {
                // Send disconnect signal - peer will be removed in process_messages
                // This is handled by the peer's connection closing naturally
                debug!("Disconnect peer {} requested", addr);
                // Note: Actual disconnection happens when peer connection closes
                // For immediate disconnect, we'd need to add a disconnect method to Peer
            } else {
                debug!("Peer {} not found", addr);
            }
        }
        Ok(Value::Null)
    }

    /// Get network totals (bytes sent/received)
    ///
    /// Params: []
    pub async fn get_net_totals(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: getnettotals");

        if let Some(ref network) = self.network_manager {
            let stats = network.get_network_stats().await;
            Ok(json!({
                "totalbytesrecv": stats.bytes_received,
                "totalbytessent": stats.bytes_sent,
                "activeconnections": stats.active_connections,
                "bannedpeers": stats.banned_peers,
                "messagequeuesize": 0, // Would need to track this separately
                "timemillis": current_timestamp() as u128 * 1000
            }))
        } else {
            Ok(json!({
            "totalbytesrecv": 0,
            "totalbytessent": 0,
            "activeconnections": 0,
            "bannedpeers": 0,
            "messagequeuesize": 0,
            "timemillis": current_timestamp() * 1000
            }))
        }
    }

    /// Get DoS protection information
    ///
    /// Params: []
    pub async fn get_dos_protection_info(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: getdosprotectioninfo");

        if let Some(network) = self.network_manager.as_ref() {
            let dos_protection = network.dos_protection();
            let dos_metrics = dos_protection.get_dos_metrics().await;
            let dos_config = dos_protection.get_config().await;

            let metrics = crate::node::metrics::DosMetrics {
                connection_rate_violations: dos_metrics.connection_rate_violations,
                auto_bans: dos_metrics.auto_bans_applied,
                message_queue_overflows: dos_metrics.message_queue_overflows,
                active_connection_limit_hits: dos_metrics.active_connection_limit_hits,
                resource_exhaustion_events: dos_metrics.resource_exhaustion_events,
            };

            Ok(json!({
                "metrics": {
                    "connection_rate_violations": metrics.connection_rate_violations,
                    "auto_bans": metrics.auto_bans,
                    "message_queue_overflows": metrics.message_queue_overflows,
                    "active_connection_limit_hits": metrics.active_connection_limit_hits,
                    "resource_exhaustion_events": metrics.resource_exhaustion_events,
                },
                "config": {
                    "max_connections_per_window": dos_config.max_connections_per_window,
                    "window_seconds": dos_config.window_seconds,
                    "max_message_queue_size": dos_config.max_message_queue_size,
                    "max_active_connections": dos_config.max_active_connections,
                    "auto_ban_connection_violations": dos_config.auto_ban_connection_violations,
                }
            }))
        } else {
            Ok(json!({
                "error": "Network manager not available"
            }))
        }
    }

    /// Clear banned nodes
    ///
    /// Params: []
    pub async fn clear_banned(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: clearbanned");

        if let Some(ref network) = self.network_manager {
            network.clear_bans();
            debug!("Cleared all bans");
        }

        Ok(Value::Null)
    }

    /// Ban a node
    ///
    /// Params: ["subnet", "command", "bantime", "absolute"]
    pub async fn set_ban(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: setban");

        let subnet = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::missing_parameter("subnet", Some("string (IP:port)")))?;

        let command = params.get(1).and_then(|p| p.as_str()).unwrap_or("add");

        // Parse address/subnet
        let addr: SocketAddr = subnet.parse().map_err(|_| {
            RpcError::invalid_params_with_fields(
                format!("Invalid address/subnet: {subnet}"),
                vec![(
                    "subnet",
                    "Must be in format IP:port (e.g., 192.168.1.1:8333)",
                )],
                Some(json!([
                    "Format: IPv4:port or [IPv6]:port",
                    "Example: 192.168.1.1:8333 or [2001:db8::1]:8333"
                ])),
            )
        })?;

        // Parse bantime (seconds) - 0 = permanent
        let bantime = params.get(2).and_then(|p| p.as_u64()).unwrap_or(86400); // Default 24 hours

        // Parse absolute (whether bantime is absolute timestamp or relative)
        let absolute = params.get(3).and_then(|p| p.as_bool()).unwrap_or(false);

        if let Some(ref network) = self.network_manager {
            let now = current_timestamp();

            let unban_timestamp = if absolute {
                bantime // Already a timestamp
            } else if bantime == 0 {
                0 // Permanent ban
            } else {
                now + bantime // Relative ban
            };

            match command {
                "add" => {
                    network.ban_peer(addr, unban_timestamp);
                    debug!("Banned peer {} until {}", addr, unban_timestamp);
                    Ok(Value::Null)
                }
                "remove" => {
                    network.unban_peer(addr);
                    debug!("Unbanned peer {}", addr);
                    Ok(Value::Null)
                }
                _ => Err(RpcError::invalid_params(format!(
                    "Invalid command: {command}. Must be 'add' or 'remove'"
                ))),
            }
        } else {
            match command {
                "add" | "remove" => Ok(json!(null)),
                _ => Err(RpcError::invalid_params(format!(
                    "Invalid command: {command}. Must be 'add' or 'remove'"
                ))),
            }
        }
    }

    /// List banned nodes
    ///
    /// Params: []
    pub async fn list_banned(&self, _params: &Value) -> RpcResult<Value> {
        debug!("RPC: listbanned");

        if let Some(ref network) = self.network_manager {
            let banned = network.get_banned_peers();
            let result: Vec<Value> = banned
                .iter()
                .map(|(addr, unban_timestamp)| {
                    json!({
                        "address": addr.to_string(),
                        "banned_until": if *unban_timestamp == u64::MAX {
                            serde_json::Value::Null // Permanent ban
                        } else {
                            serde_json::Value::Number((*unban_timestamp).into())
                        },
                        "banned_until_absolute": *unban_timestamp == u64::MAX
                    })
                })
                .collect();
            Ok(json!(result))
        } else {
            Ok(json!([]))
        }
    }

    /// Get added node information
    ///
    /// Params: ["node", "dns"] (node address, optional dns flag)
    pub async fn getaddednodeinfo(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getaddednodeinfo");

        let node = params
            .get(0)
            .and_then(|p| p.as_str())
            .ok_or_else(|| RpcError::invalid_params("Node address required".to_string()))?;

        let dns = params.get(1).and_then(|p| p.as_bool()).unwrap_or(false);

        if let Some(ref network) = self.network_manager {
            // Parse node address
            let addr: SocketAddr = node
                .parse()
                .map_err(|e| RpcError::invalid_params(format!("Invalid node address: {e}")))?;

            // Check if node is in persistent peer list
            let persistent_peers = network.get_persistent_peers().await;
            let _is_added = persistent_peers.contains(&addr);

            // Get connection status
            let peer_count = network.peer_count();
            let is_connected = peer_count > 0; // Simplified - would check actual connection

            Ok(json!([{
                "addednode": node,
                "connected": is_connected,
                "addresses": if dns {
                    vec![json!({
                        "address": node,
                        "connected": is_connected
                    })]
                } else {
                    vec![json!({
                        "address": addr.to_string(),
                        "connected": is_connected
                    })]
                }
            }]))
        } else {
            Ok(json!([{
                "addednode": node,
                "connected": false,
                "addresses": []
            }]))
        }
    }

    /// Get node addresses
    ///
    /// Params: ["count"] (optional, default: 1)
    pub async fn getnodeaddresses(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getnodeaddresses");

        let count = params.get(0).and_then(|p| p.as_u64()).unwrap_or(1).min(100) as usize; // Limit to 100

        if let Some(ref network) = self.network_manager {
            // Get peer addresses
            let peer_addrs = network.get_peer_addresses().await;

            // Convert to node address format
            let mut addresses = Vec::new();
            for addr in peer_addrs.into_iter().take(count) {
                match addr {
                    crate::network::transport::TransportAddr::Tcp(sock) => {
                        addresses.push(json!({
                            "time": current_timestamp(),
                            "services": "0000000000000001",
                            "address": sock.ip().to_string(),
                            "port": sock.port(),
                            "network": if sock.is_ipv4() { "ipv4" } else { "ipv6" }
                        }));
                    }
                    #[cfg(feature = "quinn")]
                    crate::network::transport::TransportAddr::Quinn(sock) => {
                        addresses.push(json!({
                            "time": current_timestamp(),
                            "services": "0000000000000001",
                            "address": sock.ip().to_string(),
                            "port": sock.port(),
                            "network": if sock.is_ipv4() { "ipv4" } else { "ipv6" }
                        }));
                    }
                    #[cfg(feature = "iroh")]
                    crate::network::transport::TransportAddr::Iroh(_) => {
                        // Skip Iroh peers for this method (no SocketAddr)
                    }
                }
            }

            Ok(json!(addresses))
        } else {
            Ok(json!([]))
        }
    }

    /// Set network active state
    ///
    /// Params: ["state"] (true to enable, false to disable)
    pub async fn setnetworkactive(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: setnetworkactive");

        let state = params.get(0).and_then(|p| p.as_bool()).ok_or_else(|| {
            RpcError::invalid_params("State parameter required (true/false)".to_string())
        })?;

        if let Some(ref network) = self.network_manager {
            network.set_network_active(state).await.map_err(|e| {
                RpcError::internal_error(format!("Failed to set network active: {e}"))
            })?;
            Ok(json!(state))
        } else {
            Err(RpcError::internal_error(
                "Network manager not available".to_string(),
            ))
        }
    }
}

impl Default for NetworkRpc {
    fn default() -> Self {
        Self::new()
    }
}
