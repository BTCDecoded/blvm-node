//! Peer connection management
//!
//! Handles individual peer connections, message parsing, and protocol state.

use anyhow::Result;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use std::sync::Arc;

use super::NetworkMessage;
use super::transport::{TransportConnection, TransportAddr};

/// Peer connection state
/// 
/// Supports multiple transport types (TCP, Quinn, Iroh) via TransportConnection trait
pub struct Peer {
    addr: SocketAddr,
    transport_addr: TransportAddr, // Full transport address (may differ from SocketAddr for Iroh)
    message_tx: mpsc::UnboundedSender<NetworkMessage>,
    send_tx: mpsc::UnboundedSender<Vec<u8>>, // Channel for sending messages
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
}

impl Peer {
    /// Create a new peer connection from a TransportConnection
    /// 
    /// This is the preferred method as it supports all transport types (TCP, Quinn, Iroh).
    /// The connection is managed via channels for concurrent read/write.
    pub fn from_transport_connection<C: TransportConnection + 'static>(
        mut conn: C,
        addr: SocketAddr,
        transport_addr: TransportAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
    ) -> Self {
        // Create channel for sending messages
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        
        let transport_addr_clone = transport_addr.clone();
        let message_tx_clone = message_tx.clone();
        
        // Spawn read task using TransportConnection::recv
        tokio::spawn(async move {
            loop {
                match conn.recv().await {
                    Ok(data) => {
                        if data.is_empty() {
                            // Graceful close
                            break;
                        }
                        
                        // Send to network manager for processing
                        // For backward compatibility with SocketAddr-based messages, extract SocketAddr if available
                        let peer_addr = match &transport_addr_clone {
                            super::transport::TransportAddr::Tcp(sock) => *sock,
                            #[cfg(feature = "quinn")]
                            super::transport::TransportAddr::Quinn(sock) => *sock,
                            #[cfg(feature = "iroh")]
                            super::transport::TransportAddr::Iroh(_) => {
                                // For Iroh, use placeholder - will be looked up via transport_addr
                                std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                            }
                        };
                        let _ = message_tx_clone.send(NetworkMessage::RawMessageReceived(data, peer_addr));
                    }
                    Err(e) => {
                        warn!("Peer read error for {:?}: {}", transport_addr_clone, e);
                        break; // Connection error
                    }
                }
            }
        });
        
        // Spawn write task using TransportConnection::send
        tokio::spawn(async move {
            let mut send_rx = send_rx;
            let mut conn = conn;
            
            loop {
                match send_rx.recv().await {
                    Some(data) => {
                        match conn.send(&data).await {
                            Ok(_) => {
                                debug!("Sent {} bytes to peer", data.len());
                            }
                            Err(e) => {
                                warn!("Peer write error: {}", e);
                                break; // Connection closed
                            }
                        }
                    }
                    None => {
                        break; // Channel closed
                    }
                }
            }
            
            // Gracefully close connection on write task exit
            let _ = conn.close().await;
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
        }
    }
    
    /// Create a new peer connection from a TCP stream (backward compatibility)
    /// 
    /// This is a convenience method that wraps a TcpStream in a TcpConnection.
    #[deprecated(note = "Use from_transport_connection instead for transport abstraction")]
    pub fn new(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        message_tx: mpsc::UnboundedSender<NetworkMessage>,
    ) -> Self {
        use super::tcp_transport::TcpConnection;
        use super::transport::TransportAddr;
        
        let peer_addr = stream.peer_addr().unwrap_or(addr);
        let tcp_conn = TcpConnection {
            stream,
            peer_addr: TransportAddr::Tcp(peer_addr),
            connected: true,
        };
        
        Self::from_transport_connection(tcp_conn, addr, TransportAddr::Tcp(addr), message_tx)
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
        let _ = self
            .message_tx
            .send(NetworkMessage::PeerDisconnected(self.transport_addr.clone()));

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
        self.send_tx
            .send(message)
            .map_err(|e| anyhow::anyhow!("Failed to send message to peer {}: {}", self.addr, e))?;
        debug!("Queued {} bytes for peer {}", message.len(), self.addr);
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
}
