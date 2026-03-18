//! Network manager: coordinates all network operations.
//!
//! Supports multiple transports (TCP, Quinn, Iroh) based on configuration.
//! Extracted from network/mod.rs for Phase 1 of complexity remediation.

use crate::network::protocol::cmd;
use crate::network::protocol::{AddrMessage, NetworkAddress, ProtocolMessage, ProtocolParser};
use crate::network::tcp_transport::TcpTransport;
use crate::network::transport::{Transport, TransportAddr, TransportListener, TransportPreference};
use crate::node::mempool::MempoolManager;
use crate::storage::Storage;
use crate::utils::{current_timestamp, current_timestamp_nanos};
use anyhow::Result;
use blvm_protocol::mempool::Mempool;
use blvm_protocol::{BitcoinProtocolEngine, ConsensusProof, UtxoSet};
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::net::IpAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::SystemTime;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, error, info, warn};

use super::lan_discovery;
use super::peer;
use super::peer_manager::{PeerByteRateLimiter, PeerManager, PeerRateLimiter};
use super::transport;
use super::NetworkMessage;
use hex;
use secp256k1;

/// Get wire command string for ProtocolMessage (for MessageReceived event)
fn protocol_message_type(msg: &ProtocolMessage) -> &'static str {
    match msg {
        ProtocolMessage::Version(_) => cmd::VERSION,
        ProtocolMessage::Verack => cmd::VERACK,
        ProtocolMessage::Ping(_) => cmd::PING,
        ProtocolMessage::Pong(_) => cmd::PONG,
        ProtocolMessage::GetHeaders(_) => cmd::GETHEADERS,
        ProtocolMessage::Headers(_) => cmd::HEADERS,
        ProtocolMessage::GetBlocks(_) => cmd::GETBLOCKS,
        ProtocolMessage::Block(_) => cmd::BLOCK,
        ProtocolMessage::GetData(_) => cmd::GETDATA,
        ProtocolMessage::Inv(_) => cmd::INV,
        ProtocolMessage::NotFound(_) => cmd::NOTFOUND,
        ProtocolMessage::Tx(_) => cmd::TX,
        ProtocolMessage::FeeFilter(_) => cmd::FEEFILTER,
        ProtocolMessage::SendCmpct(_) => cmd::SENDCMPCT,
        ProtocolMessage::CmpctBlock(_) => cmd::CMPCTBLOCK,
        ProtocolMessage::GetBlockTxn(_) => cmd::GETBLOCKTXN,
        ProtocolMessage::BlockTxn(_) => cmd::BLOCKTXN,
        ProtocolMessage::GetUTXOSet(_) => cmd::GETUTXOSET,
        ProtocolMessage::UTXOSet(_) => cmd::UTXOSET,
        ProtocolMessage::GetUTXOProof(_) => cmd::GETUTXOPROOF,
        ProtocolMessage::UTXOProof(_) => cmd::UTXOPROOF,
        ProtocolMessage::GetFilteredBlock(_) => cmd::GETFILTEREDBLOCK,
        ProtocolMessage::FilteredBlock(_) => cmd::FILTEREDBLOCK,
        ProtocolMessage::GetCfilters(_) => cmd::GETCFILTERS,
        ProtocolMessage::Cfilter(_) => cmd::CFILTER,
        ProtocolMessage::GetCfheaders(_) => cmd::GETCFHEADERS,
        ProtocolMessage::Cfheaders(_) => cmd::CFHEADERS,
        ProtocolMessage::GetCfcheckpt(_) => cmd::GETCFCHECKPT,
        ProtocolMessage::Cfcheckpt(_) => cmd::CFCHECKPT,
        ProtocolMessage::GetPaymentRequest(_) => cmd::GETPAYMENTREQUEST,
        ProtocolMessage::PaymentRequest(_) => cmd::PAYMENTREQUEST,
        ProtocolMessage::Payment(_) => cmd::PAYMENT,
        ProtocolMessage::PaymentACK(_) => cmd::PAYMENTACK,
        #[cfg(feature = "ctv")]
        ProtocolMessage::PaymentProof(_) => cmd::PAYMENTPROOF,
        ProtocolMessage::SettlementNotification(_) => cmd::SETTLEMENTNOTIFICATION,
        ProtocolMessage::SendPkgTxn(_) => cmd::SENDPKGTXN,
        ProtocolMessage::PkgTxn(_) => cmd::PKGTXN,
        ProtocolMessage::PkgTxnReject(_) => cmd::PKGTXNREJECT,
        ProtocolMessage::GetBanList(_) => cmd::GETBANLIST,
        ProtocolMessage::BanList(_) => cmd::BANLIST,
        ProtocolMessage::EconomicNodeRegistration(_) => cmd::ECONREG,
        ProtocolMessage::EconomicNodeVeto(_) => cmd::ECONVETO,
        ProtocolMessage::EconomicNodeStatus(_) => cmd::ECONSTATUS,
        ProtocolMessage::EconomicNodeForkDecision(_) => cmd::ECONFORK,
        ProtocolMessage::GetAddr => cmd::GETADDR,
        ProtocolMessage::Addr(_) => cmd::ADDR,
        ProtocolMessage::GetModule(_) => cmd::GETMODULE,
        ProtocolMessage::Module(_) => cmd::MODULE,
        ProtocolMessage::GetModuleByHash(_) => cmd::GETMODULEBYHASH,
        ProtocolMessage::ModuleByHash(_) => cmd::MODULEBYHASH,
        ProtocolMessage::ModuleInv(_) => cmd::MODULEINV,
        ProtocolMessage::GetModuleList(_) => cmd::GETMODULELIST,
        ProtocolMessage::ModuleList(_) => cmd::MODULELIST,
        ProtocolMessage::MeshPacket(_) => cmd::MESH,
    }
}

