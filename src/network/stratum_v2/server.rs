//! Stratum V2 Server Implementation
//!
//! Implements the Stratum V2 mining pool server, accepting miner connections
//! and coordinating mining operations.

use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use crate::network::stratum_v2::messages::message_types;
use crate::network::stratum_v2::messages::{
    NewMiningJobMessage, OpenMiningChannelMessage, OpenMiningChannelSuccessMessage,
    SetupConnectionMessage, SetupConnectionSuccessMessage, StratumV2Message, SubmitSharesMessage,
    SubmitSharesSuccessMessage,
};
use crate::network::stratum_v2::pool::StratumV2Pool;
use crate::network::stratum_v2::protocol::{TlvDecoder, TlvEncoder};
use crate::network::NetworkManager;
use crate::node::miner::MiningCoordinator;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Miner connection tracking
type MinerConnection =
    Arc<RwLock<Option<Box<dyn crate::network::transport::TransportConnection + Send>>>>;

/// Connection pool for miner connections (0-RTT reuse)
struct ConnectionPool {
    /// Active connections (endpoint -> (connection, last_used))
    connections: std::collections::HashMap<String, (MinerConnection, std::time::Instant)>,
    /// Maximum idle time before connection is considered stale
    max_idle: std::time::Duration,
}

/// Stratum V2 mining pool server
pub struct StratumV2Server {
    network_manager: Arc<RwLock<NetworkManager>>,
    pool: Arc<RwLock<StratumV2Pool>>,
    mining_coordinator: Arc<RwLock<MiningCoordinator>>,
    listen_addr: SocketAddr,
    running: bool,
    /// Active miner connections (endpoint -> (connection, last_used))
    miner_connections:
        Arc<RwLock<std::collections::HashMap<String, (MinerConnection, std::time::Instant)>>>,
}

