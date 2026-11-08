//! IPC message protocol
//!
//! Defines the message types and serialization for IPC communication
//! between modules and the base node.

use serde::{Deserialize, Serialize};

pub use crate::module::timers::types::{
    CreateMode, TimerAck, TimerFire, TimerId, TimerInfo, TimerSpec, TimerState, TimerTarget,
};

use crate::module::traits::{
    BlockServeDenylistSnapshot, ChainInfo, EventType, LightningInfo, MempoolSize, NetworkStats,
    PaymentState, PeerInfo, SyncStatus, TxServeDenylistSnapshot,
};
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
use blvm_protocol::segwit::Witness;

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
    /// Invocation from node to module (CLI or RPC dispatch)
    Invocation(InvocationMessage),
    /// Invocation result from module to node
    InvocationResult(InvocationResultMessage),
}

impl ModuleMessage {
    /// Get the correlation ID if this is a request/response
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        match self {
            ModuleMessage::Request(req) => Some(req.correlation_id),
            ModuleMessage::Response(resp) => Some(resp.correlation_id),
            ModuleMessage::Event(_) => None,
            ModuleMessage::Log(_) => None,
            ModuleMessage::Invocation(inv) => Some(inv.correlation_id),
            ModuleMessage::InvocationResult(res) => Some(res.correlation_id),
        }
    }

    /// Get message type
    pub fn message_type(&self) -> MessageType {
        match self {
            ModuleMessage::Request(req) => req.request_type.clone(),
            ModuleMessage::Response(_resp) => MessageType::Response,
            ModuleMessage::Event(_) => MessageType::Event,
            ModuleMessage::Log(_) => MessageType::Log,
            ModuleMessage::Invocation(_) => MessageType::Invocation,
            ModuleMessage::InvocationResult(_) => MessageType::InvocationResult,
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
    GetBlockAndWitnesses,
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
    // Module RPC Endpoint Registration
    RegisterRpcEndpoint,
    UnregisterRpcEndpoint,
    // Core RPC method override (allowlisted methods only)
    RegisterCoreRpcOverride,
    UnregisterCoreRpcOverride,
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
    /// Opaque bytes to a peer (`NodeAPI::send_peer_transport_payload`); wire name kept for IPC compatibility.
    SendStratumV2MessageToPeer,
    // Mining API
    GetBlockTemplate,
    SubmitBlock,
    /// Queue raw block bytes (`bincode` `Block`) like `BlockReceived` from the network.
    QueueReceivedBlock,
    /// Merge block hashes into the outbound full-block serve denylist (additive).
    /// Used for selective sync, policy, compliance, testing, etc.
    MergeBlockServeDenylist,
    GetBlockServeDenylistSnapshot,
    ClearBlockServeDenylist,
    ReplaceBlockServeDenylist,
    MergeTxServeDenylist,
    GetTxServeDenylistSnapshot,
    ClearTxServeDenylist,
    ReplaceTxServeDenylist,
    GetSyncStatus,
    BanPeer,
    SetBlockServeMaintenanceMode,
    /// CLI spec registration (module → node on connect)
    RegisterCliSpec,
    /// Invocation (node → module)
    Invocation,
    /// Invocation result (module → node)
    InvocationResult,
    /// Log message from module
    Log,
    /// Response messages
    Response,
    /// Event messages
    Event,
    /// Error response
    Error,
    /// Multi-output GBT (Commons Pool). Added at end so existing variant indices stay put.
    GetBlockTemplateWithOutputs,
    /// Stage 3b declared-txid GBT. Added at end so existing variant indices stay put.
    GetBlockTemplateDeclared,
    /// JD ProvideMissing → mempool (`SendTransactions`). Added at end.
    SubmitMempoolTransaction,
    /// Tx index block location. Added at end so existing variant indices stay put.
    GetTxMetadata,
    /// Positive mempool fee-rate summary. Added at end.
    GetMempoolFeeRateSummary,
    /// Module asks the node to advertise `NODE_FIBRE` while that module is loaded.
    SetAdvertisedServiceBits,
    // Timer service. Appended so existing variant indices stay put.
    TimerCreate,
    TimerCancel,
    TimerPause,
    TimerResume,
    TimerTrigger,
    TimerGet,
    TimerList,
    /// Handshake that also carries the IPC layout version.
    HandshakeV2,
    /// Optional name filter for `EventType::ModuleEvent`. Appended so existing indices stay put.
    SubscribeModuleEventNames,
    /// Module asks the node to GET a URL. Appended so existing indices stay put.
    HttpRequest,
    /// Module writes bytes on a hosted stream. Appended so existing indices stay put.
    StreamSend,
    /// Module closes a hosted stream. Appended so existing indices stay put.
    StreamClose,
    /// Replace one module's estimator names. Appended so existing indices stay put.
    RegisterFeeEstimators,
    /// Push one external fee table. Appended so existing indices stay put.
    SubmitFeeTable,
    /// Read the native fee table. Appended so existing indices stay put.
    GetFeeTable,
    /// Module asks the node to POST a JSON body. Appended so existing indices stay put.
    HttpPost,
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
    GetBlockAndWitnesses {
        hash: Hash,
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
    // Module RPC Endpoint Registration
    RegisterRpcEndpoint {
        method: String,
        description: String,
    },
    UnregisterRpcEndpoint {
        method: String,
    },
    // Core RPC method override (allowlisted methods only)
    RegisterCoreRpcOverride {
        method: String,
        description: String,
    },
    UnregisterCoreRpcOverride {
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
    /// See [`MessageType::SendStratumV2MessageToPeer`].
    SendStratumV2MessageToPeer {
        peer_addr: String,
        message_data: Vec<u8>,
    },
    // Mining API
    GetBlockTemplate {
        rules: Vec<String>,
        coinbase_script: Option<Vec<u8>>,
        coinbase_address: Option<String>,
    },
    SubmitBlock {
        block: Block,
    },
    /// Serialized block (same encoding as `BlockReceived`: `bincode` of `Block`).
    QueueReceivedBlock {
        block_bytes: Vec<u8>,
    },
    /// Block hashes that must not be answered with a full `block` on the wire (e.g. `getdata`).
    MergeBlockServeDenylist {
        block_hashes: Vec<Hash>,
    },
    GetBlockServeDenylistSnapshot,
    ClearBlockServeDenylist,
    ReplaceBlockServeDenylist {
        block_hashes: Vec<Hash>,
    },
    MergeTxServeDenylist {
        tx_hashes: Vec<Hash>,
    },
    GetTxServeDenylistSnapshot,
    ClearTxServeDenylist,
    ReplaceTxServeDenylist {
        tx_hashes: Vec<Hash>,
    },
    GetSyncStatus,
    BanPeer {
        peer_addr: String,
        ban_duration_seconds: Option<u64>,
    },
    SetBlockServeMaintenanceMode {
        enabled: bool,
    },
    /// Register CLI spec (module → node on connect)
    RegisterCliSpec {
        spec: CliSpec,
    },
    GetBlockTemplateWithOutputs {
        rules: Vec<String>,
        coinbase_script: Option<Vec<u8>>,
        coinbase_outputs: Vec<(i64, Vec<u8>)>,
    },
    /// Stage 3b: named mempool txids in order. Empty list is coinbase-only.
    GetBlockTemplateDeclared {
        rules: Vec<String>,
        coinbase_script: Option<Vec<u8>>,
        coinbase_outputs: Vec<(i64, Vec<u8>)>,
        declared_txids: Vec<Hash>,
    },
    /// Parsed Bitcoin tx + optional witness stacks. Already-in-mempool is ok.
    SubmitMempoolTransaction {
        tx: Transaction,
        witnesses: Option<Vec<blvm_protocol::Witness>>,
    },
    GetTxMetadata {
        txid: Hash,
    },
    GetMempoolFeeRateSummary,
    /// `bits` must be `NODE_FIBRE` (`1 << 26`). The node keys the bit by the calling module.
    SetAdvertisedServiceBits {
        bits: u64,
    },
    // Timer service. Appended so existing variant indices stay put.
    TimerCreate {
        spec: crate::module::timers::types::TimerSpec,
        mode: crate::module::timers::types::CreateMode,
    },
    TimerCancel {
        target: crate::module::timers::types::TimerTarget,
    },
    TimerPause {
        target: crate::module::timers::types::TimerTarget,
    },
    TimerResume {
        target: crate::module::timers::types::TimerTarget,
    },
    TimerTrigger {
        target: crate::module::timers::types::TimerTarget,
    },
    TimerGet {
        target: crate::module::timers::types::TimerTarget,
    },
    TimerList,
    /// Same as [`RequestPayload::Handshake`], plus the IPC layout version.
    HandshakeV2 {
        module_id: String,
        module_name: String,
        version: String,
        ipc_protocol: u32,
    },
    /// Names a subscriber wants for `EventType::ModuleEvent`. An empty list clears the filter.
    SubscribeModuleEventNames {
        names: Vec<String>,
    },
    /// GET only. The node enforces the operator allow-list.
    HttpRequest {
        url: String,
        accept: Option<String>,
        timeout_ms: u32,
    },
    /// Bytes from the module onto a hosted stream connection.
    StreamSend {
        conn_id: u64,
        bytes: Vec<u8>,
    },
    /// The module closes a hosted stream connection.
    StreamClose {
        conn_id: u64,
    },
    /// Replace this module's estimator names. `module_id` comes from the session.
    RegisterFeeEstimators {
        specs: Vec<FeeEstimatorSpecWire>,
    },
    /// Push one table. `module_id` comes from the session.
    SubmitFeeTable {
        table: ExternalFeeTableWire,
    },
    /// Native columns only. `view` is `combined`, `history`, or `mempool`.
    GetFeeTable {
        view: String,
        mode: String,
        targets: Vec<u32>,
    },
    /// POST a body. The node enforces the same origin allow-list as `HttpRequest`.
    HttpPost {
        url: String,
        content_type: String,
        body: Vec<u8>,
        timeout_ms: u32,
        /// `Authorization` header value. Not logged.
        authorization: Option<String>,
    },
}

/// One name a module wants to publish quotes under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeEstimatorSpecWire {
    pub name: String,
    pub description: String,
    pub modes: Vec<String>,
    pub default_merge: String,
    pub auto_priority: Option<u32>,
}

/// A name the node refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeNameRejectionWire {
    pub name: String,
    pub error: String,
}

/// Accepted names and the typed rejection for each refused name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeEstimatorRegistrationWire {
    pub accepted: Vec<String>,
    pub rejected: Vec<FeeNameRejectionWire>,
}

