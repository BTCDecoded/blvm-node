//! Stratum V2 Client Implementation
//!
//! Transport-agnostic Stratum V2 client that works with TCP, Quinn, and Iroh transports
//! via direct Transport trait usage (separate from Bitcoin P2P connections).

#[cfg(feature = "iroh")]
use crate::network::iroh_transport::IrohTransport;
#[cfg(feature = "quinn")]
use crate::network::quinn_transport::QuinnTransport;
use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use crate::network::stratum_v2::messages::*;
use crate::network::stratum_v2::miner::StratumV2Miner;
use crate::network::stratum_v2::protocol::{TlvDecoder, TlvEncoder};
use crate::network::tcp_transport::TcpTransport;
use crate::network::transport::{Transport, TransportAddr, TransportConnection, TransportType};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Stratum V2 client for connecting to mining pools
pub struct StratumV2Client {
    pool_url: String,
    transport_type: TransportType,
    transport_addr: Option<TransportAddr>,
    connection: Arc<RwLock<Option<Box<dyn TransportConnection + Send>>>>,
    miner: Option<Arc<RwLock<StratumV2Miner>>>,
    connected: Arc<RwLock<bool>>,
    request_id_counter: Arc<RwLock<u32>>,
    // Request/response tracking
    pending_requests:
        Arc<RwLock<std::collections::HashMap<u32, tokio::sync::oneshot::Sender<Vec<u8>>>>>,
    receive_handle: Option<JoinHandle<()>>,
}

impl StratumV2Client {
    /// Create a new Stratum V2 client
    pub fn new(pool_url: String) -> Self {
        // Determine transport type from URL
        let transport_type = if pool_url.starts_with("quinn://") {
            #[cfg(feature = "quinn")]
            {
                TransportType::Quinn
            }
            #[cfg(not(feature = "quinn"))]
            {
                TransportType::Tcp // Fallback if Quinn feature not enabled
            }
        } else if pool_url.starts_with("iroh://") {
            #[cfg(feature = "iroh")]
            {
                TransportType::Iroh
            }
            #[cfg(not(feature = "iroh"))]
            {
                TransportType::Tcp // Fallback if Iroh feature not enabled
            }
        } else {
            TransportType::Tcp
        };

        Self {
            pool_url,
            transport_type,
            transport_addr: None,
            connection: Arc::new(RwLock::new(None)),
            miner: None,
            connected: Arc::new(RwLock::new(false)),
            request_id_counter: Arc::new(RwLock::new(1)),
            pending_requests: Arc::new(RwLock::new(std::collections::HashMap::new())),
            receive_handle: None,
        }
    }

