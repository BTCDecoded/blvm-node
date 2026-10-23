//! Peer connection management
//!
//! Handles individual peer connections, message parsing, and protocol state.

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::NetworkMessage;
use super::transport::{TransportAddr, TransportConnection};
use crate::utils::current_timestamp;

/// Peer connection state
///
/// Supports multiple transport types (TCP, Quinn, Iroh) via TransportConnection trait
pub struct Peer {
    addr: SocketAddr,
    transport_addr: TransportAddr, // Full transport address (may differ from SocketAddr for Iroh)
    message_tx: mpsc::UnboundedSender<NetworkMessage>,
    pub(crate) send_tx: mpsc::UnboundedSender<Vec<u8>>, // Channel for sending messages
    /// Read loop for this connection. Aborting it drops the read half.
    read_task: Option<tokio::task::AbortHandle>,
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
    /// Service flags from version message (indicates peer capabilities)
    /// Used to filter peers by capabilities (e.g., NODE_GOVERNANCE)
    services: u64,
    /// Protocol version from version message
    version: u32,
    /// User agent (subversion) from version message
    /// Example: "/Satoshi:25.0.0/" for standard nodes
    user_agent: Option<String>,
    /// Best block height from version message (peer's chain tip)
    /// This indicates how many blocks the peer has
    start_height: i32,
    /// Peer clock from the version message, as a Unix timestamp.
    version_time: Option<i64>,
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
    /// Monotonic time of the outstanding ping, so the reported RTT is sub-second.
    ping_sent_at: Option<std::time::Instant>,
    /// Last measured ping, in seconds.
    ping_secs: f64,
    /// Stable id for this peer. It is not the source port.
    peer_id: u64,
    /// Ping timeout in seconds (default: 20 minutes)
    ping_timeout_seconds: u64,
}

