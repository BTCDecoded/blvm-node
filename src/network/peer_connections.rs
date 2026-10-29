//! Peer connection logic: DNS discovery, persistent peers, address database, connect_to_peer.
//!
//! Extracted from network_manager.rs to reduce file size.

use crate::network::NetworkMessage;
use crate::network::network_manager::NetworkManager;
use crate::network::peer;
use crate::network::transport::{Transport, TransportAddr, TransportType};
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Check once at runtime whether global (routable) IPv6 is available.
///
/// Many servers have link-local (fe80::) IPv6 only. Attempting to connect to
/// global IPv6 peers fails immediately with "Network is unreachable" (os error 101),
/// wasting 10s per attempt when connection timeout applies.  Cache the answer with
/// a `std::sync::OnceLock` so we pay the detection cost only once per process.
fn has_global_ipv6() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        // Probe by attempting to bind a UDP socket to a global IPv6 address.
        // Uses a non-blocking probe so it doesn't require root or interface enumeration.
        use std::net::UdpSocket;
        // Try to connect to a well-known global IPv6 address (Google DNS).
        // `connect` on UDP doesn't send packets — it just routes the socket.
        match UdpSocket::bind("[::]:0") {
            Ok(sock) => {
                let result = sock.connect("[2001:4860:4860::8888]:53").is_ok();
                if !result {
                    debug!("IPv6: no global routing (UDP connect failed) — skipping IPv6 peers");
                }
                result
            }
            Err(_) => {
                debug!("IPv6: cannot bind IPv6 socket — skipping IPv6 peers");
                false
            }
        }
    })
}

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
            "testnet4" => dns_seeds::TESTNET4_DNS_SEEDS,
            "signet" => dns_seeds::SIGNET_DNS_SEEDS,
            _ => {
                warn!("Unknown network: {}, skipping DNS seed discovery", network);
                return Ok(());
            }
        };

        if crate::network::ibd_peers_pinned() {
            info!("IBD: skipping DNS seeds — BLVM_IBD_PIN_PEERS set");
            return Ok(());
        }

        info!("Discovering peers from DNS seeds for {}", network);
        let timing_config_default = crate::config::NetworkTimingConfig::default();
        let timing_config = config
            .network_timing
            .as_ref()
            .unwrap_or(&timing_config_default);
        let max_addresses = timing_config.max_addresses_from_dns;
        let mut addresses = dns_seeds::resolve_dns_seeds(seeds, port, max_addresses).await;
        if addresses.is_empty() && network == "mainnet" {
            info!("DNS seeds returned no addresses; using the fixed mainnet seed list");
            addresses = dns_seeds::fixed_mainnet_addresses();
        }
        let address_count = addresses.len();

        {
            let mut db = self.address_database().write().await;
            for addr in addresses {
                // Use add_address_from_dns_seed: clears any stale NODE_NETWORK_LIMITED flag
                // so peers get a fresh classification on the next version handshake.
                db.add_address_from_dns_seed(addr);
            }
        }

        info!("Discovered {} addresses from DNS seeds", address_count);
        Ok(())
    }

    /// Discover peers that can serve this network's chain.
    ///
    /// Mainnet uses the `x1.` seeds so the answers are full-history nodes.
    /// Testnet4, testnet, and signet use their own seeds and P2P port.
    pub async fn discover_archive_peers_from_dns(&self) -> Result<usize> {
        if crate::network::ibd_peers_pinned() {
            info!("IBD: skipping archive DNS seeds — BLVM_IBD_PIN_PEERS set");
            return Ok(0);
        }
        use crate::network::dns_seeds;
        let (network, _) = crate::network::protocol::ProtocolParser::dns_seed_network();
        let (seeds, port) = dns_seeds::archive_seed_set(network);
        if seeds.is_empty() {
            return Ok(0);
        }
        info!(
            "IBD: seeding full-history peers for {network} from {} DNS seeds on port {port}",
            seeds.len()
        );
        let addresses = dns_seeds::resolve_dns_seeds(seeds, port, 1000).await;
        let count = addresses.len();
        {
            let mut db = self.address_database().write().await;
            for addr in addresses {
                db.add_address_from_dns_seed(addr);
            }
        }
        info!(
            "IBD: discovered {count} peer addresses for {network} on port {port}"
        );
        Ok(count)
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

        let iroh_transport = {
            let guard = match self.iroh_transport().lock() {
                Ok(g) => g,
                Err(_) => {
                    warn!("Iroh transport lock poisoned");
                    return Ok(0);
                }
            };
            match guard.as_ref() {
                Some(transport) => std::sync::Arc::clone(transport),
                None => {
                    warn!("Iroh transport not initialized, cannot connect to Iroh peers");
                    return Ok(0);
                }
            }
        };

        let mut connected = 0;
        for node_id in node_ids.into_iter().take(needed * 2) {
            let node_id_bytes = node_id.as_bytes().to_vec();
            let transport_addr = TransportAddr::Iroh(node_id_bytes.clone());

            match iroh_transport.connect(transport_addr.clone()).await {
                Ok(conn) => {
                    let placeholder_socket = SocketAddr::from(([0, 0, 0, 0], 0));
                    let mut peer = peer::Peer::from_transport_connection(
                        conn,
                        placeholder_socket,
                        transport_addr.clone(),
                        self.peer_tx().clone(),
                    );
                    peer.set_is_outbound(true);

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

    /// Open connections when the peer set is empty.
    ///
    /// Catch-up calls this when no peer has reported a start height, so a node
    /// whose outbound peers are gone still dials the address database.
    pub(crate) async fn connect_outbound_when_no_peers(&self) -> Result<usize> {
        if self.peer_count() > 0 {
            return Ok(0);
        }
        let target = self.network_timing().effective_target_outbound_peers();
        self.connect_peers_from_database(target).await
    }

    /// Connect to peers from address database when below target count
    pub async fn connect_peers_from_database(&self, target_count: usize) -> Result<usize> {
        if crate::network::ibd_peers_pinned() {
            let current: std::collections::HashSet<SocketAddr> = {
                let pm = self.peer_manager_mutex().lock().await;
                pm.peer_socket_addresses().into_iter().collect()
            };
            let mut connected = 0usize;
            for addr in crate::network::ibd_pin_peers() {
                if current.contains(&addr) {
                    continue;
                }
                match self.connect_to_peer(addr).await {
                    Ok(()) => connected += 1,
                    Err(e) => debug!("IBD pin reconnect {} failed: {e}", addr),
                }
            }
            return Ok(connected);
        }

        let (network_name, network_port) =
            crate::network::protocol::ProtocolParser::dns_seed_network();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager_mutex().lock().await;
            pm.peer_socket_addresses()
        };
        let on_port = connected_peers
            .iter()
            .filter(|addr| addr.port() == network_port)
            .count();
        if on_port >= target_count {
            return Ok(0);
        }

        let needed = target_count - on_port;
        info!(
            "Need {} more peers on port {network_port} (current on-port: {on_port}, connected: {}, target: {target_count})",
            needed,
            connected_peers.len()
        );

        let ban_list = self.ban_list().read().await.clone();

        // Check how many fresh non-limited addresses exist before pulling the list.
        // If the address DB has been drained of full-history peers (all marked
        // NODE_NETWORK_LIMITED from previous evictions), trigger DNS re-discovery to
        // replenish with fresh, unclassified addresses before attempting to connect.
        {
            let db = self.address_database().read().await;
            if db.count_fresh_non_limited() < needed {
                drop(db);
                // Unit tests construct an empty in-memory DB. Do not hit mainnet archive DNS
                // (`connect_peers_from_database_empty_returns_zero`).
                #[cfg(not(test))]
                {
                    warn!(
                        "IBD: address DB has fewer than {} non-limited addresses — \
                         running DNS seed re-discovery to find full-history peers",
                        needed
                    );
                    // Use archive seeds (x1. prefix = NODE_NETWORK filter) first; they return only
                    // full-history nodes. Fall back to regular seeds only if archive seeds fail or
                    // return nothing — regular seeds return mostly pruned nodes which get immediately
                    // evicted as NODE_NETWORK_LIMITED, draining the DB in a tight loop.
                    let archive_count = self.discover_archive_peers_from_dns().await.unwrap_or(0);
                    if archive_count == 0 {
                        let (net_name, port) =
                            crate::network::protocol::ProtocolParser::dns_seed_network();
                        let _ = self
                            .discover_peers_from_dns(net_name, port, &Default::default())
                            .await;
                    }
                }
            }
        }

        // R-347: during a live IBD session race more candidates per call. R-346 top-up ticks
        // connected 3–10 of the 30–44 needed from `needed × 2` sockets (stale seed
        // addresses); the attempts run concurrently so a wider fan-out costs little.
        let fanout: usize = if crate::node::parallel_ibd::PARALLEL_IBD_SESSION_ACTIVE
            .load(std::sync::atomic::Ordering::Acquire)
        {
            4
        } else {
            2
        };
        let addresses: Vec<_> = {
            let db = self.address_database().read().await;
            // IBD-biased ordering: full-history peers first, unknown second, pruned last.
            let fresh = db.get_fresh_ibd_addresses(needed * (fanout + 1));
            db.filter_addresses(fresh, &ban_list, &connected_peers)
        };

        if addresses.is_empty() {
            warn!("No fresh addresses available in database");
            return Ok(0);
        }

        let sockets: Vec<SocketAddr> = {
            let db = self.address_database().read().await;
            let mapped: Vec<SocketAddr> = addresses
                .iter()
                .take(needed * fanout)
                .map(|addr| db.network_addr_to_socket(addr))
                .collect();
            prefer_network_port(mapped, network_port)
        };
        if sockets.iter().all(|addr| addr.port() != network_port) {
            drop(sockets);
            info!(
                "IBD: no stored peers on {network_name} port {network_port}; seeding that network"
            );
            #[cfg(not(test))]
            {
                let _ = self.discover_archive_peers_from_dns().await;
            }
        }
        let sockets: Vec<SocketAddr> = {
            let db = self.address_database().read().await;
            let fresh = db.get_fresh_ibd_addresses(needed * (fanout + 1));
            let filtered = db.filter_addresses(fresh, &ban_list, &connected_peers);
            let mapped: Vec<SocketAddr> = filtered
                .iter()
                .take(needed * fanout)
                .map(|addr| db.network_addr_to_socket(addr))
                .collect();
            prefer_network_port(mapped, network_port)
        };
        let sockets: Vec<SocketAddr> = sockets
            .into_iter()
            .filter(|addr| !dial_is_cooled(*addr))
            .collect();
        if sockets.is_empty() {
            debug!("IBD: every dial candidate is in cooldown");
            return Ok(0);
        }

        // Connect to multiple candidates simultaneously to avoid serializing 10s timeouts.
        // Use FuturesUnordered so each candidate races concurrently; we collect results as
        // they arrive and stop early once `needed` connections succeed.
        use futures::StreamExt as _;
        let mut stream = futures::stream::FuturesUnordered::new();
        for socket in sockets {
            stream.push(async move { (socket, self.connect_to_peer(socket).await) });
        }

        let mut connected = 0;
        while let Some((socket, result)) = stream.next().await {
            match result {
                Ok(()) => {
                    connected += 1;
                    if connected >= needed {
                        break;
                    }
                }
                Err(e) => {
                    debug!("Failed to connect to {}: {}", socket, e);
                }
            }
        }
        // Remaining in-progress connection futures are dropped here, but the underlying
        // TCP SYN packets may already be in flight. That is harmless: if they connect,
        // `PeerConnected` will be processed normally; if they fail, no resources are held.

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

        if crate::network::ibd_peers_pinned() {
            let pinned = crate::network::ibd_pin_peers();
            info!(
                "[IBD_PINNED_PEERS] n={} — skip DNS/LAN/Iroh, connect exactly this set",
                pinned.len()
            );
            for addr in &pinned {
                self.add_persistent_peer(*addr);
                match self.connect_to_peer(*addr).await {
                    Ok(()) => info!("[IBD_PINNED_PEERS] connected {}", addr),
                    Err(e) => warn!("[IBD_PINNED_PEERS] connect {} failed: {e}", addr),
                }
            }
            return Ok(());
        }

        info!(
            "Peer discovery in progress — IBD block downloads start once peers connect (typically 15–60s on first start)"
        );
        info!("Discovering LAN sibling nodes...");
        let lan_nodes = lan_discovery::discover_lan_bitcoin_nodes_with_port(port).await;
        let mut lan_connected = 0;
        let mut connected_lan: Vec<std::net::SocketAddr> = Vec::new();

        for lan_addr in &lan_nodes {
            info!("Connecting to LAN sibling node: {}", lan_addr);
            match self.connect_to_peer(*lan_addr).await {
                Ok(_) => {
                    lan_connected += 1;
                    connected_lan.push(*lan_addr);
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
            let addrs: String = connected_lan
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let best = connected_lan[0];
            info!(
                "LAN Bitcoin node(s) at {addrs} — IBD auto-prefers LAN (override: BLVM_IBD_PEERS={best})"
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
        if dial_is_cooled(addr) {
            return Err(anyhow::anyhow!("dial cooled"));
        }
        let ip = addr.ip();

        // Fast-reject global IPv6 peers when this host has no global IPv6 routing.
        // Link-local (fe80::/10) and loopback (::1) are still allowed.
        if ip.is_ipv6() {
            use std::net::IpAddr;
            if let IpAddr::V6(v6) = ip {
                let is_link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
                let is_loopback = v6.is_loopback();
                if !is_link_local && !is_loopback && !has_global_ipv6() {
                    return Err(anyhow::anyhow!(
                        "Skipping IPv6 peer {} — no global IPv6 routing on this host",
                        addr
                    ));
                }
            }
        }

        // R-357: throttle our own dial rate, never ban the target for it. The old branch
        // auto-banned 11 productive IBD peers for 3600 s in R-356 (remote evicted our fresh
        // inbound every 1–2 s → immediate reconnect → 10 dials / 60 s → ban). Inbound
        // floods still go through `check_connection` + `should_auto_ban` elsewhere.
        if !self.dos_protection().check_outbound_connection(ip).await {
            warn!(
                "Connection rate limit exceeded for IP {}, rejecting outgoing connection",
                ip
            );
            return Err(anyhow::anyhow!(
                "Connection rate limit exceeded for IP {}",
                ip
            ));
        }

        if !self.check_eclipse_prevention(ip) {
            let prefix = self.get_ip_prefix(ip);
            warn!(
                "Eclipse attack prevention: too many connections from IP range {:?}, rejecting connection from {}",
                prefix, ip
            );
            return Err(anyhow::anyhow!(
                "Eclipse attack prevention: too many connections from IP range"
            ));
        }

        let mut last_error = None;
        let transports_to_try = self.get_transports_for_connection();

        for transport_type in transports_to_try {
            match self.try_connect_with_transport(&transport_type, addr).await {
                Ok((mut peer, transport_addr)) => {
                    if crate::network::ibd_pin_contains(addr) {
                        peer.set_is_manual(true);
                    }
                    {
                        let mut pm = self.peer_manager_mutex().lock().await;
                        pm.add_peer(transport_addr.clone(), peer)?;
                    }
                    // Record this IP in the eclipse-prevention diversity map.
                    self.add_peer_diversity(ip).await;
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

        note_dial_failure(addr);
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All transport attempts failed")))
    }

    /// Fire-and-forget TCP reconnect when a peer is no longer in the peer map (e.g. IBD GetData).
    /// Uses the full `connect_to_peer` path (stream split, DoS checks, handshake via PeerConnected).
    ///
    /// Will not reconnect to peers whose IP is in `ibd_evicted_ips` — they were evicted because
    /// they proved unable to serve historical IBD blocks (NODE_NETWORK_LIMITED / pruned).
    pub fn spawn_outbound_reconnect_attempt(self: Arc<Self>, addr: SocketAddr) {
        let reconnection_queue = Arc::clone(self.peer_reconnection_queue());
        let nm = self;
        tokio::spawn(async move {
            // Skip reconnect if this IP was evicted from IBD.
            // Use a block to drop the guard before any await.
            let is_evicted = {
                let evicted = nm.ibd_evicted_ips.read().unwrap();
                evicted.contains(&addr.ip())
            };
            if is_evicted {
                debug!(
                    "IBD: skipping reconnect to {} — IP evicted (NODE_NETWORK_LIMITED)",
                    addr
                );
                return;
            }
            if dial_is_cooled(addr) {
                return;
            }
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

        // Only fall back to TCP if the transport preference actually allows it.
        // Previously this always appended TCP, breaking IrohOnly / QuinnOnly modes.
        if transports.is_empty() && self.transport_preference().allows_tcp() {
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
                let mut peer = peer::Peer::from_tcp_stream_split(
                    stream,
                    addr,
                    self.peer_tx().clone(),
                    self.protocol_limits().max_protocol_message_length,
                );
                peer.set_is_outbound(true);
                peer.set_ping_timeout_seconds(self.background_task_config().ping_timeout_secs);
                Ok((peer, transport_addr))
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
                    let mut peer = peer::Peer::from_transport_connection(
                        conn,
                        addr,
                        quinn_addr_clone.clone(),
                        self.peer_tx().clone(),
                    );
                    peer.set_is_outbound(true);
                    Ok((peer, quinn_addr_clone))
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

/// Keep peers on the network P2P port when any are present. A testnet4 node
/// does not fill its outbound slots with mainnet `:8333` once `:48333` is known.
pub(crate) fn prefer_network_port(sockets: Vec<SocketAddr>, network_port: u16) -> Vec<SocketAddr> {
    let on_port: Vec<SocketAddr> = sockets
        .iter()
        .copied()
        .filter(|addr| addr.port() == network_port)
        .collect();
    if on_port.is_empty() {
        sockets
    } else {
        on_port
    }
}

const DIAL_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(120);

static DIAL_FAILS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<SocketAddr, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(crate) fn note_dial_failure(addr: SocketAddr) {
    let mut fails = DIAL_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    fails.retain(|_, failed_at| now.duration_since(*failed_at) < DIAL_COOLDOWN);
    fails.insert(addr, now);
}

pub(crate) fn dial_is_cooled(addr: SocketAddr) -> bool {
    let mut fails = DIAL_FAILS.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    fails.retain(|_, failed_at| now.duration_since(*failed_at) < DIAL_COOLDOWN);
    fails.contains_key(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeConfig;
    use std::net::SocketAddr;

    #[test]
    fn on_port_peers_are_dialed_ahead_of_mainnet() {
        let mainnet: SocketAddr = "1.2.3.4:8333".parse().unwrap();
        let testnet4: SocketAddr = "5.6.7.8:48333".parse().unwrap();
        let chosen = prefer_network_port(vec![mainnet, testnet4], 48333);
        assert_eq!(chosen, vec![testnet4]);
        let only_mainnet = prefer_network_port(vec![mainnet], 48333);
        assert_eq!(only_mainnet, vec![mainnet]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discover_peers_from_dns_unknown_network_is_noop() {
        let listen: SocketAddr = "127.0.0.1:18450".parse().unwrap();
        let nm = NetworkManager::new(listen);
        nm.discover_peers_from_dns("regtest", 8333, &NodeConfig::default())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_peers_from_database_empty_returns_zero() {
        let listen: SocketAddr = "127.0.0.1:18451".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let connected = nm.connect_peers_from_database(8).await.unwrap();
        assert_eq!(connected, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_persistent_peers_smoke() {
        let listen: SocketAddr = "127.0.0.1:18452".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let peer: SocketAddr = "127.0.0.1:18453".parse().unwrap();
        nm.connect_persistent_peers(&[peer]).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn discover_peers_from_dns_mainnet_and_testnet_smoke() {
        let listen: SocketAddr = "127.0.0.1:18454".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let config = NodeConfig::default();
        nm.discover_peers_from_dns("mainnet", 8333, &config)
            .await
            .unwrap();
        nm.discover_peers_from_dns("testnet", 18333, &config)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_peer_unreachable_returns_error() {
        let listen: SocketAddr = "127.0.0.1:18455".parse().unwrap();
        let nm = NetworkManager::new(listen);
        let dead: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(nm.connect_to_peer(dead).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ibd_evict_lock_keeps_node_network_and_redials_after_session() {
        use crate::config::RequestTimeoutConfig;
        use crate::network::transport::TransportPreference;
        use blvm_protocol::service_flags::standard::NODE_NETWORK;

        let mut cfg = NodeConfig::default();
        let mut timeouts = RequestTimeoutConfig::default();
        timeouts.connect_timeout_secs = 1;
        cfg.request_timeouts = Some(timeouts);
        let nm = Arc::new(NetworkManager::with_config(
            "127.0.0.1:0".parse().unwrap(),
            8,
            TransportPreference::TCP_ONLY,
            Some(&cfg),
        ));
        let addr: SocketAddr = "203.0.113.5:8333".parse().unwrap();
        let mut ip = [0u8; 16];
        ip[10] = 0xff;
        ip[11] = 0xff;
        ip[12] = 203;
        ip[13] = 0;
        ip[14] = 113;
        ip[15] = 5;
        nm.address_database().write().await.add_address(
            crate::network::protocol::NetworkAddress {
                time: current_timestamp() as u32,
                services: NODE_NETWORK,
                ip,
                port: 8333,
            },
            NODE_NETWORK,
        );
        nm.evict_ibd_peer(addr);
        let services = nm.address_database().read().await.services_for_socket(addr);
        assert_ne!(services & NODE_NETWORK, 0);
        nm.clear_ibd_evicted_ips();
        Arc::clone(&nm).spawn_outbound_reconnect_attempt(addr);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            nm.peer_reconnection_queue()
                .lock()
                .await
                .contains_key(&addr)
        );
    }
}
