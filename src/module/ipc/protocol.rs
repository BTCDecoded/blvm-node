//! IPC message protocol
//!
//! Defines the message types and serialization for IPC communication
//! between modules and the base node.

use serde::{Deserialize, Serialize};

use crate::module::traits::{
    ChainInfo, EventType, LightningInfo, MempoolSize, NetworkStats, PaymentState, PeerInfo,
};
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

/// Correlation ID for matching requests with responses
pub type CorrelationId = u64;

/// Main IPC message wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModuleMessage {
    /// Request from module to node
    Request(RequestMessage),
    /// Response from node to module
    Response(ResponseMessage),
    /// Event notification from node to module
    Event(EventMessage),
    /// Log message from module to node
    Log(LogMessage),
}

impl ModuleMessage {
    /// Get the correlation ID if this is a request/response
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        match self {
            ModuleMessage::Request(req) => Some(req.correlation_id),
            ModuleMessage::Response(resp) => Some(resp.correlation_id),
            ModuleMessage::Event(_) => None,
            ModuleMessage::Log(_) => None,
        }
    }

    /// Get message type
    pub fn message_type(&self) -> MessageType {
        match self {
            ModuleMessage::Request(req) => req.request_type.clone(),
            ModuleMessage::Response(_resp) => MessageType::Response,
            ModuleMessage::Event(_) => MessageType::Event,
            ModuleMessage::Log(_) => MessageType::Log,
        }
    }
}

/// Message type classification
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    /// Request messages
    GetBlock,
    GetBlockHeader,
    GetTransaction,
    HasTransaction,
    GetChainTip,
    GetBlockHeight,
    GetUtxo,
    SubscribeEvents,
    Handshake,
    // Mempool API
    GetMempoolTransactions,
    GetMempoolTransaction,
    GetMempoolSize,
    // Network API
    GetNetworkStats,
    GetNetworkPeers,
    // Chain API
    GetChainInfo,
    GetBlockByHeight,
    // Lightning API
    GetLightningNodeUrl,
    GetLightningInfo,
    // Payment API
    GetPaymentState,
    // Additional Mempool API
    CheckTransactionInMempool,
    GetFeeEstimate,
    // Filesystem API
    ReadFile,
    WriteFile,
    DeleteFile,
    ListDirectory,
    CreateDirectory,
    GetFileMetadata,
    // Storage API
    StorageOpenTree,
    StorageInsert,
    StorageGet,
    StorageRemove,
    StorageContainsKey,
    StorageIter,
    StorageTransaction,
    // Module RPC Endpoint Registration
    RegisterRpcEndpoint,
    UnregisterRpcEndpoint,
    // Timers and Scheduled Tasks
    RegisterTimer,
    CancelTimer,
    ScheduleTask,
    // Metrics and Telemetry
    ReportMetric,
    GetModuleMetrics,
    GetAllMetrics,
    // Module Discovery API
    DiscoverModules,
    GetModuleInfo,
    IsModuleAvailable,
    // Module Event Publishing
    PublishEvent,
    // Module-to-Module Communication
    CallModule,
    RegisterModuleApi,
    UnregisterModuleApi,
    // Module Health & Monitoring
    GetModuleHealth,
    GetAllModuleHealth,
    ReportModuleHealth,
    // Network Integration
    SendMeshPacketToPeer,
    SendStratumV2MessageToPeer,
    /// Log message from module
    Log,
    /// Response messages
    Response,
    /// Event messages
    Event,
    /// Error response
    Error,
}

/// Request message from module to node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMessage {
    pub correlation_id: CorrelationId,
    pub request_type: MessageType,
    pub payload: RequestPayload,
}

