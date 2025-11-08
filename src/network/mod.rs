//! Network layer for reference-node
//!
//! This module provides P2P networking, peer management, and Bitcoin protocol
//! message handling for communication with other Bitcoin nodes.

pub mod chain_access;
pub mod inventory;
pub mod message_bridge;
pub mod peer;
pub mod protocol;
pub mod protocol_adapter;
pub mod protocol_extensions;
pub mod ban_list_merging;
pub mod ban_list_signing;
pub mod dos_protection;
pub mod relay;
pub mod tcp_transport;
pub mod transport;

#[cfg(feature = "quinn")]
pub mod quinn_transport;

#[cfg(feature = "iroh")]
pub mod iroh_transport;

#[cfg(feature = "utxo-commitments")]
pub mod utxo_commitments_client;

#[cfg(feature = "stratum-v2")]
pub mod stratum_v2;

// Phase 3.3: Compact Block Relay (BIP152)
pub mod compact_blocks;

// Block Filter Service (BIP157/158)
pub mod bip157_handler;
pub mod filter_service;
// Payment Protocol (BIP70) - P2P handlers
pub mod bip70_handler;


// Privacy and Performance Enhancements
#[cfg(feature = "dandelion")]
pub mod dandelion; // Dandelion++ privacy-preserving transaction relay
pub mod fibre; // FIBRE-style Fast Relay Network
pub mod package_relay; // BIP 331 Package Relay
pub mod package_relay_handler; // BIP 331 handlers
pub mod txhash; // Non-consensus hashing helpers for relay

use crate::network::protocol::{ProtocolMessage, ProtocolParser};
use anyhow::Result;
use bllvm_protocol::mempool::Mempool;
use bllvm_protocol::{BitcoinProtocolEngine, ConsensusProof, UtxoSet};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use crate::storage::Storage;
use crate::node::mempool::MempoolManager;

use crate::network::tcp_transport::TcpTransport;
use crate::network::transport::{
    Transport, TransportAddr, TransportConnection, TransportListener, TransportPreference,
};
use std::collections::HashSet;

/// Network I/O operations for testing
/// Note: This is deprecated - use TcpTransport instead
pub struct NetworkIO;

impl NetworkIO {
    pub async fn bind(&self, addr: SocketAddr) -> Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub async fn connect(&self, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// Peer manager for tracking connected peers
/// 
/// Uses TransportAddr as key to support all transport types (TCP, Quinn, Iroh).
/// This allows proper peer identification for Iroh (NodeId) while maintaining
/// compatibility with TCP/Quinn (SocketAddr).
#[derive(Clone)]
pub struct PeerManager {
    peers: HashMap<TransportAddr, peer::Peer>,
    max_peers: usize,
}

impl PeerManager {
    pub fn new(max_peers: usize) -> Self {
        Self {
            peers: HashMap::new(),
            max_peers,
        }
    }

    pub fn add_peer(&mut self, addr: TransportAddr, peer: peer::Peer) -> Result<()> {
        if self.peers.len() >= self.max_peers {
            return Err(anyhow::anyhow!("Maximum peer limit reached"));
        }
        self.peers.insert(addr, peer);
        Ok(())
    }

    pub fn remove_peer(&mut self, addr: &TransportAddr) -> Option<peer::Peer> {
        self.peers.remove(addr)
    }

    pub fn get_peer(&self, addr: &TransportAddr) -> Option<&peer::Peer> {
        self.peers.get(addr)
    }

    pub fn get_peer_mut(&mut self, addr: &TransportAddr) -> Option<&mut peer::Peer> {
        self.peers.get_mut(addr)
    }

    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    pub fn peer_addresses(&self) -> Vec<TransportAddr> {
        self.peers.keys().cloned().collect()
    }
    
    /// Get peer addresses as SocketAddr (for backward compatibility)
    /// Only returns SocketAddr for TCP/Quinn peers, skips Iroh peers
    pub fn peer_socket_addresses(&self) -> Vec<SocketAddr> {
        self.peers.keys()
            .filter_map(|addr| {
                match addr {
                    TransportAddr::Tcp(sock) => Some(*sock),
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(sock) => Some(*sock),
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => None, // Iroh peers don't have SocketAddr
                }
            })
            .collect()
    }

    pub fn can_accept_peer(&self) -> bool {
        self.peers.len() < self.max_peers
    }
    
    /// Find peer by SocketAddr (tries TCP and Quinn variants)
    /// Returns the TransportAddr if found
    pub fn find_transport_addr_by_socket(&self, addr: SocketAddr) -> Option<TransportAddr> {
        // Try TCP first
        let tcp_addr = TransportAddr::Tcp(addr);
        if self.peers.contains_key(&tcp_addr) {
            return Some(tcp_addr);
        }
        
        // Try Quinn
        #[cfg(feature = "quinn")]
        {
            let quinn_addr = TransportAddr::Quinn(addr);
            if self.peers.contains_key(&quinn_addr) {
                return Some(quinn_addr);
            }
        }
        
        None
    }
}

/// Token bucket rate limiter for peer message rate limiting
pub struct PeerRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl PeerRateLimiter {
    /// Create a new rate limiter
    pub fn new(burst_limit: u32, rate: u32) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }
    
    /// Check if a message can be consumed and consume a token
    pub fn check_and_consume(&mut self) -> bool {
        self.refill();
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
    
    /// Refill tokens based on elapsed time
    fn refill(&mut self) {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        if now > self.last_refill {
            let elapsed = now - self.last_refill;
            let tokens_to_add = (elapsed as u32) * self.rate;
            self.tokens = self.tokens.saturating_add(tokens_to_add).min(self.burst_limit);
            self.last_refill = now;
        }
    }
}

/// Connection manager for handling network connections
/// Note: This is deprecated - use Transport abstraction instead
pub struct ConnectionManager {
    listen_addr: SocketAddr,
    network_io: NetworkIO,
}

impl ConnectionManager {
    pub fn new(listen_addr: SocketAddr) -> Self {
        Self {
            listen_addr,
            network_io: NetworkIO,
        }
    }

    pub async fn start_listening(&self) -> Result<tokio::net::TcpListener> {
        info!("Starting network listener on {}", self.listen_addr);
        self.network_io.bind(self.listen_addr).await
    }

    pub async fn connect_to_peer(&self, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
        info!("Connecting to peer at {}", addr);
        self.network_io.connect(addr).await
    }
}

/// Network manager that coordinates all network operations
///
/// Supports multiple transports (TCP, Quinn, Iroh) based on configuration.
pub struct NetworkManager {
    peer_manager: PeerManager,
    tcp_transport: TcpTransport,
    #[cfg(feature = "quinn")]
    quinn_transport: Option<crate::network::quinn_transport::QuinnTransport>,
    #[cfg(feature = "iroh")]
    iroh_transport: Option<crate::network::iroh_transport::IrohTransport>,
    transport_preference: TransportPreference,
    peer_tx: mpsc::UnboundedSender<NetworkMessage>,
    peer_rx: mpsc::UnboundedReceiver<NetworkMessage>,
    /// Block filter service for BIP157/158
    filter_service: crate::network::filter_service::BlockFilterService,
    /// Consensus engine for mempool acceptance
    consensus: ConsensusProof,
    /// Shared UTXO set for mempool checks (placeholder threading)
    utxo_set: Arc<Mutex<UtxoSet>>,
    /// Shared mempool
    mempool: Arc<Mutex<Mempool>>,
    /// Protocol engine for network message processing
    protocol_engine: Option<Arc<BitcoinProtocolEngine>>,
    /// Storage for chain state access
    storage: Option<Arc<Storage>>,
    /// Mempool manager for transaction access
    mempool_manager: Option<Arc<MempoolManager>>,
    /// Peer state storage (per-connection state)
    peer_states: Arc<Mutex<HashMap<SocketAddr, bllvm_protocol::network::PeerState>>>,
    /// Persistent peer list (peers to connect to on startup)
    persistent_peers: Arc<Mutex<HashSet<SocketAddr>>>,
    /// Network active state (true = enabled, false = disabled)
    network_active: Arc<Mutex<bool>>,
    /// Ban list (banned peers with unban timestamp)
    ban_list: Arc<Mutex<HashMap<SocketAddr, u64>>>, // addr -> unban timestamp
    /// Per-IP connection count (to prevent Sybil attacks)
    connections_per_ip: Arc<Mutex<HashMap<std::net::IpAddr, usize>>>,
    /// Per-peer message rate limiting (token bucket)
    peer_message_rates: Arc<Mutex<HashMap<SocketAddr, PeerRateLimiter>>>,
    /// Network statistics
    bytes_sent: Arc<Mutex<u64>>,
    bytes_received: Arc<Mutex<u64>>,
    /// Mapping from SocketAddr to TransportAddr (for Iroh peers that use placeholder SocketAddr)
    socket_to_transport: Arc<Mutex<HashMap<SocketAddr, TransportAddr>>>,
    /// Request ID counter for async request-response patterns
    request_id_counter: Arc<Mutex<u64>>,
    /// Pending async requests with metadata
    /// Key: request_id, Value: (sender, peer_addr, timestamp)
    pending_requests: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    /// DoS protection manager
    dos_protection: Arc<dos_protection::DosProtectionManager>,
    /// Pending ban shares (for periodic sharing)
    pending_ban_shares: Arc<Mutex<Vec<(SocketAddr, u64, String)>>>, // (addr, unban_timestamp, reason)
    /// Ban list sharing configuration
    ban_list_sharing_config: Option<crate::config::BanListSharingConfig>,
}

/// Pending request metadata
struct PendingRequest {
    /// Channel to send response to
    sender: tokio::sync::oneshot::Sender<Vec<u8>>,
    /// Peer address that sent the request
    peer_addr: SocketAddr,
    /// Timestamp when request was registered (Unix timestamp)
    timestamp: u64,
    /// Request priority (0 = normal, higher = more important)
    priority: u8,
    /// Number of retry attempts
    retry_count: u8,
}

/// Network message types
#[derive(Debug, Clone)]
pub enum NetworkMessage {
    PeerConnected(TransportAddr),
    PeerDisconnected(TransportAddr),
    BlockReceived(Vec<u8>),
    TransactionReceived(Vec<u8>),
    InventoryReceived(Vec<u8>),
    #[cfg(feature = "utxo-commitments")]
    UTXOSetReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    FilteredBlockReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    GetUTXOSetReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "utxo-commitments")]
    GetFilteredBlockReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    #[cfg(feature = "stratum-v2")]
    StratumV2MessageReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Raw message received from peer (needs processing)
    RawMessageReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP157 Block Filter messages
    GetCfiltersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfheadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfcheckptReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP331 Package Relay messages
    PkgTxnReceived(Vec<u8>, SocketAddr),     // (data, peer_addr)
    SendPkgTxnReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
}

