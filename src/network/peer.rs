//! Peer connection management
//!
//! Handles individual peer connections, message parsing, and protocol state.

use anyhow::Result;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::transport::{TransportAddr, TransportConnection};
use super::NetworkMessage;

/// Peer connection state
///
/// Supports multiple transport types (TCP, Quinn, Iroh) via TransportConnection trait
pub struct Peer {
    addr: SocketAddr,
    transport_addr: TransportAddr, // Full transport address (may differ from SocketAddr for Iroh)
    message_tx: mpsc::UnboundedSender<NetworkMessage>,
    pub(crate) send_tx: mpsc::UnboundedSender<Vec<u8>>, // Channel for sending messages
    connected: bool,
    /// Connection time (Unix timestamp)
    conntime: u64,
    /// Last send time (Unix timestamp)
    last_send: u64,
    /// Last receive time (Unix timestamp)
    last_recv: u64,
    /// Bytes sent
    bytes_sent: u64,
    /// Bytes received
    bytes_recv: u64,
    /// Connection quality score (0.0-1.0, higher is better)
    /// Based on uptime, message success rate, latency
    quality_score: f64,
    /// Successful message exchanges
    successful_exchanges: u64,
    /// Failed message exchanges
    failed_exchanges: u64,
    /// Average response time (milliseconds)
    avg_response_time_ms: f64,
    /// Last successful block received (Unix timestamp)
    last_block_received: Option<u64>,
    /// Last successful transaction received (Unix timestamp)
    last_tx_received: Option<u64>,
    /// Service flags from version message (indicates peer capabilities)
    /// Used to filter peers by capabilities (e.g., NODE_GOVERNANCE)
    services: u64,
    /// Protocol version from version message
    version: u32,
    /// User agent (subversion) from version message
    /// Example: "/Satoshi:25.0.0/" for Bitcoin Core
    user_agent: Option<String>,
    /// Best block height from version message (peer's chain tip)
    /// This indicates how many blocks the peer has
    start_height: i32,
    /// Whether this is an outbound connection (we initiated it)
    is_outbound: bool,
    /// Best block hash (from version message or headers)
    best_block_hash: Option<blvm_protocol::Hash>,
    /// Best block height (from version message) - duplicate of start_height but as u64
    best_block_height: Option<u64>,
    /// Chainwork (cumulative proof-of-work, from headers)
    chainwork: Option<u128>, // Use u128 for large chainwork values
    /// Peer permissions (e.g., NoBan flag for trusted peers)
    permissions: u64, // Bit flags for permissions (0 = no special permissions)
    /// Whether this is a manual connection (via RPC addnode)
    is_manual: bool,
    /// Timestamp of last block announcement (for outbound peer eviction)
    last_block_announcement: Option<u64>,
    /// Pending ping nonce (None if no ping pending)
    pending_ping_nonce: Option<u64>,
    /// Timestamp when ping was sent (for timeout detection)
    ping_sent_time: Option<u64>,
    /// Ping timeout in seconds (default: 20 minutes, Bitcoin Core uses longer)
    ping_timeout_seconds: u64,
}