impl StratumV2Server {
    /// Create a new Stratum V2 server
    pub fn new(
        network_manager: Arc<RwLock<NetworkManager>>,
        mining_coordinator: Arc<RwLock<MiningCoordinator>>,
        listen_addr: SocketAddr,
    ) -> Self {
        Self {
            network_manager,
            pool: Arc::new(RwLock::new(StratumV2Pool::new())),
            mining_coordinator,
            listen_addr,
            running: false,
            miner_connections: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Start the server
    pub async fn start(&mut self) -> StratumV2Result<()> {
        if self.running {
            return Err(StratumV2Error::Configuration(
                "Server already running".to_string(),
            ));
        }

        info!("Starting Stratum V2 server on {}", self.listen_addr);

        // Server would listen on configured address via NetworkManager
        // In full implementation, would:
        // 1. Register listener with NetworkManager
        // 2. Accept incoming connections
        // 3. Handle Stratum V2 protocol messages

        self.running = true;
        info!("Stratum V2 server started");

        Ok(())
    }

    /// Stop the server
    pub async fn stop(&mut self) -> StratumV2Result<()> {
        if !self.running {
            return Ok(());
        }

        info!("Stopping Stratum V2 server");
        self.running = false;
        Ok(())
    }

    /// Handle incoming Stratum V2 message
    pub async fn handle_message(
        &self,
        data: Vec<u8>,
        peer_addr: SocketAddr,
    ) -> StratumV2Result<Vec<u8>> {
        // Decode TLV message
        let (tag, payload) = TlvDecoder::decode_raw(&data)?;

        // Deserialize message based on tag
        let response_bytes = match tag {
            message_types::SETUP_CONNECTION => {
                let msg: SetupConnectionMessage = SetupConnectionMessage::from_bytes(&payload)?;
                // Create connection wrapper (in real implementation, would extract from accept context)
                let connection: MinerConnection = Arc::new(RwLock::new(None)); // Placeholder - would be actual connection
                let response = self.handle_setup_connection(msg, connection).await?;
                let response_payload = response.to_bytes()?;
                let mut encoder = TlvEncoder::new();
                encoder.encode(response.message_type(), &response_payload)?
            }
            message_types::OPEN_MINING_CHANNEL => {
                let msg: OpenMiningChannelMessage = OpenMiningChannelMessage::from_bytes(&payload)?;
                // Extract endpoint from connection context (would need peer tracking)
                // For now, use placeholder
                let endpoint = format!("miner_{}", peer_addr);
                let response = self.handle_open_channel(&endpoint, msg).await?;
                let response_payload = response.to_bytes()?;
                let mut encoder = TlvEncoder::new();
                encoder.encode(response.message_type(), &response_payload)?
            }
            message_types::SUBMIT_SHARES => {
                let msg: SubmitSharesMessage = SubmitSharesMessage::from_bytes(&payload)?;
                // Extract endpoint from connection context
                let endpoint = format!("miner_{}", peer_addr);
                let response = self.handle_submit_shares(&endpoint, msg).await?;
                let response_payload = response.to_bytes()?;
                let mut encoder = TlvEncoder::new();
                encoder.encode(response.message_type(), &response_payload)?
            }
            _ => {
                return Err(StratumV2Error::InvalidMessageType(tag));
            }
        };

        Ok(response_bytes)
    }

    /// Register a miner connection (with connection pooling)
    pub async fn register_miner_connection(&self, endpoint: String, connection: MinerConnection) {
        let mut connections = self.miner_connections.write().await;
        connections.insert(endpoint, (connection, std::time::Instant::now()));
    }

    /// Get a miner connection from pool (0-RTT if connection is still valid)
    pub async fn get_miner_connection(&self, endpoint: &str) -> Option<MinerConnection> {
        let mut connections = self.miner_connections.write().await;

        if let Some((conn, last_used)) = connections.get_mut(endpoint) {
            // Check if connection is still valid (not idle too long)
            let max_idle = std::time::Duration::from_secs(300); // 5 minutes
            if last_used.elapsed() < max_idle {
                // Check if connection is actually still connected
                let conn_guard = conn.read().await;
                if let Some(ref c) = *conn_guard {
                    if c.is_connected() {
                        // Update last used time
                        *last_used = std::time::Instant::now();
                        drop(conn_guard);
                        return Some(conn.clone());
                    }
                }
            }

            // Connection is stale or disconnected, remove it
            connections.remove(endpoint);
        }

        None
    }

    /// Handle Setup Connection message
    async fn handle_setup_connection(
        &self,
        msg: SetupConnectionMessage,
        connection: MinerConnection,
    ) -> StratumV2Result<SetupConnectionSuccessMessage> {
        // Register connection
        self.register_miner_connection(msg.endpoint.clone(), connection)
            .await;

        // Handle in pool
        let mut pool = self.pool.write().await;
        pool.handle_setup_connection(msg)
    }

    /// Handle Open Mining Channel message
    async fn handle_open_channel(
        &self,
        endpoint: &str,
        msg: OpenMiningChannelMessage,
    ) -> StratumV2Result<OpenMiningChannelSuccessMessage> {
        let mut pool = self.pool.write().await;
        pool.handle_open_channel(endpoint, msg)
    }

    /// Handle Submit Shares message
    async fn handle_submit_shares(
        &self,
        endpoint: &str,
        msg: SubmitSharesMessage,
    ) -> StratumV2Result<SubmitSharesSuccessMessage> {
        let mut pool = self.pool.write().await;
        pool.handle_submit_shares(endpoint, msg)
    }

    /// Generate and distribute new block template
    pub async fn update_template(&self) -> StratumV2Result<()> {
        // Get block template from MiningCoordinator
        let template = {
            let mut coordinator = self.mining_coordinator.write().await;
            coordinator.generate_block_template().await.map_err(|e| {
                StratumV2Error::MiningJob(format!("Failed to generate block template: {}", e))
            })?
        };

        debug!(
            "Generated new block template with {} transactions",
            template.transactions.len()
        );

        // Set template in pool and get distribution messages
        let (job_id, messages) = {
            let mut pool = self.pool.write().await;
            pool.set_template(template)
        };

        // Send messages to all miners (parallel where possible via QUIC streams)
        // QUIC streams enable parallel sends even if we iterate sequentially
        for (endpoint, job_msg) in messages {
            if let Some(connection) = self.get_miner_connection(&endpoint).await {
                // Each send uses its own QUIC stream (via send_on_channel for Iroh)
                // This enables parallel transmission even though we iterate sequentially
                if let Err(e) = self.send_job_message(&connection, &job_msg).await {
                    warn!("Failed to send job {} to miner {}: {}", job_id, endpoint, e);
                }
            } else {
                debug!("No connection found for miner {}", endpoint);
            }
        }

        Ok(())
    }

    /// Send a mining job message to a miner connection
    /// Uses QUIC stream multiplexing if connection supports it
    async fn send_job_message(
        &self,
        connection: &MinerConnection,
        msg: &NewMiningJobMessage,
    ) -> StratumV2Result<()> {
        // Extract channel ID from message
        let channel_id = Some(msg.channel_id);

        // Serialize message
        let payload = msg.to_bytes().map_err(|e| {
            StratumV2Error::Serialization(format!("Failed to serialize job message: {}", e))
        })?;

        // Encode TLV
        let mut encoder = TlvEncoder::new();
        let encoded = encoder
            .encode(message_types::NEW_MINING_JOB, &payload)
            .map_err(|e| {
                StratumV2Error::Serialization(format!("Failed to encode job message: {}", e))
            })?;

        // Send via connection with channel-specific stream
        let mut conn = connection.write().await;
        if let Some(ref mut conn) = *conn {
            // Use channel-specific sending - transports that support channels (QUIC/Iroh)
            // will route to the appropriate channel stream, others will use default send()
            conn.send_on_channel(channel_id, &encoded)
                .await
                .map_err(|e| {
                    StratumV2Error::Network(format!("Failed to send job message: {}", e))
                })?;
        } else {
            return Err(StratumV2Error::Connection(anyhow::anyhow!(
                "Connection not available"
            )));
        }

        Ok(())
    }

    /// Get pool statistics
    pub async fn get_statistics(&self) -> PoolStatistics {
        let pool = self.pool.read().await;
        PoolStatistics {
            connected_miners: pool.miner_count(),
            // Additional statistics would be collected here
        }
    }

    /// Check if server is running
    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// Pool statistics
#[derive(Debug, Clone)]
pub struct PoolStatistics {
    pub connected_miners: usize,
}