/// Network manager that coordinates all network operations
///
/// Supports multiple transports (TCP, Quinn, Iroh) based on configuration.
pub struct NetworkManager {
    peer_manager: Arc<Mutex<PeerManager>>,
    tcp_transport: TcpTransport,
    #[cfg(feature = "quinn")]
    quinn_transport: std::sync::Mutex<Option<Arc<crate::network::quinn_transport::QuinnTransport>>>,
    #[cfg(feature = "iroh")]
    iroh_transport: std::sync::Mutex<Option<crate::network::iroh_transport::IrohTransport>>,
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
    module_registry:
        Arc<tokio::sync::Mutex<Option<Arc<crate::module::registry::client::ModuleRegistry>>>>,
    /// Payment processor for BIP70 payments (HTTP and P2P)
    payment_processor:
        Arc<tokio::sync::Mutex<Option<Arc<crate::payment::processor::PaymentProcessor>>>>,
    /// Payment state machine for unified payment coordination
    payment_state_machine:
        Arc<tokio::sync::Mutex<Option<Arc<crate::payment::state_machine::PaymentStateMachine>>>>,
    /// Merchant private key for signing payment ACKs (optional)
    merchant_key: Arc<tokio::sync::Mutex<Option<secp256k1::SecretKey>>>,
    /// Node payment address script (for module downloads - 10% fee)
    node_payment_script: Arc<tokio::sync::Mutex<Option<Vec<u8>>>>,
    /// Module encryption for encrypted module serving
    module_encryption:
        Arc<tokio::sync::Mutex<Option<Arc<crate::module::encryption::ModuleEncryption>>>>,
    /// Modules directory for encrypted/decrypted module storage
    modules_dir: Arc<tokio::sync::Mutex<Option<std::path::PathBuf>>>,
    /// Event publisher for module event notifications (optional)
    event_publisher:
        Arc<tokio::sync::Mutex<Option<Arc<crate::node::event_publisher::EventPublisher>>>>,
    /// FIBRE relay manager (for fast block relay)
    #[cfg(feature = "fibre")]
    fibre_relay: Option<Arc<Mutex<super::fibre::FibreRelay>>>,
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
    pending_headers_requests: Arc<
        Mutex<
            HashMap<
                SocketAddr,
                std::collections::VecDeque<
                    tokio::sync::oneshot::Sender<Vec<blvm_protocol::BlockHeader>>,
                >,
            >,
        >,
    >,
    /// Pending Block requests (for IBD)
    /// Key: (peer_ip, block_hash) - uses IpAddr to match regardless of port (inbound vs outbound)
    pending_block_requests: Arc<
        Mutex<
            HashMap<
                (IpAddr, blvm_protocol::Hash),
                tokio::sync::oneshot::Sender<(
                    blvm_protocol::Block,
                    Vec<Vec<blvm_protocol::segwit::Witness>>,
                )>,
            >,
        >,
    >,
    /// DoS protection manager
    dos_protection: Arc<super::dos_protection::DosProtectionManager>,
    /// IBD bandwidth protection manager
    ibd_protection: Arc<super::ibd_protection::IbdProtectionManager>,
    /// Unified bandwidth protection manager (extends IBD protection)
    bandwidth_protection: Arc<super::bandwidth_protection::BandwidthProtectionManager>,
    /// Replay protection for custom protocol messages
    replay_protection: Arc<super::replay_protection::ReplayProtection>,
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
    address_database: Arc<RwLock<super::address_db::AddressDatabase>>,
    /// Last time we sent addr message (Unix timestamp)
    last_addr_sent: Arc<Mutex<u64>>,
    /// Enable self-advertisement (send own address to peers)
    enable_self_advertisement: bool,
    /// Request timeout configuration
    request_timeout_config: Arc<crate::config::RequestTimeoutConfig>,
    /// Protocol limits (message size, addr, inv, headers)
    protocol_limits: Arc<crate::config::ProtocolLimitsConfig>,
    /// Background task interval configuration
    background_task_config: Arc<crate::config::BackgroundTaskConfig>,
    /// Peer reconnection queue (exponential backoff)
    /// Maps SocketAddr to (attempts, last_attempt_timestamp, quality_score)
    peer_reconnection_queue: Arc<Mutex<HashMap<SocketAddr, (u32, u64, f64)>>>,
    /// Stratum V2 connections (miners on dedicated port 3333)
    /// Maps peer SocketAddr to channel for sending responses
    #[cfg(feature = "stratum-v2")]
    stratum_connections: Arc<tokio::sync::RwLock<HashMap<SocketAddr, mpsc::UnboundedSender<Vec<u8>>>>>,
}