    /// Connect to the Stratum V2 mining pool
    pub async fn connect(&mut self) -> StratumV2Result<()> {
        info!(
            "Connecting to Stratum V2 pool: {} (transport: {:?})",
            self.pool_url, self.transport_type
        );

        // Parse pool URL to get transport address
        let transport_addr = self.parse_pool_url(&self.pool_url)?;
        self.transport_addr = Some(transport_addr.clone());

        // Establish transport connection
        let connection: Box<dyn TransportConnection + Send> = {
            match self.transport_type {
                TransportType::Tcp => {
                    let tcp_transport = TcpTransport::new();
                    let conn = tcp_transport
                        .connect(transport_addr.clone())
                        .await
                        .map_err(|e| {
                            StratumV2Error::Connection(anyhow::anyhow!(
                                "TCP connection failed: {}",
                                e
                            ))
                        })?;
                    Box::new(conn)
                }
                #[cfg(feature = "quinn")]
                TransportType::Quinn => {
                    let quinn_transport = QuinnTransport::new().map_err(|e| {
                        StratumV2Error::Connection(anyhow::anyhow!(
                            "Quinn transport creation failed: {}",
                            e
                        ))
                    })?;
                    let conn = quinn_transport
                        .connect(transport_addr.clone())
                        .await
                        .map_err(|e| {
                            StratumV2Error::Connection(anyhow::anyhow!(
                                "Quinn connection failed: {}",
                                e
                            ))
                        })?;
                    Box::new(conn)
                }
                #[cfg(feature = "iroh")]
                TransportType::Iroh => {
                    let iroh_transport = IrohTransport::new().await.map_err(|e| {
                        StratumV2Error::Connection(anyhow::anyhow!(
                            "Iroh transport creation failed: {}",
                            e
                        ))
                    })?;
                    let conn = iroh_transport.connect(transport_addr).await.map_err(|e| {
                        StratumV2Error::Connection(anyhow::anyhow!("Iroh connection failed: {}", e))
                    })?;
                    Box::new(conn)
                }
                #[cfg(not(any(feature = "quinn", feature = "iroh")))]
                _ => {
                    return Err(StratumV2Error::Configuration(
                        "Transport feature not enabled".to_string(),
                    ));
                }
            }
        };

        {
            let mut conn_guard = self.connection.write().await;
            *conn_guard = Some(connection);
        }

        // Start receiving messages in background
        self.start_receive_loop().await?;

        // Create miner instance
        let miner = Arc::new(RwLock::new(StratumV2Miner::new()));

        // Perform Setup Connection handshake
        let setup_msg = SetupConnectionMessage {
            protocol_version: 2, // Stratum V2
            endpoint: "bllvm-node/0.1.0".to_string(),
            capabilities: vec!["mining".to_string(), "merge-mining".to_string()],
        };

        // Send setup message and await response
        let response = self.send_request(&setup_msg).await?;

        // Decode response
        let (tag, payload) = TlvDecoder::decode_raw(&response)?;
        match tag {
            message_types::SETUP_CONNECTION_SUCCESS => {
                let success_msg: SetupConnectionSuccessMessage =
                    SetupConnectionSuccessMessage::from_bytes(&payload)?;
                info!(
                    "Setup Connection successful: versions {:?}, capabilities {:?}",
                    success_msg.supported_versions, success_msg.capabilities
                );
                self.miner = Some(miner);
                {
                    let mut connected = self.connected.write().await;
                    *connected = true;
                }
                Ok(())
            }
            message_types::SETUP_CONNECTION_ERROR => {
                let error_msg: SetupConnectionErrorMessage =
                    SetupConnectionErrorMessage::from_bytes(&payload)?;
                Err(StratumV2Error::Connection(anyhow::anyhow!(
                    "Setup Connection failed: {} (code: {})",
                    error_msg.error_message,
                    error_msg.error_code
                )))
            }
            _ => Err(StratumV2Error::InvalidMessageType(tag)),
        }
    }

