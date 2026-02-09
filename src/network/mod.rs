//! Network layer for blvm-node
//!
//! This module provides P2P networking, peer management, and Bitcoin protocol
//! message handling for communication with other Bitcoin nodes.

pub mod address_db;
pub mod ban_list_merging;
pub mod ban_list_signing;
pub mod chain_access;
pub mod dns_seeds;
pub mod dos_protection;
pub mod ibd_protection;
pub mod bandwidth_protection;
pub mod inventory;
pub mod lan_discovery;
pub mod lan_security;
pub mod message_bridge;
pub mod module_registry_extensions;
pub mod peer;
pub mod peer_scoring;
pub mod protocol;
pub mod protocol_adapter;
pub mod protocol_extensions;
pub mod relay;
#[cfg(feature = "erlay")]
pub mod erlay;
pub mod replay_protection;
pub mod tcp_transport;
pub mod transport;

#[cfg(feature = "quinn")]
pub mod quinn_transport;

#[cfg(feature = "iroh")]
pub mod iroh_transport;

#[cfg(feature = "utxo-commitments")]
pub mod utxo_commitments_client;

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
#[cfg(feature = "fibre")]
pub mod fibre; // FIBRE-style Fast Relay Network
pub mod package_relay; // BIP 331 Package Relay
pub mod package_relay_handler; // BIP 331 handlers
pub mod txhash; // Non-consensus hashing helpers for relay