/// Pending request metadata (pub(crate) for utxo_commitments_client)
pub(crate) struct PendingRequest {
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

impl PendingRequest {
    pub(crate) fn timestamp(&self) -> u64 {
        self.timestamp
    }

    pub(crate) fn send_response(self, data: Vec<u8>) {
        let _ = self.sender.send(data);
    }
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

        let dos_protection = Arc::new(super::dos_protection::DosProtectionManager::with_ban_settings(
            dos_config.max_connections_per_window,
            dos_config.window_seconds,
            dos_config.max_message_queue_size,
            dos_config.max_active_connections,
            dos_config.auto_ban_threshold,
            dos_config.ban_duration_seconds,
        ));

        // Initialize IBD protection (bandwidth exhaustion attack mitigation)
        let ibd_protection = if let Some(ibd_config) =
            config.and_then(|c| c.ibd_protection.as_ref())
        {
            let mut ibd_protection_config = super::ibd_protection::IbdProtectionConfig::default();
            // Convert GB to bytes
            ibd_protection_config.max_bandwidth_per_peer_per_day =
                ibd_config.max_bandwidth_per_peer_per_day_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_bandwidth_per_peer_per_hour =
                ibd_config.max_bandwidth_per_peer_per_hour_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_bandwidth_per_ip_per_day =
                ibd_config.max_bandwidth_per_ip_per_day_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_bandwidth_per_ip_per_hour =
                ibd_config.max_bandwidth_per_ip_per_hour_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_bandwidth_per_subnet_per_day =
                ibd_config.max_bandwidth_per_subnet_per_day_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_bandwidth_per_subnet_per_hour =
                ibd_config.max_bandwidth_per_subnet_per_hour_gb * 1024 * 1024 * 1024;
            ibd_protection_config.max_concurrent_ibd_serving =
                ibd_config.max_concurrent_ibd_serving;
            ibd_protection_config.ibd_request_cooldown_seconds =
                ibd_config.ibd_request_cooldown_seconds;
            ibd_protection_config.suspicious_reconnection_threshold =
                ibd_config.suspicious_reconnection_threshold;
            ibd_protection_config.reputation_ban_threshold = ibd_config.reputation_ban_threshold;
            ibd_protection_config.enable_emergency_throttle = ibd_config.enable_emergency_throttle;
            ibd_protection_config.emergency_throttle_percent =
                ibd_config.emergency_throttle_percent;
            Arc::new(super::ibd_protection::IbdProtectionManager::with_config(
                ibd_protection_config,
            ))
        } else {
            Arc::new(super::ibd_protection::IbdProtectionManager::new())
        };

        // Initialize unified bandwidth protection (extends IBD protection)
        let bandwidth_protection = Arc::new(super::bandwidth_protection::BandwidthProtectionManager::new(
            Arc::clone(&ibd_protection),
        ));

        // Use config for address database
        let addr_db_config_default = crate::config::AddressDatabaseConfig::default();
        let addr_db_config = config
            .and_then(|c| c.address_database.as_ref())
            .unwrap_or(&addr_db_config_default);

        let address_database = Arc::new(RwLock::new(super::address_db::AddressDatabase::with_expiration(
            addr_db_config.max_addresses,
            addr_db_config.expiration_seconds,
        )));

        // Use config for request timeouts
        let timeout_config_default = crate::config::RequestTimeoutConfig::default();
        let timeout_config = config
            .and_then(|c| c.request_timeouts.as_ref())
            .unwrap_or(&timeout_config_default);
        let request_timeout_config = Arc::new(timeout_config.clone());

        // Use config for protocol limits
        let limits_config_default = crate::config::ProtocolLimitsConfig::default();
        let limits_config = config
            .and_then(|c| c.protocol_limits.as_ref())
            .unwrap_or(&limits_config_default);
        let protocol_limits = Arc::new(limits_config.clone());
        let tcp_transport = TcpTransport::with_max_message_length(limits_config.max_protocol_message_length);

        // Use config for background task intervals
        let bg_config_default = crate::config::BackgroundTaskConfig::default();
        let bg_config = config
            .and_then(|c| c.background_tasks.as_ref())
            .unwrap_or(&bg_config_default);
        let background_task_config = Arc::new(bg_config.clone());

