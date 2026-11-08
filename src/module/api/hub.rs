//! Module Communication Hub
//!
//! Central API hub handling all module requests with routing,
//! permissions, and auditing.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::module::ipc::protocol::{
    RequestMessage, RequestPayload, ResponseMessage, ResponsePayload,
};
use crate::module::security::{PermissionChecker, RequestValidator};
use crate::module::traits::{ModuleError, NodeAPI};

/// API request router that routes module requests to appropriate handlers
pub struct ModuleApiHub {
    /// Node API implementation
    node_api: Arc<dyn NodeAPI + Send + Sync>,
    /// Permission checker for validating module access
    permission_checker: PermissionChecker,
    /// Request validator for consensus protection
    request_validator: RequestValidator,
    /// Request audit log (for security tracking) - bounded to last 1000 entries
    audit_log: VecDeque<AuditEntry>,
    /// Maximum audit log size
    max_audit_entries: usize,
    /// Per-module rate limiters (token bucket algorithm, same as RPC)
    rate_limiters: Arc<Mutex<HashMap<String, ModuleRateLimiter>>>,
    /// Default rate limit burst (requests)
    default_rate_limit_burst: u32,
    /// Default rate limit rate (requests per second)
    default_rate_limit_rate: u32,
    /// Outbound HTTP broker. Default policy is deny.
    http_broker: crate::module::http::HttpBroker,
    /// Inbound listeners. `start` installs the manager's host.
    service_host: crate::module::services::ServiceHost,
}

/// Audit entry for tracking module API usage
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub module_id: String,
    pub api_call: String,
    pub timestamp: u64,
    pub success: bool,
    /// Set for brokered HTTP. Absent on other API calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// HTTP status, when the broker got a response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

/// Token bucket rate limiter for module requests
/// Reuses the same algorithm as RpcRateLimiter for consistency
struct ModuleRateLimiter {
    /// Current number of tokens available
    tokens: u32,
    /// Maximum burst size (initial token count)
    burst_limit: u32,
    /// Tokens per second refill rate
    rate: u32,
    /// Last refill timestamp (Unix seconds)
    last_refill: u64,
}

impl ModuleRateLimiter {
    /// Create a new rate limiter
    fn new(burst_limit: u32, rate: u32) -> Self {
        let now = crate::utils::current_timestamp();
        Self {
            tokens: burst_limit,
            burst_limit,
            rate,
            last_refill: now,
        }
    }