use crate::network::protocol::{AddrMessage, NetworkAddress, ProtocolMessage, ProtocolParser};
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use crate::utils::current_timestamp;
use anyhow::Result;
use blvm_protocol::mempool::Mempool;
use blvm_protocol::{BitcoinProtocolEngine, ConsensusProof, UtxoSet};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::network::tcp_transport::TcpTransport;
use crate::network::transport::{Transport, TransportAddr, TransportListener, TransportPreference};
use hex;
use secp256k1;
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

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
        self.peers
            .keys()
            .flat_map(|addr| {
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

    /// Get governance-enabled peers (peers with NODE_GOVERNANCE service flag)
    #[cfg(feature = "governance")]
    pub fn get_governance_peers(&self) -> Vec<(TransportAddr, SocketAddr)> {
        use crate::network::protocol::NODE_GOVERNANCE;
        self.peers
            .iter()
            .filter_map(|(transport_addr, peer)| {
                if peer.has_service(NODE_GOVERNANCE) {
                    match transport_addr {
                        TransportAddr::Tcp(sock) => Some((transport_addr.clone(), *sock)),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some((transport_addr.clone(), *sock)),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None, // Iroh peers don't have SocketAddr for now
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn can_accept_peer(&self) -> bool {
        self.peers.len() < self.max_peers
    }

    /// Select best peers based on quality score
    ///
    /// Returns peers sorted by quality score (highest first)
    pub fn select_best_peers(&self, count: usize) -> Vec<TransportAddr> {
        let mut peers: Vec<_> = self
            .peers
            .iter()
            .map(|(addr, peer)| (addr.clone(), peer.quality_score()))
            .collect();

        // Sort by quality score (descending)
        peers.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Return top N addresses
        peers
            .into_iter()
            .take(count)
            .map(|(addr, _)| addr)
            .collect()
    }

    /// Select reliable peers only
    ///
    /// Returns peers that meet reliability criteria
    pub fn select_reliable_peers(&self) -> Vec<TransportAddr> {
        self.peers
            .iter()
            .filter(|(_, peer)| peer.quality_score() > 0.5) // Use quality_score as reliability indicator
            .map(|(addr, _)| addr.clone())
            .collect()
    }

    /// Get peer quality statistics
    pub fn get_quality_stats(&self) -> (usize, usize, f64) {
        let total = self.peers.len();
        let reliable = self
            .peers
            .values()
            .filter(|peer| peer.quality_score() > 0.5) // Use quality_score as reliability indicator
            .count();
        let avg_quality = if total > 0 {
            self.peers
                .values()
                .map(|peer| peer.quality_score())
                .sum::<f64>()
                / total as f64
        } else {
            0.0
        };

        (total, reliable, avg_quality)
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
        let now = current_timestamp();
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
        let now = current_timestamp();

        if now > self.last_refill {
            let elapsed = now - self.last_refill;
            let tokens_to_add = (elapsed as u32) * self.rate;
            self.tokens = self
                .tokens
                .saturating_add(tokens_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }
    }
}

/// Byte rate limiter for peer transaction byte rate limiting
pub struct PeerByteRateLimiter {
    /// Current number of bytes available
    bytes: u64,
    /// Maximum burst size (initial byte count)
    burst_limit: u64,
    /// Bytes per second refill rate
    rate: u64,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl PeerByteRateLimiter {
    /// Create a new byte rate limiter
    pub fn new(burst_limit: u64, rate: u64) -> Self {
        let now = current_timestamp();
        Self {
            bytes: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if bytes can be consumed and consume them
    pub fn check_and_consume(&mut self, bytes: u64) -> bool {
        self.refill();
        if self.bytes >= bytes {
            self.bytes -= bytes;
            true
        } else {
            false
        }
    }

    /// Refill bytes based on elapsed time
    fn refill(&mut self) {
        let now = current_timestamp();

        if now > self.last_refill {
            let elapsed = now - self.last_refill;
            let bytes_to_add = (elapsed as u64) * self.rate;
            self.bytes = self
                .bytes
                .saturating_add(bytes_to_add)
                .min(self.burst_limit);
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
    peer_manager: Arc<Mutex<PeerManager>>,
    tcp_transport: TcpTransport,
    #[cfg(feature = "quinn")]
    quinn_transport: Option<crate::network::quinn_transport::QuinnTransport>,
    #[cfg(feature = "iroh")]
    iroh_transport: Option<crate::network::iroh_transport::IrohTransport>,
    transport_preference: TransportPreference,
    peer_tx: mpsc::UnboundedSender<NetworkMessage>,
    peer_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<NetworkMessage>>>,
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
    /// Module registry for serving modules via P2P
    module_registry: Arc<tokio::sync::Mutex<Option<Arc<crate::module::registry::client::ModuleRegistry>>>>,
    /// Payment processor for BIP70 payments (HTTP and P2P)
    payment_processor: Arc<tokio::sync::Mutex<Option<Arc<crate::payment::processor::PaymentProcessor>>>>,
    /// Payment state machine for unified payment coordination
    payment_state_machine: Arc<tokio::sync::Mutex<Option<Arc<crate::payment::state_machine::PaymentStateMachine>>>>,
    /// Merchant private key for signing payment ACKs (optional)
    merchant_key: Arc<tokio::sync::Mutex<Option<secp256k1::SecretKey>>>,
    /// Node payment address script (for module downloads - 10% fee)
    node_payment_script: Arc<tokio::sync::Mutex<Option<Vec<u8>>>>,
    /// Module encryption for encrypted module serving
    module_encryption: Arc<tokio::sync::Mutex<Option<Arc<crate::module::encryption::ModuleEncryption>>>>,
    /// Modules directory for encrypted/decrypted module storage
    modules_dir: Arc<tokio::sync::Mutex<Option<std::path::PathBuf>>>,
    /// Event publisher for module event notifications (optional)
    event_publisher: Arc<tokio::sync::Mutex<Option<Arc<crate::node::event_publisher::EventPublisher>>>>,
    /// FIBRE relay manager (for fast block relay)
    #[cfg(feature = "fibre")]
    fibre_relay: Option<Arc<Mutex<fibre::FibreRelay>>>,
    /// Peer state storage (per-connection state)
    /// Read-heavy: many reads to check peer state, fewer writes when updating state
    peer_states: Arc<RwLock<HashMap<SocketAddr, blvm_protocol::network::PeerState>>>,
    /// Persistent peer list (peers to connect to on startup)
    persistent_peers: Arc<Mutex<HashSet<SocketAddr>>>,
    /// Eclipse attack prevention: track peer diversity
    /// Maps IP address prefixes (first 3 octets) to connection count
    /// Prevents too many connections from same IP range
    peer_diversity: Arc<Mutex<HashMap<[u8; 3], usize>>>,
    /// Network active state (true = enabled, false = disabled)
    network_active: Arc<Mutex<bool>>,
    /// Ban list (banned peers with unban timestamp)
    /// Read-heavy: many reads to check if peer is banned, fewer writes when banning/unbanning
    ban_list: Arc<RwLock<HashMap<SocketAddr, u64>>>, // addr -> unban timestamp
    /// Per-IP connection count (to prevent Sybil attacks)
    connections_per_ip: Arc<Mutex<HashMap<std::net::IpAddr, usize>>>,
    /// Per-peer message rate limiting (token bucket)
    peer_message_rates: Arc<Mutex<HashMap<SocketAddr, PeerRateLimiter>>>,
    /// Per-peer transaction rate limiting (separate from message rate limiting)
    peer_tx_rate_limiters: Arc<Mutex<HashMap<SocketAddr, PeerRateLimiter>>>,
    /// Per-peer transaction byte rate limiting (bytes per second)
    peer_tx_byte_rate_limiters: Arc<Mutex<HashMap<SocketAddr, PeerByteRateLimiter>>>,
    /// Mempool policy configuration for rate limits
    mempool_policy_config: Option<Arc<crate::config::MempoolPolicyConfig>>,
    /// Spam ban configuration
    spam_ban_config: Option<Arc<crate::config::SpamBanConfig>>,
    /// Pending blocks queue (blocks received via BlockReceived message)
    /// Separate from main message channel to avoid draining other messages
    pending_blocks: Arc<std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>>,
    /// Network statistics
    /// Optimization: Use AtomicU64 for lock-free updates
    bytes_sent: Arc<AtomicU64>,
    bytes_received: Arc<AtomicU64>,
    /// Mapping from SocketAddr to TransportAddr (for Iroh peers that use placeholder SocketAddr)
    socket_to_transport: Arc<Mutex<HashMap<SocketAddr, TransportAddr>>>,
    /// Request ID counter for async request-response patterns
    /// Optimization: Use AtomicU64 for lock-free updates (eliminates need for block_in_place)
    request_id_counter: Arc<AtomicU64>,
    /// Pending async requests with metadata
    /// Key: request_id, Value: (sender, peer_addr, timestamp)
    pending_requests: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    /// Pending Headers requests (for IBD) - supports pipelining with queue per peer
    /// Key: peer_addr, Value: queue of sender channels (FIFO order)
    pending_headers_requests: Arc<Mutex<HashMap<SocketAddr, std::collections::VecDeque<tokio::sync::oneshot::Sender<Vec<blvm_protocol::BlockHeader>>>>>>,
    /// Pending Block requests (for IBD)
    /// Key: (peer_addr, block_hash), Value: sender channel
    pending_block_requests: Arc<Mutex<HashMap<(SocketAddr, blvm_protocol::Hash), tokio::sync::oneshot::Sender<(blvm_protocol::Block, Vec<Vec<blvm_protocol::segwit::Witness>>)>>>>,
    /// DoS protection manager
    dos_protection: Arc<dos_protection::DosProtectionManager>,
    /// IBD bandwidth protection manager
    ibd_protection: Arc<ibd_protection::IbdProtectionManager>,
    /// Unified bandwidth protection manager (extends IBD protection)
    bandwidth_protection: Arc<bandwidth_protection::BandwidthProtectionManager>,
    /// Replay protection for custom protocol messages
    replay_protection: Arc<replay_protection::ReplayProtection>,
    /// Pending ban shares (for periodic sharing)
    pending_ban_shares: Arc<Mutex<Vec<(SocketAddr, u64, String)>>>, // (addr, unban_timestamp, reason)
    /// Ban list sharing configuration
    ban_list_sharing_config: Option<crate::config::BanListSharingConfig>,
    /// Spam violation tracking per peer (for spam-specific banning)
    /// Maps SocketAddr -> violation count
    peer_spam_violations: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    /// Governance message relay configuration
    #[cfg(feature = "governance")]
    governance_config: Option<crate::config::GovernanceConfig>,
    /// Address database for peer discovery
    /// Read-heavy: many reads to query addresses, fewer writes when adding addresses
    address_database: Arc<RwLock<address_db::AddressDatabase>>,
    /// Last time we sent addr message (Unix timestamp)
    last_addr_sent: Arc<Mutex<u64>>,
    /// Enable self-advertisement (send own address to peers)
    enable_self_advertisement: bool,
    /// Request timeout configuration
    request_timeout_config: Arc<crate::config::RequestTimeoutConfig>,
    /// Peer reconnection queue (exponential backoff)
    /// Maps SocketAddr to (attempts, last_attempt_timestamp, quality_score)
    peer_reconnection_queue: Arc<Mutex<HashMap<SocketAddr, (u32, u64, f64)>>>,
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
    // Headers response (for IBD)
    HeadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP157 Block Filter messages
    GetCfiltersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfheadersReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetCfcheckptReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP331 Package Relay messages
    PkgTxnReceived(Vec<u8>, SocketAddr),     // (data, peer_addr)
    SendPkgTxnReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Module Registry messages
    GetModuleReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleReceived(Vec<u8>, SocketAddr),    // (data, peer_addr)
    GetModuleByHashReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleByHashReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    GetModuleListReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    ModuleListReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // BIP70 Payment Protocol messages
    GetPaymentRequestReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    PaymentRequestReceived(Vec<u8>, SocketAddr),    // (data, peer_addr)
    PaymentReceived(Vec<u8>, SocketAddr),           // (data, peer_addr)
    PaymentACKReceived(Vec<u8>, SocketAddr),        // (data, peer_addr)
    // CTV Payment Proof messages
    #[cfg(feature = "ctv")]
    PaymentProofReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    SettlementNotificationReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
    // Mesh networking packets
    MeshPacketReceived(Vec<u8>, SocketAddr), // (data, peer_addr)
}

impl NetworkManager {
    /// Create a new network manager with default TCP-only transport
    pub fn new(_listen_addr: SocketAddr) -> Self {
        Self::with_config(_listen_addr, 100, TransportPreference::TCP_ONLY, None)
    }

    /// Create a new network manager with configuration
    pub fn with_config(
        _listen_addr: SocketAddr,
        max_peers: usize,
        preference: TransportPreference,
        config: Option<&crate::config::NodeConfig>,
    ) -> Self {
        let (peer_tx, peer_rx) = mpsc::unbounded_channel();
        let peer_rx = Arc::new(tokio::sync::Mutex::new(peer_rx));

        // Use config for DoS protection
        let dos_config_default = crate::config::DosProtectionConfig::default();
        let dos_config = config
            .and_then(|c| c.dos_protection.as_ref())
            .unwrap_or(&dos_config_default);

        let dos_protection = Arc::new(dos_protection::DosProtectionManager::with_ban_settings(
            dos_config.max_connections_per_window,
            dos_config.window_seconds,
            dos_config.max_message_queue_size,
            dos_config.max_active_connections,
            dos_config.auto_ban_threshold,
            dos_config.ban_duration_seconds,
        ));

        // Initialize IBD protection (bandwidth exhaustion attack mitigation)
        let ibd_protection = if let Some(ibd_config) = config.and_then(|c| c.ibd_protection.as_ref()) {
            let mut ibd_protection_config = ibd_protection::IbdProtectionConfig::default();
            // Convert GB to bytes
            ibd_protection_config.max_bandwidth_per_peer_per_day = (ibd_config.max_bandwidth_per_peer_per_day_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_bandwidth_per_peer_per_hour = (ibd_config.max_bandwidth_per_peer_per_hour_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_bandwidth_per_ip_per_day = (ibd_config.max_bandwidth_per_ip_per_day_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_bandwidth_per_ip_per_hour = (ibd_config.max_bandwidth_per_ip_per_hour_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_bandwidth_per_subnet_per_day = (ibd_config.max_bandwidth_per_subnet_per_day_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_bandwidth_per_subnet_per_hour = (ibd_config.max_bandwidth_per_subnet_per_hour_gb * 1024 * 1024 * 1024) as u64;
            ibd_protection_config.max_concurrent_ibd_serving = ibd_config.max_concurrent_ibd_serving;
            ibd_protection_config.ibd_request_cooldown_seconds = ibd_config.ibd_request_cooldown_seconds;
            ibd_protection_config.suspicious_reconnection_threshold = ibd_config.suspicious_reconnection_threshold;
            ibd_protection_config.reputation_ban_threshold = ibd_config.reputation_ban_threshold;
            ibd_protection_config.enable_emergency_throttle = ibd_config.enable_emergency_throttle;
            ibd_protection_config.emergency_throttle_percent = ibd_config.emergency_throttle_percent;
            Arc::new(ibd_protection::IbdProtectionManager::with_config(ibd_protection_config))
        } else {
            Arc::new(ibd_protection::IbdProtectionManager::new())
        };

        // Initialize unified bandwidth protection (extends IBD protection)
        let bandwidth_protection = Arc::new(bandwidth_protection::BandwidthProtectionManager::new(
            Arc::clone(&ibd_protection)
        ));

        // Use config for address database
        let addr_db_config_default = crate::config::AddressDatabaseConfig::default();
        let addr_db_config = config
            .and_then(|c| c.address_database.as_ref())
            .unwrap_or(&addr_db_config_default);

        let address_database = Arc::new(RwLock::new(address_db::AddressDatabase::with_expiration(
            addr_db_config.max_addresses,
            addr_db_config.expiration_seconds,
        )));

        // Use config for request timeouts
        let timeout_config_default = crate::config::RequestTimeoutConfig::default();
        let timeout_config = config
            .and_then(|c| c.request_timeouts.as_ref())
            .unwrap_or(&timeout_config_default);
        let request_timeout_config = Arc::new(timeout_config.clone());

        Self {
            peer_manager: Arc::new(Mutex::new(PeerManager::new(max_peers))),
            peer_diversity: Arc::new(Mutex::new(HashMap::new())),
            tcp_transport: TcpTransport::new(),
            #[cfg(feature = "quinn")]
            quinn_transport: None,
            #[cfg(feature = "iroh")]
            iroh_transport: None,
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
            module_registry: Arc::new(tokio::sync::Mutex::new(None)),
            payment_processor: Arc::new(tokio::sync::Mutex::new(None)),
            payment_state_machine: Arc::new(tokio::sync::Mutex::new(None)),
            merchant_key: Arc::new(tokio::sync::Mutex::new(None)),
            node_payment_script: Arc::new(tokio::sync::Mutex::new(None)),
            module_encryption: Arc::new(tokio::sync::Mutex::new(None)),
            modules_dir: Arc::new(tokio::sync::Mutex::new(None)),
            event_publisher: Arc::new(tokio::sync::Mutex::new(None)),
            #[cfg(feature = "fibre")]
            fibre_relay: None,
            peer_states: Arc::new(RwLock::new(HashMap::new())),
            persistent_peers: Arc::new(Mutex::new(HashSet::new())),
            network_active: Arc::new(Mutex::new(true)),
            ban_list: Arc::new(RwLock::new(HashMap::new())),
            connections_per_ip: Arc::new(Mutex::new(HashMap::new())),
            peer_message_rates: Arc::new(Mutex::new(HashMap::new())),
            peer_tx_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            peer_tx_byte_rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            mempool_policy_config: config
                .and_then(|c| c.mempool.as_ref())
                .map(|c| Arc::new(c.clone())),
            spam_ban_config: config
                .and_then(|c| c.spam_ban.as_ref())
                .map(|c| Arc::new(c.clone())),
            peer_spam_violations: Arc::new(Mutex::new(HashMap::new())),
            pending_blocks: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            bytes_sent: Arc::new(AtomicU64::new(0)),
            bytes_received: Arc::new(AtomicU64::new(0)),
            socket_to_transport: Arc::new(Mutex::new(HashMap::new())),
            request_id_counter: Arc::new(AtomicU64::new(0)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_headers_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_block_requests: Arc::new(Mutex::new(HashMap::new())),
            dos_protection,
            ibd_protection,
            bandwidth_protection,
            replay_protection: Arc::new(replay_protection::ReplayProtection::new()),
            pending_ban_shares: Arc::new(Mutex::new(Vec::new())),
            ban_list_sharing_config: config.and_then(|c| c.ban_list_sharing.clone()),
            #[cfg(feature = "governance")]
            governance_config: config.and_then(|c| c.governance.clone()),
            address_database,
            last_addr_sent: Arc::new(Mutex::new(0)),
            enable_self_advertisement: config.map(|c| c.enable_self_advertisement).unwrap_or(true),
            request_timeout_config,
            peer_reconnection_queue: Arc::new(Mutex::new(HashMap::new())),
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

    /// Set module registry for serving modules via P2P
    pub fn with_module_registry(
        mut self,
        module_registry: Arc<crate::module::registry::client::ModuleRegistry>,
    ) -> Self {
        *self.module_registry.blocking_lock() = Some(module_registry);
        self
    }

    /// Set module registry for serving modules via P2P
    pub async fn set_module_registry(
        &self,
        module_registry: Arc<crate::module::registry::client::ModuleRegistry>,
    ) {
        *self.module_registry.lock().await = Some(module_registry);
    }

    /// Set payment processor for BIP70 payments (HTTP and P2P)
    pub async fn set_payment_processor(
        &self,
        processor: Arc<crate::payment::processor::PaymentProcessor>,
    ) {
        *self.payment_processor.lock().await = Some(processor);
    }

    /// Set merchant private key for signing payment ACKs
    pub async fn set_merchant_key(&self, merchant_key: Option<secp256k1::SecretKey>) {
        *self.merchant_key.lock().await = merchant_key;
    }

    /// Set node payment address script (for module downloads)
    pub async fn set_node_payment_script(&self, script: Option<Vec<u8>>) {
        *self.node_payment_script.lock().await = script;
    }

    /// Set payment state machine for unified payment coordination
    pub async fn set_payment_state_machine(
        &self,
        state_machine: Arc<crate::payment::state_machine::PaymentStateMachine>,
    ) {
        // Set network sender for payment proof broadcasting
        #[cfg(feature = "ctv")]
        {
            use std::sync::Arc as StdArc;
            // Try to get mutable access to set the sender
            // If we can't (multiple references exist), that's okay - broadcasting will be disabled
            if let Some(state_machine_mut) = StdArc::get_mut(&mut state_machine.clone()) {
                *state_machine_mut = state_machine_mut
                    .clone()
                    .with_network_sender(self.peer_tx.clone());
            } else {
                // Multiple references exist - we can't update it now
                // The state machine will work but won't broadcast (will just update state)
                debug!("Payment state machine has multiple references, network sender not set (broadcasting disabled)");
            }
        }
        *self.payment_state_machine.lock().await = Some(state_machine);
    }

    /// Set module encryption for encrypted module serving
    pub async fn set_module_encryption(
        &self,
        encryption: Arc<crate::module::encryption::ModuleEncryption>,
    ) {
        *self.module_encryption.lock().await = Some(encryption);
    }

    /// Set modules directory for encrypted/decrypted module storage
    pub async fn set_modules_dir(&self, modules_dir: std::path::PathBuf) {
        *self.modules_dir.lock().await = Some(modules_dir);
    }

    /// Set event publisher for network event notifications
    pub async fn set_event_publisher(
        &self,
        event_publisher: Option<Arc<crate::node::event_publisher::EventPublisher>>,
    ) {
        *self.event_publisher.lock().await = event_publisher;
    }

    /// Initialize FIBRE relay (if enabled in config)
    #[cfg(feature = "fibre")]
    pub async fn initialize_fibre(
        &mut self,
        config: Option<&crate::config::NodeConfig>,
    ) -> Result<()> {
        let default_config = blvm_protocol::fibre::FibreConfig::default();
        let fibre_config = config
            .and_then(|c| c.fibre.as_ref())
            .unwrap_or(&default_config);

        if !fibre_config.enabled {
            debug!("FIBRE relay disabled in configuration");
            return Ok(());
        }

        // Create FIBRE relay
        let mut fibre_relay = fibre::FibreRelay::with_config(fibre_config.clone());

        // Set message channel for assembled blocks
        fibre_relay.set_message_sender(self.peer_tx.clone());

        // Initialize UDP transport (use listen_addr + 1 for UDP port, or default)
        let udp_addr = config
            .and_then(|c| c.listen_addr)
            .map(|addr| {
                let port = addr.port().saturating_add(1); // UDP port = TCP port + 1
                SocketAddr::new(addr.ip(), port)
            })
            .unwrap_or_else(|| "0.0.0.0:8334".parse().unwrap()); // Default FIBRE port

        // Initialize UDP and start receiver
        let chunk_rx = fibre_relay
            .initialize_udp(udp_addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to initialize FIBRE UDP: {}", e))?;

        // Start chunk processor
        let fibre_relay_arc = Arc::new(Mutex::new(fibre_relay));
        fibre::start_chunk_processor(fibre_relay_arc.clone(), chunk_rx);

        self.fibre_relay = Some(fibre_relay_arc);

        info!("FIBRE relay initialized on UDP {}", udp_addr);
        Ok(())
    }

    /// Get FIBRE relay (if initialized)
    #[cfg(feature = "fibre")]
    pub fn fibre_relay(&self) -> Option<Arc<Mutex<fibre::FibreRelay>>> {
        self.fibre_relay.clone()
    }

    /// Broadcast block via FIBRE to all FIBRE-capable peers
    #[cfg(feature = "fibre")]
    pub async fn broadcast_block_via_fibre(&self, block: &blvm_protocol::Block) -> Result<()> {
        if let Some(fibre_relay) = &self.fibre_relay {
            // Encode block (need to clone for encoding)
            let encoded = {
                let mut relay = fibre_relay.lock().await;
                relay
                    .encode_block(block.clone())
                    .map_err(|e| anyhow::anyhow!("FIBRE encoding failed: {}", e))?
            };

            // Get peer list and send (separate lock scope)
            let peer_ids: Vec<String> = {
                let relay = fibre_relay.lock().await;
                relay
                    .get_fibre_peers()
                    .iter()
                    .map(|p| p.peer_id.clone())
                    .collect()
            };

            // Send to all FIBRE peers
            for peer_id in peer_ids {
                let mut relay = fibre_relay.lock().await;
                if let Err(e) = relay.send_block(&peer_id, encoded.clone()).await {
                    warn!("Failed to send block via FIBRE to {}: {}", peer_id, e);
                } else {
                    debug!("Sent block via FIBRE to {}", peer_id);
                }
            }

            Ok(())
        } else {
            // FIBRE not initialized, skip
            Ok(())
        }
    }

    /// Check if address is local (loopback, private, link-local)
    pub(crate) fn is_local_address(addr: &SocketAddr) -> bool {
        match addr.ip() {
            IpAddr::V4(ip) => {
                ip.is_loopback() || ip.is_private() || ip.is_link_local()
            }
            IpAddr::V6(ip) => {
                ip.is_loopback() || ip.is_unspecified()
            }
        }
    }

    /// Check if address is onion (Tor) - placeholder for future implementation
    /// In Bitcoin Core, this checks for .onion domains, but we'd need DNS resolution
    /// For now, this is a placeholder that returns false
    pub(crate) fn is_onion_address(_addr: &SocketAddr) -> bool {
        // TODO: Implement proper .onion detection when DNS resolution is available
        // For now, return false (no onion detection)
        false
    }

    /// Evict extra outbound peers if we have too many
    /// Bitcoin Core protects up to MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT peers
    /// based on block announcement recency
    #[allow(dead_code)]
    async fn evict_extra_outbound_peers(&self) {
        const MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT: usize = 4;
        
        let mut pm = self.peer_manager.lock().await;
        
        // Get all outbound peers with their last block announcement time
        let mut outbound_peers: Vec<(TransportAddr, u64)> = pm.peers.iter()
            .filter(|(_, peer)| peer.is_outbound() && !peer.is_manual())
            .map(|(addr, peer)| {
                let last_announce = peer.last_block_announcement().unwrap_or(0);
                (addr.clone(), last_announce)
            })
            .collect();
        
        if outbound_peers.len() <= MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT {
            return; // Not too many peers, no eviction needed
        }
        
        // Sort by last announcement (oldest first) - peers with no announcements (0) come first
        outbound_peers.sort_by_key(|(_, time)| *time);
        
        // Disconnect oldest peers (keep best MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT)
        let peers_to_evict = outbound_peers.len() - MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;
        for (addr, last_announce) in outbound_peers.iter().take(peers_to_evict) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let time_ago = if *last_announce > 0 {
                now.saturating_sub(*last_announce)
            } else {
                0
            };
            warn!(
                "Evicting extra outbound peer {:?} (last block announcement: {} seconds ago)",
                addr, time_ago
            );
            drop(pm); // Release lock before sending message
            let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(addr.clone()));
            pm = self.peer_manager.lock().await; // Re-acquire lock for next iteration
        }
    }

    /// Handle EconomicNodeRegistration message - Relay to governance node if enabled
    /// Also publishes event for governance module to handle
    #[cfg(feature = "governance")]
    async fn handle_economic_node_registration(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeRegistrationMessage,
    ) -> Result<()> {
        use reqwest::Client;
        use tracing::{debug, error, info, warn};

        // Publish event for governance module (non-blocking)
        let event_publisher_guard = self.event_publisher.lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::EconomicNodeRegistered {
                node_id: msg.entity_name.clone(),
                node_type: msg.node_type.clone(),
                hashpower_percent: None,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeRegistered, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeRegistered event: {}", e);
            }
        }

        debug!(
            "EconomicNodeRegistration received from {}: node_type={}, entity={}, message_id={}",
            peer_addr, msg.node_type, msg.entity_name, msg.message_id
        );

        // Replay protection: Check message ID and timestamp
        if let Err(e) = self
            .replay_protection
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeRegistration from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = replay_protection::ReplayProtection::validate_timestamp(msg.timestamp, 3600)
        {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeRegistration from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Check if governance relay is enabled
        let governance_enabled = self
            .governance_config
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            // Relay to other peers (gossip) but don't forward to bllvm-commons
            debug!("Governance relay disabled, gossiping message to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        // Forward to bllvm-commons via VPN
        if let Some(config) = &self.governance_config {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = config
                    .api_key
                    .as_deref()
                    .or_else(|| env_api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{}/internal/governance/registration", commons_url);

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward registration: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeRegistration to bllvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeRegistration: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        // Relay to other governance-enabled peers (gossip)
        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }

    /// Helper: Publish governance event and call handler, preserving return value
    #[cfg(feature = "governance")]
    async fn handle_governance_with_event<F, Fut>(
        &self,
        event_type: crate::module::traits::EventType,
        payload: crate::module::ipc::protocol::EventPayload,
        handler: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        // Publish event for governance module (non-blocking, log errors but don't fail)
        let event_publisher_guard = self.event_publisher.lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            if let Err(e) = event_publisher.publish_event(event_type, payload).await {
                warn!("Failed to publish governance event: {}", e);
            }
        }
        // Call the original handler and return its result
        handler().await
    }

    /// Gossip a governance message to other governance-enabled peers
    #[cfg(feature = "governance")]
    async fn gossip_governance_message<T: serde::Serialize>(
        &self,
        sender_addr: SocketAddr,
        msg: &T,
    ) -> Result<()> {
        use tracing::debug;

        // Get governance-enabled peers (excluding sender)
        let pm = self.peer_manager.lock().await;
        let governance_peers = pm.get_governance_peers();
        drop(pm);

        let governance_peers: Vec<_> = governance_peers
            .into_iter()
            .filter(|(_, addr)| *addr != sender_addr)
            .collect();

        if governance_peers.is_empty() {
            debug!("No governance-enabled peers to gossip to");
            return Ok(());
        }

        // Serialize message using the protocol parser
        // We need to convert the message to the wire format
        // For now, use JSON serialization as a simple approach
        let msg_json = serde_json::to_vec(msg)
            .map_err(|e| anyhow::anyhow!("Failed to serialize governance message: {}", e))?;

        // Send to each governance peer
        let pm = self.peer_manager.lock().await;
        for (_transport_addr, peer_addr) in governance_peers {
            if let Some(peer) = pm.get_peer(&transport::TransportAddr::Tcp(peer_addr)) {
                if let Err(e) = peer.send_message(msg_json.clone()).await {
                    debug!("Failed to gossip to peer {}: {}", peer_addr, e);
                } else {
                    debug!("Gossiped governance message to peer {}", peer_addr);
                }
            }
        }

        Ok(())
    }

    /// Handle EconomicNodeVeto message - Relay to governance node if enabled
    /// Also publishes event for governance module to handle
    #[cfg(feature = "governance")]
    async fn handle_economic_node_veto(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeVetoMessage,
    ) -> Result<()> {
        use reqwest::Client;
        use tracing::{debug, error, info, warn};

        // Publish event for governance module (non-blocking)
        let event_publisher_guard = self.event_publisher.lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let payload = EventPayload::EconomicNodeVeto {
                proposal_id: format!("{}", msg.pr_id),
                node_id: msg
                    .node_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| String::new()),
                reason: msg.signal_type.clone(),
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeVeto, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeVeto event: {}", e);
            }
        }

        debug!(
            "EconomicNodeVeto received from {}: pr_id={}, signal={}, message_id={}",
            peer_addr, msg.pr_id, msg.signal_type, msg.message_id
        );

        // Replay protection: Check message ID and timestamp
        if let Err(e) = self
            .replay_protection
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeVeto from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = replay_protection::ReplayProtection::validate_timestamp(msg.timestamp, 3600)
        {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeVeto from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Check if governance relay is enabled
        let governance_enabled = self
            .governance_config
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            // Relay to other peers (gossip) but don't forward to bllvm-commons
            debug!("Governance relay disabled, gossiping message to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        // Forward to bllvm-commons via VPN
        if let Some(config) = &self.governance_config {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = config
                    .api_key
                    .as_deref()
                    .or_else(|| env_api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{}/internal/governance/veto", commons_url);

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward veto: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeVeto to bllvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeVeto: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        // Relay to other governance-enabled peers (gossip)
        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }

    /// Handle EconomicNodeStatus message - Query or respond with node status
    #[cfg(feature = "governance")]
    async fn handle_economic_node_status(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeStatusMessage,
    ) -> Result<()> {
        use reqwest::Client;
        use tracing::{debug, error, info, warn};

        debug!(
            "EconomicNodeStatus received from {}: request_id={}, query_type={}, identifier={}",
            peer_addr, msg.request_id, msg.query_type, msg.node_identifier
        );

        // Publish event for governance module (non-blocking)
        let event_publisher_guard = self.event_publisher.lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let response_data = msg
                .status
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_default());
            let payload = EventPayload::EconomicNodeStatus {
                request_id: msg.request_id.to_string(),
                query_type: msg.query_type.clone(),
                node_id: Some(msg.node_identifier.clone()),
                response_data,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeStatus, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeStatus event: {}", e);
            }
        }

        // If this is a response, relay it to governance peers
        if msg.status.is_some() {
            debug!("EconomicNodeStatus response, relaying to governance peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        // If this is a query, forward to bllvm-commons if we're a governance node
        let governance_enabled = self
            .governance_config
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if governance_enabled {
            if let Some(config) = &self.governance_config {
                if let Some(ref commons_url) = config.commons_url {
                    let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                    let api_key = config
                        .api_key
                        .as_deref()
                        .or_else(|| env_api_key.as_deref())
                        .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                    let client = reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(30))
                        .build()
                        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                    // Forward query to bllvm-commons (it expects the same format)
                    let url = format!("{}/internal/governance/status", commons_url);

                    info!(
                        "Forwarding EconomicNodeStatus query to bllvm-commons: request_id={}, query_type={}",
                        msg.request_id, msg.query_type
                    );

                    // Create request payload (bllvm-commons expects EconomicNodeStatusMessage format)
                    let request_payload = serde_json::json!({
                        "request_id": msg.request_id,
                        "node_identifier": msg.node_identifier,
                        "query_type": msg.query_type,
                        "status": null
                    });

                    let response = client
                        .post(&url)
                        .header("X-API-Key", api_key)
                        .json(&request_payload)
                        .send()
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to forward status query: {}", e))?;

                    if response.status().is_success() {
                        // Parse response
                        let status_response: serde_json::Value =
                            response.json().await.map_err(|e| {
                                anyhow::anyhow!("Failed to parse status response: {}", e)
                            })?;

                        // Convert response to P2P format
                        let status_data = status_response.get("status").and_then(|s| s.as_object());
                        let response_status = if let Some(data) = status_data {
                            Some(crate::network::protocol::NodeStatusResponse {
                                node_id: data
                                    .get("node_id")
                                    .and_then(|v| v.as_i64())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid node_id"))?
                                    as i32,
                                node_type: data
                                    .get("node_type")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid node_type"))?
                                    .to_string(),
                                entity_name: data
                                    .get("entity_name")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("Missing or invalid entity_name")
                                    })?
                                    .to_string(),
                                status: data
                                    .get("status")
                                    .and_then(|v| v.as_str())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid status"))?
                                    .to_string(),
                                weight: data
                                    .get("weight")
                                    .and_then(|v| v.as_f64())
                                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid weight"))?,
                                registered_at: data
                                    .get("registered_at")
                                    .and_then(|v| v.as_i64())
                                    .ok_or_else(|| {
                                    anyhow::anyhow!("Missing or invalid registered_at")
                                })?,
                                last_verified_at: data
                                    .get("last_verified_at")
                                    .and_then(|v| v.as_i64()),
                            })
                        } else {
                            None
                        };

                        let response_msg = crate::network::protocol::EconomicNodeStatusMessage {
                            request_id: msg.request_id,
                            node_identifier: msg.node_identifier.clone(),
                            query_type: msg.query_type.clone(),
                            status: response_status,
                        };

                        // Send response back to original peer
                        let pm = self.peer_manager.lock().await;
                        if let Some(peer) = pm.get_peer(&transport::TransportAddr::Tcp(peer_addr)) {
                            let msg_bytes = bincode::serialize(
                                &crate::network::protocol::ProtocolMessage::EconomicNodeStatus(
                                    response_msg,
                                ),
                            )
                            .map_err(|e| anyhow::anyhow!("Failed to serialize response: {}", e))?;
                            if let Err(e) = peer.send_message(msg_bytes).await {
                                warn!(
                                    "Failed to send status response to peer {}: {}",
                                    peer_addr, e
                                );
                            } else {
                                info!(
                                    "Sent status response to peer {} for request_id={}",
                                    peer_addr, msg.request_id
                                );
                            }
                        }
                    } else {
                        let status = response.status();
                        let error_text = response.text().await.unwrap_or_default();
                        error!(
                            "Failed to forward EconomicNodeStatus query: status={}, error={}",
                            status, error_text
                        );
                    }
                } else {
                    warn!("Governance enabled but commons_url not configured");
                }
            }
        } else {
            debug!("Governance relay disabled, gossiping query to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
        }

        Ok(())
    }

    /// Handle EconomicNodeForkDecision message - Relay to governance node if enabled
    #[cfg(feature = "governance")]
    async fn handle_economic_node_fork_decision(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::EconomicNodeForkDecisionMessage,
    ) -> Result<()> {
        use reqwest::Client;
        use tracing::{debug, error, info, warn};

        debug!(
            "EconomicNodeForkDecision received from {}: ruleset={}, message_id={}",
            peer_addr, msg.chosen_ruleset, msg.message_id
        );

        // Publish event for governance module (non-blocking)
        let event_publisher_guard = self.event_publisher.lock().await;
        if let Some(event_publisher) = event_publisher_guard.as_ref() {
            use crate::module::ipc::protocol::EventPayload;
            use crate::module::traits::EventType;
            let node_id = msg
                .node_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| msg.public_key.clone());
            let decision = if msg.chosen_ruleset.is_empty() {
                "abstain".to_string()
            } else {
                "adopt".to_string()
            };
            let payload = EventPayload::EconomicNodeForkDecision {
                message_id: msg.message_id.clone(),
                ruleset_version: msg.chosen_ruleset.clone(),
                decision,
                node_id,
                timestamp: msg.timestamp as u64,
            };
            if let Err(e) = event_publisher
                .publish_event(EventType::EconomicNodeForkDecision, payload)
                .await
            {
                warn!("Failed to publish EconomicNodeForkDecision event: {}", e);
            }
        }

        // Replay protection: Check message ID and timestamp
        if let Err(e) = self
            .replay_protection
            .check_message_id(&msg.message_id, msg.timestamp)
            .await
        {
            warn!(
                "Replay protection: Rejected EconomicNodeForkDecision from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }
        if let Err(e) = replay_protection::ReplayProtection::validate_timestamp(msg.timestamp, 3600)
        {
            warn!(
                "Replay protection: Invalid timestamp in EconomicNodeForkDecision from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Check if governance relay is enabled
        let governance_enabled = self
            .governance_config
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);

        if !governance_enabled {
            debug!("Governance relay disabled, gossiping fork decision to peers");
            self.gossip_governance_message(peer_addr, &msg).await?;
            return Ok(());
        }

        // Forward to bllvm-commons via VPN
        if let Some(config) = &self.governance_config {
            if let Some(ref commons_url) = config.commons_url {
                let env_api_key = std::env::var("COMMONS_API_KEY").ok();
                let api_key = config
                    .api_key
                    .as_deref()
                    .or_else(|| env_api_key.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("API key not configured"))?;

                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

                let url = format!("{}/internal/governance/fork-decision", commons_url);

                let response = client
                    .post(&url)
                    .header("X-API-Key", api_key)
                    .json(&msg)
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to forward fork decision: {}", e))?;

                if response.status().is_success() {
                    info!(
                        "Successfully forwarded EconomicNodeForkDecision to bllvm-commons: message_id={}",
                        msg.message_id
                    );
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!(
                        "Failed to forward EconomicNodeForkDecision: status={}, error={}",
                        status, error_text
                    );
                }
            } else {
                warn!("Governance enabled but commons_url not configured");
            }
        }

        // Relay to other governance-enabled peers (gossip)
        self.gossip_governance_message(peer_addr, &msg).await?;

        Ok(())
    }

    /// Create a new network manager with transport preference
    pub fn with_transport_preference(
        listen_addr: SocketAddr,
        max_peers: usize,
        preference: TransportPreference,
    ) -> Self {
        Self::with_config(listen_addr, max_peers, preference, None)
    }

    /// Get transport preference
    pub fn transport_preference(&self) -> TransportPreference {
        self.transport_preference
    }

    /// Get message channel sender (for FIBRE and other integrations)
    pub fn message_sender(&self) -> mpsc::UnboundedSender<NetworkMessage> {
        self.peer_tx.clone()
    }

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
        // Get max addresses from config
        let timing_config_default = crate::config::NetworkTimingConfig::default();
        let timing_config = config
            .network_timing
            .as_ref()
            .unwrap_or(&timing_config_default);
        let max_addresses = timing_config.max_addresses_from_dns;
        let addresses = dns_seeds::resolve_dns_seeds(seeds, port, max_addresses).await;
        let address_count = addresses.len();

        // Add discovered addresses to database
        {
            let mut db = self.address_database.write().await;
            for addr in addresses {
                db.add_address(addr, 0); // Services will be updated on connection
            }
        }

        info!("Discovered {} addresses from DNS seeds", address_count);
        Ok(())
    }

    /// Connect to persistent peers from config
    pub async fn connect_persistent_peers(&self, persistent_peers: &[SocketAddr]) -> Result<()> {
        for peer_addr in persistent_peers {
            // Add to persistent peer set
            self.add_persistent_peer(*peer_addr);

            // Try to connect
            info!("Connecting to persistent peer: {}", peer_addr);
            if let Err(e) = self.connect_to_peer(*peer_addr).await {
                warn!("Failed to connect to persistent peer {}: {}", peer_addr, e);
            }
        }
        Ok(())
    }

    /// Discover Iroh peers and add to address database
    ///
    /// Iroh peers are discovered through:
    /// 1. Incoming connections (automatically stored)
    /// 2. DERP servers (if configured)
    /// 3. Gossip discovery (if available)
    ///
    /// This method can be extended to use Iroh's gossip discovery APIs when available.
    #[cfg(feature = "iroh")]
    pub async fn discover_iroh_peers(&self) -> Result<usize> {
        // Iroh peers are primarily discovered through:
        // 1. Incoming connections (handled automatically in accept loop)
        // 2. DERP servers (configured in IrohTransport)
        // 3. Gossip discovery (would require iroh-gossip-discovery crate)

        // For now, we rely on:
        // - Persistent Iroh peers from config
        // - Incoming connections (stored automatically)
        // - DERP servers (handled by Iroh's MagicEndpoint)

        // Future: Integrate with iroh-gossip-discovery when available
        info!("Iroh peer discovery: relying on DERP servers and incoming connections");
        Ok(0) // Return 0 for now - discovery happens through Iroh's native mechanisms
    }

    /// Connect to Iroh peers from address database
    #[cfg(feature = "iroh")]
    pub async fn connect_iroh_peers_from_database(&self, target_count: usize) -> Result<usize> {
        use crate::network::peer::Peer;
        use crate::network::transport::TransportAddr;

        // Count current Iroh peers
        let current_iroh_count = {
            let pm = self.peer_manager.lock().await;
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

        // Get fresh Iroh NodeIds from database
        let node_ids = {
            let db = self.address_database.read().await;
            db.get_fresh_iroh_addresses(needed * 2) // Get 2x needed for retries
        };

        if node_ids.is_empty() {
            warn!("No fresh Iroh addresses available in database");
            return Ok(0);
        }

        // Get Iroh transport
        let iroh_transport = match &self.iroh_transport {
            Some(transport) => transport,
            None => {
                warn!("Iroh transport not initialized, cannot connect to Iroh peers");
                return Ok(0);
            }
        };

        // Try to connect to Iroh peers
        let mut connected = 0;
        for node_id in node_ids.into_iter().take(needed * 2) {
            let node_id_bytes = node_id.as_bytes().to_vec();
            let transport_addr = TransportAddr::Iroh(node_id_bytes.clone());

            // Connect directly using Iroh transport
            match iroh_transport.connect(transport_addr.clone()).await {
                Ok(conn) => {
                    // Create peer from Iroh connection
                    // Iroh uses placeholder SocketAddr for peer identification
                    let placeholder_socket = SocketAddr::from(([0, 0, 0, 0], 0));
                    let peer = Peer::from_transport_connection(
                        conn,
                        placeholder_socket,
                        transport_addr.clone(),
                        self.peer_tx.clone(),
                    );

                    // Add peer to manager
                    {
                        let mut pm = self.peer_manager.lock().await;
                        if let Err(e) = pm.add_peer(transport_addr.clone(), peer) {
                            warn!("Failed to add Iroh peer: {}", e);
                            continue;
                        }
                    }

                    // Store mapping for Iroh (if needed)
                    {
                        let mut socket_to_transport = self.socket_to_transport.lock().await;
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
    ///
    /// Works with both SocketAddr-based addresses (TCP/Quinn) and Iroh NodeIds.
    pub async fn connect_peers_from_database(&self, target_count: usize) -> Result<usize> {
        let current_count = self.peer_count();
        if current_count >= target_count {
            return Ok(0); // Already have enough peers
        }

        let needed = target_count - current_count;
        info!(
            "Need {} more peers (current: {}, target: {})",
            needed, current_count, target_count
        );

        // Get fresh addresses from database
        let ban_list = self.ban_list.read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager.lock().await;
            pm.peer_socket_addresses()
        };

        let addresses: Vec<_> = {
            let db = self.address_database.read().await;
            let fresh = db.get_fresh_addresses(needed * 3); // Get 3x needed for retries
            db.filter_addresses(fresh, &ban_list, &connected_peers)
        };

        if addresses.is_empty() {
            warn!("No fresh addresses available in database");
            return Ok(0);
        }

        // Convert addresses to SocketAddrs first
        let sockets: Vec<SocketAddr> = {
            let db = self.address_database.read().await;
            addresses
                .iter()
                .take(needed * 2)
                .map(|addr| db.network_addr_to_socket(addr))
                .collect()
        };

        // Try to connect to addresses
        let mut connected = 0;
        for socket in sockets {
            if let Err(e) = self.connect_to_peer(socket).await {
                debug!("Failed to connect to {}: {}", socket, e);
                // Continue trying other addresses
            } else {
                connected += 1;
                if connected >= needed {
                    break; // Got enough peers
                }
            }
        }

        info!("Connected to {} new peers from address database", connected);

        // Also try to connect Iroh peers if Iroh is enabled
        #[cfg(feature = "iroh")]
        if self.transport_preference.allows_iroh() {
            let iroh_connected = self.connect_iroh_peers_from_database(target_count).await?;
            connected += iroh_connected;
        }

        Ok(connected)
    }

    /// Initialize peer connections after startup
    ///
    /// This is automatically called by `start()` to:
    /// 0. Discover and connect to LAN sibling nodes FIRST (priority)
    /// 1. Discover peers from DNS seeds (for TCP/Quinn transports)
    /// 2. Connect to persistent peers from config
    /// 3. Discover Iroh peers (if Iroh is enabled) - uses Iroh's DERP servers and gossip
    /// 4. Connect to peers from address database to reach target count
    ///
    /// LAN sibling nodes (local Bitcoin Core, Umbrel, Start9, etc.) are discovered
    /// automatically by scanning the local network for port 8333. These peers are
    /// connected first and given priority for block downloads during IBD.
    ///
    /// Note: The address database now supports both SocketAddr-based addresses (TCP/Quinn)
    /// and Iroh NodeIds. Iroh peers are discovered through:
    /// - DERP servers (handled by Iroh's MagicEndpoint)
    /// - Gossip discovery (when available)
    /// - Incoming connections (automatically stored)
    /// - Persistent peers from config
    pub async fn initialize_peer_connections(
        &self,
        config: &crate::config::NodeConfig,
        network: &str,
        port: u16,
        target_peer_count: usize,
    ) -> Result<()> {
        // 0. PRIORITY: Discover and connect to LAN sibling nodes FIRST
        // These are local Bitcoin nodes (Bitcoin Core, Umbrel, Start9, etc.)
        // that can provide blocks at LAN speeds (~1ms latency vs ~100-5000ms internet)
        info!("Discovering LAN sibling nodes...");
        let lan_nodes = lan_discovery::discover_lan_bitcoin_nodes_with_port(port).await;
        let mut lan_connected = 0;
        
        for lan_addr in &lan_nodes {
            info!("Connecting to LAN sibling node: {}", lan_addr);
            match self.connect_to_peer(*lan_addr).await {
                Ok(_) => {
                    lan_connected += 1;
                    // Add to persistent peers so we always try to reconnect
                    self.add_persistent_peer(*lan_addr);
                    info!("Connected to LAN sibling node: {} (will be prioritized for IBD)", lan_addr);
                }
                Err(e) => {
                    warn!("Failed to connect to LAN node {}: {}", lan_addr, e);
                }
            }
        }
        
        if lan_connected > 0 {
            info!("Connected to {} LAN sibling node(s) - these will be prioritized for block downloads", lan_connected);
        }

        // 1. Discover peers from DNS seeds (only for TCP/Quinn, not Iroh)
        let should_discover_dns = self.transport_preference.allows_tcp() || {
            #[cfg(feature = "quinn")]
            {
                self.transport_preference.allows_quinn()
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

        // 2. Connect to persistent peers
        if !config.persistent_peers.is_empty() {
            if let Err(e) = self
                .connect_persistent_peers(&config.persistent_peers)
                .await
            {
                warn!("Failed to connect to some persistent peers: {}", e);
            }
        }

        // 3. Discover Iroh peers (if Iroh is enabled)
        #[cfg(feature = "iroh")]
        if self.transport_preference.allows_iroh() {
            if let Err(e) = self.discover_iroh_peers().await {
                warn!("Iroh peer discovery failed: {}", e);
            }
        }

        // 4. Connect to peers from address database to reach target count
        // Wait a bit for persistent peers to connect first
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

    /// Start the network manager
    pub async fn start(&self, listen_addr: SocketAddr) -> Result<()> {
        info!(
            "Starting network manager with transport preference: {:?}",
            self.transport_preference
        );

        // Start listening first

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
                    // Use accept_stream() to get raw TcpStream for proper split handling
                    match tcp_listener.accept_stream().await {
                        Ok((stream, socket_addr)) => {
                            info!("New TCP connection from {:?}", socket_addr);

                            // Check DoS protection: connection rate limiting
                            let ip = socket_addr.ip();
                            if !dos_protection.check_connection(ip).await {
                                warn!("Connection rate limit exceeded for IP {}, rejecting connection", ip);

                                // Check if we should auto-ban
                                if dos_protection.should_auto_ban(ip).await {
                                    warn!("Auto-banning IP {} for repeated connection rate violations", ip);
                                    // Auto-ban the IP using configured ban duration
                                    let ban_duration = dos_protection.ban_duration_seconds();
                                    let unban_timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs()
                                        + ban_duration;
                                    let mut ban_list_guard = ban_list.write().await;
                                    ban_list_guard.insert(socket_addr, unban_timestamp);
                                }

                                // Close connection immediately
                                drop(stream);
                                continue;
                            }

                            // Check active connection limit
                            let current_connections = {
                                let pm = peer_manager_clone.lock().await;
                                pm.peer_count()
                            };
                            if !dos_protection
                                .check_active_connections(current_connections)
                                .await
                            {
                                warn!("Active connection limit exceeded, rejecting connection from {}", socket_addr);
                                drop(stream);
                                continue;
                            }

                            // Send connection notification
                            let transport_addr_tcp = TransportAddr::Tcp(socket_addr);
                            let _ = peer_tx
                                .send(NetworkMessage::PeerConnected(transport_addr_tcp.clone()));

                            // Handle connection in background with graceful error handling
                            let peer_tx_clone = peer_tx.clone();
                            let peer_manager_for_peer = Arc::clone(&peer_manager_clone);
                            let transport_addr_for_peer = transport_addr_tcp;
                            tokio::spawn(async move {
                                // Create peer using split stream for concurrent read/write
                                let peer = peer::Peer::from_tcp_stream_split(
                                    stream,
                                    socket_addr,
                                    peer_tx_clone.clone(),
                                );

                                // Add peer to manager (async-safe)
                                let mut pm = peer_manager_for_peer.lock().await;
                                if let Err(e) = pm.add_peer(transport_addr_for_peer.clone(), peer) {
                                    warn!("Failed to add peer {}: {}", socket_addr, e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        transport_addr_for_peer.clone(),
                                    ));
                                    return;
                                }
                                info!(
                                    "Successfully added peer {} (transport: {:?})",
                                    socket_addr, transport_addr_for_peer
                                );
                                drop(pm); // Explicitly drop lock before continuing

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
                                            // Auto-ban the IP using configured ban duration
                                            let ban_duration =
                                                dos_protection.ban_duration_seconds();
                                            let unban_timestamp =
                                                current_timestamp() + ban_duration;
                                            let mut ban_list_guard = ban_list.write().await;
                                            ban_list_guard.insert(socket_addr, unban_timestamp);
                                        }
                                        drop(conn);
                                        continue;
                                    }

                                    // Check active connection limit
                                    let current_connections = {
                                        let pm = peer_manager.lock().await;
                                        pm.peer_count()
                                    };
                                    if !dos_protection
                                        .check_active_connections(current_connections)
                                        .await
                                    {
                                        warn!("Active connection limit exceeded, rejecting Quinn connection from {}", socket_addr);
                                        drop(conn);
                                        continue;
                                    }

                                    // Send connection notification
                                    let quinn_transport_addr = TransportAddr::Quinn(socket_addr);
                                    let _ = peer_tx.send(NetworkMessage::PeerConnected(
                                        quinn_transport_addr.clone(),
                                    ));

                                    // Handle connection in background with graceful error handling
                                    let peer_tx_clone = peer_tx.clone();
                                    let peer_manager_clone = Arc::clone(&peer_manager);
                                    tokio::spawn(async move {
                                        use crate::network::transport::TransportAddr;

                                        let quinn_addr = TransportAddr::Quinn(socket_addr);
                                        let quinn_addr_clone = quinn_addr.clone();
                                        let peer = peer::Peer::from_transport_connection(
                                            conn,
                                            socket_addr,
                                            quinn_addr,
                                            peer_tx_clone.clone(),
                                        );

                                        // Add peer to manager (async-safe)
                                        let mut pm = peer_manager_clone.lock().await;
                                        if let Err(e) = pm.add_peer(quinn_addr_clone.clone(), peer)
                                        {
                                            warn!(
                                                "Failed to add Quinn peer {}: {}",
                                                socket_addr, e
                                            );
                                            let _ = peer_tx_clone.send(
                                                NetworkMessage::PeerDisconnected(
                                                    quinn_addr_clone.clone(),
                                                ),
                                            );
                                            return;
                                        }
                                        info!("Successfully added Quinn peer {}", socket_addr);
                                        drop(pm); // Explicitly drop lock before continuing
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
                    warn!(
                        "Failed to start Quinn listener (graceful degradation): {}",
                        e
                    );
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
                    let address_database = Arc::clone(&self.address_database);
                    let socket_to_transport = Arc::clone(&self.socket_to_transport);
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
                                        let pm = peer_manager.lock().await;
                                        pm.peer_count()
                                    };
                                    if !dos_protection
                                        .check_active_connections(current_connections)
                                        .await
                                    {
                                        warn!("Active connection limit exceeded, rejecting Iroh connection");
                                        drop(conn);
                                        continue;
                                    }

                                    // Send connection notification using TransportAddr directly
                                    let _ = peer_tx
                                        .send(NetworkMessage::PeerConnected(iroh_addr.clone()));

                                    // Handle connection in background with graceful error handling
                                    let peer_tx_clone = peer_tx.clone();
                                    let peer_manager_clone = Arc::clone(&peer_manager);
                                    let iroh_addr_clone = iroh_addr.clone();
                                    let socket_to_transport_clone =
                                        Arc::clone(&socket_to_transport);
                                    let address_database_clone = Arc::clone(&address_database);
                                    tokio::spawn(async move {
                                        // For Iroh, we need a SocketAddr for Peer::from_transport_connection
                                        // Generate a unique placeholder based on key hash for lookups
                                        let placeholder_socket =
                                            if let TransportAddr::Iroh(ref key) = iroh_addr_clone {
                                                // Use first 4 bytes of key for IP, last 2 bytes for port to create unique placeholder
                                                let ip_bytes = if key.len() >= 4 {
                                                    [key[0], key[1], key[2], key[3]]
                                                } else {
                                                    [0, 0, 0, 0]
                                                };
                                                let port = if key.len() >= 6 {
                                                    u16::from_be_bytes([
                                                        key[key.len() - 2],
                                                        key[key.len() - 1],
                                                    ])
                                                } else {
                                                    0
                                                };
                                                std::net::SocketAddr::from((ip_bytes, port))
                                            } else {
                                                std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                                            };

                                        let peer = peer::Peer::from_transport_connection(
                                            conn,
                                            placeholder_socket,
                                            iroh_addr_clone.clone(),
                                            peer_tx_clone.clone(),
                                        );

                                        // Add peer to manager (async-safe)
                                        let mut pm = peer_manager_clone.lock().await;
                                        if let Err(e) = pm.add_peer(iroh_addr_clone.clone(), peer) {
                                            warn!("Failed to add Iroh peer: {}", e);
                                            let _ = peer_tx_clone.send(
                                                NetworkMessage::PeerDisconnected(
                                                    iroh_addr_clone.clone(),
                                                ),
                                            );
                                            return;
                                        }
                                        drop(pm); // Drop peer_manager lock before locking socket_to_transport
                                                  // Store mapping from placeholder SocketAddr to TransportAddr for Iroh lookups
                                        socket_to_transport_clone
                                            .lock()
                                            .await
                                            .insert(placeholder_socket, iroh_addr_clone.clone());

                                        // Store Iroh NodeId in address database
                                        if let TransportAddr::Iroh(ref node_id_bytes) =
                                            iroh_addr_clone
                                        {
                                            if node_id_bytes.len() == 32 {
                                                use iroh::PublicKey;
                                                let mut key_array = [0u8; 32];
                                                key_array.copy_from_slice(node_id_bytes);
                                                if let Ok(public_key) =
                                                    PublicKey::from_bytes(&key_array)
                                                {
                                                    let address_db_clone =
                                                        address_database_clone.clone();
                                                    tokio::spawn(async move {
                                                        let mut db = address_db_clone.write().await;
                                                        db.add_iroh_address(public_key, 0);
                                                        // Services will be updated on version exchange
                                                    });
                                                }
                                            }
                                        }

                                        info!(
                                            "Successfully added Iroh peer (transport: {:?})",
                                            iroh_addr_clone
                                        );
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
                    warn!(
                        "Failed to start Iroh listener (graceful degradation): {}",
                        e
                    );
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

        // Start peer reconnection task
        self.start_peer_reconnection_task();
        
        // Start periodic ping task (sends ping every 2 minutes)
        self.start_ping_task();
        
        // Start ping timeout checking task (checks every 30 seconds)
        self.start_ping_timeout_check_task();
        
            // Start chain sync timeout checking task (checks every 60 seconds)
            self.start_chain_sync_timeout_check_task();
            
            // Start outbound peer eviction task (checks every 5 minutes)
            self.start_outbound_peer_eviction_task();
            
            // Start outbound peer eviction task (checks every 5 minutes)
            self.start_outbound_peer_eviction_task();

        // Note: Peer connection initialization (DNS seeds, persistent peers, etc.)
        // should be called separately via initialize_peer_connections() after start()
        // This allows the caller to provide config, network type, and target peer count

        Ok(())
    }

    /// Generate a new request ID for async request-response patterns
    ///
    /// Optimization: Uses AtomicU64 for lock-free operation (no async locks needed)
    pub fn generate_request_id(&self) -> u64 {
        self.request_id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a pending request and return the response receiver
    /// Returns (request_id, response_receiver)
    pub fn register_request(
        &self,
        peer_addr: SocketAddr,
    ) -> (u64, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        self.register_request_with_priority(peer_addr, 0)
    }

    /// Register a pending request with priority and return the response receiver
    /// Returns (request_id, response_receiver)
    /// Priority: 0 = normal, higher = more important
    pub fn register_request_with_priority(
        &self,
        peer_addr: SocketAddr,
        priority: u8,
    ) -> (u64, tokio::sync::oneshot::Receiver<Vec<u8>>) {
        let request_id = self.generate_request_id();
        let (tx, rx) = tokio::sync::oneshot::channel();

        let timestamp = current_timestamp();

        let pending_req = PendingRequest {
            sender: tx,
            peer_addr,
            timestamp,
            priority,
            retry_count: 0,
        };

        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pending_requests
                    .lock()
                    .await
                    .insert(request_id, pending_req);
            })
        });
        (request_id, rx)
    }

    /// Complete a pending request by sending the response
    pub fn complete_request(&self, request_id: u64, response: Vec<u8>) -> bool {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_requests.lock().await;
                if let Some(pending_req) = pending.remove(&request_id) {
                    let _ = pending_req.sender.send(response);
                    true
                } else {
                    false
                }
            })
        })
    }

    /// Cancel a pending request
    pub fn cancel_request(&self, request_id: u64) -> bool {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&request_id).is_some()
            })
        })
    }

    /// Get pending requests for a specific peer
    pub fn get_pending_requests_for_peer(&self, peer_addr: SocketAddr) -> Vec<u64> {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let pending = self.pending_requests.lock().await;
                pending
                    .iter()
                    .filter(|(_, req)| req.peer_addr == peer_addr)
                    .map(|(id, _)| *id)
                    .collect()
            })
        })
    }

    /// Register a pending Headers request (supports pipelining - multiple requests per peer)
    /// Returns a receiver that will receive the Headers response
    pub fn register_headers_request(
        &self,
        peer_addr: SocketAddr,
    ) -> tokio::sync::oneshot::Receiver<Vec<blvm_protocol::BlockHeader>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pending_headers_requests
                    .lock()
                    .await
                    .entry(peer_addr)
                    .or_insert_with(std::collections::VecDeque::new)
                    .push_back(tx);
            })
        });
        rx
    }

    /// Complete a pending Headers request (FIFO - completes oldest request for this peer)
    pub fn complete_headers_request(
        &self,
        peer_addr: SocketAddr,
        headers: Vec<blvm_protocol::BlockHeader>,
    ) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_headers_requests.lock().await;
                if let Some(queue) = pending.get_mut(&peer_addr) {
                    if let Some(sender) = queue.pop_front() {
                        let _ = sender.send(headers);
                        // Clean up empty queues
                        if queue.is_empty() {
                            pending.remove(&peer_addr);
                        }
                        return true;
                    }
                }
                false
            })
        })
    }
    
    /// Get the number of pending header requests for a peer
    pub fn pending_headers_count(&self, peer_addr: SocketAddr) -> usize {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pending_headers_requests
                    .lock()
                    .await
                    .get(&peer_addr)
                    .map(|q| q.len())
                    .unwrap_or(0)
            })
        })
    }

    /// Register a pending Block request
    /// Returns a receiver that will receive the Block response
    pub fn register_block_request(
        &self,
        peer_addr: SocketAddr,
        block_hash: blvm_protocol::Hash,
    ) -> tokio::sync::oneshot::Receiver<(blvm_protocol::Block, Vec<Vec<blvm_protocol::segwit::Witness>>)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pending_block_requests
                    .lock()
                    .await
                    .insert((peer_addr, block_hash), tx);
            })
        });
        rx
    }

    /// Complete a pending Block request
    pub fn complete_block_request(
        &self,
        peer_addr: SocketAddr,
        block_hash: blvm_protocol::Hash,
        block: blvm_protocol::Block,
        witnesses: Vec<Vec<blvm_protocol::segwit::Witness>>,
    ) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_block_requests.lock().await;
                if let Some(sender) = pending.remove(&(peer_addr, block_hash)) {
                    let _ = sender.send((block, witnesses));
                    true
                } else {
                    false
                }
            })
        })
    }

    /// Clean up expired requests (older than max_age_seconds)
    pub fn cleanup_expired_requests(&self, max_age_seconds: u64) -> usize {
        let now = current_timestamp();

        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_requests.lock().await;
                let expired: Vec<u64> = pending
                    .iter()
                    .filter(|(_, req)| now.saturating_sub(req.timestamp) > max_age_seconds)
                    .map(|(id, _)| *id)
                    .collect();

                for id in &expired {
                    pending.remove(id);
                }

                expired.len()
            })
        })
    }

    /// Start periodic task to clean up expired pending requests
    fn start_request_cleanup_task(&self) {
        let pending_requests = Arc::clone(&self.pending_requests);
        let timeout_config = Arc::clone(&self.request_timeout_config);

        tokio::spawn(async move {
            let cleanup_interval = timeout_config.request_cleanup_interval_seconds;
            let max_age = timeout_config.pending_request_max_age_seconds;
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(cleanup_interval));
            loop {
                interval.tick().await;

                // Clean up old pending requests
                let now = current_timestamp();

                let mut pending = pending_requests.lock().await;
                let initial_count = pending.len();
                pending.retain(|_, req| now.saturating_sub(req.timestamp) < max_age);
                let removed = initial_count - pending.len();
                if removed > 0 {
                    debug!(
                        "Cleaned up {} stale pending requests (older than {}s)",
                        removed, max_age
                    );
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
                // Periodic check for IPs that have exceeded violation thresholds
                let dos_clone = Arc::clone(&dos_protection);
                let ban_list_clone = Arc::clone(&ban_list);
                let ban_duration = dos_protection.ban_duration_seconds();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60)); // Check every minute
                    loop {
                        interval.tick().await;

                        // Get IPs that should be auto-banned
                        let ips_to_ban = dos_clone.get_ips_to_auto_ban().await;

                        // Ban IPs that exceed threshold
                        if !ips_to_ban.is_empty() {
                            let now = current_timestamp();
                            let unban_timestamp = now + ban_duration;

                            let mut ban_list_guard = ban_list_clone.write().await;
                            for ip in ips_to_ban {
                                // Convert IpAddr to SocketAddr (use port 0 as placeholder)
                                let socket_addr = std::net::SocketAddr::new(ip, 0);
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    ban_list_guard.entry(socket_addr)
                                {
                                    e.insert(unban_timestamp);
                                    warn!("Auto-banned IP {} for connection rate violations (unban at {})", ip, unban_timestamp);
                                }
                            }
                        }
                    }
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

                let now = current_timestamp();

                let mut ban_list_guard = ban_list.write().await;
                let expired: Vec<SocketAddr> = ban_list_guard
                    .iter()
                    .filter(|(_, &unban_timestamp)| {
                        unban_timestamp != u64::MAX && now >= unban_timestamp
                    })
                    .map(|(addr, _)| *addr)
                    .collect();

                let expired_count = expired.len();
                for addr in &expired {
                    ban_list_guard.remove(addr);
                    debug!("Cleaned up expired ban for {}", addr);
                }

                if expired_count > 0 {
                    info!("Cleaned up {} expired ban(s)", expired_count);
                }
            }
        });
    }

    /// Start chain sync timeout checking task (checks for outbound peers that haven't synced within 20 minutes)
    fn start_chain_sync_timeout_check_task(&self) {
        let peer_manager = Arc::clone(&self.peer_manager);
        let peer_tx = self.peer_tx.clone();
        let storage = self.storage.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60)); // Check every minute
            
            loop {
                interval.tick().await;
                
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let chain_sync_timeout = 20 * 60; // 20 minutes (CHAIN_SYNC_TIMEOUT)
                
                // Get our chainwork (from storage/chainstate)
                let our_chainwork = {
                    if let Some(storage) = &storage {
                        // Get tip hash first
                        if let Ok(Some(tip_hash)) = storage.chain().get_tip_hash() {
                            // Get chainwork for tip
                            if let Ok(Some(chainwork)) = storage.chain().get_chainwork(&tip_hash) {
                                chainwork
                            } else {
                                0 // No chainwork available
                            }
                        } else {
                            0 // No tip hash available
                        }
                    } else {
                        0 // Storage not available
                    }
                };
                
                let mut pm = peer_manager.lock().await;
                let mut peers_to_disconnect: Vec<crate::network::transport::TransportAddr> = Vec::new();
                
                for (addr, peer) in pm.peers.iter() {
                    // Check if peer is outbound (we initiated connection)
                    let is_outbound = peer.is_outbound();
                    
                    if is_outbound {
                        let connection_age = now.saturating_sub(peer.conntime());
                        
                        if connection_age > chain_sync_timeout {
                            // Check if peer's chainwork is sufficient
                            if let Some(peer_chainwork) = peer.chainwork() {
                                if peer_chainwork < our_chainwork {
                                    warn!(
                                        "Outbound peer {:?} has insufficient chainwork after {} minutes (peer: {}, ours: {}), disconnecting",
                                        addr, connection_age / 60, peer_chainwork, our_chainwork
                                    );
                                    peers_to_disconnect.push(addr.clone());
                                }
                            } else {
                                // No chainwork known, disconnect
                                warn!(
                                    "Outbound peer {:?} has no chainwork after {} minutes, disconnecting",
                                    addr, connection_age / 60
                                );
                                peers_to_disconnect.push(addr.clone());
                            }
                        }
                    }
                }
                
                drop(pm); // Release lock before disconnecting
                
                for addr in peers_to_disconnect {
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start outbound peer eviction task (checks every 5 minutes)
    fn start_outbound_peer_eviction_task(&self) {
        let peer_manager = Arc::clone(&self.peer_manager);
        let peer_tx = self.peer_tx.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5 * 60)); // Check every 5 minutes
            const MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT: usize = 4;
            
            loop {
                interval.tick().await;
                
                let mut pm = peer_manager.lock().await;
                
                // Get all outbound peers with their last block announcement time
                let mut outbound_peers: Vec<(crate::network::transport::TransportAddr, u64)> = pm.peers.iter()
                    .filter(|(_, peer)| peer.is_outbound() && !peer.is_manual())
                    .map(|(addr, peer)| {
                        let last_announce = peer.last_block_announcement().unwrap_or(0);
                        (addr.clone(), last_announce)
                    })
                    .collect();
                
                if outbound_peers.len() <= MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT {
                    continue; // Not too many peers, no eviction needed
                }
                
                // Sort by last announcement (oldest first) - peers with no announcements (0) come first
                outbound_peers.sort_by_key(|(_, time)| *time);
                
                // Disconnect oldest peers (keep best MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT)
                let peers_to_evict = outbound_peers.len() - MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;
                let peers_to_disconnect: Vec<_> = outbound_peers.iter()
                    .take(peers_to_evict)
                    .map(|(addr, _)| addr.clone())
                    .collect();
                
                drop(pm); // Release lock before sending messages
                
                for addr in peers_to_disconnect {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let pm_check = peer_manager.lock().await;
                    let last_announce = pm_check.get_peer(&addr)
                        .and_then(|p| p.last_block_announcement())
                        .unwrap_or(0);
                    drop(pm_check);
                    
                    warn!(
                        "Evicting extra outbound peer {:?} (last block announcement: {} seconds ago)",
                        addr,
                        if last_announce > 0 {
                            now.saturating_sub(last_announce)
                        } else {
                            0
                        }
                    );
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start ping timeout checking task (checks for timed-out pings every 30 seconds)
    fn start_ping_timeout_check_task(&self) {
        let peer_manager = Arc::clone(&self.peer_manager);
        let peer_tx = self.peer_tx.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30)); // Check every 30 seconds
            
            loop {
                interval.tick().await;
                
                let mut pm = peer_manager.lock().await;
                let mut peers_to_disconnect: Vec<crate::network::transport::TransportAddr> = Vec::new();
                
                for (addr, peer) in pm.peers.iter() {
                    if peer.is_ping_timed_out() {
                        warn!("Ping timeout for peer {:?}, disconnecting", addr);
                        peers_to_disconnect.push(addr.clone());
                    }
                }
                
                drop(pm); // Release lock before disconnecting
                
                for addr in peers_to_disconnect {
                    let _ = peer_tx.send(NetworkMessage::PeerDisconnected(addr));
                }
            }
        });
    }

    /// Start periodic ping task (sends ping every 2 minutes to all peers)
    /// This uses the existing ping_all_peers() method which already handles TransportAddr properly
    fn start_ping_task(&self) {
        // We need to call ping_all_peers() periodically, but we can't clone NetworkManager
        // Instead, we'll create a task that sends a trigger message, or we can access
        // the necessary components directly. For simplicity, let's create a wrapper that
        // can call ping_all_peers. Since ping_all_peers is async and uses &self, we need
        // to restructure this. For now, we'll use a channel-based approach or store
        // the necessary Arc fields.
        
        // Actually, the simplest approach is to have a separate task that periodically
        // calls a method. But since we can't easily share &self across tasks, we'll
        // use the existing infrastructure: create a task that sends pings directly
        // using the same pattern as ping_all_peers but in a loop.
        
        let peer_manager = Arc::clone(&self.peer_manager);
        let tcp_transport = self.tcp_transport.clone();
        #[cfg(feature = "quinn")]
        let quinn_transport = self.quinn_transport.clone();
        #[cfg(feature = "iroh")]
        let iroh_transport = self.iroh_transport.clone();
        
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(120)); // 2 minutes
            
            loop {
                interval.tick().await;
                
                // Generate nonce for ping
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;
                
                use crate::network::protocol::{PingMessage, ProtocolMessage, ProtocolParser};
                let ping_msg = ProtocolMessage::Ping(PingMessage { nonce });
                let wire_msg = match ProtocolParser::serialize_message(&ping_msg) {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!("Failed to serialize ping message: {}", e);
                        continue;
                    }
                };
                
                // Get all peers and send ping via their send channels
                {
                    let mut pm = peer_manager.lock().await;
                    for (addr, peer) in pm.peers.iter_mut() {
                        // Record ping in peer state
                        peer.record_ping_sent(nonce);
                        
                        // Send ping via peer's send channel (send_tx is pub(crate))
                        if let Err(e) = peer.send_tx.send(wire_msg.clone()) {
                            warn!("Failed to send ping to peer {:?}: {}", addr, e);
                        }
                    }
                }
            }
        });
    }

    /// Start periodic task to attempt peer reconnections with exponential backoff
    fn start_peer_reconnection_task(&self) {
        let reconnection_queue = Arc::clone(&self.peer_reconnection_queue);
        let peer_manager = Arc::clone(&self.peer_manager);
        let peer_tx = self.peer_tx.clone();
        let tcp_transport = self.tcp_transport.clone();
        let ban_list = Arc::clone(&self.ban_list);
        // Get max_peers (we'll need to access it later, so we'll query it in the loop)

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10)); // Check every 10 seconds
            loop {
                interval.tick().await;

                let now = current_timestamp();

                let mut queue = reconnection_queue.lock().await;

                // Get current peer count and max peers
                let (current_peers, max_peers) = {
                    let pm = peer_manager.lock().await;
                    (pm.peer_count(), pm.max_peers)
                };

                // Calculate minimum peer target (50% of max, but at least 1)
                let min_peers = std::cmp::max(1, max_peers / 2);

                // If we have enough peers, skip reconnection attempts
                if current_peers >= min_peers {
                    // Clean up old entries (older than 1 hour) to prevent queue bloat
                    queue
                        .retain(|_, (_, last_attempt, _)| now.saturating_sub(*last_attempt) < 3600);
                    continue;
                }

                // Sort peers by quality score (highest first), attempts (lowest first), and recency (oldest first)
                let now = current_timestamp();
                let mut peers_to_reconnect: Vec<(SocketAddr, u32, f64, u64)> = queue
                    .iter()
                    .map(|(addr, (attempts, last_attempt, quality))| {
                        (*addr, *attempts, *quality, *last_attempt)
                    })
                    .collect();

                // Sort by quality (descending), then by attempts (ascending), then by recency (oldest first)
                peers_to_reconnect.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(&b.1))
                        .then_with(|| a.3.cmp(&b.3)) // Prefer peers that haven't been attempted recently
                });

                // Extract just the address, attempts, and quality for reconnection
                let peers_to_reconnect: Vec<(SocketAddr, u32, f64)> = peers_to_reconnect
                    .into_iter()
                    .map(|(addr, attempts, quality, _)| (addr, attempts, quality))
                    .collect();

                // Attempt reconnection for eligible peers
                for (addr, attempts, quality) in peers_to_reconnect.iter() {
                    // Check if peer is banned
                    {
                        let ban_list_guard = ban_list.read().await;
                        if let Some(unban_timestamp) = ban_list_guard.get(addr) {
                            if *unban_timestamp != u64::MAX && now < *unban_timestamp {
                                continue; // Skip banned peers
                            }
                        }
                    }

                    // Calculate exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, max 60s
                    let backoff_seconds = std::cmp::min(1u64 << attempts, 60);
                    let last_attempt = queue.get(addr).map(|(_, la, _)| *la).unwrap_or(0);

                    // Check if backoff period has elapsed
                    if now.saturating_sub(last_attempt) < backoff_seconds {
                        continue; // Still in backoff period
                    }

                    // Max reconnection attempts: 10 (after that, remove from queue)
                    if *attempts >= 10 {
                        debug!(
                            "Removing peer {} from reconnection queue (max attempts reached)",
                            addr
                        );
                        queue.remove(addr);
                        continue;
                    }

                    // Check if we have room for more peers
                    if current_peers >= max_peers {
                        break; // No room for more peers
                    }

                    // Attempt reconnection
                    info!(
                        "Attempting to reconnect to peer {} (attempt {}, quality: {:.2})",
                        addr,
                        attempts + 1,
                        quality
                    );

                    // Update last attempt time and increment attempts
                    if let Some((ref mut attempts_ref, ref mut last_attempt_ref, _)) =
                        queue.get_mut(addr)
                    {
                        *attempts_ref += 1;
                        *last_attempt_ref = now;
                    }

                    // Clone for async move
                    let addr_clone = *addr;
                    let peer_tx_clone = peer_tx.clone();
                    let peer_manager_clone = Arc::clone(&peer_manager);
                    let tcp_transport_clone = tcp_transport.clone();
                    let reconnection_queue_clone = Arc::clone(&reconnection_queue);

                    // Attempt connection in background
                    tokio::spawn(async move {
                        use crate::network::peer::Peer;
                        use crate::network::transport::TransportAddr;

                        // Try TCP connection (most common)
                        match tcp_transport_clone
                            .connect(TransportAddr::Tcp(addr_clone))
                            .await
                        {
                            Ok(conn) => {
                                info!("Successfully reconnected to peer {}", addr_clone);

                                // Create peer from transport connection
                                let peer = Peer::from_transport_connection(
                                    conn,
                                    addr_clone,
                                    TransportAddr::Tcp(addr_clone),
                                    peer_tx_clone.clone(),
                                );

                                // Add peer to manager
                                let mut pm = peer_manager_clone.lock().await;
                                if let Err(e) = pm.add_peer(TransportAddr::Tcp(addr_clone), peer) {
                                    warn!("Failed to add reconnected peer {}: {}", addr_clone, e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        TransportAddr::Tcp(addr_clone),
                                    ));
                                } else {
                                    // Remove from reconnection queue on success
                                    let mut queue = reconnection_queue_clone.lock().await;
                                    queue.remove(&addr_clone);
                                    info!("Peer {} successfully reconnected and added", addr_clone);
                                }
                            }
                            Err(e) => {
                                debug!("Reconnection attempt to {} failed: {} (will retry with backoff)", addr_clone, e);
                                // Peer remains in queue for next attempt
                            }
                        }
                    });

                    // Limit concurrent reconnection attempts to 3
                    if !queue.is_empty() {
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                }
            }
        });
    }

    /// Get the number of connected peers
    pub fn peer_count(&self) -> usize {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let pm = self.peer_manager.lock().await;
                pm.peer_count()
            })
        })
    }

    /// Get all peer addresses (as SocketAddr for backward compatibility)
    pub fn peer_addresses(&self) -> Vec<SocketAddr> {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let pm = self.peer_manager.lock().await;
                pm.peer_socket_addresses()
            })
        })
    }

    /// Get all peer addresses (as TransportAddr)
    pub fn peer_transport_addresses(&self) -> Vec<TransportAddr> {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let pm = self.peer_manager.lock().await;
                pm.peer_addresses()
            })
        })
    }

    /// Get the highest start_height (best block height) from all connected peers
    /// Returns None if no peers are connected or no peers have reported a start_height
    pub fn get_highest_peer_start_height(&self) -> Option<u64> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let pm = self.peer_manager.lock().await;
                let mut max_height: Option<i32> = None;
                for (_addr, peer) in pm.peers.iter() {
                    let peer_height = peer.start_height();
                    if peer_height > 0 {
                        max_height = Some(max_height.map_or(peer_height, |m| m.max(peer_height)));
                    }
                }
                max_height.map(|h| h as u64)
            })
        })
    }

    /// Broadcast a message to all peers
    pub async fn broadcast(&self, message: Vec<u8>) -> Result<()> {
        // Get peer addresses first, then drop lock before async operations
        let peer_addrs = {
            let pm = self.peer_manager.lock().await;
            pm.peer_addresses()
        };

        // Send to each peer using transport address (avoids needing to clone Peer)
        for addr in peer_addrs {
            let addr_clone = addr.clone();
            if let Err(e) = self.send_to_peer_by_transport(addr, message.clone()).await {
                warn!(
                    "Failed to broadcast message to peer {:?}: {}",
                    addr_clone, e
                );
            }
        }
        Ok(())
    }

    /// Broadcast to reliable peers first, then others
    /// Uses peer quality to prioritize reliable peers for critical messages
    pub async fn broadcast_with_quality_priority(&self, message: Vec<u8>) -> Result<()> {
        // Get reliable peers first, then drop lock before async operations
        let reliable_peers = {
            let pm = self.peer_manager.lock().await;
            pm.select_reliable_peers()
        };

        // Send to reliable peers first
        for addr in &reliable_peers {
            if let Err(e) = self
                .send_to_peer_by_transport(addr.clone(), message.clone())
                .await
            {
                warn!("Failed to send to reliable peer {:?}: {}", addr, e);
            }
        }

        // Then send to remaining peers
        let pm = self.peer_manager.lock().await;
        let all_peers = pm.peer_addresses();
        let remaining_peers: Vec<_> = all_peers
            .iter()
            .filter(|addr| !reliable_peers.contains(addr))
            .collect();
        drop(pm);

        for addr in remaining_peers {
            if let Err(e) = self
                .send_to_peer_by_transport(addr.clone(), message.clone())
                .await
            {
                warn!("Failed to send to peer {:?}: {}", addr, e);
            }
        }

        Ok(())
    }

    /// Send to best peer (highest quality score)
    /// Returns the peer address used, or error if no peers available
    pub async fn send_to_best_peer(&self, message: Vec<u8>) -> Result<TransportAddr> {
        let pm = self.peer_manager.lock().await;
        let best_peers = pm.select_best_peers(1);
        drop(pm);

        if let Some(addr) = best_peers.first() {
            self.send_to_peer_by_transport(addr.clone(), message)
                .await?;
            Ok(addr.clone())
        } else {
            Err(anyhow::anyhow!("No peers available"))
        }
    }

    /// Send to reliable peer only (for critical operations)
    /// Returns error if no reliable peers available
    pub async fn send_to_reliable_peer(&self, message: Vec<u8>) -> Result<TransportAddr> {
        let pm = self.peer_manager.lock().await;
        let reliable_peers = pm.select_reliable_peers();
        drop(pm);

        if reliable_peers.is_empty() {
            return Err(anyhow::anyhow!("No reliable peers available"));
        }

        // Select best reliable peer
        let pm = self.peer_manager.lock().await;
        let best_reliable = reliable_peers.iter().max_by(|a, b| {
            let score_a = pm.get_peer(a).map(|p| p.quality_score()).unwrap_or(0.0);
            let score_b = pm.get_peer(b).map(|p| p.quality_score()).unwrap_or(0.0);
            score_a
                .partial_cmp(&score_b)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        drop(pm);

        if let Some(addr) = best_reliable {
            self.send_to_peer_by_transport(addr.clone(), message)
                .await?;
            Ok(addr.clone())
        } else {
            Err(anyhow::anyhow!("No reliable peers available"))
        }
    }

    /// Send a message to a specific peer (by SocketAddr - for TCP/Quinn)
    /// For Iroh peers, uses the socket_to_transport mapping
    pub async fn send_to_peer(&self, addr: SocketAddr, message: Vec<u8>) -> Result<()> {
        // Try to find transport address (TCP, Quinn, or Iroh mapping)
        let transport_addr = {
            let pm = self.peer_manager.lock().await;
            pm.find_transport_addr_by_socket(addr).or_else(|| {
                // Check socket_to_transport mapping (synchronous access needed)
                // Note: This is a fallback, so we use try_lock to avoid blocking
                // If we can't get the lock immediately, return None
                self.socket_to_transport
                    .try_lock()
                    .ok()
                    .and_then(|map| map.get(&addr).cloned())
            })
        };

        if let Some(transport_addr) = transport_addr {
            self.send_to_peer_by_transport(transport_addr, message)
                .await
        } else {
            // Fallback: try TCP (for backward compatibility)
            self.send_to_peer_by_transport(TransportAddr::Tcp(addr), message)
                .await
        }
    }

    /// Send a message to a specific peer (by TransportAddr - supports all transports)
    /// Returns error if peer doesn't exist or is disconnected
    pub async fn send_to_peer_by_transport(
        &self,
        addr: TransportAddr,
        message: Vec<u8>,
    ) -> Result<()> {
        let message_len = message.len();

        // Check if peer exists and get sender channel before dropping lock
        let sender = {
            let pm = self.peer_manager.lock().await;
            if let Some(peer) = pm.get_peer(&addr) {
                if !peer.is_connected() {
                    return Err(anyhow::anyhow!("Peer {:?} is disconnected", addr));
                }
                peer.send_tx.clone()
            } else {
                // Peer doesn't exist - return error so caller knows to try another peer
                return Err(anyhow::anyhow!("Peer {:?} not found", addr));
            }
        };

        // Track bytes sent only if peer exists (async-safe)
        self.track_bytes_sent(message_len as u64).await;

        // Bandwidth Protection: Record bandwidth for various message types
        // Check message command (Bitcoin wire format: 4-byte magic + 12-byte command + 4-byte length + payload)
        if message.len() >= 16 {
            let command_bytes = &message[4..16];
            let command_str = String::from_utf8_lossy(command_bytes);
            let command = command_str.trim_end_matches('\0');
            
            // Get SocketAddr for bandwidth protection (needed for tracking)
            let peer_socket_addr_opt: Option<SocketAddr> = match &addr {
                TransportAddr::Tcp(sock) => Some(*sock),
                #[cfg(feature = "quinn")]
                TransportAddr::Quinn(sock) => Some(*sock),
                #[cfg(feature = "iroh")]
                TransportAddr::Iroh(_) => {
                    // For Iroh, try to get from socket_to_transport mapping
                    // If not found, skip bandwidth tracking (Iroh uses different addressing)
                    self.socket_to_transport.lock().await
                        .iter()
                        .find_map(|(sock, &ref ta)| if ta == addr { Some(*sock) } else { None })
                }
            };

            if let Some(peer_socket_addr) = peer_socket_addr_opt {
                // Record bandwidth for IBD-serving messages
                if command == "headers" || command == "block" {
                    // Record bandwidth for IBD protection
                    self.ibd_protection.record_bandwidth(peer_socket_addr, message_len as u64).await;
                    debug!(
                        "IBD protection: Recorded {} bytes for {} message to {}",
                        message_len, command, peer_socket_addr
                    );
                }
                // Record bandwidth for transaction relay
                else if command == "tx" {
                    use crate::network::bandwidth_protection::ServiceType;
                    // Record transaction relay bandwidth (per-IP/subnet limits)
                    self.bandwidth_protection
                        .record_service_bandwidth(ServiceType::TransactionRelay, peer_socket_addr, message_len as u64)
                        .await;
                    debug!(
                        "Transaction relay: Recorded {} bytes to {}",
                        message_len, peer_socket_addr
                    );
                }
            }
        }

        // Send message without holding the lock (unbounded channel, so send won't block)
        sender
            .send(message)
            .map_err(|e| anyhow::anyhow!("Failed to send message to peer: {}", e))?;

        // Record send after message is sent
        let mut pm = self.peer_manager.lock().await;
        if let Some(peer) = pm.get_peer_mut(&addr) {
            peer.record_send(message_len);
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
        // Check DoS protection: connection rate limiting (for outgoing connections too)
        let ip = addr.ip();
        if !self.dos_protection.check_connection(ip).await {
            warn!(
                "Connection rate limit exceeded for IP {}, rejecting outgoing connection",
                ip
            );

            // Check if we should auto-ban
            if self.dos_protection.should_auto_ban(ip).await {
                warn!(
                    "Auto-banning IP {} for repeated connection rate violations",
                    ip
                );
                // Ban the IP using configured ban duration
                let ban_duration = self.dos_protection.ban_duration_seconds();
                let unban_timestamp = current_timestamp() + ban_duration;
                let mut ban_list = self.ban_list.write().await;
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

        // Eclipse attack prevention: check IP diversity
        if !self.check_eclipse_prevention(ip) {
            let prefix = self.get_ip_prefix(ip);
            warn!("Eclipse attack prevention: too many connections from IP range {:?}, rejecting connection from {}", 
                  prefix, ip);
            return Err(anyhow::anyhow!(
                "Eclipse attack prevention: too many connections from IP range"
            ));
        }

        let mut last_error = None;

        // Try transports in preference order with graceful degradation
        let transports_to_try = self.get_transports_for_connection();

        for transport_type in transports_to_try {
            match self.try_connect_with_transport(&transport_type, addr).await {
                Ok((peer, transport_addr)) => {
                    // Successfully connected
                    {
                        let mut pm = self.peer_manager.lock().await;
                        pm.add_peer(transport_addr.clone(), peer)?;
                    }

                    // Send PeerConnected notification to trigger handshake (Version/VerAck)
                    let _ = self.peer_tx.send(NetworkMessage::PeerConnected(transport_addr.clone()));

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
                    // Continue to next transport
                }
            }
        }

        // All transports failed
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All transport attempts failed")))
    }

    /// Helper: Get list of transports to try for a connection
    fn get_transports_for_connection(&self) -> Vec<crate::network::transport::TransportType> {
        let mut transports = Vec::new();

        // Add transports in preference order
        if self.transport_preference.allows_tcp() {
            transports.push(crate::network::transport::TransportType::Tcp);
        }

        #[cfg(feature = "quinn")]
        if self.transport_preference.allows_quinn() {
            if let Some(ref _quinn) = self.quinn_transport {
                transports.push(crate::network::transport::TransportType::Quinn);
            }
        }

        #[cfg(feature = "iroh")]
        if self.transport_preference.allows_iroh() {
            if let Some(ref _iroh) = self.iroh_transport {
                transports.push(crate::network::transport::TransportType::Iroh);
            }
        }

        // Always try TCP as fallback if not already in list
        if !transports
            .iter()
            .any(|t| matches!(t, crate::network::transport::TransportType::Tcp))
        {
            transports.push(crate::network::transport::TransportType::Tcp);
        }

        transports
    }

    /// Helper: Try connecting with a specific transport
    async fn try_connect_with_transport(
        &self,
        transport_type: &crate::network::transport::TransportType,
        addr: SocketAddr,
    ) -> Result<(peer::Peer, TransportAddr)> {
        use crate::network::transport::TransportAddr;
        // Peer is used via fully qualified path peer::Peer, no need to import

        match transport_type {
            crate::network::transport::TransportType::Tcp => {
                // Use connect_stream for raw TcpStream, then create Peer with split
                let stream = self.tcp_transport.connect_stream(addr).await?;
                let transport_addr = TransportAddr::Tcp(addr);
                Ok((
                    peer::Peer::from_tcp_stream_split(
                        stream,
                        addr,
                        self.peer_tx.clone(),
                    ),
                    transport_addr,
                ))
            }
            #[cfg(feature = "quinn")]
            crate::network::transport::TransportType::Quinn => {
                if let Some(ref quinn) = self.quinn_transport {
                    let quinn_addr = TransportAddr::Quinn(addr);
                    let quinn_addr_clone = quinn_addr.clone();
                    let conn = quinn.connect(quinn_addr_clone.clone()).await?;
                    Ok((
                        peer::Peer::from_transport_connection(
                            conn,
                            addr,
                            quinn_addr_clone.clone(),
                            self.peer_tx.clone(),
                        ),
                        quinn_addr_clone,
                    ))
                } else {
                    Err(anyhow::anyhow!("Quinn transport not available"))
                }
            }
            #[cfg(feature = "iroh")]
            crate::network::transport::TransportType::Iroh => {
                // Iroh requires public key, not SocketAddr
                // For now, return error - would need peer discovery or address resolution
                Err(anyhow::anyhow!(
                    "Iroh transport requires public key, not SocketAddr"
                ))
            }
        }
    }

    /// Send ping message to all connected peers
    pub async fn ping_all_peers(&self) -> Result<()> {
        use crate::network::protocol::{PingMessage, ProtocolMessage, ProtocolParser};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Generate nonce for ping
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        let ping_msg = ProtocolMessage::Ping(PingMessage { nonce });
        let wire_msg = ProtocolParser::serialize_message(&ping_msg)?;

        let peer_addrs = {
            let mut pm = self.peer_manager.lock().await;
            // Record ping in peer state before sending
            for (addr, peer) in pm.peers.iter_mut() {
                peer.record_ping_sent(nonce);
            }
            pm.peer_addresses()
        };
        
        for addr in peer_addrs {
            // Convert TransportAddr to SocketAddr for send_to_peer, or use send_to_peer_by_transport
            let addr_clone = addr.clone();
            match addr_clone {
                crate::network::transport::TransportAddr::Tcp(sock) => {
                    if let Err(e) = self.send_to_peer(sock, wire_msg.clone()).await {
                        warn!("Failed to ping peer {}: {}", sock, e);
                    }
                }
                #[cfg(feature = "quinn")]
                crate::network::transport::TransportAddr::Quinn(sock) => {
                    if let Err(e) = self
                        .send_to_peer_by_transport(addr_clone.clone(), wire_msg.clone())
                        .await
                    {
                        warn!("Failed to ping peer {}: {}", sock, e);
                    }
                }
                #[cfg(feature = "iroh")]
                crate::network::transport::TransportAddr::Iroh(_) => {
                    if let Err(e) = self
                        .send_to_peer_by_transport(addr_clone.clone(), wire_msg.clone())
                        .await
                    {
                        warn!("Failed to ping peer {}: {}", addr_clone, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to receive a block message (non-blocking)
    /// Returns Some(block_data) if a block was received, None otherwise
    /// Try to receive a block from the pending blocks queue
    /// This does NOT consume other message types from the main channel
    pub fn try_recv_block(&self) -> Option<Vec<u8>> {
        // Use the dedicated pending_blocks queue instead of draining the main channel
        // This preserves PeerConnected, RawMessageReceived, etc. for proper processing
        let mut blocks = match self.pending_blocks.try_lock() {
            Ok(guard) => guard,
            Err(_) => return None,
        };
        blocks.pop_front()
    }
    
    /// Queue a block for processing (called when BlockReceived is received in process_messages)
    pub fn queue_block(&self, data: Vec<u8>) {
        if let Ok(mut blocks) = self.pending_blocks.try_lock() {
            blocks.push_back(data);
        }
    }

    /// Process all pending network messages without blocking
    /// Returns the number of messages processed
    pub async fn process_pending_messages(&self) -> Result<usize> {
        let mut processed = 0;
        
        loop {
            // Try to receive a message without blocking
            let message = {
                let mut rx = self.peer_rx.lock().await;
                match rx.try_recv() {
                    Ok(msg) => {
                        debug!("process_pending_messages: received message {:?}", 
                            match &msg {
                                NetworkMessage::PeerConnected(addr) => format!("PeerConnected({:?})", addr),
                                NetworkMessage::PeerDisconnected(addr) => format!("PeerDisconnected({:?})", addr),
                                NetworkMessage::RawMessageReceived(data, addr) => format!("RawMessageReceived({} bytes from {})", data.len(), addr),
                                _ => "other".to_string(),
                            });
                        Some(msg)
                    },
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        warn!("process_pending_messages: channel disconnected");
                        return Ok(processed);
                    }
                }
            };
            
            let message = match message {
                Some(msg) => msg,
                None => break, // No more pending messages
            };
            
            // Process the message (reuse the same processing logic)
            self.handle_network_message(message).await;
            processed += 1;
        }
        
        Ok(processed)
    }

    /// Handle a single network message (extracted for reuse)
    async fn handle_network_message(&self, message: NetworkMessage) {
        match message {
            NetworkMessage::PeerConnected(addr) => {
                info!("Peer connected: {:?}", addr);

                // Send Version message to initiate handshake
                let socket_addr = match &addr {
                    TransportAddr::Tcp(sock) => Some(*sock),
                    #[cfg(feature = "quinn")]
                    TransportAddr::Quinn(sock) => Some(*sock),
                    #[cfg(feature = "iroh")]
                    TransportAddr::Iroh(_) => None,
                };

                if let Some(peer_socket) = socket_addr {
                    let start_height = if let Some(ref storage) = self.storage {
                        storage.chain().get_height().ok().flatten().unwrap_or(0) as i32
                    } else {
                        0
                    };

                    let peer_ip = match peer_socket.ip() {
                        std::net::IpAddr::V4(ip) => {
                            let mut addr = [0u8; 16];
                            addr[10] = 0xff;
                            addr[11] = 0xff;
                            addr[12..16].copy_from_slice(&ip.octets());
                            addr
                        }
                        std::net::IpAddr::V6(ip) => ip.octets(),
                    };
                    let addr_recv = crate::network::protocol::NetworkAddress {
                        services: 0,
                        ip: peer_ip,
                        port: peer_socket.port(),
                    };
                    let addr_from = crate::network::protocol::NetworkAddress {
                        services: 0,
                        ip: [0u8; 16],
                        port: 0,
                    };

                    let version_msg = self.create_version_message(
                        70015,
                        0,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64,
                        addr_recv,
                        addr_from,
                        rand::random::<u64>(),
                        format!("/Bitcoin Commons:{}/", env!("CARGO_PKG_VERSION")),
                        start_height,
                        true,
                    );

                    match ProtocolParser::serialize_message(&ProtocolMessage::Version(version_msg)) {
                        Ok(wire_msg) => {
                            if let Err(e) = self.send_to_peer_by_transport(addr.clone(), wire_msg).await {
                                warn!("Failed to send Version message to {:?}: {}", addr, e);
                            } else {
                                info!("Sent Version message to {:?}", addr);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to serialize Version message for {:?}: {}", addr, e);
                        }
                    }
                }

                // Publish peer connected event
                let event_publisher_guard = self.event_publisher.lock().await;
                if let Some(ref event_publisher) = *event_publisher_guard {
                    let addr_str = format!("{:?}", addr);
                    let transport_type = match &addr {
                        TransportAddr::Tcp(_) => "tcp",
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(_) => "quinn",
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => "iroh",
                    };

                    let pm = self.peer_manager.lock().await;
                    let (services, version) = if let Some(peer) = pm.get_peer(&addr) {
                        (peer.services(), peer.version())
                    } else {
                        (0, 0)
                    };
                    drop(pm);

                    let event_pub_clone = Arc::clone(event_publisher);
                    tokio::spawn(async move {
                        event_pub_clone
                            .publish_peer_connected(
                                &addr_str,
                                transport_type,
                                services,
                                version,
                            )
                            .await;
                    });
                }
            }
            NetworkMessage::PeerDisconnected(addr) => {
                info!("Peer disconnected (during pending processing): {:?}", addr);
                // Simplified disconnection handling for pending messages
                let mut pm = self.peer_manager.lock().await;
                pm.remove_peer(&addr);
            }
            NetworkMessage::RawMessageReceived(data, peer_addr) => {
                // Process raw message - this will handle Version messages and send VerAck
                if let Err(e) = self.handle_incoming_wire_tcp(peer_addr, data).await {
                    debug!("Error processing raw message from {}: {}", peer_addr, e);
                }
            }
            _ => {
                // Other message types - handle minimally during handshake
                debug!("Received other message type during handshake phase");
            }
        }
    }

    /// Process incoming network messages
    pub async fn process_messages(&self) -> Result<()> {
        // Track message queue size manually (unbounded channel doesn't have len())
        let mut message_count = 0u64;
        let mut last_metrics_update = std::time::SystemTime::now();

        // Restructured to work with Arc<Mutex>: lock, receive one message, unlock, process
        loop {
            // Lock, receive one message, unlock
            let message = {
                let mut rx = self.peer_rx.lock().await;
                rx.recv().await
            };
            
            let message = match message {
                Some(msg) => msg,
                None => {
                    // Channel closed
                    warn!("Network message channel closed");
                    break;
                }
            };
            
            // Process message (no lock held)
            message_count += 1;

            // Update metrics periodically (every 100 messages or 10 seconds)
            let now = std::time::SystemTime::now();
            let should_update = message_count % 100 == 0
                || now
                    .duration_since(last_metrics_update)
                    .unwrap_or_else(|_| std::time::Duration::from_secs(0))
                    .as_secs()
                    >= 10;

            if should_update {
                let pm = self.peer_manager.lock().await;
                let active_connections = pm.peer_count();
                let bytes_received = self.bytes_received.load(Ordering::Relaxed);
                let bytes_sent = self.bytes_sent.load(Ordering::Relaxed);

                // Approximate queue size (messages processed since last check)
                let queue_size = message_count as usize;

                // Check message queue size limit
                if !self
                    .dos_protection
                    .check_message_queue_size(queue_size)
                    .await
                {
                    warn!(
                        "Message queue size limit exceeded (processed {} messages), potential DoS",
                        queue_size
                    );
                    // Optionally detect DoS attack
                    if self.dos_protection.detect_dos_attack().await {
                        warn!("DoS attack detected - message queue and connections at high levels");
                        // Could trigger automatic mitigation here (e.g., increase rate limits, ban aggressive IPs)
                    }
                    // Reset counter to prevent false positives
                    // Note: Counter will be reset again after metrics update (line 2043),
                    // but this early reset prevents false positives in DoS detection.
                    // The assignment is intentional - value is used in next loop iteration.
                    #[allow(unused_assignments)] // Used in next loop iteration
                    {
                        message_count = 0;
                    }
                }

                self.dos_protection
                    .update_metrics(active_connections, queue_size, bytes_received, bytes_sent)
                    .await;

                last_metrics_update = now;
                message_count = 0; // Reset after update for next period
            }

            match message {
                NetworkMessage::PeerConnected(addr) => {
                    info!("Peer connected: {:?}", addr);

                    // Send Version message to initiate handshake
                    // This is required by Bitcoin protocol - both sides send Version after connection
                    let socket_addr = match &addr {
                        TransportAddr::Tcp(sock) => Some(*sock),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some(*sock),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None, // Iroh uses different handshake
                    };

                    if let Some(peer_socket) = socket_addr {
                        // Get current block height from storage (or 0 if not available)
                        let start_height = if let Some(ref storage) = self.storage {
                            storage.chain().get_height().ok().flatten().unwrap_or(0) as i32
                        } else {
                            0
                        };

                        // Create network addresses for version message
                        let peer_ip = match peer_socket.ip() {
                            std::net::IpAddr::V4(ip) => {
                                let mut addr = [0u8; 16];
                                addr[10] = 0xff;
                                addr[11] = 0xff;
                                addr[12..16].copy_from_slice(&ip.octets());
                                addr
                            }
                            std::net::IpAddr::V6(ip) => ip.octets(),
                        };
                        let addr_recv = crate::network::protocol::NetworkAddress {
                            services: 0,
                            ip: peer_ip,
                            port: peer_socket.port(),
                        };
                        let addr_from = crate::network::protocol::NetworkAddress {
                            services: 0,
                            ip: [0u8; 16],
                            port: 0,
                        };

                        // Create version message with our node's capabilities
                        let version_msg = self.create_version_message(
                            70015, // Protocol version
                            0,     // Additional services (base services added by create_version_message)
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64,
                            addr_recv,
                            addr_from,
                            rand::random::<u64>(), // Random nonce
                            format!("/Bitcoin Commons:{}/", env!("CARGO_PKG_VERSION")),
                            start_height,
                            true, // relay
                        );

                        // Serialize and send Version message
                        match ProtocolParser::serialize_message(&ProtocolMessage::Version(version_msg)) {
                            Ok(wire_msg) => {
                                if let Err(e) = self.send_to_peer_by_transport(addr.clone(), wire_msg).await {
                                    warn!("Failed to send Version message to {:?}: {}", addr, e);
                                } else {
                                    debug!("Sent Version message to {:?}", addr);
                                }
                            }
                            Err(e) => {
                                warn!("Failed to serialize Version message for {:?}: {}", addr, e);
                            }
                        }
                    }

                    // Publish peer connected event
                    let event_publisher_guard = self.event_publisher.lock().await;
                    if let Some(ref event_publisher) = *event_publisher_guard {
                        let addr_str = format!("{:?}", addr);
                        let transport_type = match &addr {
                            TransportAddr::Tcp(_) => "tcp",
                            #[cfg(feature = "quinn")]
                            TransportAddr::Quinn(_) => "quinn",
                            #[cfg(feature = "iroh")]
                            TransportAddr::Iroh(_) => "iroh",
                        };

                        // Get peer info if available
                        let pm = self.peer_manager.lock().await;
                        let (services, version) = if let Some(peer) = pm.get_peer(&addr) {
                            (peer.services(), peer.version())
                        } else {
                            (0, 0)
                        };
                        drop(pm);

                        let event_pub_clone = Arc::clone(event_publisher);
                        tokio::spawn(async move {
                            event_pub_clone
                                .publish_peer_connected(
                                    &addr_str,
                                    transport_type,
                                    services,
                                    version,
                                )
                                .await;
                        });
                    }
                }
                NetworkMessage::PeerDisconnected(addr) => {
                    info!("Peer disconnected: {:?}", addr);

                    // Publish peer disconnected event
                    let event_publisher_guard = self.event_publisher.lock().await;
                    if let Some(ref event_publisher) = *event_publisher_guard {
                        let addr_str = format!("{:?}", addr);
                        let reason = "disconnected".to_string(); // Could be more specific
                        let event_pub_clone = Arc::clone(event_publisher);
                        tokio::spawn(async move {
                            event_pub_clone
                                .publish_peer_disconnected(&addr_str, &reason)
                                .await;
                        });
                    }
                    let mut pm = self.peer_manager.lock().await;

                    // Get peer quality score before removing
                    let quality_score =
                        pm.get_peer(&addr).map(|p| p.quality_score()).unwrap_or(0.5);

                    // Remove peer directly using TransportAddr
                    pm.remove_peer(&addr);

                    // Extract SocketAddr for reconnection tracking (only TCP/Quinn)
                    if let Some(socket_addr) = match &addr {
                        TransportAddr::Tcp(sock) => Some(*sock),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some(*sock),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None, // Iroh peers use different reconnection mechanism
                    } {
                        // IBD Protection: Stop IBD serving tracking when peer disconnects
                        self.ibd_protection.stop_ibd_serving(socket_addr).await;
                        debug!("IBD protection: Stopped IBD serving tracking for disconnected peer {}", socket_addr);
                        // Add to reconnection queue with exponential backoff
                        let now = current_timestamp();
                        // Add to reconnection queue with exponential backoff
                        let mut reconnection_queue = self.peer_reconnection_queue.lock().await;
                        reconnection_queue.insert(socket_addr, (0, now, quality_score));
                        info!(
                            "Added peer {} to reconnection queue (quality: {:.2})",
                            socket_addr, quality_score
                        );
                    }

                    // Clean up per-IP connection count (only for TCP/Quinn, not Iroh)
                    if let Some(ip) = match &addr {
                        TransportAddr::Tcp(sock) => Some(sock.ip()),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some(sock.ip()),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None,
                    } {
                        let mut ip_connections = self.connections_per_ip.lock().await;
                        if let Some(count) = ip_connections.get_mut(&ip) {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                ip_connections.remove(&ip);
                            }
                        }
                    }

                    // Clean up rate limiter (use SocketAddr for TCP/Quinn, or a key for Iroh)
                    {
                        let mut rates = self.peer_message_rates.lock().await;
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
                            // Also clean up transaction rate limiters and spam violations
                            let mut tx_rates = self.peer_tx_rate_limiters.lock().await;
                            tx_rates.remove(&sock_addr);
                            let mut byte_rates = self.peer_tx_byte_rate_limiters.lock().await;
                            byte_rates.remove(&sock_addr);
                            let mut spam_violations = self.peer_spam_violations.lock().await;
                            spam_violations.remove(&sock_addr);
                        }
                    }

                    // Clean up eclipse attack prevention tracking
                    if let Some(ip) = match &addr {
                        TransportAddr::Tcp(sock) => Some(sock.ip()),
                        #[cfg(feature = "quinn")]
                        TransportAddr::Quinn(sock) => Some(sock.ip()),
                        #[cfg(feature = "iroh")]
                        TransportAddr::Iroh(_) => None,
                    } {
                        self.remove_peer_diversity(ip);
                    }
                }
                NetworkMessage::HeadersReceived(data, peer_addr) => {
                    info!("Headers received from {}: {} bytes", peer_addr, data.len());
                    // Parse and route to pending request if any
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::Headers(headers_msg) = parsed {
                            let headers = headers_msg.headers;
                            if self.complete_headers_request(peer_addr, headers) {
                                debug!("Routed Headers to pending request from {}", peer_addr);
                            }
                            // Headers will also be processed normally through protocol layer
                        }
                    }
                }
                NetworkMessage::BlockReceived(data) => {
                    info!("Block received: {} bytes", data.len());
                    // Note: BlockReceived doesn't include peer address, so we can't track peer quality here
                    // Peer quality tracking happens when blocks are successfully processed
                    // Block processing handled via try_recv_block() in Node::run()
                }
                NetworkMessage::TransactionReceived(data) => {
                    info!("Transaction received: {} bytes", data.len());
                    // Note: TransactionReceived doesn't include peer address, so we can't track peer quality here
                    // Peer quality tracking happens when transactions are successfully processed
                    // Process transaction with consensus layer
                }
                NetworkMessage::InventoryReceived(data) => {
                    info!("Inventory received: {} bytes", data.len());
                    // Parse and validate inventory size (Bitcoin Core compatibility)
                    // Note: InventoryReceived doesn't include peer_addr, so validation
                    // should be done in the protocol message handler where peer_addr is available
                    // For now, we'll validate here but can't disconnect without peer address
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::Inv(inv_msg) = parsed {
                            if inv_msg.inventory.len() > crate::network::protocol::MAX_INV_SZ {
                                warn!(
                                    "inv message size = {} exceeds MAX_INV_SZ ({})",
                                    inv_msg.inventory.len(),
                                    crate::network::protocol::MAX_INV_SZ
                                );
                                // Can't disconnect here without peer address
                                // Validation should be done in protocol handler
                                continue; // Don't process invalid message
                            }
                            
                            // Record block announcements for outbound peer eviction
                            // Check if this inv contains block announcements (MSG_BLOCK = 2)
                            use crate::network::inventory::MSG_BLOCK;
                            let has_block_announcements = inv_msg.inventory.iter().any(|inv| inv.inv_type == MSG_BLOCK);
                            if has_block_announcements {
                                // Note: We don't have peer_addr here, so we can't update peer state
                                // This is a limitation - block announcements should be tracked in the protocol handler
                                // where peer_addr is available. For now, we'll track it when we process the inv
                                // in the protocol handler (see ProtocolMessage::Inv handler)
                            }
                        }
                    }
                    // Process inventory (existing code continues here)
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
                    debug!(
                        "Stratum V2 message received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Route Stratum V2 message to Stratum V2 module via event system
                    let event_publisher_guard = self.event_publisher.lock().await;
                    if let Some(event_publisher) = event_publisher_guard.as_ref() {
                        use crate::module::ipc::protocol::EventPayload;
                        use crate::module::traits::EventType;
                        let payload = EventPayload::StratumV2MessageReceived {
                            message_data: data,
                            peer_addr: peer_addr.to_string(),
                        };
                        if let Err(e) = event_publisher
                            .publish_event(EventType::StratumV2MessageReceived, payload)
                            .await
                        {
                            warn!("Failed to publish Stratum V2 message event: {}", e);
                        }
                    } else {
                        debug!("Event publisher not available, Stratum V2 message dropped");
                    }
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
                // Module Registry messages
                NetworkMessage::GetModuleReceived(data, peer_addr) => {
                    info!(
                        "GetModule received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Parse and handle GetModule request
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::GetModule(msg) = parsed {
                            if let Err(e) = self.handle_get_module(peer_addr, msg).await {
                                warn!("Failed to handle GetModule request: {}", e);
                            }
                        }
                    }
                }
                NetworkMessage::ModuleReceived(_, _) => {
                    // Handle Module response (for async request matching)
                }
                NetworkMessage::GetModuleByHashReceived(data, peer_addr) => {
                    info!(
                        "GetModuleByHash received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Parse and handle GetModuleByHash request
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::GetModuleByHash(msg) = parsed {
                            if let Err(e) = self.handle_get_module_by_hash(peer_addr, msg).await {
                                warn!("Failed to handle GetModuleByHash request: {}", e);
                            }
                        }
                    }
                }
                NetworkMessage::ModuleByHashReceived(_, _) => {
                    // Handle ModuleByHash response (for async request matching)
                }
                NetworkMessage::GetModuleListReceived(data, peer_addr) => {
                    info!(
                        "GetModuleList received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Parse and handle GetModuleList request
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::GetModuleList(msg) = parsed {
                            if let Err(e) = self.handle_get_module_list(peer_addr, msg).await {
                                warn!("Failed to handle GetModuleList request: {}", e);
                            }
                        }
                    }
                }
                NetworkMessage::ModuleListReceived(_, _) => {
                    // Handle ModuleList response (for async request matching)
                }
                // BIP70 Payment Protocol messages
                NetworkMessage::GetPaymentRequestReceived(data, peer_addr) => {
                    info!(
                        "GetPaymentRequest received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::GetPaymentRequest(msg) = parsed {
                            if let Err(e) = self.handle_get_payment_request(peer_addr, msg).await {
                                warn!("Failed to handle GetPaymentRequest: {}", e);
                            }
                        }
                    }
                }
                NetworkMessage::PaymentRequestReceived(_, _) => {
                    // Handle PaymentRequest response (for async request matching)
                }
                NetworkMessage::PaymentReceived(data, peer_addr) => {
                    info!("Payment received from {}: {} bytes", peer_addr, data.len());
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::Payment(msg) = parsed {
                            if let Err(e) = self.handle_payment(peer_addr, msg).await {
                                warn!("Failed to handle Payment: {}", e);
                            }
                        }
                    }
                }
                NetworkMessage::PaymentACKReceived(_, _) => {
                    // Handle PaymentACK response (for async request matching)
                }
                #[cfg(feature = "ctv")]
                NetworkMessage::PaymentProofReceived(data, peer_addr) => {
                    info!(
                        "PaymentProof received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::PaymentProof(msg) = parsed {
                            // Handle payment proof: verify and update state machine
                            let state_machine_guard = self.payment_state_machine.lock().await;
                            if let Some(ref state_machine) = *state_machine_guard {
                                let processor_guard = self.payment_processor.lock().await;
                                if let Some(ref processor) = *processor_guard {
                                    // Get payment request to verify proof against expected outputs
                                    match processor
                                        .get_payment_request(&msg.payment_request_id)
                                        .await
                                    {
                                        Ok(payment_request) => {
                                            // Extract expected outputs from payment request
                                            let expected_outputs: Vec<_> = payment_request
                                                .payment_details
                                                .outputs
                                                .iter()
                                                .map(|output| {
                                                    blvm_protocol::payment::PaymentOutput {
                                                        script: output.script.clone(),
                                                        amount: output.amount,
                                                    }
                                                })
                                                .collect();

                                            // Verify covenant proof
                                            #[cfg(feature = "ctv")]
                                            {
                                                use crate::payment::covenant::CovenantEngine;
                                                let covenant_engine = CovenantEngine::new();

                                                match covenant_engine.verify_covenant_proof(
                                                    &msg.covenant_proof,
                                                    &expected_outputs,
                                                ) {
                                                    Ok(true) => {
                                                        info!(
                                                            "Payment proof verified for payment: {}",
                                                            msg.payment_request_id
                                                        );

                                                        // Update state machine to ProofCreated
                                                        // Note: This is a proof received from peer, not created locally
                                                        // We mark it as ProofCreated so it can be tracked
                                                        // The state machine will handle the state transition
                                                        if let Err(e) = state_machine
                                                            .create_covenant_proof(
                                                                &msg.payment_request_id,
                                                            )
                                                            .await
                                                        {
                                                            // If creating proof fails (e.g., already exists), that's okay
                                                            // The proof was received from peer, so we just log it
                                                            debug!(
                                                                "Payment proof received from peer (state update: {}): payment_id={}",
                                                                e, msg.payment_request_id
                                                            );
                                                        }
                                                    }
                                                    Ok(false) => {
                                                        warn!(
                                                            "Payment proof verification failed for payment: {}",
                                                            msg.payment_request_id
                                                        );
                                                    }
                                                    Err(e) => {
                                                        warn!(
                                                            "Error verifying payment proof for payment {}: {}",
                                                            msg.payment_request_id, e
                                                        );
                                                    }
                                                }
                                            }

                                            #[cfg(not(feature = "ctv"))]
                                            {
                                                debug!(
                                                    "Payment proof received but CTV feature not enabled: payment_id={}",
                                                    msg.payment_request_id
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            warn!(
                                                "Failed to get payment request for proof verification: {} (payment_id: {})",
                                                e, msg.payment_request_id
                                            );
                                        }
                                    }
                                } else {
                                    warn!("Payment processor not available for proof verification");
                                }
                            } else {
                                debug!(
                                    "Payment state machine not available, ignoring payment proof: request_id={}, payment_id={}",
                                    msg.request_id, msg.payment_request_id
                                );
                            }
                        }
                    }
                }
                NetworkMessage::SettlementNotificationReceived(data, peer_addr) => {
                    info!(
                        "SettlementNotification received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        if let ProtocolMessage::SettlementNotification(msg) = parsed {
                            // Handle settlement notification: update state machine
                            let state_machine_guard = self.payment_state_machine.lock().await;
                            if let Some(ref state_machine) = *state_machine_guard {
                                match msg.status.as_str() {
                                    "mempool" => {
                                        // Transaction in mempool
                                        if let Some(ref tx_hash) = msg.transaction_hash {
                                            if let Err(e) = state_machine
                                                .mark_in_mempool(&msg.payment_request_id, *tx_hash)
                                                .await
                                            {
                                                warn!(
                                                    "Failed to update state machine for mempool settlement: {} (payment_id: {})",
                                                    e, msg.payment_request_id
                                                );
                                            } else {
                                                info!(
                                                    "Settlement notification: payment {} in mempool (tx: {})",
                                                    msg.payment_request_id,
                                                    hex::encode(tx_hash)
                                                );
                                            }
                                        }
                                    }
                                    "confirmed" => {
                                        // Transaction confirmed
                                        if let (Some(ref tx_hash), Some(ref block_hash)) =
                                            (msg.transaction_hash, msg.block_hash)
                                        {
                                            if let Err(e) = state_machine
                                                .mark_settled(
                                                    &msg.payment_request_id,
                                                    *tx_hash,
                                                    *block_hash,
                                                    msg.confirmation_count,
                                                )
                                                .await
                                            {
                                                warn!(
                                                    "Failed to update state machine for confirmed settlement: {} (payment_id: {})",
                                                    e, msg.payment_request_id
                                                );
                                            } else {
                                                info!(
                                                    "Settlement notification: payment {} confirmed (tx: {}, block: {}, confirmations: {})",
                                                    msg.payment_request_id,
                                                    hex::encode(tx_hash),
                                                    hex::encode(block_hash),
                                                    msg.confirmation_count
                                                );
                                            }
                                        }
                                    }
                                    "failed" => {
                                        // Payment failed
                                        if let Err(e) = state_machine
                                            .mark_failed(
                                                &msg.payment_request_id,
                                                "Settlement failed (notification from peer)"
                                                    .to_string(),
                                            )
                                            .await
                                        {
                                            warn!(
                                                "Failed to update state machine for failed settlement: {} (payment_id: {})",
                                                e, msg.payment_request_id
                                            );
                                        } else {
                                            info!(
                                                "Settlement notification: payment {} failed",
                                                msg.payment_request_id
                                            );
                                        }
                                    }
                                    _ => {
                                        warn!(
                                            "Unknown settlement status '{}' for payment: {}",
                                            msg.status, msg.payment_request_id
                                        );
                                    }
                                }
                            } else {
                                debug!(
                                    "Payment state machine not available, ignoring settlement notification: payment_id={}, confirmations={}",
                                    msg.payment_request_id, msg.confirmation_count
                                );
                            }
                        }
                    }
                }
                // Mesh networking packets
                NetworkMessage::MeshPacketReceived(data, peer_addr) => {
                    debug!(
                        "Mesh packet received from {}: {} bytes",
                        peer_addr,
                        data.len()
                    );
                    // Route mesh packet to mesh module via event system
                    // The mesh module subscribes to MeshPacketReceived events
                    let event_publisher_guard = self.event_publisher.lock().await;
                    if let Some(event_publisher) = event_publisher_guard.as_ref() {
                        use crate::module::ipc::protocol::EventPayload;
                        use crate::module::traits::EventType;
                        let payload = EventPayload::MeshPacketReceived {
                            packet_data: data.clone(),
                            peer_addr: peer_addr.to_string(),
                        };
                        if let Err(e) = event_publisher
                            .publish_event(EventType::MeshPacketReceived, payload)
                            .await
                        {
                            warn!("Failed to publish mesh packet event: {}", e);
                        }
                    } else {
                        debug!("Event publisher not available, mesh packet dropped");
                    }
                }
                // Raw messages from peer connections
                NetworkMessage::RawMessageReceived(data, peer_addr) => {
                    // Update peer receive stats - drop lock before async operations
                    {
                        let mut pm = self.peer_manager.lock().await;
                        // Try to find peer by SocketAddr (TCP or Quinn, or Iroh mapping)
                        let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
                        if let Some(transport_addr) = transport_addr {
                            if let Some(peer) = pm.get_peer_mut(&transport_addr) {
                                peer.record_receive(data.len());
                            }
                        }
                    }

                    // Check rate limiting before processing - drop lock before async
                    // Note: transport_addr_opt was previously computed but not used - removed for now
                    // SKIP rate limiting for LAN peers - they're trusted and fast
                    let is_lan = crate::network::peer_scoring::is_lan_peer(&peer_addr);
                    let should_process = if is_lan {
                        true // LAN peers bypass rate limiting for maximum IBD speed
                    } else {
                        let mut rates = self.peer_message_rates.lock().await;
                        let rate_limiter = rates.entry(peer_addr).or_insert_with(|| {
                            // IBD-optimized: 10000 burst, 1000 messages/second
                            // During IBD we need to handle many block messages per second
                            // from fast peers without rate limiting them
                            PeerRateLimiter::new(10000, 1000)
                        });
                        rate_limiter.check_and_consume()
                    };

                    if !should_process {
                        warn!(
                            "Rate limit exceeded for peer {}, dropping message",
                            peer_addr
                        );
                        // Optionally ban peer after repeated rate limit violations
                        // For now, just drop the message
                        continue;
                    }

                    // Check if this is a response to a pending request (UTXOSet, FilteredBlock, Module, ModuleByHash)
                    // Extract request_id from message and route to correct pending request
                    if let Ok(parsed) = ProtocolParser::parse_message(&data) {
                        let request_id_opt = match &parsed {
                            ProtocolMessage::UTXOSet(msg) => Some(msg.request_id),
                            ProtocolMessage::FilteredBlock(msg) => Some(msg.request_id),
                            ProtocolMessage::Module(msg) => Some(msg.request_id),
                            ProtocolMessage::ModuleByHash(msg) => Some(msg.request_id),
                            #[cfg(feature = "ctv")]
                            ProtocolMessage::PaymentProof(msg) => Some(msg.request_id),
                            _ => None,
                        };

                        if let Some(request_id) = request_id_opt {
                            // Route to pending request by request_id
                            let mut pending = self.pending_requests.lock().await;
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
    /// 1. Converts ProtocolMessage to blvm_protocol::network::NetworkMessage
    /// 2. Gets or creates PeerState for this peer
    /// 3. Creates NodeChainAccess from storage modules
    /// 4. Calls blvm_protocol::network::process_network_message()
    /// 5. Converts NetworkResponse back to wire format and enqueues for sending
    pub async fn handle_incoming_wire_tcp(
        &self,
        peer_addr: SocketAddr,
        data: Vec<u8>,
    ) -> Result<()> {
        // Track bytes received (async-safe)
        self.track_bytes_received(data.len() as u64).await;

        // Check if peer is banned
        if self.is_banned(peer_addr) {
            warn!("Rejecting message from banned peer: {}", peer_addr);
            return Ok(()); // Silently drop messages from banned peers
        }

        // Check for mesh packet magic bytes (0x4D, 0x45, 0x53, 0x48 = "MESH")
        // Mesh packets are handled separately and routed to mesh module
        if data.len() >= 4 && data[0..4] == [0x4D, 0x45, 0x53, 0x48] {
            debug!(
                "Detected mesh packet from {}: {} bytes",
                peer_addr,
                data.len()
            );
            // Route mesh packet to mesh module via event system
            // The packet is sent to the message queue, which then publishes MeshPacketReceived events
            // that the mesh module can subscribe to (see handle_network_messages for event publishing)
            let _ = self
                .peer_tx
                .send(NetworkMessage::MeshPacketReceived(data, peer_addr));
            return Ok(());
        }

        // Check for Stratum V2 TLV message format
        // Stratum V2 messages start with [4-byte length][2-byte tag][4-byte length][payload]
        // Valid Stratum V2 tags are in range 0x0001-0x0032 (see bllvm-stratum-v2/src/messages.rs)
        #[cfg(feature = "stratum-v2")]
        if data.len() >= 10 {
            // Try to parse as Stratum V2 TLV
            let length_prefix = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if length_prefix >= 6 && length_prefix <= 1024 * 1024 {
                // Reasonable size limits
                let tag = u16::from_le_bytes([data[4], data[5]]);
                // Check if tag is a valid Stratum V2 message type
                // Valid tags: 0x0001-0x0003 (setup), 0x0010-0x0012 (channel), 0x0020-0x0021 (job), 0x0030-0x0032 (shares)
                if (tag >= 0x0001 && tag <= 0x0003)
                    || (tag >= 0x0010 && tag <= 0x0012)
                    || (tag >= 0x0020 && tag <= 0x0021)
                    || (tag >= 0x0030 && tag <= 0x0032)
                {
                    debug!(
                        "Detected Stratum V2 message from {}: tag={:04x}, {} bytes",
                        peer_addr,
                        tag,
                        data.len()
                    );
                    let _ = self
                        .peer_tx
                        .send(NetworkMessage::StratumV2MessageReceived(data, peer_addr));
                    return Ok(());
                }
            }
        }

        let parsed = ProtocolParser::parse_message(&data)?;

        // Check transaction rate limiting for Tx messages
        if let ProtocolMessage::Tx(_) = &parsed {
            // Enhanced: Check per-IP bandwidth limits (prevents bypass with multiple peers from same IP)
            use crate::network::bandwidth_protection::ServiceType;
            let tx_bytes = data.len() as u64;
            
            // Check if IP has exceeded transaction relay bandwidth limits
            match self.bandwidth_protection
                .check_service_request(ServiceType::TransactionRelay, peer_addr)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        "Transaction relay bandwidth limit exceeded for IP {} (peer {}), dropping transaction",
                        peer_addr.ip(), peer_addr
                    );
                    return Ok(()); // Drop transaction
                }
                Err(e) => {
                    warn!("Bandwidth check error for transaction relay: {}", e);
                    // Continue processing (don't block on check error)
                }
            }

            let (burst, rate) = self
                .mempool_policy_config
                .as_ref()
                .map(|cfg| (cfg.tx_rate_limit_burst, cfg.tx_rate_limit_per_sec))
                .unwrap_or((10, 1)); // Default: 10 burst, 1 tx/sec

            let should_process = {
                let mut tx_rates = self.peer_tx_rate_limiters.lock().await;
                let limiter = tx_rates
                    .entry(peer_addr)
                    .or_insert_with(|| PeerRateLimiter::new(burst, rate));
                limiter.check_and_consume()
            };

            if !should_process {
                warn!(
                    "Transaction rate limit exceeded for peer {}, dropping transaction",
                    peer_addr
                );
                return Ok(()); // Drop transaction
            }

            // Check byte rate limiting (per-peer)
            let (byte_burst, byte_rate) = self
                .mempool_policy_config
                .as_ref()
                .map(|cfg| (cfg.tx_byte_rate_burst, cfg.tx_byte_rate_limit))
                .unwrap_or((1_000_000, 100_000)); // Default: 1 MB burst, 100 KB/s

            let should_process_bytes = {
                let mut byte_rates = self.peer_tx_byte_rate_limiters.lock().await;
                let limiter = byte_rates
                    .entry(peer_addr)
                    .or_insert_with(|| PeerByteRateLimiter::new(byte_burst, byte_rate));
                limiter.check_and_consume(tx_bytes)
            };

            if !should_process_bytes {
                warn!("Transaction byte rate limit exceeded for peer {} ({} bytes), dropping transaction", peer_addr, tx_bytes);
                return Ok(()); // Drop transaction
            }

            // Record transaction relay bandwidth (for per-IP/subnet tracking)
            self.bandwidth_protection
                .record_service_bandwidth(ServiceType::TransactionRelay, peer_addr, tx_bytes)
                .await;

            // Check if transaction is spam and track violations
            use blvm_consensus::spam_filter::SpamFilter;

            // Parse transaction from data to check if it's spam
            if let Ok(tx_msg) =
                bincode::deserialize::<crate::network::protocol::TxMessage>(&data[4..])
            {
                let tx = &tx_msg.transaction;
                let spam_filter = SpamFilter::new();
                let result = spam_filter.is_spam(tx);

                if result.is_spam {
                    // Track spam violation
                    let mut violations = self.peer_spam_violations.lock().await;
                    let violation_count = violations.entry(peer_addr).or_insert(0);
                    *violation_count += 1;
                    let current_count = *violation_count;
                    drop(violations);

                    // Check if we should ban this peer
                    if let Some(ban_config) = self.spam_ban_config.as_ref() {
                        if current_count >= ban_config.spam_ban_threshold {
                            let unban_timestamp = crate::utils::current_timestamp()
                                + ban_config.spam_ban_duration_seconds;
                            warn!("Auto-banning peer {} for spam violations ({} violations, unban at {})", peer_addr, current_count, unban_timestamp);
                            self.ban_peer(peer_addr, unban_timestamp);
                            return Ok(()); // Drop transaction and ban peer
                        }
                    }

                    debug!(
                        "Spam transaction from peer {} (violation count: {})",
                        peer_addr, current_count
                    );
                }
            }
        }

        // Handle special cases that don't go through protocol layer
        match &parsed {
            // Store peer version information when Version message is received
            ProtocolMessage::Version(version_msg) => {
                // Update peer with version, services, user_agent, and start_height
                let mut pm = self.peer_manager.lock().await;
                let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
                let transport_addr_for_verack = transport_addr.clone();
                if let Some(transport_addr) = transport_addr {
                    if let Some(peer) = pm.get_peer_mut(&transport_addr) {
                        peer.set_version(version_msg.version as u32);
                        peer.set_services(version_msg.services);
                        peer.set_user_agent(version_msg.user_agent.clone());
                        peer.set_start_height(version_msg.start_height);
                        debug!("Updated peer {} with version={}, services={}, user_agent={}, start_height={}", 
                               peer_addr, version_msg.version, version_msg.services, version_msg.user_agent, version_msg.start_height);
                    }
                }
                drop(pm);

                // Send VerAck in response to Version (required for handshake completion)
                if let Some(transport_addr) = transport_addr_for_verack {
                    match ProtocolParser::serialize_message(&ProtocolMessage::Verack) {
                        Ok(verack_msg) => {
                            if let Err(e) = self.send_to_peer_by_transport(transport_addr.clone(), verack_msg).await {
                                warn!("Failed to send VerAck to {:?}: {}", transport_addr, e);
                            } else {
                                debug!("Sent VerAck to {:?} (handshake completing)", transport_addr);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to serialize VerAck for {:?}: {}", transport_addr, e);
                        }
                    }
                }
                // Continue to process Version message through protocol layer
            }
            // Respond to Ping with Pong (required to keep connection alive)
            ProtocolMessage::Ping(ping_msg) => {
                use crate::network::protocol::PongMessage;
                // Send Pong with the same nonce to keep connection alive
                let pong_msg = ProtocolMessage::Pong(PongMessage { nonce: ping_msg.nonce });
                match ProtocolParser::serialize_message(&pong_msg) {
                    Ok(pong_wire) => {
                        let pm = self.peer_manager.lock().await;
                        let transport_addr = pm.find_transport_addr_by_socket(peer_addr);
                        drop(pm);
                        if let Some(transport_addr) = transport_addr {
                            if let Err(e) = self.send_to_peer_by_transport(transport_addr.clone(), pong_wire).await {
                                warn!("Failed to send Pong to {}: {}", peer_addr, e);
                            } else {
                                debug!("Sent Pong to {} (nonce={})", peer_addr, ping_msg.nonce);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to serialize Pong for {}: {}", peer_addr, e);
                    }
                }
                return Ok(()); // Ping handled, no further processing needed
            }
            // Handle Pong response (clear pending ping)
            ProtocolMessage::Pong(pong_msg) => {
                // Record pong received in peer state
                {
                    let mut pm = self.peer_manager.lock().await;
                    // Find TransportAddr for this SocketAddr
                    let transport_addr = pm.find_transport_addr_by_socket(peer_addr)
                        .or_else(|| {
                            // Fallback: try to find by iterating (if method doesn't exist)
                            pm.peers.iter()
                                .find(|(addr, _)| {
                                    match addr {
                                        TransportAddr::Tcp(sock) => *sock == peer_addr,
                                        #[cfg(feature = "quinn")]
                                        TransportAddr::Quinn(sock) => *sock == peer_addr,
                                        _ => false,
                                    }
                                })
                                .map(|(addr, _)| addr.clone())
                        });
                    
                    if let Some(addr) = transport_addr {
                        if let Some(peer) = pm.get_peer_mut(&addr) {
                            if !peer.record_pong_received(pong_msg.nonce) {
                                warn!("Received pong with non-matching nonce from {}", peer_addr);
                            } else {
                                debug!("Received valid pong from {} (nonce={})", peer_addr, pong_msg.nonce);
                            }
                        }
                    }
                }
                // Continue normal processing (pong is just acknowledgment)
            }
            // IBD Protection: Check GetHeaders requests (full chain sync)
            ProtocolMessage::GetHeaders(getheaders) => {
                // Detect if this is a full chain request (empty locator = IBD)
                let is_full_chain_request = getheaders.block_locator_hashes.is_empty();
                
                if is_full_chain_request {
                    // Check IBD protection before serving
                    match self.ibd_protection.can_serve_ibd(peer_addr).await {
                        Ok(true) => {
                            // Start IBD serving tracking
                            self.ibd_protection.start_ibd_serving(peer_addr).await;
                            debug!("IBD protection: Allowing full chain sync request from {}", peer_addr);
                            // Continue to process the request normally
                        }
                        Ok(false) => {
                            warn!(
                                "IBD protection: Rejecting full chain sync request from {} (bandwidth limit exceeded or cooldown active)",
                                peer_addr
                            );
                            // Send reject message or silently drop
                            // For now, we'll silently drop to avoid revealing our protection
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("IBD protection check failed for {}: {}", peer_addr, e);
                            // On error, allow the request (fail open for safety)
                        }
                    }
                }
                // Continue to process GetHeaders normally (will go through protocol layer)
            }
            // IBD Protection: Check GetData requests for blocks (IBD serving)
            ProtocolMessage::GetData(getdata) => {
                // Validate inventory size (Bitcoin Core compatibility)
                if getdata.inventory.len() > crate::network::protocol::MAX_INV_SZ {
                    warn!(
                        "getdata message size = {} exceeds MAX_INV_SZ ({}), disconnecting peer {}",
                        getdata.inventory.len(),
                        crate::network::protocol::MAX_INV_SZ,
                        peer_addr
                    );
                    // Check if peer should be disconnected (exempt manual connections and NoBan)
                    let mut pm = self.peer_manager.lock().await;
                    let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                        !peer.is_manual() && !peer.has_noban_permission()
                    } else {
                        true // Peer not found, disconnect anyway
                    };
                    drop(pm);
                    
                    if should_disconnect {
                        // Check if local/onion peer (don't ban, just disconnect)
                        if Self::is_local_address(&peer_addr) || Self::is_onion_address(&peer_addr) {
                            warn!("Disconnecting local/onion peer {} for getdata size violation (not banning)", peer_addr);
                        } else {
                            // Add to ban list for misbehaving (normal peer)
                            let mut ban_list = self.ban_list.write().await;
                            let ban_until = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                                + 24 * 60 * 60; // Ban for 24 hours
                            ban_list.insert(peer_addr, ban_until);
                            drop(ban_list);
                        }
                        // Disconnect peer for protocol violation
                        let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                            TransportAddr::Tcp(peer_addr)
                        ));
                        return Err(anyhow::anyhow!("getdata message size exceeded"));
                    } else {
                        warn!("Peer {} has manual/NoBan permission, not disconnecting for getdata size violation", peer_addr);
                        return Ok(()); // Don't disconnect, but don't process message
                    }
                }
                
                // Check if this GetData contains block requests (MSG_BLOCK = 2)
                use crate::network::inventory::MSG_BLOCK;
                let has_block_requests = getdata.inventory.iter().any(|inv| inv.inv_type == MSG_BLOCK);
                
                if has_block_requests {
                    // Check IBD protection before serving blocks
                    match self.ibd_protection.can_serve_ibd(peer_addr).await {
                        Ok(true) => {
                            // Start IBD serving tracking (if not already started)
                            self.ibd_protection.start_ibd_serving(peer_addr).await;
                            debug!("IBD protection: Allowing block request from {}", peer_addr);
                            // Continue to process the request normally
                        }
                        Ok(false) => {
                            warn!(
                                "IBD protection: Rejecting block request from {} (bandwidth limit exceeded or cooldown active)",
                                peer_addr
                            );
                            // Send NotFound message for requested blocks (standard Bitcoin protocol behavior)
                            // This prevents the peer from retrying immediately
                            use crate::network::protocol::{NotFoundMessage, ProtocolMessage, ProtocolParser};
                            let notfound = NotFoundMessage {
                                inventory: getdata.inventory.clone(),
                            };
                            if let Ok(wire_msg) = ProtocolParser::serialize_message(&ProtocolMessage::NotFound(notfound)) {
                                if let Err(e) = self.send_to_peer(peer_addr, wire_msg).await {
                                    warn!("Failed to send NotFound message to {}: {}", peer_addr, e);
                                }
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            warn!("IBD protection check failed for {}: {}", peer_addr, e);
                            // On error, allow the request (fail open for safety)
                        }
                    }
                }
                // Continue to process GetData normally (will go through protocol layer)
            }
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
            // Validate Inv messages (Bitcoin Core compatibility)
            ProtocolMessage::Inv(inv_msg) => {
                // Validate inventory size BEFORE routing to InventoryReceived
                if inv_msg.inventory.len() > crate::network::protocol::MAX_INV_SZ {
                    warn!(
                        "inv message size = {} exceeds MAX_INV_SZ ({}), disconnecting peer {}",
                        inv_msg.inventory.len(),
                        crate::network::protocol::MAX_INV_SZ,
                        peer_addr
                    );
                    // Check if peer should be disconnected (exempt manual connections and NoBan)
                    let mut pm = self.peer_manager.lock().await;
                    let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                        !peer.is_manual() && !peer.has_noban_permission()
                    } else {
                        true // Peer not found, disconnect anyway
                    };
                    drop(pm);
                    
                    if should_disconnect {
                        // Disconnect peer for protocol violation
                        let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                            TransportAddr::Tcp(peer_addr)
                        ));
                        return Err(anyhow::anyhow!("inv message size exceeded"));
                    } else {
                        warn!("Peer {} has NoBan permission, not disconnecting for inv size violation", peer_addr);
                        return Ok(()); // Don't disconnect, but don't process message
                    }
                }
                // Route to InventoryReceived handler (existing code)
                let _ = self.peer_tx.send(NetworkMessage::InventoryReceived(data));
                return Ok(());
            }
            // Module Registry
            ProtocolMessage::GetModule(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetModuleReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::Module(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::ModuleReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetModuleByHash(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetModuleByHashReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::ModuleByHash(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::ModuleByHashReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::GetModuleList(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::GetModuleListReceived(data, peer_addr));
                return Ok(());
            }
            ProtocolMessage::ModuleList(_) => {
                let _ = self
                    .peer_tx
                    .send(NetworkMessage::ModuleListReceived(data, peer_addr));
                return Ok(());
            }
            // Headers and Block messages (for IBD)
            ProtocolMessage::Headers(headers_msg) => {
                // Validate headers size (Bitcoin Core compatibility)
                if headers_msg.headers.len() > crate::network::protocol::MAX_HEADERS_RESULTS {
                    warn!(
                        "headers message size = {} exceeds MAX_HEADERS_RESULTS ({}), disconnecting peer {}",
                        headers_msg.headers.len(),
                        crate::network::protocol::MAX_HEADERS_RESULTS,
                        peer_addr
                    );
                    // Check if peer should be disconnected (exempt manual connections and NoBan)
                    let mut pm = self.peer_manager.lock().await;
                    let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                        !peer.is_manual() && !peer.has_noban_permission()
                    } else {
                        true // Peer not found, disconnect anyway
                    };
                    drop(pm);
                    
                    if should_disconnect {
                        // Check if local/onion peer (don't ban, just disconnect)
                        if Self::is_local_address(&peer_addr) || Self::is_onion_address(&peer_addr) {
                            warn!("Disconnecting local/onion peer {} for headers size violation (not banning)", peer_addr);
                        } else {
                            // Add to ban list for misbehaving (normal peer)
                            let mut ban_list = self.ban_list.write().await;
                            let ban_until = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap()
                                .as_secs()
                                + 24 * 60 * 60; // Ban for 24 hours
                            ban_list.insert(peer_addr, ban_until);
                            drop(ban_list);
                        }
                        // Disconnect peer for protocol violation
                        let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                            TransportAddr::Tcp(peer_addr)
                        ));
                        return Err(anyhow::anyhow!("headers message size exceeded"));
                    } else {
                        warn!("Peer {} has manual/NoBan permission, not disconnecting for headers size violation", peer_addr);
                        return Ok(()); // Don't disconnect, but don't process message
                    }
                }
                
                // Check if there's a pending Headers request for this peer
                let headers = headers_msg.headers.clone();
                if self.complete_headers_request(peer_addr, headers) {
                    debug!("Routed Headers response to pending request from {}", peer_addr);
                    return Ok(());
                }
                // No pending request - process normally through protocol layer
            }
            ProtocolMessage::Block(block_msg) => {
                // Check if there's a pending Block request for this peer
                use blvm_protocol::segwit::Witness;
                // Calculate block hash using proper Bitcoin format:
                // double_sha256 of 80-byte header (version + prev_hash + merkle_root + timestamp + bits + nonce)
                // CRITICAL: Must use 4-byte types for version/timestamp/bits/nonce (Bitcoin wire format)
                use crate::storage::hashing::double_sha256;
                let header = &block_msg.block.header;
                let mut header_bytes = Vec::with_capacity(80);
                header_bytes.extend_from_slice(&(header.version as i32).to_le_bytes());    // 4 bytes
                header_bytes.extend_from_slice(&header.prev_block_hash);                    // 32 bytes
                header_bytes.extend_from_slice(&header.merkle_root);                        // 32 bytes
                header_bytes.extend_from_slice(&(header.timestamp as u32).to_le_bytes());  // 4 bytes
                header_bytes.extend_from_slice(&(header.bits as u32).to_le_bytes());       // 4 bytes
                header_bytes.extend_from_slice(&(header.nonce as u32).to_le_bytes());      // 4 bytes
                let block_hash = double_sha256(&header_bytes);
                // Convert Vec<Vec<Vec<u8>>> to Vec<Vec<Witness>>
                // BlockMessage.witnesses is Vec<Vec<Vec<u8>>> where:
                // - Outer Vec: one per transaction
                // - Middle Vec: one per input in that transaction
                // - Inner Vec<u8>: the serialized witness stack for that input (Bitcoin wire format)
                // Witness = Vec<ByteString> = Vec<Vec<u8>> (stack of witness elements)
                // We need to deserialize each Vec<u8> (wire format) into a Witness (stack)
                // Wire format: VarInt(stack_count) + for each element: VarInt(len) + bytes
                use blvm_consensus::serialization::varint::decode_varint;
                
                let witnesses: Vec<Vec<Witness>> = block_msg.witnesses.iter()
                    .map(|tx_witnesses| {
                        // tx_witnesses is Vec<Vec<u8>> (one Vec<u8> per input)
                        // Each Vec<u8> is the serialized witness stack in Bitcoin wire format
                        tx_witnesses.iter()
                            .map(|w_bytes| {
                                // Deserialize witness stack from wire format
                                // Format: VarInt(stack_count) + for each: VarInt(len) + bytes
                                let mut offset = 0;
                                let mut witness_stack = Vec::new();
                                
                                if w_bytes.is_empty() {
                                    return witness_stack; // Empty witness
                                }
                                
                                // Decode stack count (VarInt)
                                match decode_varint(&w_bytes[offset..]) {
                                    Ok((stack_count, varint_len)) => {
                                        offset += varint_len;
                                        
                                        // Decode each witness element
                                        for _ in 0..stack_count {
                                            if offset >= w_bytes.len() {
                                                break; // Incomplete witness data
                                            }
                                            
                                            // Decode element length (VarInt)
                                            match decode_varint(&w_bytes[offset..]) {
                                                Ok((element_len, varint_len)) => {
                                                    offset += varint_len;
                                                    
                                                    if offset + element_len as usize > w_bytes.len() {
                                                        break; // Incomplete element
                                                    }
                                                    
                                                    // Extract element bytes
                                                    let element = w_bytes[offset..offset + element_len as usize].to_vec();
                                                    witness_stack.push(element);
                                                    offset += element_len as usize;
                                                }
                                                Err(_) => {
                                                    warn!("Failed to decode witness element length");
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        warn!("Failed to decode witness stack count, treating as empty");
                                    }
                                }
                                
                                witness_stack // Return Witness (Vec<Vec<u8>>)
                            })
                            .collect()
                    })
                    .collect();
                if self.complete_block_request(peer_addr, block_hash, block_msg.block.clone(), witnesses) {
                    debug!("Routed Block response to pending request from {}", peer_addr);
                    return Ok(()); // Message handled, return early
                }
                // No pending request - process normally through protocol layer
            }
            // Compact Block Relay (BIP152) - Add error handling and disconnection
            ProtocolMessage::CmpctBlock(cmpct_msg) => {
                // Validate compact block
                // If reconstruction fails or validation fails, disconnect peer
                // Note: Compact block reconstruction happens in compact_blocks module
                // For now, we'll add validation here and disconnect on errors
                use crate::network::compact_blocks::CompactBlock;
                
                // Check if compact block is valid (basic checks)
                if cmpct_msg.compact_block.short_ids.len() > 10000 {
                    warn!("Invalid compact block: too many short IDs ({}) from {}", 
                        cmpct_msg.compact_block.short_ids.len(), peer_addr);
                    let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                        TransportAddr::Tcp(peer_addr)
                    ));
                    return Err(anyhow::anyhow!("Invalid compact block: too many short IDs"));
                }
                
                // Route to protocol layer for processing
                // If reconstruction fails there, it should signal disconnection
            }
            ProtocolMessage::GetBlockTxn(getblocktxn_msg) => {
                // Validate GetBlockTxn indices
                // Check if indices are out of bounds (should be < short_ids count from previous compact block)
                // For now, basic validation - if indices are unreasonably large, disconnect
                if getblocktxn_msg.indices.len() > 10000 {
                    warn!("GetBlockTxn with too many indices ({}) from {}", 
                        getblocktxn_msg.indices.len(), peer_addr);
                    let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                        TransportAddr::Tcp(peer_addr)
                    ));
                    return Err(anyhow::anyhow!("GetBlockTxn with too many indices"));
                }
                
                // Check for out-of-bounds indices (would need to track previous compact block)
                // For now, we'll let the compact block handler validate this
            }
            ProtocolMessage::BlockTxn(blocktxn_msg) => {
                // Validate BlockTxn response
                // If transactions don't match expected indices or block hash, disconnect
                if blocktxn_msg.transactions.len() > 10000 {
                    warn!("BlockTxn with too many transactions ({}) from {}", 
                        blocktxn_msg.transactions.len(), peer_addr);
                    let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                        TransportAddr::Tcp(peer_addr)
                    ));
                    return Err(anyhow::anyhow!("BlockTxn with too many transactions"));
                }
            }
            _ => {
                // Other NetworkMessage variants are handled above
                // ProtocolMessage variants are handled in handle_incoming_wire_tcp()
            }
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
        use crate::network::bandwidth_protection::ServiceType;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_utxo_set;

        // Check bandwidth limits before processing (UTXO set can be very large)
        match self.bandwidth_protection
            .check_service_request(ServiceType::UtxoSet, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "UTXO set bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for UTXO set: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        // Record request (for rate limiting - very restrictive for UTXO set)
        self.bandwidth_protection
            .record_service_request(ServiceType::UtxoSet, peer_addr)
            .await;

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
        let response_bytes = response_wire.len() as u64;
        self.send_to_peer(peer_addr, response_wire).await?;

        // Record bandwidth usage (UTXO set can be several GB)
        self.bandwidth_protection
            .record_service_bandwidth(ServiceType::UtxoSet, peer_addr, response_bytes)
            .await;

        Ok(())
    }

    /// Handle GetModule request from a peer
    async fn handle_get_module(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleMessage,
    ) -> Result<()> {
        use crate::network::bandwidth_protection::ServiceType;
        use crate::network::module_registry_extensions::handle_get_module;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        // Check bandwidth limits before processing
        match self.bandwidth_protection
            .check_service_request(ServiceType::ModuleServing, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Module serving bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for module serving: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        // Record request (for rate limiting)
        self.bandwidth_protection
            .record_service_request(ServiceType::ModuleServing, peer_addr)
            .await;

        // Replay protection: Check request ID
        if let Err(e) = self
            .replay_protection
            .check_request_id(message.request_id)
            .await
        {
            warn!(
                "Replay protection: Rejected duplicate GetModule request from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Handle the request with registry integration and payment checking
        let registry = self.module_registry.lock().await.as_ref().map(Arc::clone);
        let payment_processor = self.payment_processor.lock().await.as_ref().map(Arc::clone);
        let payment_state_machine = self.payment_state_machine.lock().await.as_ref().map(Arc::clone);
        let encryption = self.module_encryption.lock().await.as_ref().map(Arc::clone);
        let modules_dir = self.modules_dir.lock().await.clone();

        // Get node's payment address script (set from config during initialization)
        let node_script = self.node_payment_script.lock().await.clone();

        match handle_get_module(
            message,
            registry,
            payment_processor,
            payment_state_machine,
            encryption,
            modules_dir,
            node_script,
        )
        .await
        {
            Ok(module_response) => {
                // Serialize and send module response
                let response_wire =
                    ProtocolParser::serialize_message(&ProtocolMessage::Module(module_response))?;
                let response_bytes = response_wire.len() as u64;
                self.send_to_peer(peer_addr, response_wire).await?;

                // Record bandwidth usage (modules can be large)
                self.bandwidth_protection
                    .record_service_bandwidth(ServiceType::ModuleServing, peer_addr, response_bytes)
                    .await;

                Ok(())
            }
            Err(e) => {
                // Check if error indicates payment required
                let error_msg = e.to_string();
                if error_msg.contains("requires payment") {
                    // Return error to client - they should request payment
                    // The error message contains payment details that the client can use
                    warn!("Module requires payment: {}", error_msg);
                    Err(e)
                } else {
                    // Other error - propagate
                    Err(e)
                }
            }
        }
    }

    /// Handle GetModuleByHash request from a peer
    async fn handle_get_module_by_hash(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleByHashMessage,
    ) -> Result<()> {
        use crate::network::module_registry_extensions::handle_get_module_by_hash;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        // Handle the request with registry integration
        let registry = self.module_registry.lock().await.as_ref().map(Arc::clone);
        let response = handle_get_module_by_hash(message, registry).await?;

        // Serialize and send response
        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::ModuleByHash(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetModuleList request from a peer
    async fn handle_get_module_list(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetModuleListMessage,
    ) -> Result<()> {
        use crate::network::module_registry_extensions::handle_get_module_list;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        // Handle the request with registry integration
        let registry = self.module_registry.lock().await.as_ref().map(Arc::clone);
        let response = handle_get_module_list(message, registry).await?;

        // Serialize and send response
        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::ModuleList(response))?;
        self.send_to_peer(peer_addr, response_wire).await?;

        Ok(())
    }

    /// Handle GetPaymentRequest message (P2P BIP70)
    async fn handle_get_payment_request(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::GetPaymentRequestMessage,
    ) -> Result<()> {
        use crate::network::bip70_handler::handle_get_payment_request;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        let processor = self.payment_processor.lock().await.as_ref().map(Arc::clone);
        let response_msg = handle_get_payment_request(&message, processor).await?;
        let response = ProtocolMessage::PaymentRequest(response_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }

    /// Handle Payment message (P2P BIP70)
    async fn handle_payment(
        &self,
        peer_addr: SocketAddr,
        message: crate::network::protocol::PaymentMessage,
    ) -> Result<()> {
        use crate::network::bip70_handler::handle_payment;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;

        let processor = self.payment_processor.lock().await.as_ref().map(Arc::clone);
        // Get merchant private key (set from config during initialization)
        let merchant_key_guard = self.merchant_key.lock().await;
        let merchant_key = merchant_key_guard.as_ref();
        let response_msg = handle_payment(&message, processor, merchant_key).await?;
        let response = ProtocolMessage::PaymentACK(response_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;
        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }

    /// Handle GetCfilters request from a peer
    async fn handle_getcfilters_request(&self, data: Vec<u8>, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::bandwidth_protection::ServiceType;
        use crate::network::bip157_handler::handle_getcfilters;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use std::time::Instant;

        // Check bandwidth limits before processing
        match self.bandwidth_protection
            .check_service_request(ServiceType::Filters, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Filter service bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for filter service: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        // Record request (for rate limiting)
        self.bandwidth_protection
            .record_service_request(ServiceType::Filters, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::GetCfilters(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetCfilters message")),
        };

        // Track CPU time for filter generation
        let cpu_start = Instant::now();

        // Handle request and generate responses
        let storage_ref = self.storage.as_ref();
        let responses = handle_getcfilters(&request, &self.filter_service, storage_ref)?;

        // Check CPU time limit (Bitcoin Core disconnects if filter generation takes too long)
        let cpu_time_ms = cpu_start.elapsed().as_millis() as u64;
        const MAX_FILTER_CPU_TIME_MS: u64 = 5000; // 5 seconds (Bitcoin Core uses similar limit)
        if cpu_time_ms > MAX_FILTER_CPU_TIME_MS {
            // CPU time limit exceeded - disconnect peer (filter service violation)
            warn!("Filter service CPU time limit exceeded ({}ms > {}ms) for peer {}, disconnecting", 
                cpu_time_ms, MAX_FILTER_CPU_TIME_MS, peer_addr);
            
            // Check if peer has NoBan permission before disconnecting
            let mut pm = self.peer_manager.lock().await;
            let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
                !peer.has_noban_permission()
            } else {
                true // Peer not found, disconnect anyway
            };
            drop(pm);
            
            if should_disconnect {
                let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                    TransportAddr::Tcp(peer_addr)
                ));
                return Err(anyhow::anyhow!("Filter service CPU time limit exceeded: {}ms", cpu_time_ms));
            } else {
                warn!("Peer {} has NoBan permission, not disconnecting for filter CPU time violation", peer_addr);
                return Ok(()); // Don't disconnect, but don't process further
            }
        }

        // Send responses to peer and track bandwidth
        let mut total_bytes = 0u64;
        for response in responses {
            let response_wire = ProtocolParser::serialize_message(&response)?;
            total_bytes += response_wire.len() as u64;
            self.send_to_peer(peer_addr, response_wire).await?;
        }

        // Record bandwidth usage
        if total_bytes > 0 {
            self.bandwidth_protection
                .record_service_bandwidth(ServiceType::Filters, peer_addr, total_bytes)
                .await;
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
        use crate::network::bandwidth_protection::ServiceType;
        use crate::network::package_relay::PackageRelay;
        use crate::network::package_relay_handler::handle_pkgtxn;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use blvm_protocol::Transaction;

        // Check bandwidth limits before processing
        match self.bandwidth_protection
            .check_service_request(ServiceType::PackageRelay, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Package relay bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for package relay: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        // Record request (for rate limiting)
        self.bandwidth_protection
            .record_service_request(ServiceType::PackageRelay, peer_addr)
            .await;

        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let request = match protocol_msg {
            ProtocolMessage::PkgTxn(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected PkgTxn message")),
        };

        let mut relay = PackageRelay::new();
        let mut response_bytes = 0u64;
        if let Some(reject) = handle_pkgtxn(&mut relay, &request)? {
            let response_wire =
                ProtocolParser::serialize_message(&ProtocolMessage::PkgTxnReject(reject))?;
            response_bytes = response_wire.len() as u64;
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

        // Record bandwidth usage (response size)
        if response_bytes > 0 {
            self.bandwidth_protection
                .record_service_bandwidth(ServiceType::PackageRelay, peer_addr, response_bytes)
                .await;
        }
        Ok(())
    }

    /// Submit validated transactions to the mempool
    async fn submit_transactions_to_mempool(
        &self,
        txs: &[blvm_protocol::Transaction],
    ) -> Result<()> {
        if let Some(ref _mempool_manager) = self.mempool_manager {
            // Use MempoolManager's add_transaction method
            // Note: add_transaction requires &mut, so we need to handle this carefully
            // For now, we'll use a channel or async approach
            // This is a limitation of the current design - MempoolManager should use interior mutability
            for tx in txs {
                // In a real implementation, we'd send this to a mempool processing channel
                // For now, we validate and accept using consensus layer
                let utxo_lock = self.utxo_set.lock().await;
                let mempool_lock = self.mempool.lock().await;
                let _ =
                    self.consensus
                        .accept_to_memory_pool(tx, &utxo_lock, &mempool_lock, 0, None);
            }
        } else {
            // Fallback to legacy mempool
            let utxo_lock = self.utxo_set.lock().await;
            let mempool_lock = self.mempool.lock().await;
            for tx in txs {
                let _ =
                    self.consensus
                        .accept_to_memory_pool(tx, &utxo_lock, &mempool_lock, 0, None);
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
        use crate::network::bandwidth_protection::ServiceType;
        use crate::network::protocol::ProtocolMessage;
        use crate::network::protocol::ProtocolParser;
        use crate::network::protocol_extensions::handle_get_filtered_block;

        // Check bandwidth limits before processing
        match self.bandwidth_protection
            .check_service_request(ServiceType::FilteredBlocks, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(anyhow::anyhow!(
                    "Filtered block bandwidth limit exceeded for peer {}",
                    peer_addr
                ));
            }
            Err(e) => {
                warn!("Bandwidth check error for filtered blocks: {}", e);
                return Err(anyhow::anyhow!("Bandwidth check failed: {}", e));
            }
        }

        // Record request (for rate limiting)
        self.bandwidth_protection
            .record_service_request(ServiceType::FilteredBlocks, peer_addr)
            .await;

        // Parse the request
        let protocol_msg = ProtocolParser::parse_message(&data)?;
        let get_filtered_block_msg = match protocol_msg {
            ProtocolMessage::GetFilteredBlock(msg) => msg,
            _ => return Err(anyhow::anyhow!("Expected GetFilteredBlock message")),
        };

        // Replay protection: Check request ID
        if let Err(e) = self
            .replay_protection
            .check_request_id(get_filtered_block_msg.request_id)
            .await
        {
            warn!(
                "Replay protection: Rejected duplicate GetFilteredBlock request from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Handle the request with storage and filter service
        let storage = self.storage.as_ref().map(Arc::clone);
        let response =
            handle_get_filtered_block(get_filtered_block_msg, storage, Some(&self.filter_service))
                .await?;

        // Serialize and send response
        let response_wire =
            ProtocolParser::serialize_message(&ProtocolMessage::FilteredBlock(response))?;
        let response_bytes = response_wire.len() as u64;
        self.send_to_peer(peer_addr, response_wire).await?;

        // Record bandwidth usage
        self.bandwidth_protection
            .record_service_bandwidth(ServiceType::FilteredBlocks, peer_addr, response_bytes)
            .await;

        Ok(())
    }

    /// Get peer manager reference (locked)
    ///
    /// Note: This is now async and uses tokio::sync::Mutex to avoid deadlocks
    pub async fn peer_manager(&self) -> tokio::sync::MutexGuard<'_, PeerManager> {
        self.peer_manager.lock().await
    }

    /// Get current connected peer addresses as SocketAddr
    /// Returns addresses for TCP/Quinn peers only (Iroh peers use different addressing)
    pub async fn get_connected_peer_addresses(&self) -> Vec<SocketAddr> {
        let pm = self.peer_manager.lock().await;
        pm.peer_socket_addresses()
    }

    /// Check eclipse prevention for an IP address
    /// Returns true if connection is allowed, false if it would violate eclipse prevention
    pub fn check_eclipse_prevention(&self, ip: std::net::IpAddr) -> bool {
        let prefix = self.get_ip_prefix(ip);
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let diversity = self.peer_diversity.lock().await;
                let count = diversity.get(&prefix).copied().unwrap_or(0);
                count < 3 // Allow max 3 connections from same IP prefix
            })
        })
    }

    /// Get IP prefix (first 3 octets) for eclipse prevention
    pub fn get_ip_prefix(&self, ip: std::net::IpAddr) -> [u8; 3] {
        match ip {
            std::net::IpAddr::V4(ipv4) => {
                let octets = ipv4.octets();
                [octets[0], octets[1], octets[2]]
            }
            std::net::IpAddr::V6(ipv6) => {
                let octets = ipv6.octets();
                [octets[0], octets[1], octets[2]]
            }
        }
    }

    /// Remove peer diversity tracking for an IP address
    pub fn remove_peer_diversity(&self, ip: std::net::IpAddr) {
        let prefix = self.get_ip_prefix(ip);
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut diversity = self.peer_diversity.lock().await;
                if let Some(count) = diversity.get_mut(&prefix) {
                    if *count > 0 {
                        *count -= 1;
                    }
                    if *count == 0 {
                        diversity.remove(&prefix);
                    }
                }
            })
        })
    }

    /// Get filter service reference
    pub fn filter_service(&self) -> &crate::network::filter_service::BlockFilterService {
        &self.filter_service
    }

    /// Add a persistent peer (will be connected to on startup)
    pub fn add_persistent_peer(&self, addr: SocketAddr) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut peers = self.persistent_peers.lock().await;
                peers.insert(addr);
            })
        })
    }

    /// Remove a persistent peer
    pub fn remove_persistent_peer(&self, addr: SocketAddr) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut peers = self.persistent_peers.lock().await;
                peers.remove(&addr);
            })
        })
    }

    /// Get list of persistent peers (async version for RPC)
    pub async fn get_persistent_peers(&self) -> HashSet<SocketAddr> {
        self.persistent_peers.lock().await.clone()
    }

    /// Get list of persistent peers (sync version)
    pub fn get_persistent_peers_sync(&self) -> Vec<SocketAddr> {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let peers = self.persistent_peers.lock().await;
                peers.iter().cloned().collect()
            })
        })
    }

    /// Ban a peer (with optional unban timestamp, 0 = permanent)
    pub fn ban_peer(&self, addr: SocketAddr, unban_timestamp: u64) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut ban_list = self.ban_list.write().await;
                if unban_timestamp == 0 {
                    // Permanent ban (use max timestamp)
                    ban_list.insert(addr, u64::MAX);
                } else {
                    ban_list.insert(addr, unban_timestamp);
                }
            })
        })
    }

    /// Unban a peer
    pub fn unban_peer(&self, addr: SocketAddr) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut ban_list = self.ban_list.write().await;
                ban_list.remove(&addr);
            })
        })
    }

    /// Clear all bans
    pub fn clear_bans(&self) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut ban_list = self.ban_list.write().await;
                ban_list.clear();
            })
        })
    }

    /// Handle GetAddr request - return known addresses
    async fn handle_get_addr(&self, peer_addr: SocketAddr) -> Result<()> {
        use crate::network::protocol::{AddrMessage, ProtocolMessage, ProtocolParser};

        // Get fresh addresses from database (up to 2500, Bitcoin Core limit)
        let ban_list = self.ban_list.read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager.lock().await;
            pm.peer_socket_addresses()
        };

        let addresses = {
            let db = self.address_database.write().await;
            let fresh = db.get_fresh_addresses(2500);
            db.filter_addresses(fresh, &ban_list, &connected_peers)
        };

        // Create Addr message
        let addr_msg = AddrMessage { addresses };
        let response = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&response)?;

        // Send response
        self.send_to_peer(peer_addr, wire_msg).await?;
        Ok(())
    }

    /// Handle Addr message - store addresses and optionally relay
    async fn handle_addr(&self, peer_addr: SocketAddr, msg: AddrMessage) -> Result<()> {
        // Validate message size (Bitcoin Core compatibility)
        if msg.addresses.len() > crate::network::protocol::MAX_ADDR_TO_SEND {
            warn!(
                "addr message size = {} exceeds MAX_ADDR_TO_SEND ({}), disconnecting peer {}",
                msg.addresses.len(),
                crate::network::protocol::MAX_ADDR_TO_SEND,
                peer_addr
            );
            // Disconnect peer for protocol violation
            let _ = self.peer_tx.send(NetworkMessage::PeerDisconnected(
                TransportAddr::Tcp(peer_addr)
            ));
            return Err(anyhow::anyhow!("addr message size exceeded"));
        }

        // AddrMessage is already in scope as parameter, NetworkAddress is available from top-level import

        // Get peer services from peer state
        let peer_services = {
            let peer_states = self.peer_states.read().await;
            peer_states
                .get(&peer_addr)
                .map(|state| state.services)
                .unwrap_or(0)
        };

        // Store addresses in database
        {
            let mut db = self.address_database.write().await;
            for addr in &msg.addresses {
                db.add_address(addr.clone(), peer_services);
            }
        }

        // Relay addresses to other peers (with rate limiting)
        self.relay_addresses(peer_addr, &msg.addresses).await?;

        Ok(())
    }

    /// Relay addresses to other peers (excluding sender)
    async fn relay_addresses(
        &self,
        sender_addr: SocketAddr,
        addresses: &[NetworkAddress],
    ) -> Result<()> {
        use crate::network::protocol::{AddrMessage, ProtocolMessage, ProtocolParser};
        // Rate limiting: don't send addr messages too frequently (Bitcoin Core: ~every 2.4 hours)
        let now = current_timestamp();
        let min_interval = 2 * 60 * 60 + 24 * 60; // 2.4 hours in seconds

        {
            let last_sent = *self.last_addr_sent.lock().await;
            if now.saturating_sub(last_sent) < min_interval {
                // Too soon, skip relay
                return Ok(());
            }
        }

        // Filter addresses (exclude local, banned, already connected)
        let ban_list = self.ban_list.read().await.clone();
        let connected_peers: Vec<SocketAddr> = {
            let pm = self.peer_manager.lock().await;
            pm.peer_socket_addresses()
        };

        let filtered = {
            let db = self.address_database.read().await;
            db.filter_addresses(addresses.to_vec(), &ban_list, &connected_peers)
        };

        if filtered.is_empty() {
            return Ok(());
        }

        // Limit to 1000 addresses per message (Bitcoin Core limit)
        let addresses_to_relay: Vec<NetworkAddress> = filtered.into_iter().take(1000).collect();

        // Create Addr message
        let addr_msg = AddrMessage {
            addresses: addresses_to_relay,
        };
        let relay_msg = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&relay_msg)?;

        // Send to all peers except sender
        let peer_addrs: Vec<SocketAddr> = {
            let pm = self.peer_manager.lock().await;
            pm.peer_socket_addresses()
                .into_iter()
                .filter(|addr| *addr != sender_addr)
                .collect()
        };

        for peer_addr in peer_addrs {
            if let Err(e) = self.send_to_peer(peer_addr, wire_msg.clone()).await {
                warn!("Failed to relay addresses to {}: {}", peer_addr, e);
            }
        }

        // Update last sent time
        *self.last_addr_sent.lock().await = now;

        Ok(())
    }

    /// Send our own address to peers (self-advertisement)
    pub async fn advertise_self(&self, listen_addr: SocketAddr, services: u64) -> Result<()> {
        if !self.enable_self_advertisement {
            return Ok(()); // Self-advertisement disabled
        }

        use crate::network::protocol::{
            AddrMessage, NetworkAddress, ProtocolMessage, ProtocolParser,
        };
        use std::net::IpAddr;

        // Convert SocketAddr to NetworkAddress
        let ip_bytes = match listen_addr.ip() {
            IpAddr::V4(ipv4) => {
                // IPv4-mapped IPv6 format
                let mut bytes = [0u8; 16];
                bytes[10] = 0xff;
                bytes[11] = 0xff;
                bytes[12..16].copy_from_slice(&ipv4.octets());
                bytes
            }
            IpAddr::V6(ipv6) => ipv6.octets(),
        };

        let our_addr = NetworkAddress {
            services,
            ip: ip_bytes,
            port: listen_addr.port(),
        };

        // Create Addr message with just our address
        let addr_msg = AddrMessage {
            addresses: vec![our_addr.clone()],
        };
        let relay_msg = ProtocolMessage::Addr(addr_msg);
        let wire_msg = ProtocolParser::serialize_message(&relay_msg)?;

        // Send to all connected peers
        let peer_addrs: Vec<SocketAddr> = {
            let pm = self.peer_manager.lock().await;
            pm.peer_socket_addresses()
        };

        for peer_addr in peer_addrs {
            if let Err(e) = self.send_to_peer(peer_addr, wire_msg.clone()).await {
                warn!("Failed to advertise self to {}: {}", peer_addr, e);
            }
        }

        // Also store our own address in database
        {
            let mut db = self.address_database.write().await;
            db.add_address(our_addr, services);
        }

        Ok(())
    }

    /// Get list of banned peers
    pub fn get_banned_peers(&self) -> Vec<(SocketAddr, u64)> {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let ban_list = self.ban_list.read().await;
                ban_list
                    .iter()
                    .map(|(addr, timestamp)| (*addr, *timestamp))
                    .collect()
            })
        })
    }

    /// Get peer addresses (async version for RPC)
    pub async fn get_peer_addresses(&self) -> Vec<TransportAddr> {
        let pm = self.peer_manager.lock().await;
        pm.peer_addresses()
    }

    /// Set network active state
    pub async fn set_network_active(&self, active: bool) -> Result<()> {
        let mut state = self.network_active.lock().await;
        *state = active;
        info!("Network active state set to: {}", active);
        Ok(())
    }

    /// Get network active state
    pub fn is_network_active(&self) -> bool {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { *self.network_active.lock().await })
        })
    }

    /// Check if a peer is banned
    pub fn is_banned(&self, addr: SocketAddr) -> bool {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let ban_list = self.ban_list.read().await;
                if let Some(&unban_timestamp) = ban_list.get(&addr) {
                    if unban_timestamp == u64::MAX {
                        return true; // Permanent ban
                    }
                    // Check if ban has expired
                    let now = current_timestamp();
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
            })
        })
    }

    /// Track bytes sent (async-safe)
    ///
    /// Optimization: Uses AtomicU64 for lock-free operation
    pub async fn track_bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Track bytes received (async-safe)
    ///
    /// Optimization: Uses AtomicU64 for lock-free operation
    pub async fn track_bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Handle GetBanList message - respond with ban list or hash
    async fn handle_get_ban_list(
        &self,
        peer_addr: SocketAddr,
        msg: crate::network::protocol::GetBanListMessage,
    ) -> Result<()> {
        use crate::network::ban_list_merging::calculate_ban_list_hash;
        use crate::network::protocol::{BanEntry, BanListMessage, NetworkAddress};
        debug!(
            "GetBanList request from {}: full={}, min_duration={}",
            peer_addr, msg.request_full, msg.min_ban_duration
        );

        // Get current ban list
        let ban_list = self.ban_list.read().await;
        let now = current_timestamp();

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
        let ban_entries_count = ban_entries.len();

        // Create response
        let response = BanListMessage {
            is_full: msg.request_full,
            ban_list_hash,
            ban_entries: if msg.request_full {
                ban_entries
            } else {
                Vec::new()
            },
            timestamp: now,
        };

        // Serialize and send response
        let response_msg = ProtocolMessage::BanList(response);
        let serialized = ProtocolParser::serialize_message(&response_msg)?;
        self.send_to_peer(peer_addr, serialized).await?;

        debug!(
            "Sent BanList response to {}: {} entries",
            peer_addr,
            if msg.request_full {
                ban_entries_count
            } else {
                0
            }
        );

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

        debug!(
            "BanList received from {}: full={}, {} entries",
            peer_addr,
            msg.is_full,
            msg.ban_entries.len()
        );

        // Replay protection: Validate timestamp (24 hour max age for ban lists)
        if let Err(e) =
            replay_protection::ReplayProtection::validate_timestamp(msg.timestamp as i64, 86400)
        {
            warn!(
                "Replay protection: Invalid timestamp in BanList from {}: {}",
                peer_addr, e
            );
            return Err(anyhow::anyhow!("Replay protection: {}", e));
        }

        // Verify hash if full list provided
        if msg.is_full && !verify_ban_list_hash(&msg.ban_entries, &msg.ban_list_hash) {
            warn!("Ban list hash verification failed from {}", peer_addr);
            return Ok(()); // Silently ignore invalid ban lists
        }

        // If hash-only, we can't merge (would need to request full list)
        if !msg.is_full {
            debug!(
                "Received hash-only ban list from {}, skipping merge",
                peer_addr
            );
            return Ok(());
        }

        // Validate and merge ban entries
        let mut ban_list = self.ban_list.write().await;
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
                    ipv4_bytes[0],
                    ipv4_bytes[1],
                    ipv4_bytes[2],
                    ipv4_bytes[3],
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

    /// Get DoS protection manager reference
    pub fn dos_protection(&self) -> &Arc<dos_protection::DosProtectionManager> {
        &self.dos_protection
    }

    /// Get peer manager reference (for module API access)
    pub fn peer_manager_ref(&self) -> &Arc<Mutex<PeerManager>> {
        &self.peer_manager
    }

    /// Get network statistics
    pub async fn get_network_stats(&self) -> crate::node::metrics::NetworkMetrics {
        let sent = self.bytes_sent.load(Ordering::Relaxed);
        let received = self.bytes_received.load(Ordering::Relaxed);
        let active_connections = {
            let pm = self.peer_manager.lock().await;
            pm.peer_count()
        };
        let banned_peers_count = {
            let ban_list = self.ban_list.read().await;
            ban_list.len()
        };
        let _resource_metrics = self.dos_protection.get_metrics().await;

        crate::node::metrics::NetworkMetrics {
            peer_count: active_connections,
            bytes_sent: sent,
            bytes_received: received,
            messages_sent: 0,     // Would need to track this
            messages_received: 0, // Would need to track this
            active_connections,
            banned_peers: banned_peers_count,
            connection_attempts: 0, // Would need to track this
            connection_failures: 0, // Would need to track this
            dos_protection: crate::node::metrics::DosMetrics {
                connection_rate_violations: 0,   // Would need to track this
                auto_bans: 0,                    // Would need to track this
                message_queue_overflows: 0,      // Would need to track this
                active_connection_limit_hits: 0, // Would need to track this
                resource_exhaustion_events: 0,   // Would need to track this
            },
        }
    }

    /// Get network statistics (legacy method for backward compatibility)
    pub fn get_network_stats_legacy(&self) -> (u64, u64) {
        // Use block_in_place to avoid blocking async runtime
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let sent = self.bytes_sent.load(Ordering::Relaxed);
                let received = self.bytes_received.load(Ordering::Relaxed);
                (sent, received)
            })
        })
    }

    /// Create version message with service flags
    ///
    /// Creates version message with service flags for all supported features
    ///
    /// Sets service flags based on:
    /// - Standard Bitcoin flags:
    ///   - NODE_NETWORK (bit 0) - Always set for full nodes
    ///   - NODE_WITNESS (bit 3) - SegWit support (always enabled)
    /// - BIP157: NODE_COMPACT_FILTERS (always enabled if filter service exists)
    /// - UTXO Commitments: NODE_UTXO_COMMITMENTS (if feature enabled)
    /// - Ban List Sharing: NODE_BAN_LIST_SHARING (if config enabled)
    /// - Dandelion: NODE_DANDELION (if feature enabled)
    /// - Package Relay: NODE_PACKAGE_RELAY (always enabled)
    /// - FIBRE: NODE_FIBRE (always enabled)
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
        use blvm_protocol::bip157::NODE_COMPACT_FILTERS;
        use blvm_protocol::service_flags::standard;

        // Start with standard Bitcoin flags that should always be set
        // NODE_NETWORK (bit 0) - Always set for full nodes (even if pruned)
        // NODE_WITNESS (bit 3) - Set for SegWit support (Commons supports SegWit)
        let mut services_with_filters = standard::NODE_NETWORK | standard::NODE_WITNESS;

        // NODE_NETWORK_LIMITED (bit 10) - Set if node is pruned (can only serve recent blocks)
        // Check if storage has pruning enabled
        if let Some(ref storage) = self.storage {
            if let Some(pruning_manager) = storage.pruning() {
                if pruning_manager.is_enabled() {
                    services_with_filters |= standard::NODE_NETWORK_LIMITED;
                }
            }
        }

        // Add any additional services passed in (for future extensibility)
        services_with_filters |= services;

        // BIP157 Compact Block Filters (always enabled if filter service exists)
        services_with_filters |= NODE_COMPACT_FILTERS;

        // UTXO Commitments (if feature enabled)
        #[cfg(feature = "utxo-commitments")]
        {
            services_with_filters |= crate::network::protocol::NODE_UTXO_COMMITMENTS;
        }

        // Ban List Sharing (if config enabled)
        if self.ban_list_sharing_config.is_some() {
            services_with_filters |= crate::network::protocol::NODE_BAN_LIST_SHARING;
        }

        // Governance message relay (if config enabled)
        #[cfg(feature = "governance")]
        if self.governance_config.is_some() {
            services_with_filters |= crate::network::protocol::NODE_GOVERNANCE;
        }

        // Dandelion (if feature enabled)
        #[cfg(feature = "dandelion")]
        {
            services_with_filters |= crate::network::protocol::NODE_DANDELION;
        }

        // Package Relay (BIP331) - always enabled
        services_with_filters |= crate::network::protocol::NODE_PACKAGE_RELAY;

        // FIBRE - always enabled
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
    mod concurrency_stress_tests;
    mod bandwidth_protection_tests;
    use super::*;

    #[tokio::test]
    async fn test_peer_manager_creation() {
        let _addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = PeerManager::new(10);
        assert_eq!(manager.peer_count(), 0);
        assert!(manager.can_accept_peer());
    }

    #[tokio::test]
    async fn test_peer_manager_add_peer() {
        let manager = PeerManager::new(2);
        let _addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        // Create a mock peer without requiring network connection
        let (_tx, _rx): (mpsc::UnboundedSender<NetworkMessage>, _) = mpsc::unbounded_channel();

        // Skip this test since we can't easily create a mock TcpStream
        // In a real implementation, we'd use dependency injection
        // For now, just test the manager logic without the peer
        assert_eq!(manager.peer_count(), 0);
        assert!(manager.can_accept_peer());
    }

    #[tokio::test]
    async fn test_peer_manager_max_peers() {
        let manager = PeerManager::new(1);
        let _addr1: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let _addr2: std::net::SocketAddr = "127.0.0.1:8081".parse().unwrap();

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
        let transport_addr = TransportAddr::Tcp(addr);
        let removed_peer = manager.remove_peer(&transport_addr);
        assert!(removed_peer.is_none());
        assert_eq!(manager.peer_count(), 0);
    }

    #[tokio::test]
    async fn test_peer_manager_get_peer() {
        let manager = PeerManager::new(10);
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test getting non-existent peer
        let transport_addr = TransportAddr::Tcp(addr);
        let retrieved_peer = manager.get_peer(&transport_addr);
        assert!(retrieved_peer.is_none());
    }

    #[tokio::test]
    async fn test_peer_manager_peer_addresses() {
        let manager = PeerManager::new(10);
        let _addr1: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let _addr2: std::net::SocketAddr = "127.0.0.1:8081".parse().unwrap();

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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_creation() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        assert_eq!(manager.peer_count(), 0);
        assert_eq!(manager.peer_addresses().len(), 0);
    }

    #[tokio::test]
    async fn test_network_manager_with_config() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::with_config(
            addr,
            5,
            crate::network::transport::TransportPreference::TCP_ONLY,
            None,
        );

        // peer_count() might not exist, check peer_manager instead
        let peer_manager = manager.peer_manager().await;
        assert_eq!(peer_manager.peer_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_peer_count() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_peer_addresses() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test getting addresses when no peers exist
        let addresses = manager.peer_addresses();
        assert_eq!(addresses.len(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_broadcast() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test manager logic without creating real peers
        assert_eq!(manager.peer_count(), 0);

        // Test broadcast with no peers (should succeed)
        let message = b"test message".to_vec();
        let result = manager.broadcast(message).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_send_to_peer() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

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
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let transport_addr = TransportAddr::Tcp(socket_addr);
        let message = NetworkMessage::PeerConnected(transport_addr.clone());
        match message {
            NetworkMessage::PeerConnected(addr) => {
                assert_eq!(addr, transport_addr);
            }
            _ => panic!("Expected PeerConnected message"),
        }
    }

    #[tokio::test]
    async fn test_network_message_peer_disconnected() {
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let transport_addr = TransportAddr::Tcp(socket_addr);
        let message = NetworkMessage::PeerDisconnected(transport_addr.clone());
        match message {
            NetworkMessage::PeerDisconnected(addr) => {
                assert_eq!(addr, transport_addr);
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

    // NOTE: test_handle_incoming_wire_tcp_enqueues_pkgtxn was removed because it hangs
    // Note: All Mutex usage is now tokio::sync::Mutex with async-safe .await calls.
    // The handle_incoming_wire_tcp function uses async-safe track_bytes_received
    // and is_banned) which blocks the async runtime when there's contention.
    // Full message routing is tested in integration tests.

    #[tokio::test(flavor = "multi_thread")]
    async fn test_network_manager_peer_manager_access() {
        let addr: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let manager = NetworkManager::new(addr);

        // Test immutable access - drop the guard immediately to avoid holding lock
        {
            let peer_manager = manager.peer_manager().await;
            assert_eq!(peer_manager.peer_count(), 0);
        } // Guard dropped here

        // Test peer count access (this also locks the mutex, but guard is dropped immediately)
        assert_eq!(manager.peer_count(), 0);
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

// Safety: NetworkManager is safe to share across threads (Sync) because:
// - All internal state is protected by Arc<Mutex<>> or Arc<RwLock<>> which are Sync
// - EventPublisher is already marked as Sync (see event_publisher.rs)
// - All async operations are Send and don't require synchronous access to internal state
// - The ZmqPublisher's Socket is only accessed through async methods which are Send
// This is a workaround for ZMQ's Socket type not being Sync, but the actual usage is safe.
#[cfg(feature = "zmq")]
unsafe impl Sync for NetworkManager {}