        Self {
            peer_manager: Arc::new(Mutex::new(PeerManager::new(max_peers))),
            peer_diversity: Arc::new(Mutex::new(HashMap::new())),
            tcp_transport,
            #[cfg(feature = "quinn")]
            quinn_transport: std::sync::Mutex::new(None),
            #[cfg(feature = "iroh")]
            iroh_transport: std::sync::Mutex::new(None),
            transport_preference: preference,
            peer_tx,
            peer_rx,
            filter_service: crate::network::filter_service::BlockFilterService::new(),
            consensus: ConsensusProof::new(),
            utxo_set: Arc::new(Mutex::new(UtxoSet::default())),
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
            replay_protection: Arc::new({
                let rp_config = config.and_then(|c| c.replay_protection.as_ref());
                if let Some(c) = rp_config {
                    super::replay_protection::ReplayProtection::with_config(
                        std::time::Duration::from_secs(c.cleanup_interval_secs),
                        std::time::Duration::from_secs(c.message_id_expiration_secs),
                        std::time::Duration::from_secs(c.request_id_expiration_secs),
                        c.future_tolerance_secs,
                    )
                } else {
                    super::replay_protection::ReplayProtection::new()
                }
            }),
            pending_ban_shares: Arc::new(Mutex::new(Vec::new())),
            ban_list_sharing_config: config.and_then(|c| c.ban_list_sharing.clone()),
            #[cfg(feature = "governance")]
            governance_config: config.and_then(|c| c.governance.clone()),
            address_database,
            last_addr_sent: Arc::new(Mutex::new(0)),
            enable_self_advertisement: config.map(|c| c.enable_self_advertisement).unwrap_or(true),
            request_timeout_config,
            protocol_limits,
            background_task_config,
            peer_reconnection_queue: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(feature = "stratum-v2")]
            stratum_connections: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
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
        let mut fibre_relay = super::fibre::FibreRelay::with_config(fibre_config.clone());

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
        super::fibre::start_chunk_processor(fibre_relay_arc.clone(), chunk_rx);

        self.fibre_relay = Some(fibre_relay_arc);

        info!("FIBRE relay initialized on UDP {}", udp_addr);
        Ok(())
    }

