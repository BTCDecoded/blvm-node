//! Peer connection logic: DNS discovery, persistent peers, address database, connect_to_peer.
//!
//! Extracted from network_manager.rs to reduce file size.

use crate::network::network_manager::NetworkManager;
use crate::network::peer;
use crate::network::transport::{Transport, TransportAddr, TransportType};
use crate::network::NetworkMessage;
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};

impl NetworkManager {
    /// Discover peers from DNS seeds and add to address database
    pub async fn discover_peers_from_dns(
        &self,
        network: &str,
        port: u16,
        config: &crate::config::NodeConfig,
    ) -> Result<()> {
        use crate::network::dns_seeds;

        let seeds = match network {
            "mainnet" => dns_seeds::MAINNET_DNS_SEEDS,
            "testnet" => dns_seeds::TESTNET_DNS_SEEDS,
            _ => {
                warn!("Unknown network: {}, skipping DNS seed discovery", network);
                return Ok(());
            }
        };

        info!("Discovering peers from DNS seeds for {}", network);
        let timing_config_default = crate::config::NetworkTimingConfig::default();
        let timing_config = config
            .network_timing
            .as_ref()
            .unwrap_or(&timing_config_default);
        let max_addresses = timing_config.max_addresses_from_dns;
        let addresses = dns_seeds::resolve_dns_seeds(seeds, port, max_addresses).await;
        let address_count = addresses.len();

        {
            let mut db = self.address_database().write().await;
            for addr in addresses {
                db.add_address(addr, 0);
            }
        }

        info!("Discovered {} addresses from DNS seeds", address_count);
        Ok(())
    }

    /// Connect to persistent peers from config
    pub async fn connect_persistent_peers(&self, persistent_peers: &[SocketAddr]) -> Result<()> {
        for peer_addr in persistent_peers {
            self.add_persistent_peer(*peer_addr);
            info!("Connecting to persistent peer: {}", peer_addr);
            if let Err(e) = self.connect_to_peer(*peer_addr).await {
                warn!("Failed to connect to persistent peer {}: {}", peer_addr, e);
            }
        }
        Ok(())
    }

    /// Discover Iroh peers and add to address database
    #[cfg(feature = "iroh")]
    pub async fn discover_iroh_peers(&self) -> Result<usize> {
        info!("Iroh peer discovery: relying on DERP servers and incoming connections");
        Ok(0)
    }

    /// Connect to Iroh peers from address database
    #[cfg(feature = "iroh")]
    pub async fn connect_iroh_peers_from_database(&self, target_count: usize) -> Result<usize> {
        let current_iroh_count = {
            let pm = self.peer_manager_mutex().lock().await;
            pm.peer_addresses()
                .iter()
                .filter(|addr| matches!(addr, TransportAddr::Iroh(_)))
                .count()
        };

        if current_iroh_count >= target_count {
            return Ok(0);
        }

        let needed = target_count - current_iroh_count;
        info!(
            "Need {} more Iroh peers (current: {}, target: {})",
            needed, current_iroh_count, target_count
        );

        let node_ids = {
            let db = self.address_database().read().await;
            db.get_fresh_iroh_addresses(needed * 2)
        };

        if node_ids.is_empty() {
            warn!("No fresh Iroh addresses available in database");
            return Ok(0);
        }

        let guard = match self.iroh_transport().lock() {
            Ok(g) => g,
            Err(_) => {
                warn!("Iroh transport lock poisoned");
                return Ok(0);
            }
        };
        let iroh_transport = match guard.as_ref() {
            Some(transport) => transport,
            None => {
                warn!("Iroh transport not initialized, cannot connect to Iroh peers");
                return Ok(0);
            }
        };

        let mut connected = 0;
        for node_id in node_ids.into_iter().take(needed * 2) {
            let node_id_bytes = node_id.as_bytes().to_vec();
            let transport_addr = TransportAddr::Iroh(node_id_bytes.clone());

            match iroh_transport.connect(transport_addr.clone()).await {
                Ok(conn) => {
                    let placeholder_socket = SocketAddr::from(([0, 0, 0, 0], 0));
                    let peer = peer::Peer::from_transport_connection(
                        conn,
                        placeholder_socket,
                        transport_addr.clone(),
                        self.peer_tx().clone(),
                    );

                    {
                        let mut pm = self.peer_manager_mutex().lock().await;
                        if let Err(e) = pm.add_peer(transport_addr.clone(), peer) {
                            warn!("Failed to add Iroh peer: {}", e);
                            continue;
                        }
                    }

                    {
                        let mut socket_to_transport = self.socket_to_transport().lock().await;
                        socket_to_transport.insert(placeholder_socket, transport_addr.clone());
                    }

                    info!("Successfully connected to Iroh peer: {}", node_id);
                    connected += 1;
                    if connected >= needed {
                        break;
                    }
                }
                Err(e) => {
                    debug!("Failed to connect to Iroh peer {}: {}", node_id, e);
                }
            }
        }

        info!(
            "Connected to {} new Iroh peers from address database",
            connected
        );
        Ok(connected)
    }

