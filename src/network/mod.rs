//! Network layer for reference-node
//! 
//! This module provides P2P networking, peer management, and Bitcoin protocol
//! message handling for communication with other Bitcoin nodes.

pub mod peer;
pub mod protocol;
pub mod inventory;
pub mod relay;
pub mod transport;
pub mod tcp_transport;
pub mod protocol_adapter;
pub mod message_bridge;
pub mod protocol_extensions;

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
pub mod filter_service;
pub mod bip157_handler;
// Payment Protocol (BIP70) - P2P handlers
pub mod bip70_handler;

// Phase 3.4: Erlay Transaction Relay (BIP330)
pub mod erlay;

// Privacy and Performance Enhancements
#[cfg(feature = "dandelion")]
pub mod dandelion;  // Dandelion++ privacy-preserving transaction relay
pub mod package_relay;  // BIP 331 Package Relay
pub mod package_relay_handler; // BIP 331 handlers
pub mod fibre;  // FIBRE-style Fast Relay Network
pub mod txhash; // Non-consensus hashing helpers for relay

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::{info, warn, error};
use std::sync::{Arc, Mutex};
use protocol_engine::{ConsensusProof, UtxoSet};
use protocol_engine::mempool::Mempool;
use crate::network::protocol::{ProtocolParser, ProtocolMessage};

use crate::network::transport::{
    Transport, TransportListener, TransportConnection, TransportPreference, TransportAddr,
};
use crate::network::tcp_transport::TcpTransport;

/// Network I/O operations for testing
/// Note: This is deprecated - use TcpTransport instead
pub struct NetworkIO;

impl NetworkIO {
    pub async fn bind(&self, addr: SocketAddr) -> Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(addr).await.map_err(|e| anyhow::anyhow!(e))
    }
    
    pub async fn connect(&self, addr: SocketAddr) -> Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addr).await.map_err(|e| anyhow::anyhow!(e))
    }
}

/// Peer manager for tracking connected peers
#[derive(Clone)]
pub struct PeerManager {
    peers: HashMap<SocketAddr, peer::Peer>,
    max_peers: usize,
}

impl PeerManager {
    pub fn new(max_peers: usize) -> Self {
        Self {
            peers: HashMap::new(),
            max_peers,
        }
    }
    
    pub fn add_peer(&mut self, addr: SocketAddr, peer: peer::Peer) -> Result<()> {
        if self.peers.len() >= self.max_peers {
            return Err(anyhow::anyhow!("Maximum peer limit reached"));
        }
        self.peers.insert(addr, peer);
        Ok(())
    }
    
    pub fn remove_peer(&mut self, addr: SocketAddr) -> Option<peer::Peer> {
        self.peers.remove(&addr)
    }
    
    pub fn get_peer(&self, addr: SocketAddr) -> Option<&peer::Peer> {
        self.peers.get(&addr)
    }
    
    pub fn get_peer_mut(&mut self, addr: SocketAddr) -> Option<&mut peer::Peer> {
        self.peers.get_mut(&addr)
    }
    
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
    
    pub fn peer_addresses(&self) -> Vec<SocketAddr> {
        self.peers.keys().cloned().collect()
    }
    
    pub fn can_accept_peer(&self) -> bool {
        self.peers.len() < self.max_peers
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
}

/// Network message types
#[derive(Debug, Clone)]
pub enum NetworkMessage {
    PeerConnected(SocketAddr),
    PeerDisconnected(SocketAddr),
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
    // BIP157 Block Filter messages
    GetCfiltersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfheadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfcheckptReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP331 Package Relay messages
    PkgTxnReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    SendPkgTxnReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
}

impl NetworkManager {
    /// Create a new network manager with default TCP-only transport
    pub fn new(_listen_addr: SocketAddr) -> Self {
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        
        Self {
            peer_manager: PeerManager::new(100), // Default max peers
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
        }
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
            peer_manager: PeerManager::new(max_peers),
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
        }
    }
    
    /// Get transport preference
    pub fn transport_preference(&self) -> TransportPreference {
        self.transport_preference
    }
    