/// Request payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestPayload {
    /// Handshake: Module identifies itself (first message)
    Handshake {
        module_id: String,
        module_name: String,
        version: String,
    },
    GetBlock {
        hash: Hash,
    },
    GetBlockHeader {
        hash: Hash,
    },
    GetTransaction {
        hash: Hash,
    },
    HasTransaction {
        hash: Hash,
    },
    GetChainTip,
    GetBlockHeight,
    GetUtxo {
        outpoint: OutPoint,
    },
    SubscribeEvents {
        event_types: Vec<EventType>,
    },
    // Mempool API
    GetMempoolTransactions,
    GetMempoolTransaction {
        tx_hash: Hash,
    },
    GetMempoolSize,
    // Network API
    GetNetworkStats,
    GetNetworkPeers,
    // Chain API
    GetChainInfo,
    GetBlockByHeight {
        height: u64,
    },
    // Lightning API
    GetLightningNodeUrl,
    GetLightningInfo,
    // Payment API
    GetPaymentState {
        payment_id: String,
    },
    // Additional Mempool API
    CheckTransactionInMempool {
        tx_hash: Hash,
    },
    GetFeeEstimate {
        target_blocks: u32,
    },
    // Filesystem API
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        data: Vec<u8>,
    },
    DeleteFile {
        path: String,
    },
    ListDirectory {
        path: String,
    },
    CreateDirectory {
        path: String,
    },
    GetFileMetadata {
        path: String,
    },
    // Storage API
    StorageOpenTree {
        name: String,
    },
    StorageInsert {
        tree_id: String,
        key: Vec<u8>,
        value: Vec<u8>,
    },
    StorageGet {
        tree_id: String,
        key: Vec<u8>,
    },
    StorageRemove {
        tree_id: String,
        key: Vec<u8>,
    },
    StorageContainsKey {
        tree_id: String,
        key: Vec<u8>,
    },
    StorageIter {
        tree_id: String,
    },
    StorageTransaction {
        tree_id: String,
        operations: Vec<StorageOperation>,
    },
    // Module RPC Endpoint Registration
    RegisterRpcEndpoint {
        method: String,
        description: String,
    },
    UnregisterRpcEndpoint {
        method: String,
    },
    // Timers and Scheduled Tasks
    RegisterTimer {
        interval_seconds: u64,
    },
    CancelTimer {
        timer_id: u64,
    },
    ScheduleTask {
        delay_seconds: u64,
    },
    // Metrics and Telemetry
    ReportMetric {
        metric: crate::module::metrics::manager::Metric,
    },
    GetModuleMetrics {
        module_id: String,
    },
    GetAllMetrics,
    // Module Discovery API
    DiscoverModules,
    GetModuleInfo {
        module_id: String,
    },
    IsModuleAvailable {
        module_id: String,
    },
    // Module Event Publishing
    PublishEvent {
        event_type: EventType,
        payload: EventPayload,
    },
    // Module-to-Module Communication
    CallModule {
        target_module_id: Option<String>,
        method: String,
        params: Vec<u8>,
    },
    RegisterModuleApi {
        methods: Vec<String>,
        api_version: u32,
    },
    UnregisterModuleApi,
    // Module Health & Monitoring
    GetModuleHealth {
        module_id: String,
    },
    GetAllModuleHealth,
    ReportModuleHealth {
        health: crate::module::process::monitor::ModuleHealth,
    },
    // Network Integration
    SendMeshPacketToPeer {
        peer_addr: String,
        packet_data: Vec<u8>,
    },
    SendStratumV2MessageToPeer {
        peer_addr: String,
        message_data: Vec<u8>,
    },
}

/// Response message from node to module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub correlation_id: CorrelationId,
    pub success: bool,
    pub payload: Option<ResponsePayload>,
    pub error: Option<String>,
}