/// One pushed table. The session supplies the module id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalFeeTableWire {
    pub estimator: String,
    pub tip_hash: Hash,
    pub upstream_height: u64,
    pub mode: Option<String>,
    pub quotes: Vec<(u32, u64)>,
}

/// One native quote on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeQuoteWire {
    pub sat_per_kvb: u64,
    pub blocks: u32,
    pub source: String,
    pub estimator: String,
}

/// Native fee table for the requested targets. Missing quotes are `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeFeeTableWire {
    pub tip_hash: Hash,
    pub tip_height: u64,
    pub generation: u64,
    pub quotes: Vec<(u32, Option<FeeQuoteWire>)>,
}

/// CLI spec for module commands (JSON-serializable structure)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliSpec {
    pub version: u32,
    pub name: String,
    pub about: Option<String>,
    pub subcommands: Vec<CliSubcommandSpec>,
}

/// CLI subcommand spec
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliSubcommandSpec {
    pub name: String,
    pub about: Option<String>,
    pub args: Vec<CliArgSpec>,
}

/// CLI argument spec
///
/// Supports named args: `--long_name value` and `-short value`.
/// - `name`: param name for injection
/// - `long_name`: e.g. "option" for --option (None = infer from name)
/// - `short_name`: e.g. "o" for -o
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliArgSpec {
    pub name: String,
    /// Long form: "option" for --option. None = use `name`.
    #[serde(default)]
    pub long_name: Option<String>,
    /// Short form: "o" for -o
    #[serde(default)]
    pub short_name: Option<String>,
    pub required: Option<bool>,
    pub takes_value: Option<bool>,
    pub default: Option<String>,
}