    /// Connect to peers from address database when below target count
    pub async fn connect_peers_from_database(&self, target_count: usize) -> Result<usize> {
        let current_count = self.peer_count();
        if current_count >= target_count {
            return Ok(0);
        }

        let needed = target_count - current_count;
        info!(
            "Need {} more peers (current: {}, target: {})",
            needed, current_count, target_count
        );

        let ban_list = self.ban_list().read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager_mutex().lock().await;
            pm.peer_socket_addresses()
        };

        let addresses: Vec<_> = {
            let db = self.address_database().read().await;
            let fresh = db.get_fresh_addresses(needed * 3);
            db.filter_addresses(fresh, &ban_list, &connected_peers)
        };

        if addresses.is_empty() {
            warn!("No fresh addresses available in database");
            return Ok(0);
        }

        let sockets: Vec<SocketAddr> = {
            let db = self.address_database().read().await;
            addresses
                .iter()
                .take(needed * 2)
                .map(|addr| db.network_addr_to_socket(addr))
                .collect()
        };

        let mut connected = 0;
        for socket in sockets {
            if let Err(e) = self.connect_to_peer(socket).await {
                debug!("Failed to connect to {}: {}", socket, e);
            } else {
                connected += 1;
                if connected >= needed {
                    break;
                }
            }
        }

        info!("Connected to {} new peers from address database", connected);

        #[cfg(feature = "iroh")]
        if self.transport_preference().allows_iroh() {
            let iroh_connected = self.connect_iroh_peers_from_database(target_count).await?;
            connected += iroh_connected;
        }