/// Response payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponsePayload {
    /// Handshake acknowledgment with node version
    HandshakeAck {
        node_version: String,
    },
    Block(Option<Block>),
    BlockHeader(Option<BlockHeader>),
    Transaction(Option<Transaction>),
    Bool(bool),
    Hash(Hash),
    U64(u64),
    Utxo(Option<UTXO>),
    SubscribeAck,
    // Mempool API responses
    MempoolTransactions(Vec<Hash>),
    MempoolTransaction(Option<Transaction>),
    MempoolSize(MempoolSize),
    // Network API responses
    NetworkStats(NetworkStats),
    NetworkPeers(Vec<PeerInfo>),
    // Chain API responses
    ChainInfo(ChainInfo),
    BlockByHeight(Option<Block>),
    // Lightning API responses
    LightningNodeUrl(Option<String>),
    LightningInfo(Option<LightningInfo>),
    // Payment API responses
    PaymentState(Option<PaymentState>),
    // Additional Mempool API responses
    CheckTransactionInMempool(bool),
    FeeEstimate(u64),
    // Filesystem API responses
    FileData(Vec<u8>),
    DirectoryListing(Vec<String>),
    FileMetadata(FileMetadata),
    // Storage API responses
    StorageTreeId(String),
    StorageValue(Option<Vec<u8>>),
    StorageKeyValuePairs(Vec<(Vec<u8>, Vec<u8>)>),
    // Module RPC Endpoint Registration responses
    RpcEndpointRegistered,
    RpcEndpointUnregistered,
    // Timers and Scheduled Tasks responses
    TimerId(u64),
    TaskId(u64),
    TimerCancelled,
    TaskScheduled,
    // Metrics and Telemetry responses
    MetricReported,
    ModuleMetrics(Vec<crate::module::metrics::manager::Metric>),
    AllMetrics(std::collections::HashMap<String, Vec<crate::module::metrics::manager::Metric>>),
    // Module Discovery API responses
    ModuleList(Vec<crate::module::traits::ModuleInfo>),
    ModuleInfo(Option<crate::module::traits::ModuleInfo>),
    ModuleAvailable(bool),
    // Module Event Publishing responses
    EventPublished,
    // Module-to-Module Communication responses
    ModuleApiResponse(Vec<u8>),
    ModuleApiRegistered,
    ModuleApiUnregistered,
    // Module Health & Monitoring responses
    ModuleHealth(Option<crate::module::process::monitor::ModuleHealth>),
    AllModuleHealth(Vec<(String, crate::module::process::monitor::ModuleHealth)>),
    HealthReported,
}

/// Event message from node to subscribed modules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub event_type: EventType,
    pub payload: EventPayload,
}

/// Log message from module to node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMessage {
    pub level: LogLevel,
    pub module_id: String,
    pub message: String,
    pub target: String,  // Module name or component
    pub timestamp: u64,
}