/// Invocation from node to module (CLI or RPC dispatch)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationMessage {
    pub correlation_id: CorrelationId,
    pub invocation_type: InvocationType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InvocationType {
    /// CLI: run subcommand with args
    Cli {
        subcommand: String,
        args: Vec<String>,
    },
    /// RPC: call method with params
    Rpc {
        method: String,
        params: serde_json::Value,
    },
    /// Module API: forward inter-module call to subprocess handler
    ModuleApi {
        method: String,
        params: Vec<u8>,
        caller_module_id: String,
    },
    /// Timer fire. Appended so existing variant indices stay put.
    Timer(crate::module::timers::types::TimerFire),
    /// Hosted HTTP request. Appended so existing variant indices stay put.
    ServiceHttp {
        service: String,
        method: String,
        path_and_query: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Hosted stream accepted. Appended so existing variant indices stay put.
    StreamOpen { conn_id: u64, peer: String },
    /// Hosted stream bytes. `seq` is per connection, starting at 1.
    StreamData {
        conn_id: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// Hosted stream closed. Appended so existing variant indices stay put.
    StreamClose { conn_id: u64 },
}

/// Invocation result from module to node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationResultMessage {
    pub correlation_id: CorrelationId,
    pub success: bool,
    pub payload: Option<InvocationResultPayload>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InvocationResultPayload {
    /// CLI result: stdout, stderr, exit_code
    Cli {
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    /// RPC result: JSON value
    Rpc(serde_json::Value),
    /// Module API result: opaque bincode/bytes response
    ModuleApi(Vec<u8>),
    /// Acknowledgement of a timer fire. Appended so existing variant indices stay put.
    Timer(crate::module::timers::types::TimerAck),
    /// Hosted HTTP response. Appended so existing variant indices stay put.
    ServiceHttp {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// Bytes to write on a hosted stream. Appended so existing indices stay put.
    Stream(Vec<u8>),
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
    BlockAndWitnesses {
        block: Option<Block>,
        witnesses: Vec<Vec<Witness>>,
    },
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
    // Module RPC Endpoint Registration responses
    RpcEndpointRegistered,
    RpcEndpointUnregistered,
    // Core RPC override responses
    CoreRpcOverrideRegistered,
    CoreRpcOverrideUnregistered,
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
    // Mining API responses
    BlockTemplate(blvm_protocol::mining::BlockTemplate),
    SubmitBlockResult(crate::module::traits::SubmitBlockResult),
    /// Ack for [`RequestPayload::QueueReceivedBlock`].
    ReceivedBlockQueued,
    BlockServeDenylistMerged,
    TxServeDenylistMerged,
    BlockServeDenylistSnapshot(BlockServeDenylistSnapshot),
    TxServeDenylistSnapshot(TxServeDenylistSnapshot),
    /// Sync coordinator status for modules (`traits::SyncStatus`).
    NodeSyncStatus(SyncStatus),
    /// `true` = added, `false` = already present.
    MempoolTransactionSubmitted(bool),
    TxMetadata(Option<crate::module::traits::TxBlockMetadata>),
    MempoolFeeRateSummary(crate::module::traits::MempoolFeeRateSummary),
    /// Ack for [`RequestPayload::SetAdvertisedServiceBits`].
    AdvertisedServiceBitsSet,
    /// Ack for [`RequestPayload::TimerCreate`].
    TimerCreated(crate::module::timers::types::TimerInfo),
    /// Ack for [`RequestPayload::TimerGet`].
    TimerInfo(crate::module::timers::types::TimerInfo),
    /// Ack for [`RequestPayload::TimerList`].
    TimerList(Vec<crate::module::timers::types::TimerInfo>),
    /// Result of [`RequestPayload::HttpRequest`]. Appended so existing indices stay put.
    HttpResponse {
        status: u16,
        content_type: Option<String>,
        body: Vec<u8>,
    },
    /// Result of [`RequestPayload::RegisterFeeEstimators`].
    FeeEstimatorsRegistered(FeeEstimatorRegistrationWire),
    /// Result of [`RequestPayload::SubmitFeeTable`]. `generation` is the bumped counter.
    FeeTableAccepted {
        generation: u64,
    },
    /// Result of [`RequestPayload::GetFeeTable`].
    NativeFeeTable(NativeFeeTableWire),
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
    pub target: String, // Module name or component
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
    NewBlock {
        block_hash: Hash,
        height: u64,
    },
    NewTransaction {
        tx_hash: Hash,
    },
    BlockDisconnected {
        hash: Hash,
        height: u64,
    },
    ChainReorg {
        old_tip: Hash,
        new_tip: Hash,
    },

    // === Payment Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentRequestCreated,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentSettled,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentFailed,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentVerified,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentRouteFound,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPaymentRouteFailed,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedChannelOpened,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedChannelClosed,

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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMiningJobCreated,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedShareSubmitted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMergeMiningReward,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMiningPoolConnected,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMiningPoolDisconnected,

    // === Governance Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedGovernanceProposalCreated,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedGovernanceProposalVoted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedGovernanceProposalMerged,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedWebhookSent,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedWebhookFailed,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedGovernanceForkDetected,

    // === Network Events ===
    PeerConnected {
        peer_addr: String,      // SocketAddr as string for serialization
        transport_type: String, // "tcp", "quinn", "iroh", "mesh"
        services: u64,
        version: u32,
        /// Optional mesh/Bitcoin identity (32 bytes), when known at connect time.
        #[serde(default)]
        peer_node_id: Option<Vec<u8>>,
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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedBroadcastStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedBroadcastCompleted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedRouteDiscovered,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedRouteFailed,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedConnectionAttempt,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedAddressDiscovered,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedAddressExpired,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedNetworkPartition,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedNetworkReconnected,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDoSAttackDetected,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedRateLimitExceeded,

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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedScriptVerificationStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedScriptVerificationCompleted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedUTXOValidationStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedUTXOValidationCompleted,
    DifficultyAdjusted {
        old_difficulty: u32,
        new_difficulty: u32,
        height: u64,
    },
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedSoftForkActivated,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedSoftForkLockedIn,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedConsensusRuleViolation,

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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedStorageRead,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedStorageWrite,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedStorageQuery,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDatabaseBackupStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDatabaseBackupCompleted,

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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedModuleStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedModuleStopped,
    MeshPacketReceived {
        packet_data: Vec<u8>,
        peer_addr: String, // Serialized SocketAddr as string
    },
    StratumV2MessageReceived {
        message_data: Vec<u8>,
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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMaintenanceStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedMaintenanceCompleted,
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
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedResourceLimitWarning,

    // === Dandelion++ Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDandelionStemStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDandelionStemAdvanced,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDandelionFluffed,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedDandelionStemPathExpired,

    // === Compact Blocks Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedCompactBlockReceived,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedBlockReconstructionStarted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedBlockReconstructionCompleted,

    // === FIBRE Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedFibreBlockEncoded,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedFibreBlockSent,

    // === P2P companion UDP (NODE_FIBRE; e.g. blvm-fibre) ===
    CompanionUdpPeerRegistered {
        /// P2P peer key (`SocketAddr` display for TCP/QUIC).
        p2p_peer_addr: String,
        /// Companion UDP endpoint (same IP as P2P, port = P2P port + 1).
        udp_addr: String,
    },
    CompanionUdpPeerUnregistered {
        p2p_peer_addr: String,
    },

    // === Package Relay Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPackageReceived,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedPackageRejected,

    // === UTXO Commitments Events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedUtxoCommitmentReceived,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedUtxoCommitmentVerified,

    // === Ban List Sharing Events ===
    BanListShared {
        peer_addr: String,
        ban_count: usize,
    },
    BanListReceived {
        peer_addr: String,
        ban_count: usize,
    },

    // === Extended module / mining events ===
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedSelectiveSyncPolicyApplied,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedActionExecuted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedModulePurchaseCompleted,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedStratumClientConnected,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedStratumClientDisconnected,
    /// F5 reserved. Nothing publishes this. The slot keeps the bincode index.
    ReservedIBDBlockFiltered,

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
    /// Raw BIP70 frame from a peer. `command` is the P2P command name.
    PeerPayment {
        command: String,
        payload: Vec<u8>,
        peer_addr: String,
    },
    /// Published by a module. `source_module` is the authenticated session, not the caller's value.
    ModuleEvent {
        source_module: String,
        name: String,
        data: Vec<u8>,
    },
    /// First native table for a tip. One event per tip hash.
    FeeTableReady {
        tip_hash: Hash,
        tip_height: u64,
        generation: u64,
    },
}

impl EventPayload {
    /// Build a module event. The node replaces `source_module` with the session id.
    pub fn module_event(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self::ModuleEvent {
            source_module: String::new(),
            name: name.into(),
            data,
        }
    }

    /// JSON body for [`EventPayload::module_event`].
    pub fn module_event_json(name: impl Into<String>, data: &impl serde::Serialize) -> Self {
        let bytes = serde_json::to_vec(data).unwrap_or_default();
        Self::module_event(name, bytes)
    }
}

/// Typed payload extraction for event handlers.
///
/// Use these helpers in #[on_event] handlers to get event-specific data without manual matching.
/// Example: `if let Some((hash, height)) = event.payload.as_new_block() { ... }`
impl EventPayload {
    pub fn as_new_block(&self) -> Option<(&Hash, u64)> {
        match self {
            Self::NewBlock { block_hash, height } => Some((block_hash, *height)),
            _ => None,
        }
    }

    pub fn as_new_transaction(&self) -> Option<&Hash> {
        match self {
            Self::NewTransaction { tx_hash } => Some(tx_hash),
            _ => None,
        }
    }

    pub fn as_module_loaded(&self) -> Option<(&str, &str)> {
        match self {
            Self::ModuleLoaded {
                module_name,
                version,
            } => Some((module_name.as_str(), version.as_str())),
            _ => None,
        }
    }

    pub fn as_block_disconnected(&self) -> Option<(&Hash, u64)> {
        match self {
            Self::BlockDisconnected { hash, height } => Some((hash, *height)),
            _ => None,
        }
    }

    pub fn as_chain_reorg(&self) -> Option<(&Hash, &Hash)> {
        match self {
            Self::ChainReorg { old_tip, new_tip } => Some((old_tip, new_tip)),
            _ => None,
        }
    }
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