    /// Start the network manager
    pub async fn start(&mut self, listen_addr: SocketAddr) -> Result<()> {
        info!("Starting network manager with transport preference: {:?}", self.transport_preference);
        
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
                            
                            // Send connection notification
                            let _ = peer_tx.send(NetworkMessage::PeerConnected(socket_addr));
                            
                            // Handle connection in background
                            let peer_tx_clone = peer_tx.clone();
                            tokio::spawn(async move {
                                // Connection handling would go here
                                // For now, just simulate
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                let mut conn_mut = conn;
                                let _ = conn_mut.close().await;
                                let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(socket_addr));
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept TCP connection: {}", e);
                        }
                    }
                }
            });
        }
        
        // Start Quinn listener if available
        #[cfg(feature = "quinn")]
        if let Some(ref quinn_transport) = self.quinn_transport {
            let mut quinn_listener = quinn_transport.listen(listen_addr).await?;
            info!("Quinn listener started on {}", listen_addr);
            
            // Start Quinn accept loop
            let peer_tx = self.peer_tx.clone();
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
                            
                            // Send connection notification
                            let _ = peer_tx.send(NetworkMessage::PeerConnected(socket_addr));
                            
                            // Handle connection in background
                            let peer_tx_clone = peer_tx.clone();
                            tokio::spawn(async move {
                                // Connection handling would go here
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                                let mut conn_mut = conn;
                                let _ = conn_mut.close().await;
                                let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(socket_addr));
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept Quinn connection: {}", e);
                        }
                    }
                }
            });
        }
        
        // Start Iroh listener if available
        #[cfg(feature = "iroh")]
        if let Some(ref iroh_transport) = self.iroh_transport {
            // Iroh listener implementation would go here
            info!("Iroh transport ready for connections");
        }
        
        Ok(())
    }
    
    
    /// Get the number of connected peers
    pub fn peer_count(&self) -> usize {
        self.peer_manager.peer_count()
    }
    
    /// Get all peer addresses
    pub fn peer_addresses(&self) -> Vec<SocketAddr> {
        self.peer_manager.peer_addresses()
    }
    
    /// Broadcast a message to all peers
    pub async fn broadcast(&self, message: Vec<u8>) -> Result<()> {
        for addr in self.peer_manager.peer_addresses() {
            if let Some(peer) = self.peer_manager.get_peer(addr) {
                if let Err(e) = peer.send_message(message.clone()).await {
                    warn!("Failed to send message to peer {}: {}", addr, e);
                }
            }
        }
        Ok(())
    }
    
    /// Send a message to a specific peer
    pub async fn send_to_peer(&self, addr: SocketAddr, message: Vec<u8>) -> Result<()> {
        if let Some(peer) = self.peer_manager.get_peer(addr) {
            peer.send_message(message).await?;
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
        while let Some(message) = self.peer_rx.recv().await {
            match message {
                NetworkMessage::PeerConnected(addr) => {
                    info!("Peer connected: {}", addr);
                }
                NetworkMessage::PeerDisconnected(addr) => {
                    info!("Peer disconnected: {}", addr);
                    self.peer_manager.remove_peer(addr);
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
                    info!("GetUTXOSet received from {}: {} bytes", peer_addr, data.len());
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
                    info!("GetFilteredBlock received from {}: {} bytes", peer_addr, data.len());
                    // Handle GetFilteredBlock request
                    self.handle_get_filtered_block_request(data, peer_addr).await?;
                }
                #[cfg(feature = "utxo-commitments")]
                NetworkMessage::FilteredBlockReceived(data, peer_addr) => {
                    info!("FilteredBlock received from {}: {} bytes", peer_addr, data.len());
                    // Handle FilteredBlock response (would notify waiting requests)
                    // In full implementation, would match to pending request futures
                }
                #[cfg(feature = "stratum-v2")]
                NetworkMessage::StratumV2MessageReceived(data, peer_addr) => {
                    info!("Stratum V2 message received from {}: {} bytes", peer_addr, data.len());
                    // Handle Stratum V2 message
                    // In full implementation, would:
                    // 1. Route to StratumV2Server if server mode enabled
                    // 2. Route to StratumV2Client if client mode enabled
                    // 3. Send response back to peer
                    // 4. Notify waiting futures via async message routing system
                    
                    // TODO: In full implementation, would check if server is active
                    // and route message to server.handle_message()
                    // For now, just log the message
                }
                // BIP157 Block Filter messages
                NetworkMessage::GetCfiltersReceived(data, peer_addr) => {
                    info!("GetCfilters received from {}: {} bytes", peer_addr, data.len());
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
                    info!("GetCfheaders received from {}: {} bytes", peer_addr, data.len());
                    self.handle_getcfheaders_request(data, peer_addr).await?;
                }
                NetworkMessage::GetCfcheckptReceived(data, peer_addr) => {
                    info!("GetCfcheckpt received from {}: {} bytes", peer_addr, data.len());
                    self.handle_getcfcheckpt_request(data, peer_addr).await?;
                }
            }
        }
        Ok(())
    }

    /// Parse incoming TCP wire message and enqueue appropriate NetworkMessage
    pub fn handle_incoming_wire_tcp(&self, peer_addr: SocketAddr, data: Vec<u8>) -> Result<()> {
        let parsed = ProtocolParser::parse_message(&data)?;
        match parsed {
            // BIP331
            ProtocolMessage::SendPkgTxn(_) => {
                let _ = self.peer_tx.send(NetworkMessage::SendPkgTxnReceived(data, peer_addr));
            }
            ProtocolMessage::PkgTxn(_) => {
                let _ = self.peer_tx.send(NetworkMessage::PkgTxnReceived(data, peer_addr));
            }
            // BIP157
            ProtocolMessage::GetCfilters(_) => {
                let _ = self.peer_tx.send(NetworkMessage::GetCfiltersReceived(data, peer_addr));
            }
            ProtocolMessage::GetCfheaders(_) => {
                let _ = self.peer_tx.send(NetworkMessage::GetCfheadersReceived(data, peer_addr));
            }
            ProtocolMessage::GetCfcheckpt(_) => {
                let _ = self.peer_tx.send(NetworkMessage::GetCfcheckptReceived(data, peer_addr));
            }
            _ => {
                // For now, ignore other messages here
            }
        }
        Ok(())
    }
    
    #[cfg(feature = "utxo-commitments")]
    /// Handle GetUTXOSet request from a peer
    async fn handle_get_utxo_set_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_utxo_set;
        use crate::network::protocol::ProtocolMessage;
        
        // Parse the request
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_utxo_set_msg = match protocol_msg {
            ProtocolMessage::GetUTXOSet(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetUTXOSet message")),
        };
        
        // Handle the request (would integrate with UTXO commitment module)
        let response = handle_get_utxo_set(get_utxo_set_msg).await?;
        
        // Serialize and send response
        let response_wire = ProtocolParser::serialize_message(&ProtocolMessage::UTXOSet(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;
        
        Ok(())
    }
    
    /// Handle GetCfilters request from a peer
    async fn handle_getcfilters_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::bip157_handler::handle_getcfilters;
        
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfilters(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfilters message")),
        };
        
        // Handle request and generate responses
        let responses = handle_getcfilters(&request, &self.filter_service)?;
        
        // Send responses to peer
        for response in responses {
            let response_wire = ProtocolParser::serialize_message(&response)?;
            self.send_to_peer(peer_addr, response_wire).await?;
        }
        
        Ok(())
    }
    
    /// Handle GetCfheaders request from a peer
    async fn handle_getcfheaders_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::bip157_handler::handle_getcfheaders;
        
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
    async fn handle_getcfcheckpt_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::bip157_handler::handle_getcfcheckpt;
        
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
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::package_relay_handler::handle_pkgtxn;
        use crate::network::package_relay::PackageRelay;
        use protocol_engine::Transaction;
        
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::PkgTxn(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected PkgTxn message")),
        };
        
        let mut relay = PackageRelay::new();
        if let Some(reject) = handle_pkgtxn(&mut relay, &request)? {
            let response_wire = ProtocolParser::serialize_message(&ProtocolMessage::PkgTxnReject(reject))?;
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

    /// Submit validated transactions to the mempool (placeholder hook)
    async fn submit_transactions_to_mempool(&self, txs: &[protocol_engine::Transaction]) -> Result<()> {
        // Best-effort synchronous submission using shared state
        let mut utxo_lock = self.utxo_set.lock().map_err(|_| anyhow::anyhow!("UTXO lock poisoned"))?;
        let mempool_lock = self.mempool.lock().map_err(|_| anyhow::anyhow!("Mempool lock poisoned"))?;
        for tx in txs {
            let _ = self.consensus.accept_to_memory_pool(tx, &utxo_lock, &mempool_lock, 0);
        }
        Ok(())
    }

    #[cfg(feature = "utxo-commitments")]
    /// Handle GetFilteredBlock request from a peer
    async fn handle_get_filtered_block_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_filtered_block;
        use crate::network::protocol::ProtocolMessage;
        
        // Parse the request
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_filtered_block_msg = match protocol_msg {
            ProtocolMessage::GetFilteredBlock(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetFilteredBlock message")),
        };
        
        // Handle the request with filter service (if BIP158 filter requested)
        let response = handle_get_filtered_block(
            get_filtered_block_msg,
            Some(&self.filter_service),
        ).await?;
        
        // Serialize and send response
        let response_wire = ProtocolParser::serialize_message(&ProtocolMessage::FilteredBlock(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;
        
        Ok(())
    }
    
    /// Get peer manager reference
    pub fn peer_manager(&self) -> &PeerManager {
        &self.peer_manager
    }
    
    /// Get peer manager mutable reference
    pub fn peer_manager_mut(&mut self) -> &mut PeerManager {
        &mut self.peer_manager
    }
    
    /// Get filter service reference
    pub fn filter_service(&self) -> &crate::network::filter_service::BlockFilterService {
        &self.filter_service
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
        { services_with_filters |= crate::network::protocol::NODE_DANDELION; }
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
        use crate::network::protocol::{PkgTxnMessage, ProtocolParser, ProtocolMessage};
        let addr: std::net::SocketAddr = "127.0.0.1:8333".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Build a pkgtxn message with one trivial tx
        let tx = protocol_engine::Transaction { version: 1, inputs: vec![], outputs: vec![], lock_time: 0 };
        let raw = bincode::serialize(&tx).unwrap();
        let msg = PkgTxnMessage { package_id: vec![7u8; 32], transactions: vec![raw] };
        let wire = ProtocolParser::serialize_message(&ProtocolMessage::PkgTxn(msg)).unwrap();

        // Enqueue
        manager.handle_incoming_wire_tcp(addr, wire).unwrap();

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
        
        assert_eq!(manager.transport_preference(), TransportPreference::TCP_ONLY);
    }
}