/// Log level for module logging
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Event payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    // === Core Blockchain Events ===
    NewBlock { block_hash: Hash, height: u64 },
    NewTransaction { tx_hash: Hash },
    BlockDisconnected { hash: Hash, height: u64 },
    ChainReorg { old_tip: Hash, new_tip: Hash },
    
    // === Payment Events ===
    PaymentRequestCreated {
        payment_id: String,
        amount_sats: u64,
        invoice: Option<String>,
    },
    PaymentSettled {
        payment_id: String,
        tx_hash: Hash,
        confirmations: u32,
    },
    PaymentFailed {
        payment_id: String,
        reason: String,
    },
    PaymentVerified {
        payment_id: String,
        amount_msats: u64,
        invoice: String,
    },
    PaymentRouteFound {
        payment_id: String,
        route_hops: usize,
        route_cost_msats: u64,
    },
    PaymentRouteFailed {
        payment_id: String,
        reason: String,
    },
    ChannelOpened {
        channel_id: String,
        peer_pubkey: Vec<u8>,
        capacity_sats: u64,
    },
    ChannelClosed {
        channel_id: String,
        reason: String,
    },
    
    // === Mining Events ===
    BlockMined {
        block_hash: Hash,
        height: u64,
        miner_id: Option<String>,
    },
    BlockTemplateUpdated {
        prev_hash: Hash,
        height: u64,
        tx_count: usize,
    },
    MiningDifficultyChanged {
        old_difficulty: u32,
        new_difficulty: u32,
        height: u64,
    },
    MiningJobCreated {
        job_id: String,
        prev_hash: Hash,
        height: u64,
    },
    ShareSubmitted {
        job_id: String,
        share_hash: Hash,
        miner_id: Option<String>,
    },
    MergeMiningReward {
        secondary_chain: String,
        reward_amount: u64,
        block_hash: Hash,
    },
    MiningPoolConnected {
        pool_url: String,
        pool_id: Option<String>,
    },
    MiningPoolDisconnected {
        pool_url: String,
        reason: String,
    },
    
    // === Governance Events ===
    GovernanceProposalCreated {
        proposal_id: String,
        repository: String,
        pr_number: u64,
        tier: String,
    },
    GovernanceProposalVoted {
        proposal_id: String,
        voter: String,
        vote: String, // "approve", "reject", "abstain"
    },
    GovernanceProposalMerged {
        proposal_id: String,
        repository: String,
        pr_number: u64,
    },
    EconomicNodeRegistered {
        node_id: String,
        node_type: String, // "miner", "exchange", "service"
        hashpower_percent: Option<f64>,
    },
    EconomicNodeStatus {
        request_id: String,
        query_type: String, // "status", "hashpower", "uptime"
        node_id: Option<String>,
        response_data: Option<String>, // JSON string with status data
    },
    EconomicNodeForkDecision {
        message_id: String,
        ruleset_version: String,
        decision: String, // "adopt", "reject", "abstain"
        node_id: String,
        timestamp: u64,
    },
    EconomicNodeVeto {
        proposal_id: String,
        node_id: String,
        reason: String,
    },
    WebhookSent {
        webhook_url: String,
        event_type: String,
        success: bool,
    },
    WebhookFailed {
        webhook_url: String,
        event_type: String,
        error: String,
    },
    VetoThresholdReached {
        proposal_id: String,
        veto_percent: f64,
    },
    GovernanceForkDetected {
        fork_id: String,
        ruleset_version: String,
        adoption_count: usize,
    },
    
    // === Network Events ===
    PeerConnected {
        peer_addr: String, // SocketAddr as string for serialization
        transport_type: String, // "tcp", "quinn", "iroh", "mesh"
        services: u64,
        version: u32,
    },
    PeerDisconnected {
        peer_addr: String,
        reason: String, // "timeout", "error", "ban", "manual"
    },
    PeerBanned {
        peer_addr: String,
        reason: String,
        ban_duration_seconds: u64,
    },
    PeerUnbanned {
        peer_addr: String,
    },
    MessageReceived {
        peer_addr: String,
        message_type: String, // "block", "tx", "inv", etc.
        message_size: usize,
        protocol_version: u32,
    },
    MessageSent {
        peer_addr: String,
        message_type: String,
        message_size: usize,
    },
    BroadcastStarted {
        message_type: String,
        target_peers: usize,
    },
    BroadcastCompleted {
        message_type: String,
        successful: usize,
        failed: usize,
    },
    RouteDiscovered {
        destination: Vec<u8>, // Node ID or address
        route_path: Vec<String>, // SocketAddrs as strings
        route_cost: u64,
    },
    RouteFailed {
        destination: Vec<u8>,
        reason: String,
    },
    ConnectionAttempt {
        peer_addr: String,
        success: bool,
        error: Option<String>,
    },
    AddressDiscovered {
        peer_addr: String,
        source: String, // "dns", "peer", "manual", "mesh"
    },
    AddressExpired {
        peer_addr: String,
    },
    NetworkPartition {
        partition_id: Vec<u8>,
        disconnected_peers: Vec<String>,
        partition_size: usize,
    },
    NetworkReconnected {
        partition_id: Vec<u8>,
        reconnected_peers: Vec<String>,
    },
    DoSAttackDetected {
        peer_addr: String,
        attack_type: String, // "flood", "spam", "resource_exhaustion"
        severity: String, // "low", "medium", "high", "critical"
    },
    RateLimitExceeded {
        peer_addr: String,
        limit_type: String, // "messages", "bytes", "connections"
        current_rate: f64,
        limit: f64,
    },
    
    // === Consensus Events ===
    BlockValidationStarted {
        block_hash: Hash,
        height: u64,
    },
    BlockValidationCompleted {
        block_hash: Hash,
        height: u64,
        success: bool,
        validation_time_ms: u64,
        error: Option<String>,
    },
    ScriptVerificationStarted {
        tx_hash: Hash,
        input_index: usize,
    },
    ScriptVerificationCompleted {
        tx_hash: Hash,
        input_index: usize,
        success: bool,
        verification_time_ms: u64,
    },
    UTXOValidationStarted {
        block_hash: Hash,
        height: u64,
    },
    UTXOValidationCompleted {
        block_hash: Hash,
        height: u64,
        success: bool,
    },
    DifficultyAdjusted {
        old_difficulty: u32,
        new_difficulty: u32,
        height: u64,
    },
    SoftForkActivated {
        fork_name: String, // "segwit", "taproot", "ctv", etc.
        height: u64,
    },
    SoftForkLockedIn {
        fork_name: String,
        height: u64,
    },
    ConsensusRuleViolation {
        rule_name: String,
        block_hash: Option<Hash>,
        tx_hash: Option<Hash>,
        error: String,
    },
    
    // === Sync Events ===
    HeadersSyncStarted {
        start_height: u64,
    },
    HeadersSyncProgress {
        current_height: u64,
        target_height: u64,
        progress_percent: f64,
    },
    HeadersSyncCompleted {
        final_height: u64,
        duration_seconds: u64,
    },
    BlockSyncStarted {
        start_height: u64,
        target_height: u64,
    },
    BlockSyncProgress {
        current_height: u64,
        target_height: u64,
        progress_percent: f64,
        blocks_per_second: f64,
    },
    BlockSyncCompleted {
        final_height: u64,
        duration_seconds: u64,
    },
    SyncStateChanged {
        old_state: String, // "Initial", "Headers", "Blocks", "Synced"
        new_state: String,
    },
    
    // === Mempool Events ===
    MempoolTransactionAdded {
        tx_hash: Hash,
        fee_rate: f64,
        mempool_size: usize,
    },
    MempoolTransactionRemoved {
        tx_hash: Hash,
        reason: String, // "confirmed", "expired", "replaced", "rejected"
        mempool_size: usize,
    },
    MempoolThresholdExceeded {
        current_size: usize,
        threshold: usize,
    },
    FeeRateChanged {
        old_fee_rate: f64,
        new_fee_rate: f64,
        mempool_size: usize,
    },
    MempoolCleared {
        cleared_count: usize,
    },
    
    // === Storage Events ===
    StorageRead {
        operation: String,
        duration_ms: u64,
    },
    StorageWrite {
        operation: String,
        duration_ms: u64,
        bytes_written: usize,
    },
    StorageQuery {
        query_type: String,
        duration_ms: u64,
    },
    DatabaseBackupStarted {
        backup_path: String,
    },
    DatabaseBackupCompleted {
        backup_path: String,
        success: bool,
        size_bytes: u64,
        duration_seconds: u64,
    },
    
    // === Module Lifecycle Events ===
    ModuleLoaded {
        module_name: String,
        version: String,
    },
    ModuleUnloaded {
        module_name: String,
        version: String,
    },
    ModuleReloaded {
        module_name: String,
        old_version: String,
        new_version: String,
    },
    ModuleStarted {
        module_name: String,
    },
    ModuleStopped {
        module_name: String,
    },
    MeshPacketReceived {
        packet_data: Vec<u8>,
        peer_addr: String, // Serialized SocketAddr as string
    },
    ModuleCrashed {
        module_name: String,
        error: String,
    },
    ModuleHealthChanged {
        module_name: String,
        old_health: String, // "healthy", "degraded", "unhealthy"
        new_health: String,
    },
    ModuleStateChanged {
        module_name: String,
        old_state: String,
        new_state: String,
    },
    
    // === Configuration Events ===
    ConfigLoaded {
        /// Configuration sections that changed (e.g., ["network", "governance"])
        changed_sections: Vec<String>,
        /// Full config as JSON string (for modules that need full config)
        config_json: Option<String>,
    },
    
    // === Node Lifecycle Events ===
    NodeShutdown {
        /// Shutdown reason (e.g., "signal", "rpc", "error")
        reason: String,
        /// Graceful shutdown timeout in seconds
        timeout_seconds: u64,
    },
    NodeShutdownCompleted {
        /// Shutdown duration in milliseconds
        duration_ms: u64,
    },
    NodeStartupCompleted {
        /// Startup duration in milliseconds
        duration_ms: u64,
        /// Components initialized
        components: Vec<String>,
    },
    
    // === Maintenance Events ===
    MaintenanceStarted {
        /// Maintenance type (e.g., "backup", "cleanup", "prune")
        maintenance_type: String,
        /// Estimated duration in seconds (if known)
        estimated_duration_seconds: Option<u64>,
    },
    MaintenanceCompleted {
        /// Maintenance type
        maintenance_type: String,
        /// Success status
        success: bool,
        /// Duration in milliseconds
        duration_ms: u64,
        /// Results/statistics (optional JSON string)
        results: Option<String>,
    },
    DataMaintenance {
        /// Maintenance operation type: "cleanup" (delete old data), "flush" (write pending data), or "both"
        operation: String,
        /// Urgency level: "low" (periodic), "medium" (scheduled), "high" (shutdown/low-disk)
        urgency: String,
        /// Reason for maintenance (e.g., "periodic", "shutdown", "low_disk", "manual")
        reason: String,
        /// Target age for cleanup in days (if operation includes cleanup)
        target_age_days: Option<u64>,
        /// Timeout in seconds (for high urgency operations)
        timeout_seconds: Option<u64>,
    },
    HealthCheck {
        /// Health check type (e.g., "periodic", "manual", "startup")
        check_type: String,
        /// Node health status
        node_healthy: bool,
        /// Health report (optional JSON string)
        health_report: Option<String>,
    },
    
    // === Resource Management Events ===
    DiskSpaceLow {
        /// Available space in bytes
        available_bytes: u64,
        /// Total space in bytes
        total_bytes: u64,
        /// Percentage free
        percent_free: f64,
        /// Disk path
        disk_path: String,
    },
    ResourceLimitWarning {
        /// Resource type (e.g., "memory", "cpu", "disk", "network")
        resource_type: String,
        /// Current usage percentage
        usage_percent: f64,
        /// Current usage value
        current_usage: u64,
        /// Limit value
        limit: u64,
        /// Warning threshold percentage
        threshold_percent: f64,
    },
    
    // === Dandelion++ Events ===
    DandelionStemStarted {
        tx_hash: Hash,
        current_peer: String,
        next_peer: String,
    },
    DandelionStemAdvanced {
        tx_hash: Hash,
        hop_count: u8,
        next_peer: String,
    },
    DandelionFluffed {
        tx_hash: Hash,
        stem_hops: u8,
    },
    DandelionStemPathExpired {
        peer_addr: String,
    },
    
    // === Compact Blocks Events ===
    CompactBlockReceived {
        block_hash: Hash,
        height: u64,
        short_ids_count: usize,
    },
    BlockReconstructionStarted {
        block_hash: Hash,
        height: u64,
    },
    BlockReconstructionCompleted {
        block_hash: Hash,
        height: u64,
        success: bool,
        missing_txs: usize,
    },
    
    // === FIBRE Events ===
    FibreBlockEncoded {
        block_hash: Hash,
        height: u64,
        chunks: usize,
        encoding_time_ms: u64,
    },
    FibreBlockSent {
        block_hash: Hash,
        height: u64,
        peer_addr: String,
    },
    FibrePeerRegistered {
        peer_addr: String,
    },
    
    // === Package Relay Events ===
    PackageReceived {
        package_id: Vec<u8>,
        transaction_count: usize,
        peer_addr: String,
    },
    PackageRejected {
        package_id: Vec<u8>,
        reason: String,
        peer_addr: String,
    },
    
    // === UTXO Commitments Events ===
    UtxoCommitmentReceived {
        block_hash: Hash,
        height: u64,
        commitment_hash: Hash,
        peer_addr: String,
    },
    UtxoCommitmentVerified {
        block_hash: Hash,
        height: u64,
        commitment_hash: Hash,
        valid: bool,
    },
    
    // === Ban List Sharing Events ===
    BanListShared {
        peer_addr: String,
        ban_count: usize,
    },
    BanListReceived {
        peer_addr: String,
        ban_count: usize,
    },
    
    // === Module Registry Events ===
    ModuleDiscovered {
        module_name: String,
        version: String,
        source: String, // "filesystem", "registry", "p2p"
    },
    ModuleInstalled {
        module_name: String,
        version: String,
    },
    ModuleUpdated {
        module_name: String,
        old_version: String,
        new_version: String,
    },
    ModuleRemoved {
        module_name: String,
        version: String,
    },
}

