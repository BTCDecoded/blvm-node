//! Network RPC methods
//!
//! Implements network-related JSON-RPC methods for querying and managing network state.

use crate::network::NetworkManager;
use crate::rpc::errors::{RpcError, RpcResult};
use crate::rpc::params::{
    param_bool, param_bool_default, param_str, param_str_required, param_u64_default,
};
use crate::utils::current_timestamp;
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tracing::debug;

fn services_without_a_network_manager() -> u64 {
    use blvm_protocol::bip157::NODE_COMPACT_FILTERS;
    use blvm_protocol::service_flags::standard;

    let mut services = standard::NODE_WITNESS | standard::NODE_NETWORK | NODE_COMPACT_FILTERS;
    #[cfg(feature = "utxo-commitments")]
    {
        services |= crate::network::protocol::NODE_UTXO_COMMITMENTS;
    }
    services
}

fn parse_ban_ip(subnet: &str) -> RpcResult<IpAddr> {
    if let Ok(sock) = subnet.parse::<SocketAddr>() {
        return Ok(sock.ip());
    }
    subnet.parse::<IpAddr>().map_err(|_| {
        RpcError::invalid_params(format!("setban: {subnet} is not an IP address or IP:port"))
    })
}

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

        let (services, relayfee, connections, networkactive, subversion) =
            if let Some(ref network) = self.network_manager {
                (
                    network.advertised_services(),
                    network.relay_fee_btc_per_kvb(),
                    network.peer_count(),
                    network.is_network_active(),
                    format!("/BitcoinCommons:{}/", env!("CARGO_PKG_VERSION")),
                )
            } else {
                (
                    services_without_a_network_manager(),
                    0.00001,
                    0,
                    false,
                    "/blvm-node:0.1.0/".to_string(),
                )
            };

        Ok(json!({
            "version": 70015,
            "subversion": subversion,
            "protocolversion": 70015,
            "localservices": format!("{services:016x}"),
            "localrelay": true,
            "timeoffset": 0,
            "networkactive": networkactive,
            "connections": connections,
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
            "relayfee": relayfee,
            "incrementalfee": relayfee,
            "localaddresses": [],
            "warnings": ""
        }))
    }

    /// Get peer information
    pub async fn get_peer_info(&self) -> RpcResult<Value> {
        debug!("RPC: getpeerinfo");

        if let Some(ref network) = self.network_manager {
            let filters = {
                let states = network.peer_states().read().await;
                states
                    .iter()
                    .map(|(addr, state)| (*addr, (state.relay_tx, state.min_fee_rate)))
                    .collect::<std::collections::HashMap<_, _>>()
            };
            let persistent = network.get_persistent_peers().await;
            let now = current_timestamp();
            let peer_manager = network.peer_manager().await;
            let mut peers = Vec::new();
            for addr in peer_manager.peer_addresses() {
                if let Some(peer) = peer_manager.get_peer(&addr) {
                    let sock = match addr {
                        crate::network::transport::TransportAddr::Tcp(sock) => Some(sock),
                        #[cfg(feature = "quinn")]
                        crate::network::transport::TransportAddr::Quinn(sock) => Some(sock),
                        #[cfg(feature = "iroh")]
                        crate::network::transport::TransportAddr::Iroh(_) => None,
                    };
                    let state = sock.and_then(|sock| filters.get(&sock));
                    let relaytxes = state.map(|(relay, _)| *relay).unwrap_or(true);
                    let minfeefilter = state
                        .and_then(|(_, fee)| *fee)
                        .map(|sats| sats as f64 / 100_000_000.0)
                        .unwrap_or(0.0);
                    peers.push(json!({
                        "id": peer.id(),
                        "addr": addr.to_string(),
                        "addrlocal": "",
                        "services": format!("{:016x}", peer.services()),
                        "relaytxes": relaytxes,
                        "lastsend": peer.last_send(),
                        "lastrecv": peer.last_recv(),
                        "bytessent": peer.bytes_sent(),
                        "bytesrecv": peer.bytes_recv(),
                        "conntime": peer.conntime(),
                        "timeoffset": peer.time_offset(now),
                        "pingtime": peer.ping_secs(),
                        "minping": 0.0,
                        "version": peer.version() as i64,
                        "subver": peer.user_agent().unwrap_or(&"/unknown/".to_string()).clone(),
                        "inbound": !peer.is_outbound(),
                        "addnode": sock.map(|addr| persistent.contains(&addr)).unwrap_or(false),
                        "startingheight": peer.start_height(),
                        "synced_headers": peer.synced_block_height(),
                        "synced_blocks": peer.synced_block_height(),
                        "inflight": [],
                        "whitelisted": false,
                        "minfeefilter": minfeefilter,
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
    /// Sends a P2P ping message to all connected peers. The ping message includes
    /// a nonce that peers must echo back in a pong message.
    ///
    /// This method handles all transport types (TCP, Quinn, Iroh) via
    /// `NetworkManager::ping_all_peers()`. Each transport sends the same
    /// Bitcoin protocol ping message.
    ///
    /// Params: []
    /// Returns: null (Bitcoin Core compatible)
    pub async fn ping(&self, _params: &Value) -> RpcResult<Value> {
        #[cfg(debug_assertions)]
        debug!("RPC: ping");

        if let Some(ref network) = self.network_manager {
            network
                .ping_all_peers()
                .await
                .map_err(|e| RpcError::internal_error(format!("Failed to ping peers: {e}")))?;
        }

        Ok(Value::Null)
    }

    /// Add a node to connect to
    ///
    /// Params: ["node", "command"]
    /// command can be: "add", "remove", "onetry"
    pub async fn add_node(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: addnode");

        let node = param_str_required(params, 0, "addnode")?;

        let command = param_str(params, 1).unwrap_or("add");

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
                    if let Err(e) = network.connect_to_peer(addr).await {
                        debug!("addnode add stored {addr}; dial failed: {e}");
                    }
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

        let Some(ref network) = self.network_manager else {
            return Err(RpcError::invalid_params(format!(
                "Peer {address} is not connected"
            )));
        };
        use crate::network::transport::TransportAddr;
        let transport_addr = TransportAddr::Tcp(addr);
        let mut peer_manager = network.peer_manager().await;
        if peer_manager.get_peer(&transport_addr).is_none() {
            return Err(RpcError::invalid_params(format!(
                "Peer {address} is not connected"
            )));
        }
        peer_manager.remove_peer(&transport_addr);
        drop(peer_manager);
        network.peer_states().write().await.remove(&addr);
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
            let messagequeuesize = network
                .dos_protection()
                .get_metrics()
                .await
                .message_queue_size;
            Ok(json!({
                "totalbytesrecv": stats.bytes_received,
                "totalbytessent": stats.bytes_sent,
                "activeconnections": stats.active_connections,
                "bannedpeers": stats.banned_peers,
                "messagequeuesize": messagequeuesize,
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

        let subnet = param_str_required(params, 0, "setban")?;

        let command = param_str(params, 1).unwrap_or("add");

        if subnet.contains('/') {
            return Err(RpcError::invalid_params(
                "setban: a subnet is not accepted until a prefix parser exists".to_string(),
            ));
        }
        let ip = parse_ban_ip(&subnet)?;
        let addr = SocketAddr::new(ip, 0);

        // Parse bantime (seconds) - 0 = permanent
        let bantime = param_u64_default(params, 2, 86400); // Default 24 hours

        // Parse absolute (whether bantime is absolute timestamp or relative)
        let absolute = param_bool_default(params, 3, false);

        if let Some(ref network) = self.network_manager {
            let now = current_timestamp();

            let unban_timestamp = if absolute {
                bantime
            } else if bantime == 0 {
                0
            } else {
                now.checked_add(bantime).ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "setban: bantime {bantime} overflows the unban time"
                    ))
                })?
            };

            match command {
                "add" => {
                    if absolute && bantime == 0 {
                        network.unban_peer(addr);
                        return Ok(Value::Null);
                    }
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
        } else if command == "add" || command == "remove" {
            Err(RpcError::internal_error(
                "setban: network manager is not available".to_string(),
            ))
        } else {
            Err(RpcError::invalid_params(format!(
                "Invalid command: {command}. Must be 'add' or 'remove'"
            )))
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

        let requested = param_str(params, 0).map(str::to_string);
        let _dns = param_bool_default(params, 1, false);

        let Some(ref network) = self.network_manager else {
            return Ok(json!([]));
        };

        let persistent_peers = network.get_persistent_peers().await;
        let targets: Vec<SocketAddr> = if let Some(node) = requested.as_deref() {
            let addr: SocketAddr = node
                .parse()
                .map_err(|e| RpcError::invalid_params(format!("Invalid node address: {e}")))?;
            if !persistent_peers.contains(&addr) {
                return Ok(json!([]));
            }
            vec![addr]
        } else {
            persistent_peers.into_iter().collect()
        };

        let peer_manager = network.peer_manager().await;
        use crate::network::transport::TransportAddr;
        let mut rows = Vec::new();
        for addr in targets {
            let transport_addr = TransportAddr::Tcp(addr);
            let is_connected = peer_manager
                .get_peer(&transport_addr)
                .map(|peer| peer.is_connected())
                .unwrap_or(false);
            let shown = requested.clone().unwrap_or_else(|| addr.to_string());
            rows.push(json!({
                "addednode": shown,
                "connected": is_connected,
                "addresses": [{
                    "address": addr.to_string(),
                    "connected": is_connected
                }]
            }));
        }
        Ok(json!(rows))
    }

    /// Get node addresses
    ///
    /// Params: ["count"] (optional, default: 1)
    pub async fn getnodeaddresses(&self, params: &Value) -> RpcResult<Value> {
        debug!("RPC: getnodeaddresses");

        let count = param_u64_default(params, 0, 1).min(100) as usize; // Limit to 100

        if let Some(ref network) = self.network_manager {
            let peer_manager = network.peer_manager().await;
            let mut addresses = Vec::new();
            for addr in peer_manager.peer_addresses().into_iter().take(count) {
                let services = peer_manager
                    .get_peer(&addr)
                    .map(|peer| peer.services())
                    .unwrap_or(0);
                let sock = match addr {
                    crate::network::transport::TransportAddr::Tcp(sock) => Some(sock),
                    #[cfg(feature = "quinn")]
                    crate::network::transport::TransportAddr::Quinn(sock) => Some(sock),
                    #[cfg(feature = "iroh")]
                    crate::network::transport::TransportAddr::Iroh(_) => None,
                };
                let Some(sock) = sock else {
                    continue;
                };
                addresses.push(json!({
                    "time": current_timestamp(),
                    "services": format!("{services:016x}"),
                    "address": sock.ip().to_string(),
                    "port": sock.port(),
                    "network": if sock.is_ipv4() { "ipv4" } else { "ipv6" }
                }));
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

        let state = param_bool(params, 0).ok_or_else(|| {
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

#[cfg(test)]
mod info_locks {
    use super::*;
    use crate::network::NetworkManager;
    use crate::network::peer::Peer;
    use crate::network::transport::TransportAddr;

    #[tokio::test(flavor = "multi_thread")]
    async fn networkactive_follows_setnetworkactive() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let rpc = NetworkRpc::with_dependencies(Arc::clone(&nm));
        rpc.setnetworkactive(&json!([false])).await.unwrap();
        let info = rpc.get_network_info().await.unwrap();
        assert_eq!(info["networkactive"], false);
    }

    #[tokio::test]
    async fn missing_manager_is_not_network_active() {
        let info = NetworkRpc::new().get_network_info().await.unwrap();
        assert_eq!(info["networkactive"], false);
        assert_eq!(info["connections"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn advertised_service_bit_is_shown() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let sock: SocketAddr = "10.1.1.1:8333".parse().unwrap();
        let mut peer = Peer::new_for_testing(sock);
        peer.set_services(8);
        {
            let mut peers = nm.peer_manager().await;
            peers.add_peer(TransportAddr::Tcp(sock), peer).unwrap();
        }
        let info = NetworkRpc::with_dependencies(nm)
            .get_peer_info()
            .await
            .unwrap();
        assert_eq!(info[0]["services"], "0000000000000008");
        assert_ne!(info[0]["id"].as_u64(), Some(sock.port() as u64));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_info_uses_start_height_offset_and_addnode() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let sock: SocketAddr = "1.2.3.4:8333".parse().unwrap();
        nm.add_persistent_peer(sock);
        let mut peer = Peer::new_for_testing(sock);
        peer.set_start_height(800_000);
        peer.set_version_time(1_700_000_100);
        {
            let mut peers = nm.peer_manager().await;
            peers.add_peer(TransportAddr::Tcp(sock), peer).unwrap();
        }
        let info = NetworkRpc::with_dependencies(Arc::clone(&nm))
            .get_peer_info()
            .await
            .unwrap();
        assert_eq!(info[0]["synced_blocks"], 800_000);
        assert_eq!(info[0]["addnode"], true);
        let offset = info[0]["timeoffset"].as_i64().unwrap();
        let expected = 1_700_000_100 - current_timestamp() as i64;
        assert!((offset - expected).abs() <= 2, "{offset} vs {expected}");
    }

    #[tokio::test]
    async fn setban_rejects_a_subnet_and_a_missing_manager() {
        let rpc = NetworkRpc::new();
        assert!(rpc.set_ban(&json!(["10.0.0.0/8", "add"])).await.is_err());
        assert!(rpc.set_ban(&json!(["10.0.0.1", "add"])).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn localservices_and_relayfee_follow_the_node() {
        use crate::node::mempool::MempoolManager;
        use crate::storage::Storage;
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(dir.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_relay_fee_rate = 5;
        mempool.set_policy_config(Some(policy));
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap())
                .with_dependencies(protocol, storage, mempool),
        );
        let info = NetworkRpc::with_dependencies(Arc::clone(&nm))
            .get_network_info()
            .await
            .unwrap();
        assert_eq!(
            info["localservices"].as_str().unwrap(),
            format!("{:016x}", nm.advertised_services())
        );
        assert!((info["relayfee"].as_f64().unwrap() - 0.00005).abs() < 1e-12);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn incrementalfee_follows_the_relay_floor() {
        use crate::node::mempool::MempoolManager;
        use crate::storage::Storage;
        use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

        let dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(Storage::new(dir.path()).unwrap());
        let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
        let mempool = Arc::new(MempoolManager::new());
        let mut policy = crate::config::mempool::MempoolPolicyConfig::default();
        policy.min_relay_fee_rate = 5;
        mempool.set_policy_config(Some(policy));
        let nm = Arc::new(
            NetworkManager::new("127.0.0.1:0".parse().unwrap())
                .with_dependencies(protocol, storage, mempool),
        );
        let info = NetworkRpc::with_dependencies(nm)
            .get_network_info()
            .await
            .unwrap();
        assert!((info["incrementalfee"].as_f64().unwrap() - 0.00005).abs() < 1e-12);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_relay_feefilter_and_address_services() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let quiet: SocketAddr = "1.2.3.4:8333".parse().unwrap();
        let open: SocketAddr = "1.2.3.5:8333".parse().unwrap();
        let mut quiet_peer = Peer::new_for_testing(quiet);
        quiet_peer.set_services(0x0600_0009);
        let mut open_peer = Peer::new_for_testing(open);
        open_peer.set_services(1);
        {
            let mut peers = nm.peer_manager().await;
            peers
                .add_peer(TransportAddr::Tcp(quiet), quiet_peer)
                .unwrap();
            peers.add_peer(TransportAddr::Tcp(open), open_peer).unwrap();
        }
        let mut state = blvm_protocol::network::PeerState::new();
        state.relay_tx = false;
        state.min_fee_rate = Some(1500);
        nm.peer_states().write().await.insert(quiet, state);

        let rpc = NetworkRpc::with_dependencies(Arc::clone(&nm));
        let info = rpc.get_peer_info().await.unwrap();
        let rows = info.as_array().unwrap();
        let quiet_row = rows
            .iter()
            .find(|row| row["addr"].as_str().unwrap().contains("1.2.3.4"))
            .unwrap();
        let open_row = rows
            .iter()
            .find(|row| row["addr"].as_str().unwrap().contains("1.2.3.5"))
            .unwrap();
        assert_eq!(quiet_row["relaytxes"], false);
        assert!((quiet_row["minfeefilter"].as_f64().unwrap() - 0.000015).abs() < 1e-12);
        assert_eq!(open_row["minfeefilter"].as_f64(), Some(0.0));

        let addrs = rpc.getnodeaddresses(&json!([10])).await.unwrap();
        let quiet_addr = addrs
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["address"] == "1.2.3.4")
            .unwrap();
        assert_eq!(quiet_addr["services"], "0000000006000009");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn added_peers_disconnect_queue_and_bans() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let sock: SocketAddr = "1.2.3.4:8333".parse().unwrap();
        let rpc = NetworkRpc::with_dependencies(Arc::clone(&nm));

        assert!(
            rpc.getaddednodeinfo(&json!(["1.2.3.4:8333"]))
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        nm.add_persistent_peer(sock);
        {
            let mut peers = nm.peer_manager().await;
            peers
                .add_peer(TransportAddr::Tcp(sock), Peer::new_for_testing(sock))
                .unwrap();
        }
        let added = rpc
            .getaddednodeinfo(&json!(["1.2.3.4:8333"]))
            .await
            .unwrap();
        assert_eq!(added[0]["connected"], true);
        let listed = crate::rpc::rest::network::get_added_node_info(&rpc, None, false)
            .await
            .unwrap();
        assert!(listed.to_string().contains("1.2.3.4:8333"));

        rpc.disconnect_node(&json!(["1.2.3.4:8333"])).await.unwrap();
        assert!(
            rpc.get_peer_info()
                .await
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(rpc.disconnect_node(&json!(["9.9.9.9:8333"])).await.is_err());

        nm.dos_protection().update_metrics(0, 10, 0, 0).await;
        let totals = rpc.get_net_totals(&json!([])).await.unwrap();
        assert_eq!(totals["messagequeuesize"].as_u64(), Some(10));

        assert!(
            rpc.set_ban(&json!(["1.2.3.4", "add", u64::MAX, false]))
                .await
                .is_err()
        );
        assert!(nm.get_banned_peers().is_empty());
        let before = current_timestamp();
        rpc.set_ban(&json!(["1.2.3.4", "add", 86_400, false]))
            .await
            .unwrap();
        let until = nm
            .get_banned_peers()
            .iter()
            .find(|(addr, _)| addr.ip().to_string() == "1.2.3.4")
            .unwrap()
            .1;
        assert!(until >= before + 86_400);
        assert!(until <= current_timestamp() + 86_400);

        assert!(
            crate::rpc::rest::network::set_ban(&rpc, "8.8.8.8", "add", None, Some(true))
                .await
                .is_err()
        );
        crate::rpc::rest::network::set_ban(&rpc, "8.8.8.8", "add", Some(1_700_000_000), Some(true))
            .await
            .unwrap();
        let banned = rpc.list_banned(&json!([])).await.unwrap();
        let absolute = banned
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["address"].as_str().unwrap().starts_with("8.8.8.8"))
            .unwrap();
        assert_eq!(absolute["banned_until"].as_u64(), Some(1_700_000_000));

        rpc.setnetworkactive(&json!([false])).await.unwrap();
        let stayed = crate::rpc::rest::network::set_network_active_body(&rpc, &json!({}))
            .await
            .unwrap();
        assert_eq!(stayed, false);
        assert_eq!(
            rpc.get_network_info().await.unwrap()["networkactive"],
            false
        );
        crate::rpc::rest::network::set_network_active_body(&rpc, &json!({"state": false}))
            .await
            .unwrap();
        assert_eq!(
            rpc.get_network_info().await.unwrap()["networkactive"],
            false
        );
        crate::rpc::rest::network::set_network_active_body(&rpc, &json!({"state": true}))
            .await
            .unwrap();
        assert_eq!(rpc.get_network_info().await.unwrap()["networkactive"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn absolute_unban_time_zero_is_not_a_ban() {
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let rpc = NetworkRpc::with_dependencies(Arc::clone(&nm));
        rpc.set_ban(&json!(["1.2.3.4", "add", 86_400, false]))
            .await
            .unwrap();
        rpc.set_ban(&json!(["1.2.3.4", "add", 0, true]))
            .await
            .unwrap();
        assert!(
            nm.get_banned_peers()
                .iter()
                .all(|(addr, _)| addr.ip().to_string() != "1.2.3.4")
        );
        rpc.set_ban(&json!(["1.2.3.5", "add", 0, false]))
            .await
            .unwrap();
        let until = nm
            .get_banned_peers()
            .iter()
            .find(|(addr, _)| addr.ip().to_string() == "1.2.3.5")
            .unwrap()
            .1;
        assert_eq!(until, u64::MAX);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn addnode_add_dials_the_address() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let nm = Arc::new(NetworkManager::new("127.0.0.1:0".parse().unwrap()));
        let rpc = NetworkRpc::with_dependencies(Arc::clone(&nm));
        let accepted = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept()).await
        });
        rpc.add_node(&json!([addr.to_string(), "add"]))
            .await
            .unwrap();
        assert!(
            accepted.await.unwrap().is_ok(),
            "add did not open a connection"
        );
        assert!(nm.get_persistent_peers().await.contains(&addr));
    }
}