    /// Check if a request is allowed and consume a token
    fn check_and_consume(&mut self) -> bool {
        let now = crate::utils::current_timestamp();

        // Refill tokens based on elapsed time
        let elapsed = now.saturating_sub(self.last_refill);
        if elapsed > 0 {
            let tokens_to_add = (elapsed as u32).saturating_mul(self.rate);
            self.tokens = self
                .tokens
                .saturating_add(tokens_to_add)
                .min(self.burst_limit);
            self.last_refill = now;
        }

        // Check if we have tokens available
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

impl ModuleApiHub {
    /// Create a new API hub
    pub fn new<A: NodeAPI + Send + Sync + 'static>(node_api: Arc<A>) -> Self {
        Self {
            node_api,
            permission_checker: PermissionChecker::new(),
            request_validator: RequestValidator::new(),
            audit_log: VecDeque::new(),
            max_audit_entries: 1000,
            rate_limiters: Arc::new(Mutex::new(HashMap::new())),
            default_rate_limit_burst: 1000, // Higher than RPC (modules may make more calls)
            default_rate_limit_rate: 100,   // 100 req/sec per module
            http_broker: crate::module::http::HttpBroker::default(),
            service_host: crate::module::services::ServiceHost::new(),
        }
    }

    /// Operator allow-list and optional proxy for the HTTP broker.
    pub fn set_http_policy(&mut self, policy: crate::module::http::HttpPolicy) {
        self.http_broker.set_policy(policy);
    }

    /// Share the manager's listener table so `StreamSend` reaches the socket.
    pub fn set_service_host(&mut self, host: crate::module::services::ServiceHost) {
        self.service_host = host;
    }

    /// Create a new rate limiter (reuses same algorithm as RpcRateLimiter)
    fn create_rate_limiter(&self) -> ModuleRateLimiter {
        ModuleRateLimiter::new(self.default_rate_limit_burst, self.default_rate_limit_rate)
    }

    /// Check if a module request is within rate limits
    /// Returns true if allowed, false if rate limited
    async fn check_module_rate_limit(&self, module_id: &str) -> bool {
        let mut limiters = self.rate_limiters.lock().await;

        // Get or create rate limiter for this module
        let limiter = limiters
            .entry(module_id.to_string())
            .or_insert_with(|| self.create_rate_limiter());

        limiter.check_and_consume()
    }

    /// Return a clone of the underlying NodeAPI arc for callers that need to make
    /// inter-module calls (e.g. `loadmodule` marketplace fallback).
    pub fn node_api(&self) -> Arc<dyn NodeAPI + Send + Sync> {
        Arc::clone(&self.node_api)
    }

    /// `get_block_height` for a WASM guest. Uses the permission set registered at load.
    /// An id that was never registered is denied, so the checker defaults do not apply.
    pub async fn wasm_host_get_block_height(&self, module_name: &str) -> Result<u64, ModuleError> {
        if !self
            .permission_checker
            .has_explicit_permissions(module_name)
        {
            return Err(ModuleError::PermissionDenied(
                "WASM host call requires the permission set registered at load".to_string(),
            ));
        }
        self.permission_checker
            .check_api_call(module_name, &RequestPayload::GetBlockHeight)?;
        self.node_api.get_block_height().await
    }

    /// `get_block` for a WASM guest. Uses the permission set registered at load.
    pub async fn wasm_host_get_block(
        &self,
        module_name: &str,
        hash: &crate::Hash,
    ) -> Result<Option<crate::Block>, ModuleError> {
        if !self
            .permission_checker
            .has_explicit_permissions(module_name)
        {
            return Err(ModuleError::PermissionDenied(
                "WASM host call requires the permission set registered at load".to_string(),
            ));
        }
        self.permission_checker
            .check_api_call(module_name, &RequestPayload::GetBlock { hash: *hash })?;
        self.node_api.get_block(hash).await
    }

    /// Register a module's permissions
    pub fn register_module_permissions(
        &mut self,
        module_id: String,
        permissions: crate::module::security::permissions::PermissionSet,
    ) {
        self.permission_checker
            .register_module_permissions(module_id, permissions);
    }

    /// Unregister a module (permissions + rate limiters) when it unloads.
    pub async fn unregister_module(&mut self, module_name: &str) {
        self.permission_checker
            .unregister_module_permissions(module_name);
        let mut limiters = self.rate_limiters.lock().await;
        limiters.retain(|id, _| id != module_name && !id.starts_with(&format!("{module_name}_")));
    }

    /// Drop version-message service bits this module registered.
    pub async fn clear_module_service_bits(&self, module_name: &str) -> Result<(), ModuleError> {
        self.node_api.clear_module_service_bits(module_name).await
    }

    /// Handle a request from a module
    pub async fn handle_request(
        &mut self,
        module_id: &str,
        request: RequestMessage,
    ) -> Result<ResponseMessage, ModuleError> {
        debug!(
            "API hub handling request from module {}: {:?}",
            module_id, request.payload
        );

        // Validate permissions
        self.permission_checker
            .check_api_call(module_id, &request.payload)?;

        // Validate that request doesn't modify consensus
        match self
            .request_validator
            .validate_request(module_id, &request.payload)?
        {
            crate::module::security::ValidationResult::Allowed => {}
            crate::module::security::ValidationResult::Denied(reason) => {
                return Err(ModuleError::OperationError(format!(
                    "Request denied: {reason}"
                )));
            }
        }

        // Handle handshake specially (no validation needed, just acknowledge)
        if let RequestPayload::Handshake {
            module_id: handshake_id,
            module_name,
            version,
        } = &request.payload
        {
            // Verify module_id matches
            if handshake_id != module_id {
                return Err(ModuleError::OperationError(format!(
                    "Handshake module_id mismatch: expected {module_id}, got {handshake_id}"
                )));
            }

            info!(
                "Handshake from module {} (name={}, version={})",
                module_id, module_name, version
            );
            return Ok(ResponseMessage::success(
                request.correlation_id,
                ResponsePayload::HandshakeAck {
                    node_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            ));
        }

        // Get operation ID for resource limits and audit logging (avoid duplicate matching)
        let operation_id = Self::get_operation_id(&request.payload);
        self.request_validator
            .validate_resource_limits(module_id, operation_id)?;

        // Route request to appropriate handler
        let response = match &request.payload {
            RequestPayload::Handshake { .. } => {
                // Handshake is handled at connection level, not here
                return Err(crate::module::traits::ModuleError::IpcError(
                    "Handshake should be handled at connection level".to_string(),
                ));
            }
            RequestPayload::RegisterCliSpec { .. } => {
                // RegisterCliSpec is handled in IPC server before hub
                return Err(crate::module::traits::ModuleError::IpcError(
                    "RegisterCliSpec should be handled at connection level".to_string(),
                ));
            }
            RequestPayload::GetBlock { hash } => {
                let block = self.node_api.get_block(hash).await?;
                ResponsePayload::Block(block)
            }
            RequestPayload::GetBlockHeader { hash } => {
                let header = self.node_api.get_block_header(hash).await?;
                ResponsePayload::BlockHeader(header)
            }
            RequestPayload::GetTransaction { hash } => {
                let tx = self.node_api.get_transaction(hash).await?;
                ResponsePayload::Transaction(tx)
            }
            RequestPayload::HasTransaction { hash } => {
                let exists = self.node_api.has_transaction(hash).await?;
                ResponsePayload::Bool(exists)
            }
            RequestPayload::GetChainTip => {
                let tip = self.node_api.get_chain_tip().await?;
                ResponsePayload::Hash(tip)
            }
            RequestPayload::GetBlockHeight => {
                let height = self.node_api.get_block_height().await?;
                ResponsePayload::U64(height)
            }
            RequestPayload::GetUtxo { outpoint } => {
                let utxo = self.node_api.get_utxo(outpoint).await?;
                ResponsePayload::Utxo(utxo)
            }
            RequestPayload::SubscribeEvents { event_types: _ } => {
                // Event subscription is handled in IPC server
                // Return success acknowledgment (Empty response)
                ResponsePayload::SubscribeAck
            }
            // Mempool API
            RequestPayload::GetMempoolTransactions => {
                let txs = self.node_api.get_mempool_transactions().await?;
                ResponsePayload::MempoolTransactions(txs)
            }
            RequestPayload::GetMempoolTransaction { tx_hash } => {
                let tx = self.node_api.get_mempool_transaction(tx_hash).await?;
                ResponsePayload::MempoolTransaction(tx)
            }
            RequestPayload::GetMempoolSize => {
                let size = self.node_api.get_mempool_size().await?;
                ResponsePayload::MempoolSize(size)
            }
            // Network API
            RequestPayload::GetNetworkStats => {
                let stats = self.node_api.get_network_stats().await?;
                ResponsePayload::NetworkStats(stats)
            }
            RequestPayload::GetNetworkPeers => {
                let peers = self.node_api.get_network_peers().await?;
                ResponsePayload::NetworkPeers(peers)
            }
            // Chain API
            RequestPayload::GetChainInfo => {
                let info = self.node_api.get_chain_info().await?;
                ResponsePayload::ChainInfo(info)
            }
            RequestPayload::GetBlockByHeight { height } => {
                let block = self.node_api.get_block_by_height(*height).await?;
                ResponsePayload::BlockByHeight(block)
            }
            RequestPayload::GetBlockAndWitnesses { hash } => {
                let pair = self.node_api.get_block_and_witnesses(hash).await?;
                let (block, witnesses) = match pair {
                    Some((block, witnesses)) => (Some(block), witnesses),
                    None => (None, Vec::new()),
                };
                ResponsePayload::BlockAndWitnesses { block, witnesses }
            }
            // Lightning API
            RequestPayload::GetLightningNodeUrl => {
                let url = self.node_api.get_lightning_node_url().await?;
                ResponsePayload::LightningNodeUrl(url)
            }
            RequestPayload::GetLightningInfo => {
                let info = self.node_api.get_lightning_info().await?;
                ResponsePayload::LightningInfo(info)
            }
            // Payment API
            RequestPayload::GetPaymentState { payment_id } => {
                let state = self.node_api.get_payment_state(payment_id).await?;
                ResponsePayload::PaymentState(state)
            }
            // Additional Mempool API
            RequestPayload::CheckTransactionInMempool { tx_hash } => {
                let exists = self.node_api.check_transaction_in_mempool(tx_hash).await?;
                ResponsePayload::CheckTransactionInMempool(exists)
            }
            RequestPayload::GetFeeEstimate { target_blocks } => {
                match self.node_api.get_fee_estimate(*target_blocks).await {
                    Ok(fee_rate) => ResponsePayload::FeeEstimate(fee_rate),
                    Err(err) => {
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.to_string(),
                        ));
                    }
                }
            }
            // Filesystem API
            RequestPayload::ReadFile { path } => {
                // Set module_id in thread-local for this call
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.read_file(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let data = result?;
                ResponsePayload::FileData(data)
            }
            RequestPayload::WriteFile { path, data } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.write_file(path.clone(), data.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::DeleteFile { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.delete_file(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::ListDirectory { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.list_directory(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let entries = result?;
                ResponsePayload::DirectoryListing(entries)
            }
            RequestPayload::CreateDirectory { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.create_directory(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::GetFileMetadata { path } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.get_file_metadata(path.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                let metadata = result?;
                ResponsePayload::FileMetadata(metadata)
            }
            // Metrics and Telemetry
            RequestPayload::ReportMetric { metric } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.report_metric(metric.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::MetricReported
            }
            RequestPayload::GetModuleMetrics { module_id } => {
                let metrics = self.node_api.get_module_metrics(module_id).await?;
                ResponsePayload::ModuleMetrics(metrics)
            }
            RequestPayload::GetAllMetrics => {
                let metrics = self.node_api.get_all_metrics().await?;
                ResponsePayload::AllMetrics(metrics)
            }
            // Module Discovery API
            RequestPayload::DiscoverModules => {
                let modules = self.node_api.discover_modules().await?;
                ResponsePayload::ModuleList(modules)
            }
            RequestPayload::GetModuleInfo {
                module_id: target_module_id,
            } => {
                let info = self.node_api.get_module_info(target_module_id).await?;
                ResponsePayload::ModuleInfo(info)
            }
            RequestPayload::IsModuleAvailable {
                module_id: target_module_id,
            } => {
                let available = self.node_api.is_module_available(target_module_id).await?;
                ResponsePayload::ModuleAvailable(available)
            }
            // Module Event Publishing
            RequestPayload::PublishEvent {
                event_type,
                payload,
            } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self
                    .node_api
                    .publish_event(*event_type, payload.clone())
                    .await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                if let Err(err) = result {
                    return Ok(ResponseMessage::error(
                        request.correlation_id,
                        err.to_string(),
                    ));
                }
                ResponsePayload::EventPublished
            }
            RequestPayload::SubscribeModuleEventNames { .. } => ResponsePayload::SubscribeAck,
            // Module-to-Module Communication
            RequestPayload::CallModule {
                target_module_id,
                method,
                params,
            } => {
                let response = self
                    .node_api
                    .call_module(target_module_id.as_deref(), method, params.clone())
                    .await?;
                ResponsePayload::ModuleApiResponse(response)
            }
            RequestPayload::RegisterModuleApi {
                methods,
                api_version,
            } => {
                self.node_api
                    .register_subprocess_module_api(module_id, methods.clone(), *api_version)
                    .await?;
                ResponsePayload::ModuleApiRegistered
            }
            RequestPayload::UnregisterModuleApi => {
                self.node_api.unregister_module_api().await?;
                ResponsePayload::ModuleApiUnregistered
            }
            // Module Health & Monitoring
            RequestPayload::GetModuleHealth { module_id } => {
                let health = self.node_api.get_module_health(module_id).await?;
                ResponsePayload::ModuleHealth(health)
            }
            RequestPayload::GetAllModuleHealth => {
                let health = self.node_api.get_all_module_health().await?;
                ResponsePayload::AllModuleHealth(health)
            }
            RequestPayload::ReportModuleHealth { health } => {
                self.node_api.report_module_health(health.clone()).await?;
                ResponsePayload::HealthReported
            }
            // Network Integration
            RequestPayload::SendMeshPacketToPeer {
                peer_addr,
                packet_data,
            } => {
                self.node_api
                    .send_mesh_packet_to_peer(peer_addr.clone(), packet_data.clone())
                    .await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::SendStratumV2MessageToPeer {
                peer_addr,
                message_data,
            } => {
                self.node_api
                    .send_peer_transport_payload(peer_addr.clone(), message_data.clone())
                    .await?;
                ResponsePayload::Bool(true)
            }
            // Mining API
            RequestPayload::GetBlockTemplate {
                rules,
                coinbase_script,
                coinbase_address,
            } => {
                let template = self
                    .node_api
                    .get_block_template(
                        rules.clone(),
                        coinbase_script.clone(),
                        coinbase_address.clone(),
                    )
                    .await?;
                ResponsePayload::BlockTemplate(template)
            }
            RequestPayload::GetBlockTemplateWithOutputs {
                rules,
                coinbase_script,
                coinbase_outputs,
            } => {
                let template = self
                    .node_api
                    .get_block_template_with_outputs(
                        rules.clone(),
                        coinbase_script.clone(),
                        coinbase_outputs.clone(),
                    )
                    .await?;
                ResponsePayload::BlockTemplate(template)
            }
            RequestPayload::GetBlockTemplateDeclared {
                rules,
                coinbase_script,
                coinbase_outputs,
                declared_txids,
            } => {
                let template = self
                    .node_api
                    .get_block_template_declared(
                        rules.clone(),
                        coinbase_script.clone(),
                        coinbase_outputs.clone(),
                        declared_txids.clone(),
                    )
                    .await?;
                ResponsePayload::BlockTemplate(template)
            }
            RequestPayload::SubmitMempoolTransaction { tx, witnesses } => {
                let added = self
                    .node_api
                    .submit_mempool_transaction(tx.clone(), witnesses.clone())
                    .await?;
                ResponsePayload::MempoolTransactionSubmitted(added)
            }
            RequestPayload::SubmitBlock { block } => {
                let result = self.node_api.submit_block(block.clone()).await?;
                ResponsePayload::SubmitBlockResult(result)
            }
            RequestPayload::QueueReceivedBlock { block_bytes } => {
                self.node_api
                    .queue_received_block_bytes(block_bytes.clone())
                    .await?;
                ResponsePayload::ReceivedBlockQueued
            }
            RequestPayload::SetAdvertisedServiceBits { bits } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.set_advertised_service_bits(*bits).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::AdvertisedServiceBitsSet
            }
            RequestPayload::MergeBlockServeDenylist { block_hashes } => {
                self.node_api
                    .merge_block_serve_denylist(block_hashes.as_slice())
                    .await?;
                ResponsePayload::BlockServeDenylistMerged
            }
            RequestPayload::GetBlockServeDenylistSnapshot => {
                let s = self.node_api.get_block_serve_denylist_snapshot().await?;
                ResponsePayload::BlockServeDenylistSnapshot(s)
            }
            RequestPayload::ClearBlockServeDenylist => {
                self.node_api.clear_block_serve_denylist().await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::ReplaceBlockServeDenylist { block_hashes } => {
                self.node_api
                    .replace_block_serve_denylist(block_hashes.as_slice())
                    .await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::MergeTxServeDenylist { tx_hashes } => {
                self.node_api
                    .merge_tx_serve_denylist(tx_hashes.as_slice())
                    .await?;
                ResponsePayload::TxServeDenylistMerged
            }
            RequestPayload::GetTxServeDenylistSnapshot => {
                let s = self.node_api.get_tx_serve_denylist_snapshot().await?;
                ResponsePayload::TxServeDenylistSnapshot(s)
            }
            RequestPayload::ClearTxServeDenylist => {
                self.node_api.clear_tx_serve_denylist().await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::ReplaceTxServeDenylist { tx_hashes } => {
                self.node_api
                    .replace_tx_serve_denylist(tx_hashes.as_slice())
                    .await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::GetSyncStatus => {
                let s = self.node_api.get_sync_status().await?;
                ResponsePayload::NodeSyncStatus(s)
            }
            RequestPayload::BanPeer {
                peer_addr,
                ban_duration_seconds,
            } => {
                self.node_api
                    .ban_peer(peer_addr.as_str(), *ban_duration_seconds)
                    .await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::SetBlockServeMaintenanceMode { enabled } => {
                self.node_api
                    .set_block_serve_maintenance_mode(*enabled)
                    .await?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::GetTxMetadata { txid } => {
                let metadata = self.node_api.get_tx_metadata(txid).await?;
                ResponsePayload::TxMetadata(metadata)
            }
            RequestPayload::GetMempoolFeeRateSummary => {
                let summary = self.node_api.get_mempool_fee_rate_summary().await?;
                ResponsePayload::MempoolFeeRateSummary(summary)
            }
            // Module RPC Endpoint Registration
            RequestPayload::RegisterRpcEndpoint {
                method,
                description,
            } => {
                self.node_api
                    .register_rpc_endpoint(method.clone(), description.clone())
                    .await?;
                ResponsePayload::RpcEndpointRegistered
            }
            RequestPayload::UnregisterRpcEndpoint { method } => {
                self.node_api.unregister_rpc_endpoint(method).await?;
                ResponsePayload::RpcEndpointUnregistered
            }
            RequestPayload::RegisterCoreRpcOverride {
                method,
                description,
            } => {
                self.node_api
                    .register_core_rpc_override(method.clone(), description.clone())
                    .await?;
                ResponsePayload::CoreRpcOverrideRegistered
            }
            RequestPayload::UnregisterCoreRpcOverride { method } => {
                self.node_api.unregister_core_rpc_override(method).await?;
                ResponsePayload::CoreRpcOverrideUnregistered
            }
            RequestPayload::RegisterTimer { .. }
            | RequestPayload::CancelTimer { .. }
            | RequestPayload::ScheduleTask { .. } => {
                return Ok(ResponseMessage::error(
                    request.correlation_id,
                    crate::module::timers::types::LEGACY_TIMER_UNSUPPORTED.to_string(),
                ));
            }
            RequestPayload::TimerCreate { spec, mode } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_create(spec.clone(), *mode).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                ResponsePayload::TimerCreated(result?)
            }
            RequestPayload::TimerCancel { target } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_cancel(target.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::TimerPause { target } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_pause(target.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::TimerResume { target } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_resume(target.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::TimerTrigger { target } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_trigger(target.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                result?;
                ResponsePayload::Bool(true)
            }
            RequestPayload::TimerGet { target } => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_get(target.clone()).await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                ResponsePayload::TimerInfo(result?)
            }
            RequestPayload::TimerList => {
                crate::module::api::node_api::NodeApiImpl::set_current_module_id(
                    module_id.to_string(),
                );
                let result = self.node_api.timer_list().await;
                crate::module::api::node_api::NodeApiImpl::clear_current_module_id();
                ResponsePayload::TimerList(result?)
            }
            RequestPayload::HandshakeV2 { .. } => {
                return Err(crate::module::traits::ModuleError::OperationError(
                    "HandshakeV2 is handled at connection setup".to_string(),
                ));
            }
            RequestPayload::HttpPost {
                url,
                content_type,
                body,
                timeout_ms,
                authorization,
            } => {
                match self
                    .http_broker
                    .execute_post(
                        module_id,
                        url,
                        content_type,
                        body,
                        authorization.as_deref(),
                        *timeout_ms,
                    )
                    .await
                {
                    Ok(fetched) => {
                        let status = fetched.status;
                        let origin = fetched.origin;
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            true,
                            Some(origin),
                            Some(status),
                        );
                        return Ok(ResponseMessage::success(
                            request.correlation_id,
                            ResponsePayload::HttpResponse {
                                status,
                                content_type: fetched.content_type,
                                body: fetched.body,
                            },
                        ));
                    }
                    Err(err) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            false,
                            err.origin,
                            err.status,
                        );
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.message,
                        ));
                    }
                }
            }
            RequestPayload::HttpRequest {
                url,
                accept,
                timeout_ms,
            } => {
                match self
                    .http_broker
                    .execute(module_id, url, accept.as_deref(), *timeout_ms)
                    .await
                {
                    Ok(fetched) => {
                        let status = fetched.status;
                        let origin = fetched.origin;
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            true,
                            Some(origin),
                            Some(status),
                        );
                        return Ok(ResponseMessage::success(
                            request.correlation_id,
                            ResponsePayload::HttpResponse {
                                status,
                                content_type: fetched.content_type,
                                body: fetched.body,
                            },
                        ));
                    }
                    Err(err) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            false,
                            err.origin,
                            err.status,
                        );
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.message,
                        ));
                    }
                }
            }
            RequestPayload::StreamSend { conn_id, bytes } => {
                let name = crate::module::module_name_from_id(module_id);
                match self.service_host.write(name, *conn_id, bytes).await {
                    Ok(()) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            true,
                            None,
                            None,
                        );
                        return Ok(ResponseMessage::success(
                            request.correlation_id,
                            ResponsePayload::Bool(true),
                        ));
                    }
                    Err(message) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            false,
                            None,
                            None,
                        );
                        return Ok(ResponseMessage::error(request.correlation_id, message));
                    }
                }
            }
            RequestPayload::RegisterFeeEstimators { specs } => {
                match self
                    .node_api
                    .register_fee_estimators(module_id, specs.clone())
                    .await
                {
                    Ok(registration) => ResponsePayload::FeeEstimatorsRegistered(registration),
                    Err(err) => {
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.to_string(),
                        ));
                    }
                }
            }
            RequestPayload::SubmitFeeTable { table } => {
                match self
                    .node_api
                    .submit_fee_table(module_id, table.clone())
                    .await
                {
                    Ok(generation) => ResponsePayload::FeeTableAccepted { generation },
                    Err(err) => {
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.to_string(),
                        ));
                    }
                }
            }
            RequestPayload::GetFeeTable { view, mode, targets } => {
                match self.node_api.get_fee_table(view, mode, targets).await {
                    Ok(table) => ResponsePayload::NativeFeeTable(table),
                    Err(err) => {
                        return Ok(ResponseMessage::error(
                            request.correlation_id,
                            err.to_string(),
                        ));
                    }
                }
            }
            RequestPayload::StreamClose { conn_id } => {
                let name = crate::module::module_name_from_id(module_id);
                match self.service_host.close(name, *conn_id).await {
                    Ok(()) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            true,
                            None,
                            None,
                        );
                        return Ok(ResponseMessage::success(
                            request.correlation_id,
                            ResponsePayload::Bool(true),
                        ));
                    }
                    Err(message) => {
                        self.log_audit(
                            module_id.to_string(),
                            operation_id.to_string(),
                            false,
                            None,
                            None,
                        );
                        return Ok(ResponseMessage::error(request.correlation_id, message));
                    }
                }
            }
            #[allow(unreachable_patterns)]
            other => {
                return Err(crate::module::traits::ModuleError::OperationError(format!(
                    "Unimplemented request payload: {other:?}"
                )));
            }
        };

        // Log audit entry (use operation ID from earlier)
        self.log_audit(module_id.to_string(), operation_id.to_string(), true, None, None);

        Ok(ResponseMessage::success(request.correlation_id, response))
    }

    /// Get operation identifier from request payload (for logging/rate limiting)
    #[inline]
    fn get_operation_id(payload: &RequestPayload) -> &'static str {
        match payload {
            RequestPayload::Handshake { .. } => "handshake",
            RequestPayload::GetBlock { .. } => "get_block",
            RequestPayload::GetBlockHeader { .. } => "get_block_header",
            RequestPayload::GetTransaction { .. } => "get_transaction",
            RequestPayload::HasTransaction { .. } => "has_transaction",
            RequestPayload::GetChainTip => "get_chain_tip",
            RequestPayload::GetBlockHeight => "get_block_height",
            RequestPayload::GetUtxo { .. } => "get_utxo",
            RequestPayload::SubscribeEvents { .. } => "subscribe_events",
            RequestPayload::SubscribeModuleEventNames { .. } => "subscribe_events",
            RequestPayload::GetMempoolTransactions => "get_mempool_transactions",
            RequestPayload::GetMempoolTransaction { .. } => "get_mempool_transaction",
            RequestPayload::GetMempoolSize => "get_mempool_size",
            RequestPayload::GetNetworkStats => "get_network_stats",
            RequestPayload::GetNetworkPeers => "get_network_peers",
            RequestPayload::GetChainInfo => "get_chain_info",
            RequestPayload::GetBlockByHeight { .. } => "get_block_by_height",
            RequestPayload::GetBlockAndWitnesses { .. } => "get_block_and_witnesses",
            RequestPayload::GetLightningNodeUrl => "get_lightning_node_url",
            RequestPayload::GetLightningInfo => "get_lightning_info",
            RequestPayload::GetPaymentState { .. } => "get_payment_state",
            RequestPayload::CheckTransactionInMempool { .. } => "check_transaction_in_mempool",
            RequestPayload::GetFeeEstimate { .. } => "get_fee_estimate",
            RequestPayload::GetBlockTemplate { .. } => "get_block_template",
            RequestPayload::GetBlockTemplateWithOutputs { .. } => "get_block_template_with_outputs",
            RequestPayload::GetBlockTemplateDeclared { .. } => "get_block_template_declared",
            RequestPayload::SubmitMempoolTransaction { .. } => "submit_mempool_transaction",
            RequestPayload::SubmitBlock { .. } => "submit_block",
            RequestPayload::QueueReceivedBlock { .. } => "queue_received_block",
            RequestPayload::SetAdvertisedServiceBits { .. } => "set_advertised_service_bits",
            RequestPayload::MergeBlockServeDenylist { .. } => "merge_block_serve_denylist",
            RequestPayload::GetBlockServeDenylistSnapshot => "get_block_serve_denylist_snapshot",
            RequestPayload::ClearBlockServeDenylist => "clear_block_serve_denylist",
            RequestPayload::ReplaceBlockServeDenylist { .. } => "replace_block_serve_denylist",
            RequestPayload::MergeTxServeDenylist { .. } => "merge_tx_serve_denylist",
            RequestPayload::GetTxServeDenylistSnapshot => "get_tx_serve_denylist_snapshot",
            RequestPayload::ClearTxServeDenylist => "clear_tx_serve_denylist",
            RequestPayload::ReplaceTxServeDenylist { .. } => "replace_tx_serve_denylist",
            RequestPayload::GetSyncStatus => "get_sync_status",
            RequestPayload::BanPeer { .. } => "ban_peer",
            RequestPayload::SetBlockServeMaintenanceMode { .. } => {
                "set_block_serve_maintenance_mode"
            }
            RequestPayload::GetTxMetadata { .. } => "get_tx_metadata",
            RequestPayload::GetMempoolFeeRateSummary => "get_mempool_fee_rate_summary",
            // Filesystem API
            RequestPayload::ReadFile { .. } => "read_file",
            RequestPayload::WriteFile { .. } => "write_file",
            RequestPayload::DeleteFile { .. } => "delete_file",
            RequestPayload::ListDirectory { .. } => "list_directory",
            RequestPayload::CreateDirectory { .. } => "create_directory",
            RequestPayload::GetFileMetadata { .. } => "get_file_metadata",
            // Module RPC Endpoint Registration
            RequestPayload::RegisterRpcEndpoint { .. } => "register_rpc_endpoint",
            RequestPayload::UnregisterRpcEndpoint { .. } => "unregister_rpc_endpoint",
            RequestPayload::RegisterCoreRpcOverride { .. } => "register_core_rpc_override",
            RequestPayload::UnregisterCoreRpcOverride { .. } => "unregister_core_rpc_override",
            // Timers and Scheduled Tasks
            RequestPayload::RegisterTimer { .. } => "register_timer",
            RequestPayload::CancelTimer { .. } => "cancel_timer",
            RequestPayload::ScheduleTask { .. } => "schedule_task",
            RequestPayload::TimerCreate { .. } => "timer_create",
            RequestPayload::TimerCancel { .. } => "timer_cancel",
            RequestPayload::TimerPause { .. } => "timer_pause",
            RequestPayload::TimerResume { .. } => "timer_resume",
            RequestPayload::TimerTrigger { .. } => "timer_trigger",
            RequestPayload::TimerGet { .. } => "timer_get",
            RequestPayload::TimerList => "timer_list",
            RequestPayload::HandshakeV2 { .. } => "handshake",
            // Metrics and Telemetry
            RequestPayload::ReportMetric { .. } => "report_metric",
            RequestPayload::GetModuleMetrics { .. } => "get_module_metrics",
            RequestPayload::GetAllMetrics => "get_all_metrics",
            // Module Discovery API
            RequestPayload::DiscoverModules => "discover_modules",
            RequestPayload::GetModuleInfo { .. } => "get_module_info",
            RequestPayload::IsModuleAvailable { .. } => "is_module_available",
            // Module Event Publishing
            RequestPayload::PublishEvent { .. } => "publish_event",
            // Module-to-Module Communication
            RequestPayload::CallModule { .. } => "call_module",
            RequestPayload::RegisterModuleApi { .. } => "register_module_api",
            RequestPayload::UnregisterModuleApi => "unregister_module_api",
            // Module Health & Monitoring
            RequestPayload::GetModuleHealth { .. } => "get_module_health",
            RequestPayload::GetAllModuleHealth => "get_all_module_health",
            RequestPayload::ReportModuleHealth { .. } => "report_module_health",
            // Network Integration
            RequestPayload::SendMeshPacketToPeer { .. } => "send_mesh_packet_to_peer",
            RequestPayload::SendStratumV2MessageToPeer { .. } => "send_peer_transport_payload",
            RequestPayload::RegisterCliSpec { .. } => "register_cli_spec",
            RequestPayload::HttpRequest { .. } => "http_request",
            RequestPayload::HttpPost { .. } => "http_post",
            RequestPayload::StreamSend { .. } => "stream_send",
            RequestPayload::StreamClose { .. } => "stream_close",
            RequestPayload::RegisterFeeEstimators { .. } => "register_fee_estimators",
            RequestPayload::SubmitFeeTable { .. } => "submit_fee_table",
            RequestPayload::GetFeeTable { .. } => "get_fee_table",
        }
    }

    /// Log an audit entry
    fn log_audit(
        &mut self,
        module_id: String,
        api_call: String,
        success: bool,
        origin: Option<String>,
        status: Option<u16>,
    ) {
        let timestamp = crate::utils::current_timestamp();

        let entry = AuditEntry {
            module_id: module_id.clone(),
            api_call: api_call.clone(),
            timestamp,
            success,
            origin,
            status,
        };

        // Store in-memory log (for programmatic access)
        self.audit_log.push_back(entry.clone());

        // Limit log size (keep last N entries)
        while self.audit_log.len() > self.max_audit_entries {
            self.audit_log.pop_front();
        }

        // Log to structured logging for monitoring
        use tracing::{info, warn};
        if success {
            info!(
                target: "blvm_node::module::audit",
                module_id = %module_id,
                api_call = %api_call,
                timestamp = timestamp,
                "Module API call"
            );
        } else {
            warn!(
                target: "blvm_node::module::audit",
                module_id = %module_id,
                api_call = %api_call,
                timestamp = timestamp,
                "Module API call failed"
            );
        }
    }

    /// Get audit log (for debugging/monitoring)
    pub fn get_audit_log(&self, limit: usize) -> Vec<AuditEntry> {
        let start = self.audit_log.len().saturating_sub(limit);
        self.audit_log.range(start..).cloned().collect()
    }
}
