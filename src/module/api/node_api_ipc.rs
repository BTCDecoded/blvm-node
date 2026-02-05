//! NodeAPI IPC wrapper implementation
//!
//! This module provides a NodeAPI trait implementation that translates
//! method calls into IPC requests to the node. This can be reused by all modules.

use async_trait::async_trait;
use crate::module::ipc::client::ModuleIpcClient;
use crate::module::ipc::protocol::{
    EventPayload, MessageType, RequestMessage, RequestPayload, ResponsePayload,
};
use crate::module::traits::{
    ChainInfo, EventType, LightningInfo, MempoolSize, ModuleError, NetworkStats, NodeAPI,
    PaymentState, PeerInfo,
};
use crate::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
use crate::module::ipc::protocol::ModuleMessage;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

/// NodeAPI implementation that uses IPC to communicate with the node
pub struct NodeApiIpc {
    /// IPC client (wrapped in Arc<Mutex> for thread safety)
    ipc_client: Arc<Mutex<ModuleIpcClient>>,
    /// Module ID for logging and identification
    module_id: String,
    /// Correlation ID counter
    correlation_id: Arc<tokio::sync::Mutex<u64>>,
    /// Event broadcast sender (for creating multiple receivers)
    event_broadcast: Option<Arc<tokio::sync::broadcast::Sender<ModuleMessage>>>,
}

impl NodeApiIpc {
    /// Create a new NodeAPI IPC wrapper
    pub fn new(ipc_client: Arc<Mutex<ModuleIpcClient>>, module_id: String) -> Self {
        Self {
            ipc_client,
            module_id,
            correlation_id: Arc::new(tokio::sync::Mutex::new(0)),
            event_broadcast: None,
        }
    }

    /// Set event broadcast sender (called by ModuleIntegration)
    pub fn set_event_broadcast(&mut self, broadcast: Arc<tokio::sync::broadcast::Sender<ModuleMessage>>) {
        self.event_broadcast = Some(broadcast);
    }

    /// Get next correlation ID
    async fn next_correlation_id(&self) -> u64 {
        let mut id = self.correlation_id.lock().await;
        *id += 1;
        *id
    }

    /// Helper to send a request and parse the response
    async fn request<T, F>(&self, payload: RequestPayload, parser: F) -> Result<T, ModuleError>
    where
        F: FnOnce(ResponsePayload) -> Result<T, ModuleError>,
    {
        let mut client = self.ipc_client.lock().await;
        let correlation_id = client.next_correlation_id();

        let request = RequestMessage {
            correlation_id,
            request_type: Self::payload_to_message_type(&payload),
            payload,
        };

        let response = client.request(request).await?;

        if !response.success {
            return Err(ModuleError::OperationError(
                response
                    .error
                    .unwrap_or_else(|| "Unknown error".to_string()),
            ));
        }

        match response.payload {
            Some(payload) => parser(payload),
            None => Err(ModuleError::OperationError(
                "Empty response payload".to_string(),
            )),
        }
    }