/// Helper to create request messages
impl RequestMessage {
    pub fn get_block(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlock,
            payload: RequestPayload::GetBlock { hash },
        }
    }

    pub fn get_block_header(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlockHeader,
            payload: RequestPayload::GetBlockHeader { hash },
        }
    }

    pub fn get_transaction(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetTransaction,
            payload: RequestPayload::GetTransaction { hash },
        }
    }

    pub fn has_transaction(correlation_id: CorrelationId, hash: Hash) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::HasTransaction,
            payload: RequestPayload::HasTransaction { hash },
        }
    }

    pub fn get_chain_tip(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetChainTip,
            payload: RequestPayload::GetChainTip,
        }
    }

    pub fn get_block_height(correlation_id: CorrelationId) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetBlockHeight,
            payload: RequestPayload::GetBlockHeight,
        }
    }

    pub fn get_utxo(correlation_id: CorrelationId, outpoint: OutPoint) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::GetUtxo,
            payload: RequestPayload::GetUtxo { outpoint },
        }
    }

    pub fn subscribe_events(correlation_id: CorrelationId, event_types: Vec<EventType>) -> Self {
        Self {
            correlation_id,
            request_type: MessageType::SubscribeEvents,
            payload: RequestPayload::SubscribeEvents { event_types },
        }
    }
}

/// File metadata for filesystem operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub path: String,
    pub size: u64,
    pub is_file: bool,
    pub is_directory: bool,
    pub modified: Option<u64>, // Unix timestamp
    pub created: Option<u64>,  // Unix timestamp
}

/// Storage operation for transactions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StorageOperation {
    Insert { key: Vec<u8>, value: Vec<u8> },
    Remove { key: Vec<u8> },
}

/// Helper to create response messages
impl ResponseMessage {
    pub fn success(correlation_id: CorrelationId, payload: ResponsePayload) -> Self {
        Self {
            correlation_id,
            success: true,
            payload: Some(payload),
            error: None,
        }
    }

    pub fn error(correlation_id: CorrelationId, error: String) -> Self {
        Self {
            correlation_id,
            success: false,
            payload: None,
            error: Some(error),
        }
    }
}