    /// Get FIBRE relay (if initialized)
    #[cfg(feature = "fibre")]
    pub fn fibre_relay(&self) -> Option<Arc<Mutex<super::fibre::FibreRelay>>> {
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
            IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => ip.is_loopback() || ip.is_unspecified(),
        }
    }

    /// Check if address is onion (Tor). Always returns false.
    /// Onion detection would require NetworkAddress (AddrV2) layer; SocketAddr is IP:port only.
    /// Onion connections arrive via proxy; we see the proxy's address, not the .onion hostname.
    pub(crate) fn is_onion_address(_addr: &SocketAddr) -> bool {
        false
    }

    /// Handle protocol violation: disconnect peer, optionally ban.
    /// Returns Ok(()) if peer has manual/NoBan (don't disconnect, but don't process message).
    /// Returns Err if we disconnected (caller should return early).
    pub(crate) async fn disconnect_for_protocol_violation(
        &self,
        peer_addr: SocketAddr,
        violation_msg: &str,
        ban_for_violation: bool,
    ) -> Result<(), anyhow::Error> {
        let mut pm = self.peer_manager.lock().await;
        let should_disconnect = if let Some(peer) = pm.get_peer(&TransportAddr::Tcp(peer_addr)) {
            !peer.is_manual() && !peer.has_noban_permission()
        } else {
            true
        };
        drop(pm);

        if should_disconnect {
            if ban_for_violation
                && !Self::is_local_address(&peer_addr)
                && !Self::is_onion_address(&peer_addr)
            {
                let ban_until = current_timestamp()
                    + 24 * 60 * 60;
                let mut ban_list = self.ban_list.write().await;
                ban_list.insert(peer_addr, ban_until);
                drop(ban_list);
            } else if ban_for_violation {
                warn!(
                    "Disconnecting local/onion peer {} for {} (not banning)",
                    peer_addr, violation_msg
                );
            }
            let _ = self
                .peer_tx
                .send(NetworkMessage::PeerDisconnected(TransportAddr::Tcp(peer_addr)));
            Err(anyhow::anyhow!("{}", violation_msg))
        } else {
            warn!(
                "Peer {} has manual/NoBan permission, not disconnecting for {}",
                peer_addr, violation_msg
            );
            Ok(())
        }
    }

    /// Check Tx message rate limits and spam. Returns true if the tx should be dropped.
    async fn should_drop_tx(&self, peer_addr: SocketAddr, data: &[u8]) -> bool {
        use super::bandwidth_protection::ServiceType;
        let tx_bytes = data.len() as u64;

        match self
            .bandwidth_protection
            .check_service_request(ServiceType::TransactionRelay, peer_addr)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    "Transaction relay bandwidth limit exceeded for IP {} (peer {}), dropping transaction",
                    peer_addr.ip(), peer_addr
                );
                return true;
            }
            Err(e) => {
                warn!("Bandwidth check error for transaction relay: {}", e);
            }
        }

        let (burst, rate) = self
            .mempool_policy_config
            .as_ref()
            .map(|cfg| (cfg.tx_rate_limit_burst, cfg.tx_rate_limit_per_sec))
            .unwrap_or((10, 1));

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
            return true;
        }

        let (byte_burst, byte_rate) = self
            .mempool_policy_config
            .as_ref()
            .map(|cfg| (cfg.tx_byte_rate_burst, cfg.tx_byte_rate_limit))
            .unwrap_or((1_000_000, 100_000));

        let should_process_bytes = {
            let mut byte_rates = self.peer_tx_byte_rate_limiters.lock().await;
            let limiter = byte_rates
                .entry(peer_addr)
                .or_insert_with(|| PeerByteRateLimiter::new(byte_burst, byte_rate));
            limiter.check_and_consume(tx_bytes)
        };

        if !should_process_bytes {
            warn!(
                "Transaction byte rate limit exceeded for peer {} ({} bytes), dropping transaction",
                peer_addr, tx_bytes
            );
            return true;
        }

        self.bandwidth_protection
            .record_service_bandwidth(ServiceType::TransactionRelay, peer_addr, tx_bytes)
            .await;

        use blvm_protocol::spam_filter::SpamFilter;
        if data.len() <= 4 {
            return false;
        }
        if let Ok(tx_msg) =
            bincode::deserialize::<crate::network::protocol::TxMessage>(&data[4..])
        {
            let tx = &tx_msg.transaction;
            let spam_filter = SpamFilter::new();
            let result = spam_filter.is_spam(tx);

            if result.is_spam {
                let mut violations = self.peer_spam_violations.lock().await;
                let violation_count = violations.entry(peer_addr).or_insert(0);
                *violation_count += 1;
                let current_count = *violation_count;
                drop(violations);

                if let Some(ban_config) = self.spam_ban_config.as_ref() {
                    if current_count >= ban_config.spam_ban_threshold {
                        let unban_timestamp =
                            crate::utils::current_timestamp() + ban_config.spam_ban_duration_seconds;
                        warn!(
                            "Auto-banning peer {} for spam violations ({} violations, unban at {})",
                            peer_addr, current_count, unban_timestamp
                        );
                        self.ban_peer(peer_addr, unban_timestamp);
                        return true;
                    }
                }

                debug!(
                    "Spam transaction from peer {} (violation count: {})",
                    peer_addr, current_count
                );
            }
        }

        false
    }

    /// Evict extra outbound peers if we have too many
    /// Standard policy protects up to MAX_OUTBOUND_PEERS_TO_PROTECT peers
    /// based on block announcement recency
    #[allow(dead_code)]
    async fn evict_extra_outbound_peers(&self) {
        use crate::network::peer_manager::MAX_OUTBOUND_PEERS_TO_PROTECT_FROM_DISCONNECT;

        let mut pm = self.peer_manager.lock().await;

        // Get all outbound peers with their last block announcement time
        let mut outbound_peers: Vec<(TransportAddr, u64)> = pm
            .peers()
            .iter()
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
            let now = current_timestamp();
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
            let _ = self
                .peer_tx
                .send(NetworkMessage::PeerDisconnected(addr.clone()));
            pm = self.peer_manager.lock().await; // Re-acquire lock for next iteration
        }
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

    /// Start the network manager
    pub async fn start(&self, listen_addr: SocketAddr) -> Result<()> {
        info!(
            "Starting network manager with transport preference: {:?}",
            self.transport_preference
        );

        #[cfg(feature = "quinn")]
        super::startup::init_quinn_transport(self).await?;

        #[cfg(feature = "iroh")]
        super::startup::init_iroh_transport(self).await?;

        super::startup::start_tcp_listener(self, listen_addr).await?;

        #[cfg(feature = "quinn")]
        super::startup::start_quinn_listener(self, listen_addr).await?;

        #[cfg(feature = "iroh")]
        super::startup::start_iroh_listener(self, listen_addr).await?;

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
                    pending_req.send_response(response);
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

    /// Key for block request matching: IpAddr so inbound (diff port) matches outbound
    fn block_request_key(addr: SocketAddr) -> IpAddr {
        let ip = addr.ip();
        if let std::net::IpAddr::V6(v6) = ip {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return std::net::IpAddr::V4(v4);
            }
        }
        ip
    }

    /// Register a pending Block request
    /// Returns a receiver that will receive the Block response
    pub fn register_block_request(
        &self,
        peer_addr: SocketAddr,
        block_hash: blvm_protocol::Hash,
    ) -> tokio::sync::oneshot::Receiver<(
        blvm_protocol::Block,
        Vec<Vec<blvm_protocol::segwit::Witness>>,
    )> {
        let key = (Self::block_request_key(peer_addr), block_hash);
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                self.pending_block_requests.lock().await.insert(key, tx);
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
        let key = (Self::block_request_key(peer_addr), block_hash);
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut pending = self.pending_block_requests.lock().await;
                if let Some(sender) = pending.remove(&key) {
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
                for (_addr, peer) in pm.peers().iter() {
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

    /// Send Stratum V2 message to peer. Checks stratum_connections first (port 3333),
    /// then falls back to P2P peer manager (port 8333).
    #[cfg(feature = "stratum-v2")]
    pub async fn send_stratum_v2_to_peer(
        &self,
        addr: SocketAddr,
        message: Vec<u8>,
    ) -> Result<()> {
        let conns = self.stratum_connections.read().await;
        if let Some(send_tx) = conns.get(&addr) {
            send_tx.send(message).map_err(|e| {
                anyhow::anyhow!("Stratum V2 send failed for {}: {}", addr, e)
            })?;
            return Ok(());
        }
        drop(conns);
        self.send_to_peer(addr, message).await
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

        // Extract message type for event publishing (before message is moved)
        let msg_type = if message_len >= 16 {
            String::from_utf8_lossy(&message[4..16]).trim_end_matches('\0').to_string()
        } else {
            "raw".to_string()
        };

        // Bandwidth Protection: Record bandwidth for various message types
        // Check message command (Bitcoin wire format: 4-byte magic + 12-byte command + 4-byte length + payload)
        if message_len >= 16 {
            let command = msg_type.as_str();

            // Get SocketAddr for bandwidth protection (needed for tracking)
            let peer_socket_addr_opt: Option<SocketAddr> = match &addr {
                TransportAddr::Tcp(sock) => Some(*sock),
                #[cfg(feature = "quinn")]
                TransportAddr::Quinn(sock) => Some(*sock),
                #[cfg(feature = "iroh")]
                TransportAddr::Iroh(_) => {
                    // For Iroh, try to get from socket_to_transport mapping
                    // If not found, skip bandwidth tracking (Iroh uses different addressing)
                    self.socket_to_transport
                        .lock()
                        .await
                        .iter()
                        .find_map(|(sock, &ref ta)| if ta == addr { Some(*sock) } else { None })
                }
            };

            if let Some(peer_socket_addr) = peer_socket_addr_opt {
                // Record bandwidth for IBD-serving messages
                if command == cmd::HEADERS || command == cmd::BLOCK {
                    // Record bandwidth for IBD protection
                    self.ibd_protection
                        .record_bandwidth(peer_socket_addr, message_len as u64)
                        .await;
                    debug!(
                        "IBD protection: Recorded {} bytes for {} message to {}",
                        message_len, command, peer_socket_addr
                    );
                }
                // Record bandwidth for transaction relay
                else if command == "tx" {
                    use super::bandwidth_protection::ServiceType;
                    // Record transaction relay bandwidth (per-IP/subnet limits)
                    self.bandwidth_protection
                        .record_service_bandwidth(
                            ServiceType::TransactionRelay,
                            peer_socket_addr,
                            message_len as u64,
                        )
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

        // Publish MessageSent event for module subscribers
        if let Some(ref ep) = *self.event_publisher.lock().await {
            let addr_str = format!("{:?}", addr);
            let ep_clone = Arc::clone(ep);
            let msg_type_clone = msg_type.clone();
            tokio::spawn(async move {
                ep_clone
                    .publish_message_sent(&addr_str, &msg_type_clone, message_len)
                    .await;
            });
        }

        // Record send after message is sent
        let mut pm = self.peer_manager.lock().await;
        if let Some(peer) = pm.get_peer_mut(&addr) {
            peer.record_send(message_len);
        }
        Ok(())
    }

    /// Send ping message to all connected peers
    pub async fn ping_all_peers(&self) -> Result<()> {
        use crate::network::protocol::{PingMessage, ProtocolMessage, ProtocolParser};
        // Generate nonce for ping
        let nonce = current_timestamp_nanos();

        let ping_msg = ProtocolMessage::Ping(PingMessage { nonce });
        let wire_msg = ProtocolParser::serialize_message(&ping_msg)?;

        let peer_addrs = {
            let mut pm = self.peer_manager.lock().await;
            // Record ping in peer state before sending
            for (addr, peer) in pm.peers_mut().iter_mut() {
                peer.record_ping_sent(nonce);
            }
            pm.peer_addresses()
        };

        for addr in peer_addrs {
            // Convert TransportAddr to SocketAddr for send_to_peer, or use send_to_peer_by_transport
            let addr_clone = addr.clone();
            match addr_clone {
                super::transport::TransportAddr::Tcp(sock) => {
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
                        debug!(
                            "process_pending_messages: received message {:?}",
                            match &msg {
                                NetworkMessage::PeerConnected(addr) =>
                                    format!("PeerConnected({addr:?})"),
                                NetworkMessage::PeerDisconnected(addr) =>
                                    format!("PeerDisconnected({addr:?})"),
                                NetworkMessage::RawMessageReceived(data, addr) => format!(
                                    "RawMessageReceived({} bytes from {})",
                                    data.len(),
                                    addr
                                ),
                                _ => "other".to_string(),
                            }
                        );
                        Some(msg)
                    }
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
                        crate::utils::current_timestamp() as i64,
                        addr_recv,
                        addr_from,
                        rand::random::<u64>(),
                        format!("/Bitcoin Commons:{}/", env!("CARGO_PKG_VERSION")),
                        start_height,
                        true,
                    );

                    match ProtocolParser::serialize_message(&ProtocolMessage::Version(version_msg))
                    {
                        Ok(wire_msg) => {
                            if let Err(e) =
                                self.send_to_peer_by_transport(addr.clone(), wire_msg).await
                            {
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
                    let addr_str = format!("{addr:?}");
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
                            .publish_peer_connected(&addr_str, transport_type, services, version)
                            .await;
                    });
                }
            }
            NetworkMessage::PeerDisconnected(addr) => {
                info!("Peer disconnected (during pending processing): {:?}", addr);
                let mut pm = self.peer_manager.lock().await;
                pm.remove_peer(&addr);
                drop(pm);

                let event_publisher_guard = self.event_publisher.lock().await;
                if let Some(ref event_publisher) = *event_publisher_guard {
                    let addr_str = format!("{addr:?}");
                    let event_pub_clone = Arc::clone(event_publisher);
                    tokio::spawn(async move {
                        event_pub_clone
                            .publish_peer_disconnected(&addr_str, "disconnected")
                            .await;
                    });
                }
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

            super::network_message_dispatch::handle_network_message(self, message).await?;
        }
        Ok(())
    }

    // NOTE: Network message handling match extracted to network_message_dispatch::handle_network_message

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
        // Valid Stratum V2 tags are in range 0x0001-0x0032 (see blvm-stratum-v2/src/messages.rs)
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
                    // Forward TLV part only (handle_message uses decode_raw, expects [tag][len][payload])
                    let _ = self.peer_tx.send(NetworkMessage::StratumV2MessageReceived(
                        data[4..].to_vec(),
                        peer_addr,
                    ));
                    return Ok(());
                }
            }
        }

        let parsed = ProtocolParser::parse_message(&data)?;

        // Publish MessageReceived event for module subscribers
        if let Some(ref ep) = *self.event_publisher.lock().await {
            let msg_type = protocol_message_type(&parsed);
            let peer_str = peer_addr.to_string();
            let size = data.len();
            let ep_clone = Arc::clone(ep);
            tokio::spawn(async move {
                ep_clone
                    .publish_message_received(&peer_str, msg_type, size, 0)
                    .await;
            });
        }

        if let ProtocolMessage::Tx(_) = &parsed {
            if self.should_drop_tx(peer_addr, &data).await {
                return Ok(());
            }
        }

        self.dispatch_protocol_message(peer_addr, &parsed, data).await
    }

    /// Submit validated transactions to the mempool
    pub(crate) async fn submit_transactions_to_mempool(
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
                drop(ban_list);

                // Publish PeerBanned event for module subscribers
                let ban_duration_secs = if unban_timestamp == 0 || unban_timestamp == u64::MAX {
                    0 // Permanent
                } else {
                    unban_timestamp.saturating_sub(crate::utils::current_timestamp())
                };
                if let Some(ref ep) = *self.event_publisher.lock().await {
                    ep.publish_peer_banned(&addr.to_string(), "ban", ban_duration_secs)
                        .await;
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
                drop(ban_list);

                // Publish PeerUnbanned event for module subscribers
                if let Some(ref ep) = *self.event_publisher.lock().await {
                    ep.publish_peer_unbanned(&addr.to_string()).await;
                }
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
    /// Get bytes sent (for tests)
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    pub async fn track_bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Track bytes received (async-safe)
    ///
    /// Optimization: Uses AtomicU64 for lock-free operation
    pub async fn track_bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Get DoS protection manager reference
    pub fn dos_protection(&self) -> &Arc<super::dos_protection::DosProtectionManager> {
        &self.dos_protection
    }

    /// Get peer manager reference (for module API access)
    pub fn peer_manager_ref(&self) -> &Arc<Mutex<PeerManager>> {
        &self.peer_manager
    }

    pub(crate) fn ban_list(&self) -> &Arc<RwLock<HashMap<SocketAddr, u64>>> {
        &self.ban_list
    }

    pub(crate) fn address_database(&self) -> &Arc<RwLock<super::address_db::AddressDatabase>> {
        &self.address_database
    }

    pub(crate) fn peer_tx(&self) -> &mpsc::UnboundedSender<NetworkMessage> {
        &self.peer_tx
    }

    pub(crate) fn last_addr_sent(&self) -> &Arc<Mutex<u64>> {
        &self.last_addr_sent
    }

    pub(crate) fn bandwidth_protection(
        &self,
    ) -> &Arc<super::bandwidth_protection::BandwidthProtectionManager> {
        &self.bandwidth_protection
    }

    pub(crate) fn storage(&self) -> &Option<Arc<Storage>> {
        &self.storage
    }

    pub(crate) fn replay_protection(&self) -> &Arc<super::replay_protection::ReplayProtection> {
        &self.replay_protection
    }

    pub(crate) fn module_registry(
        &self,
    ) -> &Arc<tokio::sync::Mutex<Option<Arc<crate::module::registry::client::ModuleRegistry>>>> {
        &self.module_registry
    }

    pub(crate) fn payment_processor(
        &self,
    ) -> &Arc<
        tokio::sync::Mutex<Option<Arc<crate::payment::processor::PaymentProcessor>>>,
    > {
        &self.payment_processor
    }

    pub(crate) fn payment_state_machine(
        &self,
    ) -> &Arc<
        tokio::sync::Mutex<Option<Arc<crate::payment::state_machine::PaymentStateMachine>>>,
    > {
        &self.payment_state_machine
    }

    pub(crate) fn module_encryption(
        &self,
    ) -> &Arc<
        tokio::sync::Mutex<Option<Arc<crate::module::encryption::ModuleEncryption>>>,
    > {
        &self.module_encryption
    }

    pub(crate) fn modules_dir(
        &self,
    ) -> &Arc<tokio::sync::Mutex<Option<std::path::PathBuf>>> {
        &self.modules_dir
    }

    pub(crate) fn node_payment_script(
        &self,
    ) -> &Arc<tokio::sync::Mutex<Option<Vec<u8>>>> {
        &self.node_payment_script
    }

    pub(crate) fn merchant_key(
        &self,
    ) -> &Arc<tokio::sync::Mutex<Option<secp256k1::SecretKey>>> {
        &self.merchant_key
    }

    pub(crate) fn event_publisher(
        &self,
    ) -> &Arc<tokio::sync::Mutex<Option<Arc<crate::node::event_publisher::EventPublisher>>>> {
        &self.event_publisher
    }

    #[cfg(feature = "stratum-v2")]
    pub(crate) fn stratum_connections(
        &self,
    ) -> &Arc<tokio::sync::RwLock<HashMap<SocketAddr, mpsc::UnboundedSender<Vec<u8>>>>> {
        &self.stratum_connections
    }

    #[cfg(feature = "governance")]
    pub(crate) fn governance_config(&self) -> &Option<crate::config::GovernanceConfig> {
        &self.governance_config
    }

    pub(crate) fn ibd_protection(&self) -> &Arc<super::ibd_protection::IbdProtectionManager> {
        &self.ibd_protection
    }

    pub(crate) fn peer_manager_mutex(&self) -> &Arc<Mutex<PeerManager>> {
        &self.peer_manager
    }

    pub(crate) fn peer_reconnection_queue(
        &self,
    ) -> &Arc<Mutex<HashMap<SocketAddr, (u32, u64, f64)>>> {
        &self.peer_reconnection_queue
    }

    pub(crate) fn connections_per_ip(
        &self,
    ) -> &Arc<Mutex<HashMap<std::net::IpAddr, usize>>> {
        &self.connections_per_ip
    }

    pub(crate) fn peer_message_rates(
        &self,
    ) -> &Arc<Mutex<HashMap<SocketAddr, PeerRateLimiter>>> {
        &self.peer_message_rates
    }

    pub(crate) fn peer_tx_rate_limiters(
        &self,
    ) -> &Arc<Mutex<HashMap<SocketAddr, PeerRateLimiter>>> {
        &self.peer_tx_rate_limiters
    }

    pub(crate) fn peer_tx_byte_rate_limiters(
        &self,
    ) -> &Arc<Mutex<HashMap<SocketAddr, PeerByteRateLimiter>>> {
        &self.peer_tx_byte_rate_limiters
    }

    pub(crate) fn peer_spam_violations(
        &self,
    ) -> &Arc<Mutex<HashMap<SocketAddr, usize>>> {
        &self.peer_spam_violations
    }

    pub(crate) fn tcp_transport(&self) -> &TcpTransport {
        &self.tcp_transport
    }

    #[cfg(feature = "quinn")]
    pub(crate) fn quinn_transport(
        &self,
    ) -> &std::sync::Mutex<Option<Arc<crate::network::quinn_transport::QuinnTransport>>> {
        &self.quinn_transport
    }

    #[cfg(feature = "iroh")]
    pub(crate) fn iroh_transport(
        &self,
    ) -> &std::sync::Mutex<Option<crate::network::iroh_transport::IrohTransport>> {
        &self.iroh_transport
    }

    /// Socket-to-transport mapping (for utxo_commitments_client)
    pub(crate) fn socket_to_transport(&self) -> &Arc<Mutex<HashMap<SocketAddr, super::transport::TransportAddr>>> {
        &self.socket_to_transport
    }

    /// Peer states (for utxo_commitments_client)
    pub(crate) fn peer_states(&self) -> &Arc<RwLock<HashMap<SocketAddr, blvm_protocol::network::PeerState>>> {
        &self.peer_states
    }

    /// Request timeout config (for utxo_commitments_client)
    pub(crate) fn request_timeout_config(&self) -> &Arc<crate::config::RequestTimeoutConfig> {
        &self.request_timeout_config
    }

    /// Protocol limits (for wire_dispatch, addr handler, etc.)
    pub(crate) fn protocol_limits(&self) -> &Arc<crate::config::ProtocolLimitsConfig> {
        &self.protocol_limits
    }

    /// Background task interval config (for background_tasks)
    pub(crate) fn background_task_config(&self) -> &Arc<crate::config::BackgroundTaskConfig> {
        &self.background_task_config
    }

    /// Pending requests (for utxo_commitments_client)
    pub(crate) fn pending_requests(&self) -> &Arc<Mutex<HashMap<u64, PendingRequest>>> {
        &self.pending_requests
    }

    /// Pending block requests (for utxo_commitments_client)
    pub(crate) fn pending_block_requests(
        &self,
    ) -> &Arc<
        Mutex<
            HashMap<
                (IpAddr, blvm_protocol::Hash),
                tokio::sync::oneshot::Sender<(
                    blvm_protocol::Block,
                    Vec<Vec<blvm_protocol::segwit::Witness>>,
                )>,
            >,
        >,
    > {
        &self.pending_block_requests
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

// Safety: NetworkManager is safe to share across threads (Sync) because:
// - All internal state is protected by Arc<Mutex<>> or Arc<RwLock<>> which are Sync
// - EventPublisher is already marked as Sync (see event_publisher.rs)
// - All async operations are Send and don't require synchronous access to internal state
// - The ZmqPublisher's Socket is only accessed through async methods which are Send
// This is a workaround for ZMQ's Socket type not being Sync, but the actual usage is safe.
#[cfg(feature = "zmq")]
unsafe impl Sync for NetworkManager {}