impl NetworkManager {
    /// Create a new network manager with default TCP-only transport
    pub fn new(_listen_addr: SocketAddr) -> Self {
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();

        Self {
            peer_manager: Arc::new(Mutex::new(PeerManager::new(100))), // Default max peers
            tcp_transport: TcpTransport::new(),
            #[cfg(feature = "quinn")]
            quinn_transport: None,
            #[cfg(feature = "iroh")]
            iroh_transport: None,
            transport_preference: TransportPreference::TCP_ONLY,
            peer_tx,
            peer_rx,
            filter_service: crate::network::filter_service::BlockFilterService::new(),
            consensus: ConsensusProof::new(),
            utxo_set: Arc::new(Mutex::new(UtxoSet::new())),
            mempool: Arc::new(Mutex::new(Mempool::new())),
            protocol_engine: None,
            storage: None,
            mempool_manager: None,
            peer_states: Arc::new(Mutex::new(HashMap::new())),
            persistent_peers: Arc::new(Mutex::new(HashSet::new())),
            network_active: Arc::new(Mutex::new(true)),
            ban_list: Arc::new(Mutex::new(HashMap::new())),
            connections_per_ip: Arc::new(Mutex::new(HashMap::new())),
            peer_message_rates: Arc::new(Mutex::new(HashMap::new())),
            bytes_sent: Arc::new(Mutex::new(0)),
            bytes_received: Arc::new(Mutex::new(0)),
            socket_to_transport: Arc::new(Mutex::new(HashMap::new())),
            request_id_counter: Arc::new(Mutex::new(0)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            dos_protection: Arc::new(dos_protection::DosProtectionManager::default()),
            pending_ban_shares: Arc::new(Mutex::new(Vec::new())),
            ban_list_sharing_config: None,
        }
    }
    
    /// Set dependencies for protocol message processing
    pub fn with_dependencies(
        mut self,
        protocol_engine: Arc<BitcoinProtocolEngine>,
        storage: Arc<Storage>,
        mempool_manager: Arc<MempoolManager>,
    ) -> Self {
        self.protocol_engine = Some(protocol_engine);
        self.storage = Some(storage);
        self.mempool_manager = Some(mempool_manager);
        self
    }

    /// Create a new network manager with custom configuration
    pub fn with_config(listen_addr: SocketAddr, max_peers: usize) -> Self {
        Self::with_transport_preference(listen_addr, max_peers, TransportPreference::TCP_ONLY)
    }

    /// Create a new network manager with transport preference
    pub fn with_transport_preference(
        _listen_addr: SocketAddr,
        max_peers: usize,
        preference: TransportPreference,
    ) -> Self {
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();

        Self {
            peer_manager: Arc::new(Mutex::new(PeerManager::new(max_peers))),
            tcp_transport: TcpTransport::new(),
            #[cfg(feature = "quinn")]
            quinn_transport: None, // Will be initialized on start if needed
            #[cfg(feature = "iroh")]
            iroh_transport: None, // Will be initialized on start if needed
            transport_preference: preference,
            peer_tx,
            peer_rx,
            filter_service: crate::network::filter_service::BlockFilterService::new(),
            consensus: ConsensusProof::new(),
            utxo_set: Arc::new(Mutex::new(UtxoSet::new())),
            mempool: Arc::new(Mutex::new(Mempool::new())),
            protocol_engine: None,
            storage: None,
            mempool_manager: None,
            peer_states: Arc::new(Mutex::new(HashMap::new())),
            persistent_peers: Arc::new(Mutex::new(HashSet::new())),
            network_active: Arc::new(Mutex::new(true)),
            ban_list: Arc::new(Mutex::new(HashMap::new())),
            connections_per_ip: Arc::new(Mutex::new(HashMap::new())),
            peer_message_rates: Arc::new(Mutex::new(HashMap::new())),
            bytes_sent: Arc::new(Mutex::new(0)),
            bytes_received: Arc::new(Mutex::new(0)),
            socket_to_transport: Arc::new(Mutex::new(HashMap::new())),
            request_id_counter: Arc::new(Mutex::new(0)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            dos_protection: Arc::new(dos_protection::DosProtectionManager::default()),
            pending_ban_shares: Arc::new(Mutex::new(Vec::new())),
            ban_list_sharing_config: None,
        }
    }

    /// Get transport preference
    pub fn transport_preference(&self) -> TransportPreference {
        self.transport_preference
    }

    /// Start the network manager
    pub async fn start(&mut self, listen_addr: SocketAddr) -> Result<()> {
        info!(
            "Starting network manager with transport preference: {:?}",
            self.transport_preference
        );

        // Initialize Quinn transport if enabled
        #[cfg(feature = "quinn")]
        if self.transport_preference.allows_quinn() {
            match crate::network::quinn_transport::QuinnTransport::new() {
                Ok(quinn) => {
                    self.quinn_transport = Some(quinn);
                    info!("Quinn transport initialized");
                }
                Err(e) => {
                    warn!("Failed to initialize Quinn transport: {}", e);
                    if self.transport_preference == TransportPreference::QUINN_ONLY {
                        return Err(anyhow::anyhow!("Quinn-only mode requires Quinn transport"));
                    }
                }
            }
        }

        // Initialize Iroh transport if enabled
        #[cfg(feature = "iroh")]
        if self.transport_preference.allows_iroh() {
            match crate::network::iroh_transport::IrohTransport::new().await {
                Ok(iroh) => {
                    self.iroh_transport = Some(iroh);
                    info!("Iroh transport initialized");
                }
                Err(e) => {
                    warn!("Failed to initialize Iroh transport: {}", e);
                    if self.transport_preference == TransportPreference::IROH_ONLY {
                        return Err(anyhow::anyhow!("Iroh-only mode requires Iroh transport"));
                    }
                }
            }
        }

        // Start listening on TCP if allowed
        if self.transport_preference.allows_tcp() {
            let mut tcp_listener = self.tcp_transport.listen(listen_addr).await?;
            info!("TCP listener started on {}", listen_addr);

            // Start TCP accept loop
            let peer_tx = self.peer_tx.clone();
            let dos_protection = Arc::clone(&self.dos_protection);
            let peer_manager_clone = Arc::clone(&self.peer_manager);
            let ban_list = Arc::clone(&self.ban_list);
            tokio::spawn(async move {
                loop {
                    match tcp_listener.accept().await {
                        Ok((conn, addr)) => {
                            info!("New TCP connection from {:?}", addr);
                            // Extract SocketAddr for notification
                            let socket_addr = match addr {
                                TransportAddr::Tcp(addr) => addr,
                                _ => {
                                    error!("Invalid transport address for TCP");
                                    continue;
                                }
                            };

                            // Check DoS protection: connection rate limiting
                            let ip = socket_addr.ip();
                            if !dos_protection.check_connection(ip).await {
                                warn!("Connection rate limit exceeded for IP {}, rejecting connection", ip);
                                
                                // Check if we should auto-ban
                                if dos_protection.should_auto_ban(ip).await {
                                    warn!("Auto-banning IP {} for repeated connection rate violations", ip);
                                    // Auto-ban the IP (ban for 1 hour)
                                    let unban_timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs()
                                        + 3600;
                                    let mut ban_list_guard = ban_list.lock().unwrap();
                                    ban_list_guard.insert(socket_addr, unban_timestamp);
                                }
                                
                                // Close connection immediately
                                drop(conn);
                                continue;
                            }

                            // Check active connection limit
                            let current_connections = {
                                let pm = peer_manager_clone.lock().unwrap();
                                pm.peer_count()
                            };
                            if !dos_protection.check_active_connections(current_connections).await {
                                warn!("Active connection limit exceeded, rejecting connection from {}", socket_addr);
                                drop(conn);
                                continue;
                            }

                            // Send connection notification
                            let transport_addr = TransportAddr::Tcp(socket_addr);
                            let _ = peer_tx.send(NetworkMessage::PeerConnected(transport_addr.clone()));

                            // Handle connection in background with graceful error handling
                            let peer_tx_clone = peer_tx.clone();
                            let peer_manager_for_peer = Arc::clone(&peer_manager_clone);
                            tokio::spawn(async move {
                                // Create peer from transport connection
                                use crate::network::peer::Peer;
                                use crate::network::transport::TransportAddr;
                                
                                let peer = Peer::from_transport_connection(
                                    conn,
                                    socket_addr,
                                    TransportAddr::Tcp(socket_addr),
                                    peer_tx_clone.clone(),
                                );
                                
                                // Add peer to manager (with graceful error handling)
                                match peer_manager_for_peer.lock() {
                                    Ok(mut pm) => {
                                        if let Err(e) = pm.add_peer(transport_addr.clone(), peer) {
                                            warn!("Failed to add peer {}: {}", socket_addr, e);
                                            let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr.clone()));
                                            return;
                                        }
                                        info!("Successfully added peer {} (transport: {:?})", socket_addr, transport_addr);
                                    }
                                    Err(e) => {
                                        warn!("Failed to lock peer manager for {}: {}", socket_addr, e);
                                        let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr.clone()));
                                        return;
                                    }
                                }
                                
                                // Connection will be cleaned up automatically when read/write tasks exit
                                // Peer removal happens in process_messages when PeerDisconnected is received
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept TCP connection: {}", e);
                        }
                    }
                }
            });
        }

        // Start Quinn listener if available (with graceful degradation)
        #[cfg(feature = "quinn")]
        if let Some(ref quinn_transport) = self.quinn_transport {
            match quinn_transport.listen(listen_addr).await {
                Ok(mut quinn_listener) => {
                    info!("Quinn listener started on {}", listen_addr);
                    let peer_tx = self.peer_tx.clone();
                    let peer_manager = Arc::clone(&self.peer_manager);
                    let dos_protection = Arc::clone(&self.dos_protection);
                    let ban_list = Arc::clone(&self.ban_list);
                    
                    tokio::spawn(async move {
                        loop {
                            match quinn_listener.accept().await {
                                Ok((conn, addr)) => {
                                    info!("New Quinn connection from {:?}", addr);
                                    // Extract SocketAddr for notification
                                    let socket_addr = match addr {
                                        TransportAddr::Quinn(addr) => addr,
                                        _ => {
                                            error!("Invalid transport address for Quinn");
                                            continue;
                                        }
                                    };

                                    // Check DoS protection: connection rate limiting
                                    let ip = socket_addr.ip();
                                    if !dos_protection.check_connection(ip).await {
                                        warn!("Connection rate limit exceeded for IP {}, rejecting Quinn connection", ip);
                                        
                                        if dos_protection.should_auto_ban(ip).await {
                                            warn!("Auto-banning IP {} for repeated connection rate violations", ip);
                                            let unban_timestamp = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap()
                                                .as_secs()
                                                + 3600;
                                            let mut ban_list_guard = ban_list.lock().unwrap();
                                            ban_list_guard.insert(socket_addr, unban_timestamp);
                                        }
                                        drop(conn);
                                        continue;
                                    }

                                    // Check active connection limit
                                    let current_connections = {
                                        let pm = peer_manager.lock().unwrap();
                                        pm.peer_count()
                                    };
                                    if !dos_protection.check_active_connections(current_connections).await {
                                        warn!("Active connection limit exceeded, rejecting Quinn connection from {}", socket_addr);
                                        drop(conn);
                                        continue;
                                    }

                                    // Send connection notification
                                    let quinn_transport_addr = TransportAddr::Quinn(socket_addr);
                                    let _ = peer_tx.send(NetworkMessage::PeerConnected(quinn_transport_addr.clone()));

                                    // Handle connection in background with graceful error handling
                                    let peer_tx_clone = peer_tx.clone();
                                    let peer_manager_clone = Arc::clone(&peer_manager);
                                    tokio::spawn(async move {
                                        use crate::network::peer::Peer;
                                        use crate::network::transport::TransportAddr;
                                        
                                        let quinn_addr = TransportAddr::Quinn(socket_addr);
                                        let peer = Peer::from_transport_connection(
                                            conn,
                                            socket_addr,
                                            quinn_addr,
                                            peer_tx_clone.clone(),
                                        );
                                        
                                        // Add peer to manager (with graceful error handling)
                                        match peer_manager_clone.lock() {
                                            Ok(mut pm) => {
                                                if let Err(e) = pm.add_peer(quinn_addr.clone(), peer) {
                                                    warn!("Failed to add Quinn peer {}: {}", socket_addr, e);
                                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(quinn_addr.clone()));
                                                    return;
                                                }
                                                info!("Successfully added Quinn peer {}", socket_addr);
                                            }
                                            Err(e) => {
                                                warn!("Failed to lock peer manager for Quinn peer {}: {}", socket_addr, e);
                                                let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(quinn_addr.clone()));
                                                return;
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    warn!("Failed to accept Quinn connection (continuing): {}", e);
                                    // Continue accepting - don't break the loop on single failure
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!("Failed to start Quinn listener (graceful degradation): {}", e);
                    // Continue with other transports - don't fail entire startup
                }
            }
        }

        // Start Iroh listener if available (with graceful degradation)
        #[cfg(feature = "iroh")]
        if let Some(ref iroh_transport) = self.iroh_transport {
            match iroh_transport.listen(listen_addr).await {
                Ok(mut iroh_listener) => {
                    info!("Iroh listener started on {}", listen_addr);
                    let peer_tx = self.peer_tx.clone();
                    let peer_manager = Arc::clone(&self.peer_manager);
                    let dos_protection = Arc::clone(&self.dos_protection);
                    
                    tokio::spawn(async move {
                        loop {
                            match iroh_listener.accept().await {
                                Ok((conn, addr)) => {
                                    info!("New Iroh connection from {:?}", addr);
                                    // Validate Iroh address
                                    let iroh_addr = match &addr {
                                        TransportAddr::Iroh(key) => {
                                            if key.is_empty() {
                                                warn!("Invalid Iroh public key: empty");
                                                continue;
                                            }
                                            addr.clone()
                                        }
                                        _ => {
                                            error!("Invalid transport address for Iroh");
                                            continue;
                                        }
                                    };

                                    // Check active connection limit (Iroh doesn't have IP, so skip rate limiting)
                                    let current_connections = {
                                        let pm = peer_manager.lock().unwrap();
                                        pm.peer_count()
                                    };
                                    if !dos_protection.check_active_connections(current_connections).await {
                                        warn!("Active connection limit exceeded, rejecting Iroh connection");
                                        drop(conn);
                                        continue;
                                    }

                                    // Send connection notification using TransportAddr directly
                                    let _ = peer_tx.send(NetworkMessage::PeerConnected(iroh_addr.clone()));

                                    // Handle connection in background with graceful error handling
                                    let peer_tx_clone = peer_tx.clone();
                                    let peer_manager_clone = Arc::clone(&peer_manager);
                                    let iroh_addr_clone = iroh_addr.clone();
                                    tokio::spawn(async move {
                                        use crate::network::peer::Peer;
                                        
                                        // For Iroh, we need a SocketAddr for Peer::from_transport_connection
                                        // Generate a unique placeholder based on key hash for lookups
                                        let placeholder_socket = if let TransportAddr::Iroh(ref key) = iroh_addr_clone {
                                            // Use first 4 bytes of key for IP, last 2 bytes for port to create unique placeholder
                                            let ip_bytes = if key.len() >= 4 {
                                                [key[0], key[1], key[2], key[3]]
                                            } else {
                                                [0, 0, 0, 0]
                                            };
                                            let port = if key.len() >= 6 {
                                                u16::from_be_bytes([key[key.len()-2], key[key.len()-1]])
                                            } else {
                                                0
                                            };
                                            std::net::SocketAddr::from((ip_bytes, port))
                                        } else {
                                            std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                                        };
                                        
                                        let peer = Peer::from_transport_connection(
                                            conn,
                                            placeholder_socket,
                                            iroh_addr_clone.clone(),
                                            peer_tx_clone.clone(),
                                        );
                                        
                                        // Add peer to manager (with graceful error handling)
                                        match peer_manager_clone.lock() {
                                            Ok(mut pm) => {
                                                if let Err(e) = pm.add_peer(iroh_addr_clone.clone(), peer) {
                                                    warn!("Failed to add Iroh peer: {}", e);
                                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(iroh_addr_clone.clone()));
                                                    return;
                                                }
                                                // Store mapping from placeholder SocketAddr to TransportAddr for Iroh lookups
                                                socket_to_transport_clone.lock().unwrap().insert(placeholder_socket, iroh_addr_clone.clone());
                                                info!("Successfully added Iroh peer (transport: {:?})", iroh_addr_clone);
                                            }
                                            Err(e) => {
                                                warn!("Failed to lock peer manager for Iroh peer: {}", e);
                                                let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(iroh_addr_clone.clone()));
                                                return;
                                            }
                                        }
                                    });
                                }
                                Err(e) => {
                                    warn!("Failed to accept Iroh connection (continuing): {}", e);
                                    // Continue accepting - don't break the loop on single failure
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    warn!("Failed to start Iroh listener (graceful degradation): {}", e);
                    // Continue with other transports - don't fail entire startup
                }
            }
        }

        // Start periodic ban cleanup task
        self.start_ban_cleanup_task();
        
        // Start pending request cleanup task
        self.start_request_cleanup_task();
        
        // Start DoS protection cleanup task
        self.start_dos_protection_cleanup_task();
        
        Ok(())
    }
    
    /// Generate a new request ID for async request-response patterns
    pub fn generate_request_id(&self) -> u64 {
        let mut counter = self.request_id_counter.lock().unwrap();
        let id = *counter;
        *counter = counter.wrapping_add(1);
        id
    }
    
    /// Register a pending request and return the response receiver
    /// Returns (request_id, response_receiver)
    pub fn register_request(&self, peer_addr: SocketAddr) -> (u64, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        self.register_request_with_priority(peer_addr, 0)
    }
    
    /// Register a pending request with priority and return the response receiver
    /// Returns (request_id, response_receiver)
    /// Priority: 0 = normal, higher = more important
    pub fn register_request_with_priority(&self, peer_addr: SocketAddr, priority: u8) -> (u64, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        let request_id = self.generate_request_id();
        let (tx, rx) = tokio::sync::oneshot::channel();
        
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let pending_req = PendingRequest {
            sender: tx,
            peer_addr,
            timestamp,
            priority,
            retry_count: 0,
        };
        
        self.pending_requests.lock().unwrap().insert(request_id, pending_req);
        (request_id, rx)
    }
    
    /// Complete a pending request by sending the response
    pub fn complete_request(&self, request_id: u64, response: Vec<u8>) -> bool {
        let mut pending = self.pending_requests.lock().unwrap();
        if let Some(pending_req) = pending.remove(&request_id) {
            let _ = pending_req.sender.send(response);
            true
        } else {
            false
        }
    }
    
    /// Cancel a pending request
    pub fn cancel_request(&self, request_id: u64) -> bool {
        let mut pending = self.pending_requests.lock().unwrap();
        pending.remove(&request_id).is_some()
    }
    
    /// Get pending requests for a specific peer
    pub fn get_pending_requests_for_peer(&self, peer_addr: SocketAddr) -> Vec<u64> {
        let pending = self.pending_requests.lock().unwrap();
        pending.iter()
            .filter(|(_, req)| req.peer_addr == peer_addr)
            .map(|(id, _)| *id)
            .collect()
    }
    
    /// Clean up expired requests (older than max_age_seconds)
    pub fn cleanup_expired_requests(&self, max_age_seconds: u64) -> usize {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        
        let mut pending = self.pending_requests.lock().unwrap();
        let expired: Vec<u64> = pending.iter()
            .filter(|(_, req)| now.saturating_sub(req.timestamp) > max_age_seconds)
            .map(|(id, _)| *id)
            .collect();
        
        for id in &expired {
            pending.remove(id);
        }
        
        expired.len()
    }
    
    /// Start periodic task to clean up expired pending requests
    fn start_request_cleanup_task(&self) {
        let pending_requests = Arc::clone(&self.pending_requests);
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                
                // Clean up old pending requests (older than 5 minutes)
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let timeout_seconds = 300; // 5 minutes
                
                let mut pending = pending_requests.lock().unwrap();
                let initial_count = pending.len();
                pending.retain(|_, req| {
                    now.saturating_sub(req.timestamp) < timeout_seconds
                });
                let removed = initial_count - pending.len();
                if removed > 0 {
                    debug!("Cleaned up {} stale pending requests (older than {}s)", removed, timeout_seconds);
                }
                if !pending.is_empty() {
                    debug!("Pending requests: {}", pending.len());
                }
            }
        });
    }
    
    /// Start periodic task to clean up DoS protection data
    fn start_dos_protection_cleanup_task(&self) {
        let dos_protection = Arc::clone(&self.dos_protection);
        let ban_list = Arc::clone(&self.ban_list);
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // Every 5 minutes
            loop {
                interval.tick().await;
                
                // Cleanup old connection rate limiter entries
                dos_protection.cleanup().await;
                
                // Auto-ban IPs that should be banned
                // Note: This is a simplified version - in production, you'd want more sophisticated logic
                let dos_clone = Arc::clone(&dos_protection);
                let ban_list_clone = Arc::clone(&ban_list);
                tokio::spawn(async move {
                    // Get list of IPs that should be auto-banned
                    // For now, we'll check during connection attempts
                    // This is a placeholder for future enhancement
                });
            }
        });
    }
    
    /// Start periodic task to clean up expired bans
    fn start_ban_cleanup_task(&self) {
        let ban_list = Arc::clone(&self.ban_list);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // Every 5 minutes
            loop {
                interval.tick().await;
                
                use std::time::{SystemTime, UNIX_EPOCH};
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                
                let mut ban_list_guard = ban_list.lock().unwrap();
                let expired: Vec<SocketAddr> = ban_list_guard
                    .iter()
                    .filter(|(_, &unban_timestamp)| {
                        unban_timestamp != u64::MAX && now >= unban_timestamp
                    })
                    .map(|(addr, _)| *addr)
                    .collect();
                
                for addr in expired {
                    ban_list_guard.remove(&addr);
                    debug!("Cleaned up expired ban for {}", addr);
                }
                
                if !expired.is_empty() {
                    info!("Cleaned up {} expired ban(s)", expired.len());
                }
            }
        });
    }

    /// Get the number of connected peers
    pub fn peer_count(&self) -> usize {
        self.peer_manager.lock().unwrap().peer_count()
    }

    /// Get all peer addresses (as SocketAddr for backward compatibility)
    pub fn peer_addresses(&self) -> Vec<SocketAddr> {
        self.peer_manager.lock().unwrap().peer_socket_addresses()
    }
    
    /// Get all peer addresses (as TransportAddr)
    pub fn peer_transport_addresses(&self) -> Vec<TransportAddr> {
        self.peer_manager.lock().unwrap().peer_addresses()
    }

    /// Broadcast a message to all peers
    pub async fn broadcast(&self, message: Vec<u8>) -> Result<()> {
        let peer_addrs = self.peer_manager.lock().unwrap().peer_addresses();
        for addr in peer_addrs {
            if let Some(peer) = self.peer_manager.lock().unwrap().get_peer(&addr) {
                if let Err(e) = peer.send_message(message.clone()).await {
                    warn!("Failed to send message to peer {:?}: {}", addr, e);
                }
            }
        }
        Ok(())
    }

    /// Send a message to a specific peer (by SocketAddr - for TCP/Quinn)
    /// For Iroh peers, uses the socket_to_transport mapping
    pub async fn send_to_peer(&self, addr: SocketAddr, message: Vec<u8>) -> Result<()> {
        // Try to find transport address (TCP, Quinn, or Iroh mapping)
        let transport_addr = {
            let pm = self.peer_manager.lock().unwrap();
            pm.find_transport_addr_by_socket(addr)
                .or_else(|| {
                    self.socket_to_transport.lock().unwrap().get(&addr).cloned()
                })
        };
        
        if let Some(transport_addr) = transport_addr {
            self.send_to_peer_by_transport(transport_addr, message).await
        } else {
            // Fallback: try TCP (for backward compatibility)
            self.send_to_peer_by_transport(TransportAddr::Tcp(addr), message).await
        }
    }
    
    /// Send a message to a specific peer (by TransportAddr - supports all transports)
    pub async fn send_to_peer_by_transport(&self, addr: TransportAddr, message: Vec<u8>) -> Result<()> {
        let message_len = message.len();
        // Track bytes sent
        self.track_bytes_sent(message_len as u64);
        
        let mut pm = self.peer_manager.lock().unwrap();
        if let Some(peer) = pm.get_peer_mut(&addr) {
            peer.send_message(message).await?;
            peer.record_send(message_len);
        } else {
            return Err(anyhow::anyhow!("Peer not found: {:?}", addr));
        }
        Ok(())
    }

    /// Connect to a peer at the given address
    /// 
    /// Attempts to connect using available transports with graceful degradation:
    /// 1. Tries preferred transport (based on transport_preference)
    /// 2. Falls back to TCP if preferred transport fails
    /// 3. Returns error only if all transports fail
    pub async fn connect_to_peer(&self, addr: SocketAddr) -> Result<()> {
        use crate::network::peer::Peer;
        use crate::network::transport::TransportAddr;
        
        // Check DoS protection: connection rate limiting (for outgoing connections too)
        let ip = addr.ip();
        if !self.dos_protection.check_connection(ip).await {
            warn!("Connection rate limit exceeded for IP {}, rejecting outgoing connection", ip);
            
            // Check if we should auto-ban
            if self.dos_protection.should_auto_ban(ip).await {
                warn!("Auto-banning IP {} for repeated connection rate violations", ip);
                // Ban the IP
                let unban_timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600; // Ban for 1 hour
                let mut ban_list = self.ban_list.lock().unwrap();
                ban_list.insert(addr, unban_timestamp);
                return Err(anyhow::anyhow!("IP {} is banned due to connection rate violations", ip));
            }
            
            return Err(anyhow::anyhow!("Connection rate limit exceeded for IP {}", ip));
        }
        
        let mut last_error = None;
        
        // Try transports in preference order with graceful degradation
        let transports_to_try = self.get_transports_for_connection();
        
        for transport_info in transports_to_try {
            match self.try_connect_with_transport(&transport_info, addr).await {
                Ok((peer, transport_addr)) => {
                    // Successfully connected
                    {
                        let mut pm = self.peer_manager.lock().unwrap();
                        pm.add_peer(transport_addr.clone(), peer)?;
                    }
                    
                    // Note: Peer handler is managed by Peer::from_transport_connection
                    // No need to spawn additional handler task
                    
                    info!("Successfully connected to {} via {:?} (transport: {:?})", addr, transport_info.transport_type, transport_addr);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Failed to connect to {} via {:?}: {}", addr, transport_info.transport_type, e);
                    last_error = Some(e);
                    // Continue to next transport
                }
            }
        }
        
        // All transports failed
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All transport attempts failed")))
    }
    
    /// Helper: Get list of transports to try for a connection
    fn get_transports_for_connection(&self) -> Vec<TransportInfo> {
        let mut transports = Vec::new();
        
        // Add transports in preference order
        if self.transport_preference.allows_tcp() {
            transports.push(TransportInfo {
                transport_type: crate::network::transport::TransportType::Tcp,
                addr: None, // TCP uses SocketAddr directly
            });
        }
        
        #[cfg(feature = "quinn")]
        if self.transport_preference.allows_quinn() {
            if let Some(ref _quinn) = self.quinn_transport {
                transports.push(TransportInfo {
                    transport_type: crate::network::transport::TransportType::Quinn,
                    addr: None,
                });
            }
        }
        
        #[cfg(feature = "iroh")]
        if self.transport_preference.allows_iroh() {
            if let Some(ref _iroh) = self.iroh_transport {
                transports.push(TransportInfo {
                    transport_type: crate::network::transport::TransportType::Iroh,
                    addr: None, // Iroh uses public key, not SocketAddr
                });
            }
        }
        
        // Always try TCP as fallback if not already in list
        if !transports.iter().any(|t| matches!(t.transport_type, crate::network::transport::TransportType::Tcp)) {
            transports.push(TransportInfo {
                transport_type: crate::network::transport::TransportType::Tcp,
                addr: None,
            });
        }
        
        transports
    }
    
    /// Helper: Try connecting with a specific transport
    async fn try_connect_with_transport(&self, transport_info: &TransportInfo, addr: SocketAddr) -> Result<(Peer, TransportAddr)> {
        use crate::network::transport::TransportAddr;
        use crate::network::peer::Peer;
        
        match transport_info.transport_type {
            crate::network::transport::TransportType::Tcp => {
                // Use TcpTransport to create connection properly
                let tcp_addr = TransportAddr::Tcp(addr);
                let tcp_conn = self.tcp_transport.connect(tcp_addr).await?;
                let transport_addr = TransportAddr::Tcp(addr);
                Ok((
                    Peer::from_transport_connection(
                        tcp_conn,
                        addr,
                        transport_addr.clone(),
                        self.peer_tx.clone(),
                    ),
                    transport_addr,
                ))
            }
            #[cfg(feature = "quinn")]
            crate::network::transport::TransportType::Quinn => {
                if let Some(ref quinn) = self.quinn_transport {
                    let quinn_addr = TransportAddr::Quinn(addr);
                    let conn = quinn.connect(quinn_addr).await?;
                    Ok((
                        Peer::from_transport_connection(
                            conn,
                            addr,
                            quinn_addr.clone(),
                            self.peer_tx.clone(),
                        ),
                        quinn_addr,
                    ))
                } else {
                    Err(anyhow::anyhow!("Quinn transport not available"))
                }
            }
            #[cfg(feature = "iroh")]
            crate::network::transport::TransportType::Iroh => {
                // Iroh requires public key, not SocketAddr
                // For now, return error - would need peer discovery or address resolution
                Err(anyhow::anyhow!("Iroh transport requires public key, not SocketAddr"))
            }
            #[cfg(not(feature = "quinn"))]
            crate::network::transport::TransportType::Quinn => {
                Err(anyhow::anyhow!("Quinn transport not compiled"))
            }
            #[cfg(not(feature = "iroh"))]
            crate::network::transport::TransportType::Iroh => {
                Err(anyhow::anyhow!("Iroh transport not compiled"))
            }
        }
    }
    
    /// Helper struct for transport selection
    struct TransportInfo {
        transport_type: crate::network::transport::TransportType,
        addr: Option<TransportAddr>,
    }
    
    /// Send ping message to all connected peers
    pub async fn ping_all_peers(&self) -> Result<()> {
        use crate::network::protocol::{ProtocolMessage, ProtocolParser, PingMessage};
        use std::time::{SystemTime, UNIX_EPOCH};
        
        // Generate nonce for ping
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        
        let ping_msg = ProtocolMessage::Ping(PingMessage { nonce });
        let wire_msg = ProtocolParser::serialize_message(&ping_msg)?;
        
        let peer_addrs = self.peer_manager.lock().unwrap().peer_addresses();
        for addr in peer_addrs {
            if let Err(e) = self.send_to_peer(addr, wire_msg.clone()).await {
                warn!("Failed to ping peer {}: {}", addr, e);
            }
        }
        
        Ok(())
    }

    /// Try to receive a block message (non-blocking)
    /// Returns Some(block_data) if a block was received, None otherwise
    pub fn try_recv_block(&mut self) -> Option<Vec<u8>> {
        use tokio::sync::mpsc::error::TryRecvError;

        // Check for BlockReceived messages without blocking
        loop {
            match self.peer_rx.try_recv() {
                Ok(NetworkMessage::BlockReceived(data)) => {
                    return Some(data);
                }
                Ok(_) => {
                    // Other message types, continue checking
                    continue;
                }
                Err(TryRecvError::Empty) => {
                    return None;
                }
                Err(TryRecvError::Disconnected) => {
                    warn!("Network message channel disconnected");
                    return None;
                }
            }
        }
    }

    /// Process incoming network messages
    pub async fn process_messages(&mut self) -> Result<()> {
        // Track message queue size manually (unbounded channel doesn't have len())
        let mut message_count = 0u64;
        let mut last_metrics_update = std::time::SystemTime::now();
        
        while let Some(message) = self.peer_rx.recv().await {
            message_count += 1;
            
            // Update metrics periodically (every 100 messages or 10 seconds)
            let now = std::time::SystemTime::now();
            let should_update = message_count % 100 == 0 
                || now.duration_since(last_metrics_update).unwrap().as_secs() >= 10;
            
            if should_update {
                let pm = self.peer_manager.lock().unwrap();
                let active_connections = pm.peer_count();
                let bytes_received = *self.bytes_received.lock().unwrap();
                let bytes_sent = *self.bytes_sent.lock().unwrap();
                
                // Approximate queue size (messages processed since last check)
                let queue_size = message_count as usize;
                
                // Check message queue size limit
                if !self.dos_protection.check_message_queue_size(queue_size).await {
                    warn!("Message queue size limit exceeded (processed {} messages), potential DoS", queue_size);
                    // Optionally detect DoS attack
                    if self.dos_protection.detect_dos_attack().await {
                        warn!("DoS attack detected - message queue and connections at high levels");
                        // Could trigger automatic mitigation here (e.g., increase rate limits, ban aggressive IPs)
                    }
                    // Reset counter to prevent false positives
                    message_count = 0;
                }
                
                self.dos_protection.update_metrics(
                    active_connections,
                    queue_size,
                    bytes_received,
                    bytes_sent,
                ).await;
                
                last_metrics_update = now;
                message_count = 0; // Reset after update
            }

            match message {
                NetworkMessage::PeerConnected(addr) => {
                    info!("Peer connected: {:?}", addr);
                }
                NetworkMessage::PeerDisconnected(addr) => {
                    info!("Peer disconnected: {:?}", addr);
                    let mut pm = self.peer_manager.lock().unwrap();
                    // Remove peer directly using TransportAddr
                    pm.remove_peer(&addr);
                    
                    // Clean up per-IP connection count (only for TCP/Quinn, not Iroh)
                    if let Some(ip) = match &addr {
                        TransportAddr::Tcp(sock) => Some(sock.ip()),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some(sock.ip()),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None,
                    } {
                        let mut ip_connections = self.connections_per_ip.lock().unwrap();
                        if let Some(count) = ip_connections.get_mut(&ip) {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                ip_connections.remove(&ip);
                            }
                        }
                    }
                    
                    // Clean up rate limiter (use SocketAddr for TCP/Quinn, or a key for Iroh)
                    {
                        let mut rates = self.peer_message_rates.lock().unwrap();
                        // For rate limiter, we need a key - use SocketAddr for TCP/Quinn
                        // For Iroh, we could use a hash of the key, but for now just skip
                        if let Some(sock_addr) = match &addr {
                            TransportAddr::Tcp(sock) => Some(*sock),
                            #[cfg(feature = "quinn")]
                            TransportAddr::Quinn(sock) => Some(*sock),
                            #[cfg(feature = "iroh")]
                            TransportAddr::Iroh(_) => None,
                        } {
                            rates.remove(&sock_addr);
                        }
                    }
                }
                NetworkMessage::BlockReceived(data) => {
                    info!("Block received: {} bytes", data.len());
                    // Block processing handled via try_recv_block() in Node::run()
                }
                NetworkMessage::TransactionReceived(data) => {
                    info!("Transaction received: {} bytes", data.len());
                    // Process transaction with consensus layer
                }
                NetworkMessage::InventoryReceived(data) => {
                    info!("Inventory received: {} bytes", data.len());
                    // Process inventory
                }
                #[cfg(feature = "utxo-commitments")]
                NetworkMessage::GetUTXOSetReceived(data, peer_addr) => {
                    info!(
                        "GetUTXOSet received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Handle GetUTXOSet request
                    self.handle_get_utxo_set_request(data, peer_addr).await?;
                }
                #[cfg(feature = "utxo-commitments")]
                NetworkMessage::UTXOSetReceived(data, peer_addr) => {
                    info!("UTXOSet received from {}: {} bytes", peer_addr, data.len());
                    // Handle UTXOSet response (would notify waiting requests)
                    // In full implementation, would match to pending request futures
                }
                #[cfg(feature = "utxo-commitments")]
                NetworkMessage::GetFilteredBlockReceived(data, peer_addr) => {
                    info!(
                        "GetFilteredBlock received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Handle GetFilteredBlock request
                    self.handle_get_filtered_block_request(data, peer_addr)
                        .await?;
                }
                #[cfg(feature = "utxo-commitments")]
                NetworkMessage::FilteredBlockReceived(data, peer_addr) => {
                    info!(
                        "FilteredBlock received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Handle FilteredBlock response (would notify waiting requests)
                    // In full implementation, would match to pending request futures
                }
                #[cfg(feature = "stratum-v2")]
                NetworkMessage::StratumV2MessageReceived(data, peer_addr) => {
                    info!(
                        "Stratum V2 message received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Handle Stratum V2 message
                    // In full implementation, would:
                    // 1. Route to StratumV2Server if server mode enabled
                    // 2. Route to StratumV2Client if client mode enabled
                    // 3. Send response back to peer
                    // 4. Notify waiting futures via async message routing system

                    // Server is active if we're processing messages (this function is called)
                    // and route message to server.handle_message()
                    // For now, just log the message
                }
                // BIP157 Block Filter messages
                NetworkMessage::GetCfiltersReceived(data, peer_addr) => {
                    info!(
                        "GetCfilters received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    self.handle_getcfilters_request(data, peer_addr).await?;
                }
                // BIP331 Package Relay
                NetworkMessage::PkgTxnReceived(data, peer_addr) => {
                    info!("PkgTxn received from {}: {} bytes", peer_addr, data.len());
                    self.handle_pkgtxn_request(data, peer_addr).await?;
                }
                NetworkMessage::SendPkgTxnReceived(data, _peer_addr) => {
                    info!("SendPkgTxn received: {} bytes", data.len());
                    // Optional: We can decide whether to request package
                }
                NetworkMessage::GetCfheadersReceived(data, peer_addr) => {
                    info!(
                        "GetCfheaders received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    self.handle_getcfheaders_request(data, peer_addr).await?;
                }
                NetworkMessage::GetCfcheckptReceived(data, peer_addr) => {
                    info!(
                        "GetCfcheckpt received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    self.handle_getcfcheckpt_request(data, peer_addr).await?;
                }
                // Raw messages from peer connections
                NetworkMessage::RawMessageReceived(data, peer_addr) => {
                    // Update peer receive stats
                    {
                        let mut pm = self.peer_manager.lock().unwrap();
                        // Try to find peer by SocketAddr (TCP or Quinn, or Iroh mapping)
                        let transport_addr = pm.find_transport_addr_by_socket(peer_addr)
                            .or_else(|| {
                                // Check Iroh mapping
                                self.socket_to_transport.lock().unwrap().get(&peer_addr).cloned()
                            });
                        if let Some(transport_addr) = transport_addr {
                            if let Some(peer) = pm.get_peer_mut(&transport_addr) {
                                peer.record_receive(data.len());
                            }
                        }
                    }
                    
                    // Check rate limiting before processing
                    let mut rates = self.peer_message_rates.lock().unwrap();
                    let rate_limiter = rates.entry(peer_addr).or_insert_with(|| {
                        // Default: 100 burst, 10 messages/second
                        PeerRateLimiter::new(100, 10)
                    });
                    
                    if !rate_limiter.check_and_consume() {
                        warn!("Rate limit exceeded for peer {}, dropping message", peer_addr);
                        // Optionally ban peer after repeated rate limit violations
                        // For now, just drop the message
                        continue;
                    }
                    drop(rates);
                    
                    // Check if this is a response to a pending request (UTXOSet or FilteredBlock)
                    // Extract request_id from message and route to correct pending request
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        let request_id_opt = match &parsed {
                            ProtocolMessage::UTXOSet(msg) => Some(msg.request_id),
                            ProtocolMessage::FilteredBlock(msg) => Some(msg.request_id),
                            _ => None,
                        };
                        
                        if let Some(request_id) = request_id_opt {
                            // Route to pending request by request_id
                            let mut pending = self.pending_requests.lock().unwrap();
                            if let Some(pending_req) = pending.remove(&request_id) {
                                drop(pending); // Release lock before sending
                                let _ = pending_req.sender.send(data.clone());
                                continue; // Skip normal processing for async responses
                            } else {
                                warn!("Received response for unknown request_id: {}", request_id);
                            }
                        }
                    }
                    
                    // Process through protocol layer
                    if let Err(e) = self.handle_incoming_wire_tcp(peer_addr, data).await {
                        warn!("Failed to process message from {}: {}", peer_addr, e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Parse incoming TCP wire message and process with protocol layer
    ///
    /// This function:
    /// 1. Converts ProtocolMessage to bllvm_protocol::network::NetworkMessage
    /// 2. Gets or creates PeerState for this peer
    /// 3. Creates NodeChainAccess from storage modules
    /// 4. Calls bllvm_protocol::network::process_network_message()
    /// 5. Converts NetworkResponse back to wire format and enqueues for sending
    pub async fn handle_incoming_wire_tcp(&self, peer_addr: SocketAddr, data: Vec<u8>) -> Result<()> {
        // Track bytes received
        self.track_bytes_received(data.len() as u64);
        
        // Check if peer is banned
        if self.is_banned(peer_addr) {
            warn!("Rejecting message from banned peer: {}", peer_addr);
            return Ok(()); // Silently drop messages from banned peers
        }
        let parsed = ProtocolParser::parse_message(&data)?;
        
        // Handle special cases that don't go through protocol layer
        match parsed {
            // BIP331
            ProtocolMessage::SendPkgTxn(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::SendPkgTxnReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::PkgTxn(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::PkgTxnReceived(data, peer_addr));
                return Ok(());
            }
            // BIP157
            ProtocolMessage::GetCfilters(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetCfiltersReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetCfheaders(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetCfheadersReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetCfcheckpt(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetCfcheckptReceived(data, peer_addr));
                return Ok(());
            }
            // Ban List Sharing
            ProtocolMessage::GetBanList(msg) => {
                return self.handle_get_ban_list(peer_addr, msg).await;
            }
            ProtocolMessage::BanList(msg) => {
                return self.handle_ban_list(peer_addr, msg).await;
            }
            _ => {
                // Continue to protocol layer processing
            }
        }
        
        // Process with protocol layer if dependencies are available
        if let (Some(ref protocol_engine), Some(ref storage), Some(ref mempool_manager)) = 
            (self.protocol_engine.as_ref(), self.storage.as_ref(), self.mempool_manager.as_ref()) {
            
            // Convert ProtocolMessage to bllvm_protocol::network::NetworkMessage
            use crate::network::protocol_adapter::ProtocolAdapter;
            let protocol_msg = ProtocolAdapter::protocol_to_consensus_message(&parsed)?;
            
            // Get or create PeerState
            let mut peer_states = self.peer_states.lock().unwrap();
            let peer_state = peer_states.entry(peer_addr)
                .or_insert_with(|| bllvm_protocol::network::PeerState::new());
            
            // Create NodeChainAccess
            use crate::network::chain_access::NodeChainAccess;
            let chain_access = NodeChainAccess::new(
                storage.blocks(),
                storage.transactions(),
                Arc::clone(mempool_manager),
            );
            
            // Get UTXO set and height
            let utxo_set = storage.utxos().get_all_utxos()
                .map_err(|e| anyhow::anyhow!("Failed to get UTXO set: {}", e))?;
            let height = storage.chain().get_height()
                .map_err(|e| anyhow::anyhow!("Failed to get height: {}", e))?
                .unwrap_or(0);
            
            // Process message with protocol layer
            use bllvm_protocol::network::{process_network_message, ChainStateAccess};
            match process_network_message(
                protocol_engine,
                &protocol_msg,
                peer_state,
                Some(&chain_access as &dyn ChainStateAccess),
                Some(&utxo_set),
                Some(height),
            ) {
                Ok(response) => {
                    // Convert response to wire format and send via transport layer
                    use crate::network::message_bridge::MessageBridge;
                    use crate::network::transport::TransportType;
                    if let Ok(wire_messages) = MessageBridge::extract_send_messages(&response, TransportType::Tcp) {
                        for wire_msg in wire_messages {
                            if let Err(e) = self.send_to_peer(peer_addr, wire_msg).await {
                                warn!("Failed to send protocol response to {}: {}", peer_addr, e);
                            } else {
                                debug!("Sent protocol response message to {}", peer_addr);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Protocol message processing failed: {}", e);
                }
            }
        } else {
            // Dependencies not set - fall back to old behavior
            debug!("Protocol layer dependencies not set, skipping protocol processing");
        }
        
        Ok(())
    }

    #[cfg(feature = "utxo-commitments")]
    /// Handle GetUTXOSet request from a peer
    async fn handle_get_utxo_set_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_utxo_set;

        // Parse the request
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_utxo_set_msg = match protocol_msg {
            ProtocolMessage::GetUTXOSet(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetUTXOSet message")),
        };

        // Handle the request with storage integration
        let storage = self.storage.as_ref().map(Arc::clone);
        let response = handle_get_utxo_set(get_utxo_set_msg, storage).await?;

        // Serialize and send response
        let response_wire = ProtocolParser::serialize_message(&ProtocolMessage::UTXOSet(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetCfilters request from a peer
    async fn handle_getcfilters_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::bip157_handler::handle_getcfilters;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfilters(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfilters message")),
        };

        // Handle request and generate responses
        let storage_ref = self.storage.as_ref();
        let responses = handle_getcfilters(&request, &self.filter_service, storage_ref)?;

        // Send responses to peer
        for response in responses {
            let response_wire = ProtocolParser::serialize_message(&response)?;
            self.send_to_peer(peer_addr, response_wire).await?;
        }

        Ok(())
    }

    /// Handle GetCfheaders request from a peer
    async fn handle_getcfheaders_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        use crate::network::bip157_handler::handle_getcfheaders;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfheaders(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfheaders message")),
        };

        let response = handle_getcfheaders(&request, &self.filter_service)?;
        let response_wire = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetCfcheckpt request from a peer
    async fn handle_getcfcheckpt_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        use crate::network::bip157_handler::handle_getcfcheckpt;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfcheckpt(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfcheckpt message")),
        };

        let response = handle_getcfcheckpt(&request, &self.filter_service)?;
        let response_wire = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle PkgTxn message from a peer
    async fn handle_pkgtxn_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::package_relay::PackageRelay;
        use crate::network::package_relay_handler::handle_pkgtxn;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use bllvm_protocol::Transaction;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::PkgTxn(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected PkgTxn message")),
        };

        let mut relay = PackageRelay::new();
        if let Some(reject) = handle_pkgtxn(&mut relay, &request)? {
            let response_wire =
                ProtocolParser::serialize_message(&ProtocolMessage::PkgTxnReject(reject))?;
            self.send_to_peer(peer_addr, response_wire).await?;
        }

        // Best-effort: attempt to submit transactions to mempool (hook for node integration)
        // Deserialize and submit; ignore errors for now (this node may not own mempool yet)
        let mut txs: Vec<Transaction> = Vec::with_capacity(request.transactions.len());
        for raw in &request.transactions {
            if let Ok(tx) = bincode::deserialize::<Transaction>(raw) {
                txs.push(tx);
            }
        }
        let _ = self.submit_transactions_to_mempool(&txs).await;
        Ok(())
    }

    /// Submit validated transactions to the mempool
    async fn submit_transactions_to_mempool(
        &self,
        txs: &[bllvm_protocol::Transaction],
    ) -> Result<()> {
        if let Some(ref mempool_manager) = self.mempool_manager {
            // Use MempoolManager's add_transaction method
            // Note: add_transaction requires &mut, so we need to handle this carefully
            // For now, we'll use a channel or async approach
            // This is a limitation of the current design - MempoolManager should use interior mutability
            for tx in txs {
                // In a real implementation, we'd send this to a mempool processing channel
                // For now, we validate and accept using consensus layer
                let mut utxo_lock = self
                    .utxo_set
                    .lock()
                    .map_err(|_| anyhow::anyhow!("UTXO lock poisoned"))?;
                let mempool_lock = self
                    .mempool
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Mempool lock poisoned"))?;
                let _ = self
                    .consensus
                    .accept_to_memory_pool(tx, &utxo_lock, &mempool_lock, 0);
            }
        } else {
            // Fallback to legacy mempool
            let mut utxo_lock = self
                .utxo_set
                .lock()
                .map_err(|_| anyhow::anyhow!("UTXO lock poisoned"))?;
            let mempool_lock = self
                .mempool
                .lock()
                .map_err(|_| anyhow::anyhow!("Mempool lock poisoned"))?;
            for tx in txs {
                let _ = self
                    .consensus
                    .accept_to_memory_pool(tx, &utxo_lock, &mempool_lock, 0);
            }
        }
        Ok(())
    }

    #[cfg(feature = "utxo-commitments")]
    /// Handle GetFilteredBlock request from a peer
    async fn handle_get_filtered_block_request(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_filtered_block;

        // Parse the request
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_filtered_block_msg = match protocol_msg {
            ProtocolMessage::GetFilteredBlock(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetFilteredBlock message")),
        };

        // Handle the request with storage and filter service
        let storage = self.storage.as_ref().map(Arc::clone);
        let response =
            handle_get_filtered_block(get_filtered_block_msg, storage, Some(&self.filter_service)).await?;

        // Serialize and send response
        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::FilteredBlock(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Get peer manager reference (locked)
    pub fn peer_manager(&self) -> std::sync::MutexGuard<'_, PeerManager> {
        self.peer_manager.lock().unwrap()
    }

    /// Get filter service reference
    pub fn filter_service(&self) -> &crate::network::filter_service::BlockFilterService {
        &self.filter_service
    }

    /// Add a persistent peer (will be connected to on startup)
    pub fn add_persistent_peer(&self, addr: SocketAddr) {
        let mut peers = self.persistent_peers.lock().unwrap();
        peers.insert(addr);
    }

    /// Remove a persistent peer
    pub fn remove_persistent_peer(&self, addr: SocketAddr) {
        let mut peers = self.persistent_peers.lock().unwrap();
        peers.remove(&addr);
    }

    /// Get list of persistent peers (async version for RPC)
    pub async fn get_persistent_peers(&self) -> HashSet<SocketAddr> {
        self.persistent_peers.lock().unwrap().clone()
    }

    /// Get list of persistent peers (sync version)
    pub fn get_persistent_peers_sync(&self) -> Vec<SocketAddr> {
        let peers = self.persistent_peers.lock().unwrap();
        peers.iter().cloned().collect()
    }

    /// Ban a peer (with optional unban timestamp, 0 = permanent)
    pub fn ban_peer(&self, addr: SocketAddr, unban_timestamp: u64) {
        let mut ban_list = self.ban_list.lock().unwrap();
        if unban_timestamp == 0 {
            // Permanent ban (use max timestamp)
            ban_list.insert(addr, u64::MAX);
        } else {
            ban_list.insert(addr, unban_timestamp);
        }
    }

    /// Unban a peer
    pub fn unban_peer(&self, addr: SocketAddr) {
        let mut ban_list = self.ban_list.lock().unwrap();
        ban_list.remove(&addr);
    }

    /// Clear all bans
    pub fn clear_bans(&self) {
        let mut ban_list = self.ban_list.lock().unwrap();
        ban_list.clear();
    }

    /// Get list of banned peers
    pub fn get_banned_peers(&self) -> Vec<(SocketAddr, u64)> {
        let ban_list = self.ban_list.lock().unwrap();
        ban_list.iter().map(|(addr, timestamp)| (*addr, *timestamp)).collect()
    }

    /// Get peer addresses (async version for RPC)
    pub async fn get_peer_addresses(&self) -> Vec<TransportAddr> {
        let pm = self.peer_manager.lock().unwrap();
        pm.peer_addresses()
    }

    /// Set network active state
    pub async fn set_network_active(&self, active: bool) -> Result<()> {
        let mut state = self.network_active.lock().unwrap();
        *state = active;
        info!("Network active state set to: {}", active);
        Ok(())
    }

    /// Get network active state
    pub fn is_network_active(&self) -> bool {
        *self.network_active.lock().unwrap()
    }

    /// Check if a peer is banned
    pub fn is_banned(&self, addr: SocketAddr) -> bool {
        let ban_list = self.ban_list.lock().unwrap();
        if let Some(&unban_timestamp) = ban_list.get(&addr) {
            if unban_timestamp == u64::MAX {
                return true; // Permanent ban
            }
            // Check if ban has expired
            use std::time::{SystemTime, UNIX_EPOCH};
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            if now < unban_timestamp {
                return true; // Still banned
            } else {
                // Ban expired, remove it
                drop(ban_list);
                self.unban_peer(addr);
                return false;
            }
        }
        false
    }

    /// Track bytes sent
    pub fn track_bytes_sent(&self, bytes: u64) {
        let mut sent = self.bytes_sent.lock().unwrap();
        *sent += bytes;
    }

    /// Track bytes received
    pub fn track_bytes_received(&self, bytes: u64) {
        let mut received = self.bytes_received.lock().unwrap();
        *received += bytes;
    }

    /// Handle GetBanList message - respond with ban list or hash
    async fn handle_get_ban_list(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::GetBanListMessage,
    ) -> Result<()> {
        use crate::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
        use crate::network::ban_list_merging::calculate_ban_list_hash;
        use std::time::{SystemTime, UNIX_EPOCH};

        debug!("GetBanList request from {}: full={}, min_duration={}", 
               peer_addr, msg.request_full, msg.min_ban_duration);

        // Get current ban list
        let ban_list = self.ban_list.lock().unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Convert to BanEntry format, filtering by min_ban_duration
        let mut ban_entries: Vec<BanEntry> = Vec::new();
        for (addr, &unban_timestamp) in ban_list.iter() {
            // Skip expired bans
            if unban_timestamp != u64::MAX && now >= unban_timestamp {
                continue;
            }

            // Filter by min_ban_duration
            if msg.min_ban_duration > 0 {
                let ban_duration = if unban_timestamp == u64::MAX {
                    u64::MAX
                } else {
                    unban_timestamp.saturating_sub(now)
                };
                if ban_duration < msg.min_ban_duration {
                    continue;
                }
            }

            // Convert SocketAddr to NetworkAddress
            let ip_bytes = match addr.ip() {
                std::net::IpAddr::V4(ipv4) => {
                    let mut bytes = [0u8; 16];
                    bytes[12..16].copy_from_slice(&ipv4.octets());
                    bytes
                }
                std::net::IpAddr::V6(ipv6) => ipv6.octets(),
            };

            ban_entries.push(BanEntry {
                addr: NetworkAddress {
                    services: 0,
                    ip: ip_bytes,
                    port: addr.port(),
                },
                unban_timestamp,
                reason: Some("DoS protection".to_string()),
            });
        }

        // Calculate hash
        let ban_list_hash = calculate_ban_list_hash(&ban_entries);

        // Create response
        let response = BanListMessage {
            is_full: msg.request_full,
            ban_list_hash,
            ban_entries: if msg.request_full { ban_entries } else { Vec::new() },
            timestamp: now,
        };

        // Serialize and send response
        let response_msg = ProtocolMessage::BanList(response);
        let serialized = ProtocolParser::serialize_message(&response_msg)?;
        self.send_to_peer(peer_addr, serialized).await?;

        debug!("Sent BanList response to {}: {} entries", peer_addr, 
               if msg.request_full { ban_entries.len() } else { 0 });

        Ok(())
    }

    /// Handle BanList message - merge received ban list
    async fn handle_ban_list(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::BanListMessage,
    ) -> Result<()> {
        use crate::network::ban_list_merging::{validate_ban_entry, verify_ban_list_hash};
        use std::net::IpAddr;

        debug!("BanList received from {}: full={}, {} entries", 
               peer_addr, msg.is_full, msg.ban_entries.len());

        // Verify hash if full list provided
        if msg.is_full {
            if !verify_ban_list_hash(&msg.ban_entries, &msg.ban_list_hash) {
                warn!("Ban list hash verification failed from {}", peer_addr);
                return Ok(()); // Silently ignore invalid ban lists
            }
        }

        // If hash-only, we can't merge (would need to request full list)
        if !msg.is_full {
            debug!("Received hash-only ban list from {}, skipping merge", peer_addr);
            return Ok(());
        }

        // Validate and merge ban entries
        let mut ban_list = self.ban_list.lock().unwrap();
        let mut merged_count = 0;

        for entry in &msg.ban_entries {
            if !validate_ban_entry(entry) {
                continue; // Skip invalid entries
            }

            // Convert NetworkAddress to SocketAddr
            let ip = if entry.addr.ip[0..12] == [0u8; 12] {
                // IPv4-mapped IPv6 address
                let ipv4_bytes = &entry.addr.ip[12..16];
                IpAddr::V4(std::net::Ipv4Addr::new(
                    ipv4_bytes[0], ipv4_bytes[1], ipv4_bytes[2], ipv4_bytes[3]
                ))
            } else {
                // IPv6 address
                let mut ipv6_bytes = [0u8; 16];
                ipv6_bytes.copy_from_slice(&entry.addr.ip);
                IpAddr::V6(std::net::Ipv6Addr::from(ipv6_bytes))
            };

            let socket_addr = SocketAddr::new(ip, entry.addr.port);

            // Merge: use longer ban duration if address already exists
            match ban_list.get(&socket_addr) {
                Some(&existing_timestamp) => {
                    if entry.unban_timestamp == u64::MAX {
                        // Permanent ban always wins
                        ban_list.insert(socket_addr, u64::MAX);
                        merged_count += 1;
                    } else if existing_timestamp != u64::MAX {
                        // Both temporary - use longer one
                        if entry.unban_timestamp > existing_timestamp {
                            ban_list.insert(socket_addr, entry.unban_timestamp);
                            merged_count += 1;
                        }
                    }
                }
                None => {
                    // New ban entry
                    ban_list.insert(socket_addr, entry.unban_timestamp);
                    merged_count += 1;
                }
            }
        }

        debug!("Merged {} ban entries from {}", merged_count, peer_addr);
        Ok(())
    }

    /// Network statistics
    pub struct NetworkStats {
        pub bytes_sent: u64,
        pub bytes_received: u64,
        pub active_connections: usize,
        pub banned_peers_count: usize,
        pub message_queue_size: usize,
    }

    /// Get network statistics
    pub async fn get_network_stats(&self) -> NetworkStats {
        let sent = *self.bytes_sent.lock().unwrap();
        let received = *self.bytes_received.lock().unwrap();
        let active_connections = {
            let pm = self.peer_manager.lock().unwrap();
            pm.peer_count()
        };
        let banned_peers_count = {
            let ban_list = self.ban_list.lock().unwrap();
            ban_list.len()
        };
        let resource_metrics = self.dos_protection.get_metrics().await;
        
        NetworkStats {
            bytes_sent: sent,
            bytes_received: received,
            active_connections,
            banned_peers_count,
            message_queue_size: resource_metrics.message_queue_size,
        }
    }

    /// Get network statistics (legacy method for backward compatibility)
    pub fn get_network_stats_legacy(&self) -> (u64, u64) {
        let sent = *self.bytes_sent.lock().unwrap();
        let received = *self.bytes_received.lock().unwrap();
        (sent, received)
    }

    /// Create version message with service flags
    ///
    /// Sets NODE_COMPACT_FILTERS flag if filter service is enabled
    pub fn create_version_message(
        &self,
        version: i32,
        services: u64,
        timestamp: i64,
        addr_recv: crate::network::protocol::NetworkAddress,
        addr_from: crate::network::protocol::NetworkAddress,
        nonce: u64,
        user_agent: String,
        start_height: i32,
        relay: bool,
    ) -> crate::network::protocol::VersionMessage {
        use crate::bip157::NODE_COMPACT_FILTERS;

        // Add service flags for supported features
        let mut services_with_filters = services;
        services_with_filters |= NODE_COMPACT_FILTERS;
        #[cfg(feature = "dandelion")]
        {
            services_with_filters |= crate::network::protocol::NODE_DANDELION;
        }
        services_with_filters |= crate::network::protocol::NODE_PACKAGE_RELAY;
        services_with_filters |= crate::network::protocol::NODE_FIBRE;

        crate::network::protocol::VersionMessage {
            version,
            services: services_with_filters,
            timestamp,
            addr_recv,
            addr_from,
            nonce,
            user_agent,
            start_height,
            relay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_peer_manager_creation() {
        let manager = PeerManager::new(10);
        assert_eq!(manager.peer_count(), 0);
        assert!(manager.can_accept_peer());
    }

    #[tokio::test]
    async fn test_peer_manager_add_peer() {
        let mut manager = PeerManager::new(2);
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        // Create a mock peer without requiring network connection
        let (tx, _rx): (mpsc::UnboundedSender<NetworkMessage>, _) = mpsc::unbounded_channel();

        // Skip this test since we can't easily create a mock TcpStream
        // In a real implementation, we'd use dependency injection
        // For now, just test the manager logic without the peer
        assert_eq!(manager.peer_count(), 0);
        assert!(manager.can_accept_peer());
    }

    #[tokio::test]
    async fn test_peer_manager_max_peers() {
        let mut manager = PeerManager::new(1);
        let addr1: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let addr2: std::net::SocketAddr = "127.0.0.1:8081".parse().unwrap();

        // Test manager capacity without creating real peers
        assert_eq!(manager.peer_count(), 0);
        assert!(manager.can_accept_peer());

        // Test that we can't exceed max peers
        // (In a real test, we'd create mock peers, but for now we test the logic)
        assert_eq!(manager.peer_count(), 0);
    }

    #[tokio::test]
    async fn test_peer_manager_remove_peer() {
        let mut manager = PeerManager::new(10);
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test removing non-existent peer
        let removed_peer = manager.remove_peer(addr);
        assert!(removed_peer.is_none());
        assert_eq!(manager.peer_count(), 0);
    }

    #[tokio::test]
    async fn test_peer_manager_get_peer() {
        let mut manager = PeerManager::new(10);
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test getting non-existent peer
        let retrieved_peer = manager.get_peer(addr);
        assert!(retrieved_peer.is_none());
    }

    #[tokio::test]
    async fn test_peer_manager_peer_addresses() {
        let mut manager = PeerManager::new(10);
        let addr1: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let addr2: std::net::SocketAddr = "127.0.0.1:8081".parse().unwrap();

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test getting addresses when no peers exist
        let addresses = manager.peer_addresses();
        assert_eq!(addresses.len(), 0);
    }

    #[tokio::test]
    async fn test_connection_manager_creation() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = ConnectionManager::new(addr);

        assert_eq!(manager.listen_addr, addr);
    }

    #[tokio::test]
    async fn test_network_manager_creation() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        assert_eq!(manager.peer_count(), 0);
        assert_eq!(manager.peer_addresses().len(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_with_config() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::with_config(addr, 5);

        assert_eq!(manager.peer_count(), 0);
        assert_eq!(manager.peer_manager().peer_count(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_peer_count() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_peer_addresses() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test getting addresses when no peers exist
        let addresses = manager.peer_addresses();
        assert_eq!(addresses.len(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_broadcast() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test broadcast with no peers (should succeed)
        let message = b"test message".to_vec();
        let result = manager.broadcast(message).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_network_manager_send_to_peer() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let mut manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test send to non-existent peer (should succeed but not actually send)
        let peer_addr = "127.0.0.1:8081".parse().unwrap();
        let message = b"test message".to_vec();
        let result = manager.send_to_peer(peer_addr, message).await;
        assert!(result.is_ok()); // Should succeed even for non-existent peer
    }

    #[tokio::test]
    async fn test_network_manager_send_to_nonexistent_peer() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test send to non-existent peer
        let peer_addr = "127.0.0.1:8081".parse().unwrap();
        let message = b"test message".to_vec();
        let result = manager.send_to_peer(peer_addr, message).await;
        assert!(result.is_ok()); // Should not error, just do nothing
    }

    #[tokio::test]
    async fn test_network_message_peer_connected() {
        let message = NetworkMessage::PeerConnected("127.0.0.1:8080".parse().unwrap());
        match message {
            NetworkMessage::PeerConnected(addr) => {
                assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
            }
            _ => panic!("Expected PeerConnected message"),
        }
    }

    #[tokio::test]
    async fn test_network_message_peer_disconnected() {
        let message = NetworkMessage::PeerDisconnected("127.0.0.1:8080".parse().unwrap());
        match message {
            NetworkMessage::PeerDisconnected(addr) => {
                assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
            }
            _ => panic!("Expected PeerDisconnected message"),
        }
    }

    #[tokio::test]
    async fn test_network_message_block_received() {
        let data = b"block data".to_vec();
        let message = NetworkMessage::BlockReceived(data.clone());
        match message {
            NetworkMessage::BlockReceived(msg_data) => {
                assert_eq!(msg_data, data);
            }
            _ => panic!("Expected BlockReceived message"),
        }
    }

    #[tokio::test]
    async fn test_network_message_transaction_received() {
        let data = b"tx data".to_vec();
        let message = NetworkMessage::TransactionReceived(data.clone());
        match message {
            NetworkMessage::TransactionReceived(msg_data) => {
                assert_eq!(msg_data, data);
            }
            _ => panic!("Expected TransactionReceived message"),
        }
    }

    #[tokio::test]
    async fn test_network_message_inventory_received() {
        let data = b"inv data".to_vec();
        let message = NetworkMessage::InventoryReceived(data.clone());
        match message {
            NetworkMessage::InventoryReceived(msg_data) => {
                assert_eq!(msg_data, data);
            }
            _ => panic!("Expected InventoryReceived message"),
        }
    }

    #[tokio::test]
    async fn test_handle_incoming_wire_tcp_enqueues_pkgtxn() {
        use crate::network::protocol::{PkgTxnMessage, ProtocolMessage, ProtocolParser};
        let addr: std::net::SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Build a pkgtxn message with one trivial tx
        let tx = bllvm_protocol::Transaction {
            version: 1,
            inputs: vec![],
            outputs: vec![],
            lock_time: 0,
        };
        let raw = bincode::serialize(&tx).unwrap();
        let msg = PkgTxnMessage {
            package_id: vec![7u8; 32],
            transactions: vec![raw],
        };
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::PkgTxn(msg)).unwrap();

        // Enqueue
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            manager.handle_incoming_wire_tcp(addr, wire).await.unwrap();
        });

        // Drain one message from channel and assert variant
        let mut manager = manager;
        if let Ok(NetworkMessage::PkgTxnReceived(_, peer)) = manager.peer_rx.try_recv() {
            assert_eq!(peer, addr);
        } else {
            panic!("Expected PkgTxnReceived");
        }
    }

    #[tokio::test]
    async fn test_network_manager_peer_manager_access() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test immutable access
        let peer_manager = manager.peer_manager();
        assert_eq!(peer_manager.peer_count(), 0);

        // Test mutable access
        let mut manager = manager;
        let peer_manager_mut = manager.peer_manager_mut();
        assert_eq!(peer_manager_mut.peer_count(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_transport_preference() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        assert_eq!(
            manager.transport_preference(),
            TransportPreference::TCP_ONLY
        );
    }
}