    /// Map RequestPayload to MessageType
    fn payload_to_message_type(payload: &RequestPayload) -> MessageType {
        match payload {
            RequestPayload::GetBlock { .. } => MessageType::GetBlock,
            RequestPayload::GetBlockHeader { .. } => MessageType::GetBlockHeader,
            RequestPayload::GetTransaction { .. } => MessageType::GetTransaction,
            RequestPayload::HasTransaction { .. } => MessageType::HasTransaction,
            RequestPayload::GetChainTip => MessageType::GetChainTip,
            RequestPayload::GetBlockHeight => MessageType::GetBlockHeight,
            RequestPayload::GetUtxo { .. } => MessageType::GetUtxo,
            RequestPayload::GetMempoolTransactions => MessageType::GetMempoolTransactions,
            RequestPayload::GetMempoolTransaction { .. } => MessageType::GetMempoolTransaction,
            RequestPayload::GetMempoolSize => MessageType::GetMempoolSize,
            RequestPayload::GetNetworkStats => MessageType::GetNetworkStats,
            RequestPayload::GetNetworkPeers => MessageType::GetNetworkPeers,
            RequestPayload::GetChainInfo => MessageType::GetChainInfo,
            RequestPayload::GetBlockByHeight { .. } => MessageType::GetBlockByHeight,
            RequestPayload::GetLightningNodeUrl => MessageType::GetLightningNodeUrl,
            RequestPayload::GetLightningInfo => MessageType::GetLightningInfo,
            RequestPayload::GetPaymentState { .. } => MessageType::GetPaymentState,
            RequestPayload::CheckTransactionInMempool { .. } => {
                MessageType::CheckTransactionInMempool
            }
            RequestPayload::GetFeeEstimate { .. } => MessageType::GetFeeEstimate,
            RequestPayload::ReadFile { .. } => MessageType::ReadFile,
            RequestPayload::WriteFile { .. } => MessageType::WriteFile,
            RequestPayload::DeleteFile { .. } => MessageType::DeleteFile,
            RequestPayload::ListDirectory { .. } => MessageType::ListDirectory,
            RequestPayload::CreateDirectory { .. } => MessageType::CreateDirectory,
            RequestPayload::GetFileMetadata { .. } => MessageType::GetFileMetadata,
            RequestPayload::StorageOpenTree { .. } => MessageType::StorageOpenTree,
            RequestPayload::StorageInsert { .. } => MessageType::StorageInsert,
            RequestPayload::StorageGet { .. } => MessageType::StorageGet,
            RequestPayload::StorageRemove { .. } => MessageType::StorageRemove,
            RequestPayload::StorageContainsKey { .. } => MessageType::StorageContainsKey,
            RequestPayload::StorageIter { .. } => MessageType::StorageIter,
            RequestPayload::StorageTransaction { .. } => MessageType::StorageTransaction,
            RequestPayload::SubscribeEvents { .. } => MessageType::SubscribeEvents,
            RequestPayload::Handshake { .. } => MessageType::Handshake,
            RequestPayload::DiscoverModules => MessageType::DiscoverModules,
            RequestPayload::GetModuleInfo { .. } => MessageType::GetModuleInfo,
            RequestPayload::IsModuleAvailable { .. } => MessageType::IsModuleAvailable,
            RequestPayload::PublishEvent { .. } => MessageType::PublishEvent,
            RequestPayload::GetAllMetrics => MessageType::GetAllMetrics,
            RequestPayload::CallModule { .. } => MessageType::CallModule,
            RequestPayload::RegisterModuleApi { .. } => MessageType::RegisterModuleApi,
            RequestPayload::UnregisterModuleApi => MessageType::UnregisterModuleApi,
            RequestPayload::GetModuleHealth { .. } => MessageType::GetModuleHealth,
            RequestPayload::GetAllModuleHealth => MessageType::GetAllModuleHealth,
            RequestPayload::ReportModuleHealth { .. } => MessageType::ReportModuleHealth,
            RequestPayload::SendMeshPacketToPeer { .. } => MessageType::SendMeshPacketToPeer,
            RequestPayload::SendStratumV2MessageToPeer { .. } => MessageType::SendStratumV2MessageToPeer,
            RequestPayload::GetBlockTemplate { .. } => MessageType::GetBlockTemplate,
            RequestPayload::SubmitBlock { .. } => MessageType::SubmitBlock,
            _ => MessageType::Response,
        }
    }
}