        Ok(connected)
    }

    /// Initialize peer connections after startup
    pub async fn initialize_peer_connections(
        &self,
        config: &crate::config::NodeConfig,
        network: &str,
        port: u16,
        target_peer_count: usize,
    ) -> Result<()> {
        use super::lan_discovery;

        info!("Discovering LAN sibling nodes...");
        let lan_nodes = lan_discovery::discover_lan_bitcoin_nodes_with_port(port).await;
        let mut lan_connected = 0;

        for lan_addr in &lan_nodes {
            info!("Connecting to LAN sibling node: {}", lan_addr);
            match self.connect_to_peer(*lan_addr).await {
                Ok(_) => {
                    lan_connected += 1;
                    self.add_persistent_peer(*lan_addr);
                    info!(
                        "Connected to LAN sibling node: {} (will be prioritized for IBD)",
                        lan_addr
                    );
                }
                Err(e) => {
                    warn!("Failed to connect to LAN node {}: {}", lan_addr, e);
                }
            }
        }

        if lan_connected > 0 {
            info!(
                "Connected to {} LAN sibling node(s) - these will be prioritized for block downloads",
                lan_connected
            );
        }

        let should_discover_dns = self.transport_preference().allows_tcp() || {
            #[cfg(feature = "quinn")]
            {
                self.transport_preference().allows_quinn()
            }
            #[cfg(not(feature = "quinn"))]
            {
                false
            }
        };
        if should_discover_dns {
            if let Err(e) = self.discover_peers_from_dns(network, port, config).await {
                warn!("DNS seed discovery failed: {}", e);
            }
        }

        if !config.persistent_peers.is_empty() {
            if let Err(e) = self
                .connect_persistent_peers(&config.persistent_peers)
                .await
            {
                warn!("Failed to connect to some persistent peers: {}", e);
            }
        }

        #[cfg(feature = "iroh")]
        if self.transport_preference().allows_iroh() {
            if let Err(e) = self.discover_iroh_peers().await {
                warn!("Iroh peer discovery failed: {}", e);
            }
        }

        let timing_config_default = crate::config::NetworkTimingConfig::default();
        let timing_config = config
            .network_timing
            .as_ref()
            .unwrap_or(&timing_config_default);
        let delay_seconds = timing_config.peer_connection_delay_seconds;
        tokio::time::sleep(tokio::time::Duration::from_secs(delay_seconds)).await;

        if let Err(e) = self.connect_peers_from_database(target_peer_count).await {
            warn!("Failed to connect peers from database: {}", e);
        }

        Ok(())
    }

    /// Connect to a peer at the given address
    pub async fn connect_to_peer(&self, addr: SocketAddr) -> Result<()> {
        let ip = addr.ip();
        if !self.dos_protection().check_connection(ip).await {
            warn!(
                "Connection rate limit exceeded for IP {}, rejecting outgoing connection",
                ip
            );
            if self.dos_protection().should_auto_ban(ip).await {
                warn!(
                    "Auto-banning IP {} for repeated connection rate violations",
                    ip
                );
                let ban_duration = self.dos_protection().ban_duration_seconds();
                let unban_timestamp = current_timestamp() + ban_duration;
                let mut ban_list = self.ban_list().write().await;
                ban_list.insert(addr, unban_timestamp);
                return Err(anyhow::anyhow!(
                    "IP {} is banned due to connection rate violations",
                    ip
                ));
            }
            return Err(anyhow::anyhow!(
                "Connection rate limit exceeded for IP {}",
                ip
            ));
        }

        if !self.check_eclipse_prevention(ip) {
            let prefix = self.get_ip_prefix(ip);
            warn!("Eclipse attack prevention: too many connections from IP range {:?}, rejecting connection from {}", prefix, ip);
            return Err(anyhow::anyhow!(
                "Eclipse attack prevention: too many connections from IP range"
            ));
        }

        let mut last_error = None;
        let transports_to_try = self.get_transports_for_connection();

        for transport_type in transports_to_try {
            match self.try_connect_with_transport(&transport_type, addr).await {
                Ok((peer, transport_addr)) => {
                    {
                        let mut pm = self.peer_manager_mutex().lock().await;
                        pm.add_peer(transport_addr.clone(), peer)?;
                    }
                    let _ = self
                        .peer_tx()
                        .send(NetworkMessage::PeerConnected(transport_addr.clone()));
                    info!(
                        "Successfully connected to {} via {:?} (transport: {:?})",
                        addr, transport_type, transport_addr
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "Failed to connect to {} via {:?}: {}",
                        addr, transport_type, e
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All transport attempts failed")))
    }

    /// Fire-and-forget TCP reconnect when a peer is no longer in the peer map (e.g. IBD GetData).
    /// Uses the full `connect_to_peer` path (stream split, DoS checks, handshake via PeerConnected).
    pub fn spawn_outbound_reconnect_attempt(self: Arc<Self>, addr: SocketAddr) {
        let reconnection_queue = Arc::clone(self.peer_reconnection_queue());
        let nm = self;
        tokio::spawn(async move {
            {
                let pm = nm.peer_manager_mutex().lock().await;
                if let Some(p) = pm.get_peer(&TransportAddr::Tcp(addr)) {
                    if p.is_connected() {
                        return;
                    }
                }
            }
            {
                let mut q = reconnection_queue.lock().await;
                q.entry(addr).or_insert((0, 0, 0.85));
            }
            info!(
                "Attempting immediate reconnect to {} (IBD / transient disconnect)",
                addr
            );
            match nm.connect_to_peer(addr).await {
                Ok(()) => {
                    let mut q = reconnection_queue.lock().await;
                    q.remove(&addr);
                    info!("Immediate reconnect succeeded for {}", addr);
                }
                Err(e) => {
                    debug!(
                        "Immediate reconnect to {} failed: {} (periodic task may retry)",
                        addr, e
                    );
                }
            }
        });
    }

    fn get_transports_for_connection(&self) -> Vec<TransportType> {
        let mut transports = Vec::new();

        if self.transport_preference().allows_tcp() {
            transports.push(TransportType::Tcp);
        }

        #[cfg(feature = "quinn")]
        if self.transport_preference().allows_quinn() {
            if let Ok(guard) = self.quinn_transport().lock() {
                if guard.is_some() {
                    transports.push(TransportType::Quinn);
                }
            }
        }

        #[cfg(feature = "iroh")]
        if self.transport_preference().allows_iroh() {
            if let Ok(guard) = self.iroh_transport().lock() {
                if guard.is_some() {
                    transports.push(TransportType::Iroh);
                }
            }
        }

        if !transports.iter().any(|t| matches!(t, TransportType::Tcp)) {
            transports.push(TransportType::Tcp);
        }

        transports
    }

    async fn try_connect_with_transport(
        &self,
        transport_type: &TransportType,
        addr: SocketAddr,
    ) -> Result<(peer::Peer, TransportAddr)> {
        match transport_type {
            TransportType::Tcp => {
                let connect_secs = self.request_timeout_config().connect_timeout_secs;
                let stream = self
                    .tcp_transport()
                    .connect_stream_with_timeout(addr, connect_secs)
                    .await?;
                let transport_addr = TransportAddr::Tcp(addr);
                Ok((
                    peer::Peer::from_tcp_stream_split(
                        stream,
                        addr,
                        self.peer_tx().clone(),
                        self.protocol_limits().max_protocol_message_length,
                    ),
                    transport_addr,
                ))
            }
            #[cfg(feature = "quinn")]
            TransportType::Quinn => {
                let quinn = self
                    .quinn_transport()
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().cloned());
                if let Some(quinn) = quinn {
                    let quinn_addr = TransportAddr::Quinn(addr);
                    let quinn_addr_clone = quinn_addr.clone();
                    let conn = quinn.connect(quinn_addr_clone.clone()).await?;
                    Ok((
                        peer::Peer::from_transport_connection(
                            conn,
                            addr,
                            quinn_addr_clone.clone(),
                            self.peer_tx().clone(),
                        ),
                        quinn_addr_clone,
                    ))
                } else {
                    Err(anyhow::anyhow!("Quinn transport not available"))
                }
            }
            #[cfg(feature = "iroh")]
            TransportType::Iroh => Err(anyhow::anyhow!(
                "Iroh transport requires public key, not SocketAddr"
            )),
        }
    }
}