    /// Start background task to receive messages
    async fn start_receive_loop(&mut self) -> StratumV2Result<()> {
        let connection = Arc::clone(&self.connection);
        let pending_requests = Arc::clone(&self.pending_requests);

        let handle = tokio::spawn(async move {
            loop {
                // Get connection from Arc<RwLock<Option<...>>>
                let mut conn_guard = connection.write().await;
                let mut conn = match conn_guard.take() {
                    Some(c) => c,
                    None => {
                        debug!("Stratum V2 connection closed");
                        break;
                    }
                };
                drop(conn_guard); // Release lock before async operation

                match conn.recv().await {
                    Ok(data) => {
                        if data.is_empty() {
                            debug!("Stratum V2 connection closed");
                            // Put connection back (even though closed)
                            let mut conn_guard = connection.write().await;
                            *conn_guard = Some(conn);
                            break;
                        }

                        // Decode TLV message
                        match TlvDecoder::decode_raw(&data) {
                            Ok((tag, _payload)) => {
                                debug!("Received Stratum V2 message: tag={:04x}", tag);

                                // Try to find matching pending request
                                // For simplicity, use first pending request (would need request_id matching in full impl)
                                let mut pending = pending_requests.write().await;
                                if let Some((&req_id, _sender)) = pending.iter().next() {
                                    // Clone sender before moving it
                                    if let Some(sender) = pending.remove(&req_id) {
                                        let _ = sender.send(data.clone());
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Failed to decode Stratum V2 message: {}", e);
                            }
                        }

                        // Put connection back
                        let mut conn_guard = connection.write().await;
                        *conn_guard = Some(conn);
                    }
                    Err(e) => {
                        error!("Stratum V2 receive error: {}", e);
                        // Put connection back even on error (will be cleaned up on disconnect)
                        let mut conn_guard = connection.write().await;
                        *conn_guard = Some(conn);
                        break;
                    }
                }
            }
        });

        self.receive_handle = Some(handle);
        Ok(())
    }

    /// Disconnect from the pool
    pub async fn disconnect(&mut self) -> StratumV2Result<()> {
        let is_connected = {
            let connected = self.connected.read().await;
            *connected
        };

        if is_connected {
            info!("Disconnecting from Stratum V2 pool");

            // Close connection
            {
                let mut conn_guard = self.connection.write().await;
                if let Some(mut conn) = conn_guard.take() {
                    let _ = conn.close().await;
                }
            }

            // Cancel receive loop
            if let Some(handle) = self.receive_handle.take() {
                handle.abort();
            }

            {
                let mut connected = self.connected.write().await;
                *connected = false;
            }
            self.miner = None;
            self.transport_addr = None;
        }
        Ok(())
    }

    /// Get current mining job
    pub async fn get_current_job(&self) -> StratumV2Result<Option<NewMiningJobMessage>> {
        if let Some(ref miner) = self.miner {
            let miner = miner.read().await;
            miner.get_current_job().await
        } else {
            Err(StratumV2Error::MiningJob(
                "Not connected to pool".to_string(),
            ))
        }
    }

    /// Submit shares to the pool
    pub async fn submit_shares(&self, shares: Vec<ShareData>) -> StratumV2Result<()> {
        let is_connected = {
            let connected = self.connected.read().await;
            *connected
        };

        if !is_connected {
            return Err(StratumV2Error::Connection(anyhow::anyhow!(
                "Not connected to pool"
            )));
        }

        if let Some(ref miner) = self.miner {
            let miner = miner.read().await;
            if let Some(channel_id) = miner.channel_id() {
                let submit_msg = SubmitSharesMessage { channel_id, shares };

                // Send shares and await response
                let response = self.send_request(&submit_msg).await?;

                // Decode response
                let (tag, payload) = TlvDecoder::decode_raw(&response)?;
                match tag {
                    message_types::SUBMIT_SHARES_SUCCESS => {
                        let success_msg: SubmitSharesSuccessMessage =
                            SubmitSharesSuccessMessage::from_bytes(&payload)?;
                        info!(
                            "Shares submitted successfully: channel_id={}, last_job_id={}",
                            success_msg.channel_id, success_msg.last_job_id
                        );
                        Ok(())
                    }
                    message_types::SUBMIT_SHARES_ERROR => {
                        let error_msg: SubmitSharesErrorMessage =
                            SubmitSharesErrorMessage::from_bytes(&payload)?;
                        warn!(
                            "Share submission failed: {} (code: {})",
                            error_msg.error_message, error_msg.error_code
                        );
                        Err(StratumV2Error::ShareValidation(error_msg.error_message))
                    }
                    _ => Err(StratumV2Error::InvalidMessageType(tag)),
                }
            } else {
                Err(StratumV2Error::MiningJob(
                    "No open mining channel".to_string(),
                ))
            }
        } else {
            Err(StratumV2Error::Connection(anyhow::anyhow!(
                "Miner not initialized"
            )))
        }
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        let connected = self.connected.read().await;
        *connected
    }

    /// Send a request message and await response
    async fn send_request<T: StratumV2Message>(&self, message: &T) -> StratumV2Result<Vec<u8>> {
        // Get connection
        let mut conn_guard = self.connection.write().await;
        let conn = conn_guard
            .as_mut()
            .ok_or_else(|| StratumV2Error::Connection(anyhow::anyhow!("Not connected")))?;

        // Serialize message
        let payload = message.to_bytes()?;

        // Encode as TLV
        let mut encoder = TlvEncoder::new();
        let encoded = encoder.encode(message.message_type(), &payload)?;

        // Generate request ID for correlation
        let request_id = {
            let mut counter = self.request_id_counter.write().await;
            let id = *counter;
            *counter = counter.wrapping_add(1);
            id
        };

        // Create response channel
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = self.pending_requests.write().await;
            pending.insert(request_id, tx);
        }

        // Send message
        conn.send(&encoded)
            .await
            .map_err(|e| StratumV2Error::Network(format!("Failed to send message: {}", e)))?;
        drop(conn_guard); // Release lock before async wait

        // Await response with timeout
        tokio::select! {
            result = rx => {
                match result {
                    Ok(response) => Ok(response),
                    Err(_) => Err(StratumV2Error::Network("Response channel closed".to_string()))
                }
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                // Clean up pending request
                {
                    let mut pending = self.pending_requests.write().await;
                    pending.remove(&request_id);
                }
                Err(StratumV2Error::Network("Request timeout".to_string()))
            }
        }
    }

    /// Parse pool URL to get transport address
    fn parse_pool_url(&self, url: &str) -> StratumV2Result<TransportAddr> {
        if url.starts_with("tcp://") {
            let addr_str = url.strip_prefix("tcp://").ok_or_else(|| {
                StratumV2Error::Configuration("Invalid TCP URL format".to_string())
            })?;
            let socket_addr = addr_str.parse::<SocketAddr>().map_err(|e| {
                StratumV2Error::Configuration(format!("Invalid TCP address: {}", e))
            })?;
            Ok(TransportAddr::Tcp(socket_addr))
        } else if url.starts_with("quinn://") {
            #[cfg(feature = "quinn")]
            {
                // Parse Quinn address from URL
                // Format: "quinn://host:port"
                let addr_str = url.strip_prefix("quinn://").ok_or_else(|| {
                    StratumV2Error::Configuration("Invalid Quinn URL format".to_string())
                })?;
                let socket_addr: SocketAddr = addr_str.parse().map_err(|e| {
                    StratumV2Error::Configuration(format!("Invalid Quinn address: {}", e))
                })?;
                Ok(TransportAddr::Quinn(socket_addr))
            }
            #[cfg(not(feature = "quinn"))]
            {
                Err(StratumV2Error::Configuration(
                    "Quinn feature not enabled".to_string(),
                ))
            }
        } else if url.starts_with("iroh://") {
            #[cfg(feature = "iroh")]
            {
                // Parse Iroh node ID from URL
                // Format: "iroh://<hex_encoded_node_id>"
                let node_id_hex = url.strip_prefix("iroh://").ok_or_else(|| {
                    StratumV2Error::Configuration("Invalid Iroh URL format".to_string())
                })?;
                // Use hex crate (already in dependencies via bitcoin_hashes)
                use hex;
                let node_id_bytes = hex::decode(node_id_hex).map_err(|e| {
                    StratumV2Error::Configuration(format!("Invalid Iroh node ID hex: {}", e))
                })?;
                Ok(TransportAddr::Iroh(node_id_bytes))
            }
            #[cfg(not(feature = "iroh"))]
            {
                Err(StratumV2Error::Configuration(
                    "Iroh feature not enabled".to_string(),
                ))
            }
        } else {
            // Try parsing as direct SocketAddr (default to TCP)
            let socket_addr = url.parse::<SocketAddr>().map_err(|e| {
                StratumV2Error::Configuration(format!("Invalid address format: {}", e))
            })?;
            Ok(TransportAddr::Tcp(socket_addr))
        }
    }
}