#[async_trait]
impl NodeAPI for NodeApiIpc {
    async fn get_block(&self, hash: &Hash) -> Result<Option<Block>, ModuleError> {
        self.request(
            RequestPayload::GetBlock { hash: *hash },
            |payload| match payload {
                ResponsePayload::Block(block) => Ok(block),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_block_header(&self, hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        self.request(
            RequestPayload::GetBlockHeader { hash: *hash },
            |payload| match payload {
                ResponsePayload::BlockHeader(header) => Ok(header),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_transaction(&self, hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        self.request(
            RequestPayload::GetTransaction { hash: *hash },
            |payload| match payload {
                ResponsePayload::Transaction(tx) => Ok(tx),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn has_transaction(&self, hash: &Hash) -> Result<bool, ModuleError> {
        self.request(
            RequestPayload::HasTransaction { hash: *hash },
            |payload| match payload {
                ResponsePayload::Bool(b) => Ok(b),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        self.request(RequestPayload::GetChainTip, |payload| match payload {
            ResponsePayload::Hash(hash) => Ok(hash),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        self.request(RequestPayload::GetBlockHeight, |payload| match payload {
            ResponsePayload::U64(height) => Ok(height),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_utxo(&self, outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        self.request(
            RequestPayload::GetUtxo {
                outpoint: outpoint.clone(),
            },
            |payload| match payload {
                ResponsePayload::Utxo(utxo) => Ok(utxo),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn subscribe_events(
        &self,
        event_types: Vec<EventType>,
    ) -> Result<mpsc::Receiver<ModuleMessage>, ModuleError> {
        // Send SubscribeEvents IPC request
        let correlation_id = self.next_correlation_id().await;
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::SubscribeEvents,
            payload: RequestPayload::SubscribeEvents { event_types },
        };
        
        let mut client = self.ipc_client.lock().await;
        let response = client.request(request).await?;
        
        if !response.success {
            return Err(ModuleError::IpcError(
                response.error.unwrap_or_else(|| "Subscribe failed".to_string()),
            ));
        }
        
        // Return a receiver from the broadcast channel
        // If broadcast is not set, create a dummy receiver (shouldn't happen if using ModuleIntegration)
        if let Some(broadcast) = &self.event_broadcast {
            // Create a new receiver from the broadcast channel
            let broadcast_rx = broadcast.subscribe();
            // Convert broadcast receiver to mpsc receiver
            let (tx, rx) = mpsc::channel(100);
            let tx_clone = tx.clone();
            tokio::spawn(async move {
                let mut brx = broadcast_rx;
                loop {
                    match brx.recv().await {
                        Ok(msg) => {
                            if tx_clone.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            Ok(rx)
        } else {
            // Fallback: return empty receiver
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    async fn get_mempool_transactions(&self) -> Result<Vec<Hash>, ModuleError> {
        self.request(
            RequestPayload::GetMempoolTransactions,
            |payload| match payload {
                ResponsePayload::MempoolTransactions(hashes) => Ok(hashes),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_mempool_transaction(
        &self,
        tx_hash: &Hash,
    ) -> Result<Option<Transaction>, ModuleError> {
        self.request(
            RequestPayload::GetMempoolTransaction { tx_hash: *tx_hash },
            |payload| match payload {
                ResponsePayload::MempoolTransaction(tx) => Ok(tx),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_mempool_size(&self) -> Result<MempoolSize, ModuleError> {
        self.request(RequestPayload::GetMempoolSize, |payload| match payload {
            ResponsePayload::MempoolSize(size) => Ok(size),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_network_stats(&self) -> Result<NetworkStats, ModuleError> {
        self.request(RequestPayload::GetNetworkStats, |payload| match payload {
            ResponsePayload::NetworkStats(stats) => Ok(stats),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_network_peers(&self) -> Result<Vec<PeerInfo>, ModuleError> {
        self.request(RequestPayload::GetNetworkPeers, |payload| match payload {
            ResponsePayload::NetworkPeers(peers) => Ok(peers),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_chain_info(&self) -> Result<ChainInfo, ModuleError> {
        self.request(RequestPayload::GetChainInfo, |payload| match payload {
            ResponsePayload::ChainInfo(info) => Ok(info),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_block_by_height(&self, height: u64) -> Result<Option<Block>, ModuleError> {
        self.request(
            RequestPayload::GetBlockByHeight { height },
            |payload| match payload {
                ResponsePayload::BlockByHeight(block) => Ok(block),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_lightning_node_url(&self) -> Result<Option<String>, ModuleError> {
        self.request(
            RequestPayload::GetLightningNodeUrl,
            |payload| match payload {
                ResponsePayload::LightningNodeUrl(url) => Ok(url),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_lightning_info(&self) -> Result<Option<LightningInfo>, ModuleError> {
        self.request(RequestPayload::GetLightningInfo, |payload| match payload {
            ResponsePayload::LightningInfo(info) => Ok(info),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_payment_state(
        &self,
        payment_id: &str,
    ) -> Result<Option<PaymentState>, ModuleError> {
        self.request(
            RequestPayload::GetPaymentState {
                payment_id: payment_id.to_string(),
            },
            |payload| match payload {
                ResponsePayload::PaymentState(state) => Ok(state),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn check_transaction_in_mempool(&self, tx_hash: &Hash) -> Result<bool, ModuleError> {
        self.request(
            RequestPayload::CheckTransactionInMempool { tx_hash: *tx_hash },
            |payload| match payload {
                ResponsePayload::CheckTransactionInMempool(b) => Ok(b),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_fee_estimate(&self, target_blocks: u32) -> Result<u64, ModuleError> {
        self.request(
            RequestPayload::GetFeeEstimate { target_blocks },
            |payload| match payload {
                ResponsePayload::FeeEstimate(fee) => Ok(fee),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn register_rpc_endpoint(
        &self,
        method: String,
        description: String,
    ) -> Result<(), ModuleError> {
        let correlation_id = self.next_correlation_id().await;
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::RegisterRpcEndpoint,
            payload: RequestPayload::RegisterRpcEndpoint {
                method,
                description,
            },
        };

        let response = self.ipc_client.lock().await.request(request).await?;
        if response.success {
            Ok(())
        } else {
            Err(ModuleError::OperationError(response.error.unwrap_or_else(
                || "Failed to register RPC endpoint".to_string(),
            )))
        }
    }

    async fn unregister_rpc_endpoint(&self, method: &str) -> Result<(), ModuleError> {
        let correlation_id = self.next_correlation_id().await;
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::UnregisterRpcEndpoint,
            payload: RequestPayload::UnregisterRpcEndpoint {
                method: method.to_string(),
            },
        };

        let response = self.ipc_client.lock().await.request(request).await?;
        if response.success {
            Ok(())
        } else {
            Err(ModuleError::OperationError(response.error.unwrap_or_else(
                || "Failed to unregister RPC endpoint".to_string(),
            )))
        }
    }

    async fn register_timer(
        &self,
        _interval_seconds: u64,
        _callback: Arc<dyn crate::module::timers::manager::TimerCallback>,
    ) -> Result<crate::module::timers::manager::TimerId, ModuleError> {
        Err(ModuleError::OperationError(
            "Timer callbacks cannot be serialized over IPC. Use tokio::time::interval for module-side timers.".to_string(),
        ))
    }

    async fn cancel_timer(
        &self,
        _timer_id: crate::module::timers::manager::TimerId,
    ) -> Result<(), ModuleError> {
        Err(ModuleError::OperationError(
            "Timer callbacks cannot be serialized over IPC. Manage timers locally in the module."
                .to_string(),
        ))
    }

    async fn schedule_task(
        &self,
        _delay_seconds: u64,
        _callback: Arc<dyn crate::module::timers::manager::TaskCallback>,
    ) -> Result<crate::module::timers::manager::TaskId, ModuleError> {
        Err(ModuleError::OperationError(
            "Task callbacks cannot be serialized over IPC. Use tokio::time::sleep for module-side delayed tasks.".to_string(),
        ))
    }

    async fn report_metric(
        &self,
        metric: crate::module::metrics::manager::Metric,
    ) -> Result<(), ModuleError> {
        let correlation_id = self.next_correlation_id().await;
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::ReportMetric,
            payload: RequestPayload::ReportMetric { metric },
        };

        let response = self.ipc_client.lock().await.request(request).await?;
        if response.success {
            Ok(())
        } else {
            Err(ModuleError::OperationError(
                response
                    .error
                    .unwrap_or_else(|| "Failed to report metric".to_string()),
            ))
        }
    }

    async fn get_module_metrics(
        &self,
        module_id: &str,
    ) -> Result<Vec<crate::module::metrics::manager::Metric>, ModuleError> {
        let correlation_id = self.next_correlation_id().await;
        let request = RequestMessage {
            correlation_id,
            request_type: MessageType::GetModuleMetrics,
            payload: RequestPayload::GetModuleMetrics {
                module_id: module_id.to_string(),
            },
        };

        let response = self.ipc_client.lock().await.request(request).await?;
        match response.payload {
            Some(ResponsePayload::ModuleMetrics(metrics)) => Ok(metrics),
            _ => {
                Err(ModuleError::OperationError(response.error.unwrap_or_else(
                    || "Failed to get module metrics".to_string(),
                )))
            }
        }
    }

    async fn read_file(&self, path: String) -> Result<Vec<u8>, ModuleError> {
        self.request(RequestPayload::ReadFile { path }, |payload| match payload {
            ResponsePayload::FileData(data) => Ok(data),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn write_file(&self, path: String, data: Vec<u8>) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::WriteFile { path, data },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn delete_file(&self, path: String) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::DeleteFile { path },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn list_directory(&self, path: String) -> Result<Vec<String>, ModuleError> {
        self.request(
            RequestPayload::ListDirectory { path },
            |payload| match payload {
                ResponsePayload::DirectoryListing(strings) => Ok(strings),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn create_directory(&self, path: String) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::CreateDirectory { path },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_file_metadata(
        &self,
        path: String,
    ) -> Result<crate::module::ipc::protocol::FileMetadata, ModuleError> {
        self.request(
            RequestPayload::GetFileMetadata { path },
            |payload| match payload {
                ResponsePayload::FileMetadata(metadata) => Ok(metadata),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_open_tree(&self, name: String) -> Result<String, ModuleError> {
        self.request(
            RequestPayload::StorageOpenTree { name },
            |payload| match payload {
                ResponsePayload::StorageTreeId(tree_id) => Ok(tree_id),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_insert(
        &self,
        tree_id: String,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::StorageInsert {
                tree_id,
                key,
                value,
            },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_get(
        &self,
        tree_id: String,
        key: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, ModuleError> {
        self.request(
            RequestPayload::StorageGet { tree_id, key },
            |payload| match payload {
                ResponsePayload::StorageValue(data) => Ok(data),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_remove(&self, tree_id: String, key: Vec<u8>) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::StorageRemove { tree_id, key },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_contains_key(
        &self,
        tree_id: String,
        key: Vec<u8>,
    ) -> Result<bool, ModuleError> {
        self.request(
            RequestPayload::StorageContainsKey { tree_id, key },
            |payload| match payload {
                ResponsePayload::Bool(b) => Ok(b),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_iter(&self, tree_id: String) -> Result<Vec<(Vec<u8>, Vec<u8>)>, ModuleError> {
        self.request(
            RequestPayload::StorageIter { tree_id },
            |payload| match payload {
                ResponsePayload::StorageKeyValuePairs(pairs) => Ok(pairs),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn storage_transaction(
        &self,
        tree_id: String,
        operations: Vec<crate::module::ipc::protocol::StorageOperation>,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::StorageTransaction {
                tree_id,
                operations,
            },
            |payload| match payload {
                ResponsePayload::Bool(_) | ResponsePayload::SubscribeAck => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn initialize_module(
        &self,
        _module_id: String,
        _module_data_dir: std::path::PathBuf,
        _base_data_dir: std::path::PathBuf,
    ) -> Result<(), ModuleError> {
        Err(ModuleError::OperationError(
            "initialize_module should not be called by modules".to_string(),
        ))
    }

    async fn discover_modules(
        &self,
    ) -> Result<Vec<crate::module::traits::ModuleInfo>, ModuleError> {
        self.request(RequestPayload::DiscoverModules, |payload| match payload {
            ResponsePayload::ModuleList(modules) => Ok(modules),
            _ => Err(ModuleError::OperationError(
                "Unexpected response type".to_string(),
            )),
        })
        .await
    }

    async fn get_module_info(
        &self,
        module_id: &str,
    ) -> Result<Option<crate::module::traits::ModuleInfo>, ModuleError> {
        self.request(
            RequestPayload::GetModuleInfo {
                module_id: module_id.to_string(),
            },
            |payload| match payload {
                ResponsePayload::ModuleInfo(info) => Ok(info),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn is_module_available(&self, module_id: &str) -> Result<bool, ModuleError> {
        self.request(
            RequestPayload::IsModuleAvailable {
                module_id: module_id.to_string(),
            },
            |payload| match payload {
                ResponsePayload::ModuleAvailable(available) => Ok(available),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn publish_event(
        &self,
        event_type: EventType,
        payload: EventPayload,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::PublishEvent {
                event_type,
                payload,
            },
            |payload| match payload {
                ResponsePayload::EventPublished => Ok(()),
                _ => Err(ModuleError::OperationError(
                    "Unexpected response type".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_all_metrics(
        &self,
    ) -> Result<
        std::collections::HashMap<String, Vec<crate::module::metrics::manager::Metric>>,
        ModuleError,
    > {
        self.request(
            RequestPayload::GetAllMetrics,
            |payload| match payload {
                ResponsePayload::AllMetrics(metrics) => Ok(metrics),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn call_module(
        &self,
        target_module_id: Option<&str>,
        method: &str,
        params: Vec<u8>,
    ) -> Result<Vec<u8>, ModuleError> {
        self.request(
            RequestPayload::CallModule {
                target_module_id: target_module_id.map(|s| s.to_string()),
                method: method.to_string(),
                params,
            },
            |payload| match payload {
                ResponsePayload::ModuleApiResponse(response) => Ok(response),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn register_module_api(
        &self,
        _api: Arc<dyn crate::module::inter_module::api::ModuleAPI>,
    ) -> Result<(), ModuleError> {
        Err(ModuleError::OperationError(
            "Module API registration must be done via module-side registration, not IPC".to_string(),
        ))
    }

    async fn unregister_module_api(&self) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::UnregisterModuleApi,
            |payload| match payload {
                ResponsePayload::ModuleApiUnregistered => Ok(()),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn send_mesh_packet_to_module(
        &self,
        _module_id: &str,
        _packet_data: Vec<u8>,
        _peer_addr: String,
    ) -> Result<(), ModuleError> {
        Err(ModuleError::OperationError(
            "send_mesh_packet_to_module is not available over IPC - use send_mesh_packet_to_peer instead".to_string(),
        ))
    }

    async fn send_mesh_packet_to_peer(
        &self,
        peer_addr: String,
        packet_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::SendMeshPacketToPeer {
                peer_addr,
                packet_data,
            },
            |payload| match payload {
                ResponsePayload::Bool(success) => {
                    if success {
                        Ok(())
                    } else {
                        Err(ModuleError::OperationError(
                            "Failed to send mesh packet".to_string(),
                        ))
                    }
                }
                _ => Err(ModuleError::OperationError(
                    "Invalid response format".to_string(),
                )),
            },
        )
        .await
    }

    async fn send_stratum_v2_message_to_peer(
        &self,
        peer_addr: String,
        message_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::SendStratumV2MessageToPeer {
                peer_addr,
                message_data,
            },
            |payload| match payload {
                ResponsePayload::Bool(success) => {
                    if success {
                        Ok(())
                    } else {
                        Err(ModuleError::OperationError(
                            "Failed to send Stratum V2 message".to_string(),
                        ))
                    }
                }
                _ => Err(ModuleError::OperationError(
                    "Invalid response format".to_string(),
                )),
            },
        )
        .await
    }

    async fn get_module_health(
        &self,
        module_id: &str,
    ) -> Result<Option<crate::module::process::monitor::ModuleHealth>, ModuleError> {
        self.request(
            RequestPayload::GetModuleHealth {
                module_id: module_id.to_string(),
            },
            |payload| match payload {
                ResponsePayload::ModuleHealth(health) => Ok(health),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn get_all_module_health(
        &self,
    ) -> Result<Vec<(String, crate::module::process::monitor::ModuleHealth)>, ModuleError> {
        self.request(
            RequestPayload::GetAllModuleHealth,
            |payload| match payload {
                ResponsePayload::AllModuleHealth(health) => Ok(health),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn report_module_health(
        &self,
        health: crate::module::process::monitor::ModuleHealth,
    ) -> Result<(), ModuleError> {
        self.request(
            RequestPayload::ReportModuleHealth { health },
            |payload| match payload {
                ResponsePayload::HealthReported => Ok(()),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn get_block_template(
        &self,
        rules: Vec<String>,
        coinbase_script: Option<Vec<u8>>,
        coinbase_address: Option<String>,
    ) -> Result<blvm_protocol::mining::BlockTemplate, ModuleError> {
        self.request(
            RequestPayload::GetBlockTemplate {
                rules,
                coinbase_script,
                coinbase_address,
            },
            |payload| match payload {
                ResponsePayload::BlockTemplate(template) => Ok(template),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }

    async fn submit_block(&self, block: Block) -> Result<crate::module::traits::SubmitBlockResult, ModuleError> {
        self.request(
            RequestPayload::SubmitBlock { block },
            |payload| match payload {
                ResponsePayload::SubmitBlockResult(result) => Ok(result),
                _ => Err(ModuleError::OperationError("Unexpected response type".to_string())),
            },
        )
        .await
    }
}