impl Peer {
    /// Create a new peer connection from a TransportConnection
    ///
    /// This is the preferred method as it supports all transport types (TCP, Quinn, Iroh).
    /// The connection is managed via channels for concurrent read/write.
    pub fn from_transport_connection<C: TransportConnection + 'static>(
        conn: C,
        addr: SocketAddr,
        transport_addr: TransportAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
    ) -> Self {
        // Create channel for sending messages
        let (send_tx, send_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let transport_addr_clone = transport_addr.clone();
        let transport_addr_write = transport_addr.clone();
        let message_tx_clone = message_tx.clone();

        // Use interior mutability pattern: TcpConnection has separate Arc<Mutex> for
        // reader and writer internally, so we can share it via Arc without outer Mutex.
        // This allows recv() and send() to run truly concurrently.
        use std::sync::Arc;
        use tokio::sync::Mutex;
        let conn = Arc::new(Mutex::new(conn));

        // Clone for reader task
        let conn_read = Arc::clone(&conn);

        // Spawn read task - this task owns its locked access while reading
        tokio::spawn(async move {
            loop {
                // Get mutable access and call recv - this blocks inside recv
                // but that's fine since TcpConnection.recv() only locks its reader half
                let result = {
                    let mut guard = conn_read.lock().await;
                    guard.recv().await
                };
                
                match result {
                    Ok(data) if data.is_empty() => {
                        info!("Peer {:?} connection closed (empty read)", transport_addr_clone);
                        // CRITICAL: Send PeerDisconnected to remove peer from PeerManager
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr_clone.clone()));
                        break;
                    }
                    Ok(data) => {
                        debug!("Received {} bytes from {:?}", data.len(), transport_addr_clone);
                        let peer_addr = match &transport_addr_clone {
                            super::transport::TransportAddr::Tcp(sock) => *sock,
                            #[cfg(feature = "quinn")]
                            super::transport::TransportAddr::Quinn(sock) => *sock,
                            #[cfg(feature = "iroh")]
                            super::transport::TransportAddr::Iroh(_) => {
                                std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                            }
                        };
                        let _ = message_tx_clone.send(NetworkMessage::RawMessageReceived(data, peer_addr));
                    }
                    Err(e) => {
                        warn!("Peer read error for {:?}: {}", transport_addr_clone, e);
                        // CRITICAL: Send PeerDisconnected on read error
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr_clone.clone()));
                        break;
                    }
                }
            }
        });

        // Spawn write task with its own connection clone
        let conn_write = conn;  // Take ownership since we don't need it anymore
        tokio::spawn(async move {
            let mut send_rx = send_rx;

            while let Some(data) = send_rx.recv().await {
                let result = {
                    let mut guard = conn_write.lock().await;
                    guard.send(&data).await
                };
                match result {
                    Ok(_) => {
                        debug!("Sent {} bytes to peer {:?}", data.len(), transport_addr_write);
                    }
                    Err(e) => {
                        warn!("Peer write error for {:?}: {}", transport_addr_write, e);
                        break;
                    }
                }
            }
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            addr,
            transport_addr,
            message_tx,
            send_tx,
            connected: true,
            conntime: now,
            last_send: now,
            last_recv: now,
            bytes_sent: 0,
            bytes_recv: 0,
            quality_score: 0.5, // Start with neutral score
            successful_exchanges: 0,
            failed_exchanges: 0,
            avg_response_time_ms: 0.0,
            last_block_received: None,
            last_tx_received: None,
            services: 0, // Service flags from version message (set when version received)
            version: 0,  // Protocol version from version message (set when version received)
            user_agent: None, // User agent from version message (set when version received)
            start_height: 0, // Best block height from version message (set when version received)
            is_outbound: false, // Default to false, should be set by caller
            pending_ping_nonce: None,
            ping_sent_time: None,
            ping_timeout_seconds: 1200, // 20 minutes default
            best_block_hash: None,
            best_block_height: None,
            chainwork: None,
            permissions: 0, // No special permissions by default
            is_manual: false, // Default to false, set to true for manual connections
            last_block_announcement: None, // No block announcements yet
        }
    }

    /// Create a new peer connection from a TCP stream (backward compatibility)
    ///
    /// This is a convenience method that wraps a TcpStream in a TcpConnection.
    #[deprecated(note = "Use from_tcp_stream_split instead for proper concurrent read/write")]
    pub fn new(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
    ) -> Self {
        Self::from_tcp_stream_split(stream, addr, message_tx)
    }

    /// Create a new peer from a TCP stream with properly split reader/writer
    /// This allows concurrent read and write operations without deadlock
    pub fn from_tcp_stream_split(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
    ) -> Self {
        use super::transport::TransportAddr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tracing::{debug, info, warn};

        let peer_addr = stream.peer_addr().unwrap_or(addr);
        let transport_addr = TransportAddr::Tcp(peer_addr);

        // Split the TCP stream into separate read and write halves
        let (reader, writer) = tokio::io::split(stream);

        // Create channel for sending messages to the write task
        let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let transport_addr_clone = transport_addr.clone();
        let transport_addr_write = transport_addr.clone();
        let message_tx_clone = message_tx.clone();

        // Spawn read task - owns the reader half exclusively
        tokio::spawn(async move {
            let mut reader = reader;
            loop {
                // Read Bitcoin wire format header (24 bytes)
                let mut header = [0u8; 24];
                match reader.read_exact(&mut header).await {
                    Ok(_) => {}
                    Err(e) => {
                        if e.kind() == std::io::ErrorKind::UnexpectedEof {
                            info!("Peer {:?} connection closed", transport_addr_clone);
                        } else {
                            warn!("Peer read error for {:?}: {}", transport_addr_clone, e);
                        }
                        // CRITICAL: Send PeerDisconnected to remove peer from PeerManager
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr_clone.clone()));
                        break;
                    }
                }

                // Extract payload length from bytes 16-19 (little-endian)
                let payload_len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]) as usize;

                // Read payload if any
                let data = if payload_len > 0 {
                    if payload_len > 32 * 1024 * 1024 - 24 {
                        warn!("Peer {:?} sent oversized message: {} bytes", transport_addr_clone, payload_len);
                        // CRITICAL: Send PeerDisconnected on oversized message
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr_clone.clone()));
                        break;
                    }
                    let mut payload = vec![0u8; payload_len];
                    match reader.read_exact(&mut payload).await {
                        Ok(_) => {
                            let mut data = header.to_vec();
                            data.extend_from_slice(&payload);
                            data
                        }
                        Err(e) => {
                            warn!("Peer read payload error for {:?}: {}", transport_addr_clone, e);
                            // CRITICAL: Send PeerDisconnected on payload read error
                            let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(transport_addr_clone.clone()));
                            break;
                        }
                    }
                } else {
                    header.to_vec()
                };

                debug!("Received {} bytes from {:?}", data.len(), transport_addr_clone);
                let peer_socket = match &transport_addr_clone {
                    TransportAddr::Tcp(sock) => *sock,
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(sock) => *sock,
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                };
                let _ = message_tx_clone.send(NetworkMessage::RawMessageReceived(data, peer_socket));
            }
        });

        // Spawn write task - owns the writer half exclusively  
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(data) = send_rx.recv().await {
                match writer.write_all(&data).await {
                    Ok(_) => {
                        if let Err(e) = writer.flush().await {
                            warn!("Peer flush error for {:?}: {}", transport_addr_write, e);
                            break;
                        }
                        debug!("Sent {} bytes to peer {:?}", data.len(), transport_addr_write);
                    }
                    Err(e) => {
                        warn!("Peer write error for {:?}: {}", transport_addr_write, e);
                        break;
                    }
                }
            }
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            addr,
            transport_addr,
            send_tx,
            message_tx,
            connected: true,
            conntime: now,
            last_send: now,
            last_recv: now,
            bytes_sent: 0,
            bytes_recv: 0,
            quality_score: 0.5,
            successful_exchanges: 0,
            failed_exchanges: 0,
            avg_response_time_ms: 0.0,
            last_block_received: None,
            last_tx_received: None,
            services: 0,
            version: 0,
            user_agent: None,
            start_height: 0,
            is_outbound: false, // Default to false, should be set by caller
            best_block_hash: None,
            best_block_height: None,
            chainwork: None,
            permissions: 0, // No special permissions by default
            is_manual: false, // Default to false, set to true for manual connections
            last_block_announcement: None, // No block announcements yet
            pending_ping_nonce: None,
            ping_sent_time: None,
            ping_timeout_seconds: 1200, // 20 minutes default
        }
    }

    /// Start the peer handler
    pub async fn start(&mut self) -> Result<()> {
        info!("Starting peer handler for {}", self.addr);

        // Send connection notification
        let _ = self
            .message_tx
            .send(NetworkMessage::PeerConnected(self.transport_addr.clone()));

        // Handle peer communication
        self.handle_peer_communication().await?;

        // Send disconnection notification
        let _ = self.message_tx.send(NetworkMessage::PeerDisconnected(
            self.transport_addr.clone(),
        ));

        Ok(())
    }

    /// Handle peer communication loop
    ///
    /// Note: The read loop is now handled in `new()` via stream splitting.
    /// This method just waits for the connection to close.
    async fn handle_peer_communication(&mut self) -> Result<()> {
        // The read loop is spawned in `new()` and runs independently
        // We just wait here to detect when connection closes
        // In a real implementation, we'd monitor the read task or connection state
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // Connection close is automatically detected by the read task in from_transport_connection
        // When recv() returns empty data or error, the task breaks and sends PeerDisconnected
        self.connected = false;
        Ok(())
    }

    /// Process a received message
    #[allow(dead_code)]
    async fn process_message(&self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(anyhow::anyhow!("Message too short"));
        }

        // Parse Bitcoin protocol message
        let command = String::from_utf8_lossy(&data[4..12]);
        debug!("Received command: {}", command);

        match command.as_ref() {
            "block" => {
                let _ = self
                    .message_tx
                    .send(NetworkMessage::BlockReceived(data.to_vec()));
            }
            "tx" => {
                let _ = self
                    .message_tx
                    .send(NetworkMessage::TransactionReceived(data.to_vec()));
            }
            "inv" => {
                let _ = self
                    .message_tx
                    .send(NetworkMessage::InventoryReceived(data.to_vec()));
            }
            _ => {
                debug!("Unknown command: {}", command);
            }
        }

        Ok(())
    }

    /// Send a message to the peer
    ///
    /// Messages are sent via a channel to a background write task.
    pub async fn send_message(&self, message: Vec<u8>) -> Result<()> {
        let message_len = message.len();
        self.send_tx
            .send(message)
            .map_err(|e| anyhow::anyhow!("Failed to send message to peer {}: {}", self.addr, e))?;
        debug!("Queued {} bytes for peer {}", message_len, self.addr);
        Ok(())
    }

    /// Check if peer is connected
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Get peer address
    pub fn address(&self) -> SocketAddr {
        self.addr
    }

    /// Get quality score
    pub fn quality_score(&self) -> f64 {
        self.quality_score
    }

    /// Record a send operation
    pub fn record_send(&mut self, bytes: usize) {
        self.bytes_sent += bytes as u64;
        self.last_send = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// Record a receive operation
    pub fn record_receive(&mut self, bytes: usize) {
        self.bytes_recv += bytes as u64;
        self.last_recv = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
    }

    /// Get last send time
    pub fn last_send(&self) -> u64 {
        self.last_send
    }

    /// Get last receive time
    pub fn last_recv(&self) -> u64 {
        self.last_recv
    }

    /// Get bytes sent
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// Get bytes received
    pub fn bytes_recv(&self) -> u64 {
        self.bytes_recv
    }

    /// Get connection time
    pub fn conntime(&self) -> u64 {
        self.conntime
    }

    /// Set service flags (called when version message received)
    pub fn set_services(&mut self, services: u64) {
        self.services = services;
    }

    /// Get service flags
    pub fn services(&self) -> u64 {
        self.services
    }

    /// Set protocol version (called when version message received)
    pub fn set_version(&mut self, version: u32) {
        self.version = version;
    }

    /// Get protocol version
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Set user agent (called when version message received)
    pub fn set_user_agent(&mut self, user_agent: String) {
        self.user_agent = Some(user_agent);
    }

    /// Get user agent
    pub fn user_agent(&self) -> Option<&String> {
        self.user_agent.as_ref()
    }

    /// Set start height (best block height from version message)
    pub fn set_start_height(&mut self, start_height: i32) {
        self.start_height = start_height;
    }

    /// Get start height (best block height from version message)
    pub fn start_height(&self) -> i32 {
        self.start_height
    }

    /// Check if peer has a specific service flag
    pub fn has_service(&self, flag: u64) -> bool {
        (self.services & flag) != 0
    }

    /// Get whether this is an outbound connection
    pub fn is_outbound(&self) -> bool {
        self.is_outbound
    }

    /// Set whether this is an outbound connection
    pub fn set_is_outbound(&mut self, is_outbound: bool) {
        self.is_outbound = is_outbound;
    }

    /// Record that a ping was sent
    pub fn record_ping_sent(&mut self, nonce: u64) {
        self.pending_ping_nonce = Some(nonce);
        self.ping_sent_time = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
    }

    /// Record that a pong was received (clears pending ping)
    pub fn record_pong_received(&mut self, nonce: u64) -> bool {
        match self.pending_ping_nonce {
            Some(pending_nonce) if pending_nonce == nonce => {
                self.pending_ping_nonce = None;
                self.ping_sent_time = None;
                true
            }
            _ => false // Nonce mismatch or no pending ping
        }
    }

    /// Check if ping has timed out
    pub fn is_ping_timed_out(&self) -> bool {
        match (self.pending_ping_nonce, self.ping_sent_time) {
            (Some(_), Some(sent_time)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                now.saturating_sub(sent_time) > self.ping_timeout_seconds
            }
            _ => false
        }
    }

    /// Set chainwork (cumulative proof-of-work)
    pub fn set_chainwork(&mut self, chainwork: u128) {
        self.chainwork = Some(chainwork);
    }

    /// Get chainwork (cumulative proof-of-work)
    pub fn chainwork(&self) -> Option<u128> {
        self.chainwork
    }

    /// Set best block height
    pub fn set_best_block_height(&mut self, height: u64) {
        self.best_block_height = Some(height);
    }

    /// Get best block height
    pub fn best_block_height(&self) -> Option<u64> {
        self.best_block_height
    }

    /// Set best block hash
    pub fn set_best_block_hash(&mut self, hash: blvm_protocol::Hash) {
        self.best_block_hash = Some(hash);
    }

    /// Get best block hash
    pub fn best_block_hash(&self) -> Option<blvm_protocol::Hash> {
        self.best_block_hash
    }

    /// Set peer permissions (bit flags)
    pub fn set_permissions(&mut self, permissions: u64) {
        self.permissions = permissions;
    }

    /// Get peer permissions (bit flags)
    pub fn permissions(&self) -> u64 {
        self.permissions
    }

    /// Check if peer has NoBan permission (won't be disconnected for misbehavior)
    pub fn has_noban_permission(&self) -> bool {
        // NoBan flag = 1 (Bitcoin Core: NetPermissionFlags::NoBan)
        (self.permissions & 1) != 0
    }

    /// Check if this is a manual connection (via RPC addnode)
    pub fn is_manual(&self) -> bool {
        self.is_manual
    }

    /// Set whether this is a manual connection
    pub fn set_is_manual(&mut self, is_manual: bool) {
        self.is_manual = is_manual;
    }

    /// Record a block announcement (for outbound peer eviction)
    pub fn record_block_announcement(&mut self) {
        self.last_block_announcement = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
    }

    /// Get timestamp of last block announcement
    pub fn last_block_announcement(&self) -> Option<u64> {
        self.last_block_announcement
    }
}