fn next_peer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn reserve_raw_frame(len: usize) -> bool {
    use crate::network::parse_offload::{PEER_RX_BYTE_CAP, try_reserve_peer_rx};
    if len > PEER_RX_BYTE_CAP {
        return false;
    }
    for _ in 0..40 {
        if try_reserve_peer_rx(len) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

fn enqueue_raw(
    message_tx: &mpsc::UnboundedSender<NetworkMessage>,
    data: Vec<u8>,
    peer_socket: std::net::SocketAddr,
) {
    let len = data.len();
    if message_tx
        .send(NetworkMessage::RawMessageReceived(data, peer_socket))
        .is_ok()
    {
        crate::network::parse_offload::note_peer_rx_enqueue();
    } else {
        crate::network::parse_offload::release_peer_rx(len);
    }
}

impl Peer {
    /// Create a minimal peer instance for testing purposes.
    ///
    /// This constructor creates a peer with mock channels that discard all messages.
    /// The peer is marked as connected but does not perform actual network I/O.
    /// Use this only in tests to verify peer state changes (e.g., ping nonce).
    #[doc(hidden)]
    pub fn new_for_testing(addr: SocketAddr) -> Self {
        Self::pair_for_testing(addr).0
    }

    /// Same peer as [`Self::new_for_testing`], plus the channel `send_to_peer` writes.
    pub(crate) fn pair_for_testing(addr: SocketAddr) -> (Self, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (message_tx, _) = mpsc::unbounded_channel();
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let transport_addr = super::transport::TransportAddr::Tcp(addr);
        let now = current_timestamp();

        let peer = Self {
            addr,
            transport_addr,
            message_tx,
            send_tx,
            read_task: None,
            connected: true,
            conntime: now,
            last_send: now,
            last_recv: now,
            bytes_sent: 0,
            bytes_recv: 0,
            quality_score: 0.5,
            services: 0,
            version: 70015,
            user_agent: Some("/test-peer/".to_string()),
            start_height: 0,
            version_time: None,
            is_outbound: false,
            best_block_hash: None,
            best_block_height: None,
            chainwork: None,
            permissions: 0,
            is_manual: false,
            last_block_announcement: None,
            pending_ping_nonce: None,
            ping_sent_time: None,
            ping_sent_at: None,
            ping_secs: 0.0,
            peer_id: next_peer_id(),
            ping_timeout_seconds: 1200,
        };
        (peer, send_rx)
    }

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
        let message_tx_write = message_tx.clone();

        // Use interior mutability pattern: TcpConnection has separate Arc<Mutex> for
        // reader and writer internally, so we can share it via Arc without outer Mutex.
        // This allows recv() and send() to run truly concurrently.
        use std::sync::Arc;
        use tokio::sync::Mutex;
        let conn = Arc::new(Mutex::new(conn));

        // Clone for reader task
        let conn_read = Arc::clone(&conn);

        // Spawn read task - this task owns its locked access while reading
        let read_task = tokio::spawn(async move {
            loop {
                // Get mutable access and call recv - this blocks inside recv
                // but that's fine since TcpConnection.recv() only locks its reader half
                let result = {
                    let mut guard = conn_read.lock().await;
                    guard.recv().await
                };

                match result {
                    Ok(data) if data.is_empty() => {
                        info!(
                            "Peer {:?} connection closed (empty read)",
                            transport_addr_clone
                        );
                        // CRITICAL: Send PeerDisconnected to remove peer from PeerManager
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                            transport_addr_clone.clone(),
                        ));
                        break;
                    }
                    Ok(data) => {
                        let peer_addr = match &transport_addr_clone {
                            super::transport::TransportAddr::Tcp(sock) => *sock,
                            #[cfg(feature = "quinn")]
                            super::transport::TransportAddr::Quinn(sock) => *sock,
                            #[cfg(feature = "iroh")]
                            super::transport::TransportAddr::Iroh(_) => {
                                std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                            }
                        };
                        if data.len() < 24
                            || !crate::network::protocol::wire_header_magic_ok(&data)
                            || !crate::network::protocol::wire_checksum_ok(&data, &data[24..])
                        {
                            warn!(
                                "Peer {:?} sent a frame that failed magic or checksum, disconnecting",
                                transport_addr_clone
                            );
                            let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                                transport_addr_clone.clone(),
                            ));
                            break;
                        }
                        if !reserve_raw_frame(data.len()).await {
                            warn!(
                                "Peer {:?} filled the receive queue ({} bytes), disconnecting",
                                transport_addr_clone,
                                data.len()
                            );
                            let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                                transport_addr_clone.clone(),
                            ));
                            break;
                        }
                        enqueue_raw(&message_tx_clone, data, peer_addr);
                    }
                    Err(e) => {
                        warn!("Peer read error for {:?}: {}", transport_addr_clone, e);
                        // CRITICAL: Send PeerDisconnected on read error
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                            transport_addr_clone.clone(),
                        ));
                        break;
                    }
                }
            }
        });

        // Spawn write task with its own connection clone
        let conn_write = conn; // Take ownership since we don't need it anymore
        tokio::spawn(async move {
            let mut send_rx = send_rx;

            while let Some(data) = send_rx.recv().await {
                let result = {
                    let mut guard = conn_write.lock().await;
                    guard.send(&data).await
                };
                match result {
                    Ok(_) => {
                        debug!(
                            "Sent {} bytes to peer {:?}",
                            data.len(),
                            transport_addr_write
                        );
                    }
                    Err(e) => {
                        warn!("Peer write error for {:?}: {}", transport_addr_write, e);
                        let _ = message_tx_write.send(NetworkMessage::PeerDisconnected(
                            transport_addr_write.clone(),
                        ));
                        break;
                    }
                }
            }
        });

        let now = current_timestamp();

        Self {
            addr,
            transport_addr,
            message_tx,
            send_tx,
            read_task: Some(read_task.abort_handle()),
            connected: true,
            conntime: now,
            last_send: now,
            last_recv: now,
            bytes_sent: 0,
            bytes_recv: 0,
            quality_score: 0.5, // Start with neutral score
            services: 0,        // Service flags from version message (set when version received)
            version: 0,         // Protocol version from version message (set when version received)
            user_agent: None,   // User agent from version message (set when version received)
            start_height: 0, // Best block height from version message (set when version received)
            version_time: None,
            is_outbound: false, // Default to false, should be set by caller
            pending_ping_nonce: None,
            ping_sent_time: None,
            ping_sent_at: None,
            ping_secs: 0.0,
            peer_id: next_peer_id(),
            ping_timeout_seconds: 1200, // 20 minutes default
            best_block_hash: None,
            best_block_height: None,
            chainwork: None,
            permissions: 0,                // No special permissions by default
            is_manual: false,              // Default to false, set to true for manual connections
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
        Self::from_tcp_stream_split(
            stream,
            addr,
            message_tx,
            super::protocol::MAX_PROTOCOL_MESSAGE_LENGTH,
        )
    }

    /// Create a new peer from a TCP stream with properly split reader/writer
    /// This allows concurrent read and write operations without deadlock
    pub fn from_tcp_stream_split(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
        max_message_length: usize,
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
        let message_tx_write = message_tx.clone();

        // Spawn read task - owns the reader half exclusively
        let read_task = tokio::spawn(async move {
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
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                            transport_addr_clone.clone(),
                        ));
                        break;
                    }
                }

                if !crate::network::protocol::wire_header_magic_ok(&header) {
                    warn!(
                        "Peer {:?} sent a bad magic, disconnecting",
                        transport_addr_clone
                    );
                    let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                        transport_addr_clone.clone(),
                    ));
                    break;
                }

                // Extract payload length from bytes 16-19 (little-endian)
                let payload_len =
                    u32::from_le_bytes([header[16], header[17], header[18], header[19]]) as usize;

                // Read payload if any. Magic is already checked, so a bad magic
                // does not allocate the claimed length.
                let max_payload = max_message_length.saturating_sub(24);
                let payload = if payload_len > 0 {
                    if payload_len > max_payload {
                        warn!(
                            "Peer {:?} sent oversized message: {} bytes",
                            transport_addr_clone, payload_len
                        );
                        let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                            transport_addr_clone.clone(),
                        ));
                        break;
                    }
                    let mut payload = vec![0u8; payload_len];
                    match reader.read_exact(&mut payload).await {
                        Ok(_) => payload,
                        Err(e) => {
                            warn!(
                                "Peer read payload error for {:?}: {}",
                                transport_addr_clone, e
                            );
                            let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                                transport_addr_clone.clone(),
                            ));
                            break;
                        }
                    }
                } else {
                    Vec::new()
                };
                if !crate::network::protocol::wire_checksum_ok(&header, &payload) {
                    warn!(
                        "Peer {:?} sent a bad checksum, disconnecting",
                        transport_addr_clone
                    );
                    let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                        transport_addr_clone.clone(),
                    ));
                    break;
                }
                let mut data = header.to_vec();
                data.extend_from_slice(&payload);

                let peer_socket = match &transport_addr_clone {
                    TransportAddr::Tcp(sock) => *sock,
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(sock) => *sock,
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => std::net::SocketAddr::from(([0, 0, 0, 0], 0)),
                };
                if !reserve_raw_frame(data.len()).await {
                    warn!(
                        "Peer {:?} filled the receive queue ({} bytes), disconnecting",
                        transport_addr_clone,
                        data.len()
                    );
                    let _ = message_tx_clone.send(NetworkMessage::PeerDisconnected(
                        transport_addr_clone.clone(),
                    ));
                    break;
                }
                enqueue_raw(&message_tx_clone, data, peer_socket);
            }
        });

        // Spawn write task - owns the writer half exclusively
        tokio::spawn(run_peer_writer(
            writer,
            send_rx,
            message_tx_write,
            transport_addr_write,
        ));

        let now = current_timestamp();

        Self {
            addr,
            transport_addr,
            send_tx,
            read_task: Some(read_task.abort_handle()),
            message_tx,
            connected: true,
            conntime: now,
            last_send: now,
            last_recv: now,
            bytes_sent: 0,
            bytes_recv: 0,
            quality_score: 0.5,
            services: 0,
            version: 0,
            user_agent: None,
            start_height: 0,
            version_time: None,
            is_outbound: false, // Default to false, should be set by caller
            best_block_hash: None,
            best_block_height: None,
            chainwork: None,
            permissions: 0,                // No special permissions by default
            is_manual: false,              // Default to false, set to true for manual connections
            last_block_announcement: None, // No block announcements yet
            pending_ping_nonce: None,
            ping_sent_time: None,
            ping_sent_at: None,
            ping_secs: 0.0,
            peer_id: next_peer_id(),
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
        self.last_send = current_timestamp();
    }

    /// Record a receive operation
    pub fn record_receive(&mut self, bytes: usize) {
        self.bytes_recv += bytes as u64;
        self.last_recv = current_timestamp();
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

    /// Chain-sync lock tests age a peer past the timeout without waiting.
    #[cfg(test)]
    pub(crate) fn set_conntime(&mut self, conntime: u64) {
        self.conntime = conntime;
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

    pub fn set_version_time(&mut self, timestamp: i64) {
        self.version_time = Some(timestamp);
    }

    /// Version timestamp minus `now`. Zero when the peer has not sent a version time.
    pub fn time_offset(&self, now: u64) -> i64 {
        self.version_time
            .map(|timestamp| timestamp.saturating_sub(now as i64))
            .unwrap_or(0)
    }

    /// Header height when one is stored, otherwise the version start height.
    pub fn synced_block_height(&self) -> i64 {
        self.best_block_height
            .map(|height| height as i64)
            .unwrap_or(self.start_height as i64)
    }

    /// Check if peer has a specific service flag
    pub fn has_service(&self, flag: u64) -> bool {
        (self.services & flag) != 0
    }

    /// Get whether this is an outbound connection
    pub fn is_outbound(&self) -> bool {
        self.is_outbound
    }

    pub fn id(&self) -> u64 {
        self.peer_id
    }

    pub fn ping_secs(&self) -> f64 {
        self.ping_secs
    }

    /// Set whether this is an outbound connection
    pub fn set_is_outbound(&mut self, is_outbound: bool) {
        self.is_outbound = is_outbound;
    }

    /// Record that a ping was sent.
    ///
    /// The nonce tracks the latest ping so a pong can match it. The timeout
    /// stamp stays at the first unanswered ping so a later send cannot move it
    /// forward and hide a peer that never answers.
    pub fn record_ping_sent(&mut self, nonce: u64) {
        self.pending_ping_nonce = Some(nonce);
        self.ping_sent_at = Some(std::time::Instant::now());
        if self.ping_sent_time.is_none() {
            self.ping_sent_time = Some(current_timestamp());
        }
    }

    /// Unanswered-ping age comes from background-task config (default 1200s).
    pub(crate) fn set_ping_timeout_seconds(&mut self, seconds: u64) {
        self.ping_timeout_seconds = seconds;
    }

    /// Ping-timeout lock tests age the outstanding stamp without waiting.
    #[cfg(test)]
    pub(crate) fn set_ping_sent_time(&mut self, sent: u64) {
        self.ping_sent_time = Some(sent);
    }

    /// Record that a pong was received (clears pending ping)
    pub fn record_pong_received(&mut self, nonce: u64) -> bool {
        match self.pending_ping_nonce {
            Some(pending_nonce) if pending_nonce == nonce => {
                if let Some(sent) = self.ping_sent_at {
                    self.ping_secs = sent.elapsed().as_secs_f64();
                }
                self.pending_ping_nonce = None;
                self.ping_sent_time = None;
                self.ping_sent_at = None;
                true
            }
            _ => false, // Nonce mismatch or no pending ping
        }
    }

    /// Get the pending ping nonce (if any)
    pub fn pending_ping_nonce(&self) -> Option<u64> {
        self.pending_ping_nonce
    }

    /// Check if ping has timed out
    pub fn is_ping_timed_out(&self) -> bool {
        match (self.pending_ping_nonce, self.ping_sent_time) {
            (Some(_), Some(sent_time)) => {
                let now = current_timestamp();
                now.saturating_sub(sent_time) > self.ping_timeout_seconds
            }
            _ => false,
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

    /// Drop the read and write halves so the TCP connection closes.
    pub fn shutdown_connection(&mut self) {
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        drop(closed_rx);
        drop(std::mem::replace(&mut self.send_tx, closed_tx));
        self.connected = false;
    }

    /// Check if peer has NoBan permission (won't be disconnected for misbehavior)
    pub fn has_noban_permission(&self) -> bool {
        // NoBan flag = 1 (NetPermissionFlags::NoBan)
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
        self.last_block_announcement = Some(current_timestamp());
    }

    /// Get timestamp of last block announcement
    pub fn last_block_announcement(&self) -> Option<u64> {
        self.last_block_announcement
    }
}

async fn run_peer_writer<W>(
    mut writer: W,
    mut send_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    message_tx: mpsc::UnboundedSender<NetworkMessage>,
    addr: TransportAddr,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    while let Some(data) = send_rx.recv().await {
        match writer.write_all(&data).await {
            Ok(()) => {
                if let Err(e) = writer.flush().await {
                    warn!("Peer flush error for {:?}: {}", addr, e);
                    let _ = message_tx.send(NetworkMessage::PeerDisconnected(addr));
                    break;
                }
                debug!("Sent {} bytes to peer {:?}", data.len(), addr);
            }
            Err(e) => {
                warn!("Peer write error for {:?}: {}", addr, e);
                let _ = message_tx.send(NetworkMessage::PeerDisconnected(addr));
                break;
            }
        }
    }
}

#[cfg(test)]
mod write_error_tests {
    use super::run_peer_writer;
    use crate::network::NetworkMessage;
    use crate::network::transport::TransportAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;
    use tokio::sync::mpsc;

    struct BrokenWrite;

    impl AsyncWrite for BrokenWrite {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "write",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn write_error_removes_the_peer() {
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (message_tx, mut message_rx) = mpsc::unbounded_channel();
        let addr = TransportAddr::Tcp("127.0.0.1:8333".parse().unwrap());
        let writer = tokio::spawn(run_peer_writer(
            BrokenWrite,
            send_rx,
            message_tx,
            addr.clone(),
        ));
        send_tx.send(vec![1, 2, 3]).unwrap();
        let message = message_rx.recv().await;
        assert!(matches!(message, Some(NetworkMessage::PeerDisconnected(got)) if got == addr));
        writer.await.unwrap();
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::Peer;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn shutdown_closes_the_tcp_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (message_tx, _message_rx) = mpsc::unbounded_channel();
        let mut peer = Peer::from_tcp_stream_split(server, addr, message_tx, 4_000_000);
        peer.shutdown_connection();
        let mut client = client;
        let mut buf = [0u8; 8];
        let read = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read, 0);
    }
}
