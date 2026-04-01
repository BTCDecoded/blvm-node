//! Full `NodeAPI` implementation for integration tests (module hub, IPC, etc.).

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

use blvm_node::module::inter_module::api::ModuleAPI;
use blvm_node::module::ipc::protocol::{EventPayload, FileMetadata, ModuleMessage};
use blvm_node::module::metrics::manager::Metric;
use blvm_node::module::process::monitor::ModuleHealth;
use blvm_node::module::timers::manager::{TaskCallback, TaskId, TimerCallback, TimerId};
use blvm_node::module::traits::{
    ChainInfo, EventType, LightningInfo, MempoolSize, ModuleError, ModuleInfo, NetworkStats,
    NodeAPI, PaymentState, PeerInfo, SubmitBlockResult,
};
use blvm_node::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

pub struct MockNodeAPI;

#[async_trait]
impl NodeAPI for MockNodeAPI {
    async fn get_block(&self, _hash: &Hash) -> Result<Option<Block>, ModuleError> {
        Ok(Some(Block {
            header: BlockHeader {
                version: 1,
                prev_block_hash: Hash::default(),
                merkle_root: Hash::default(),
                timestamp: 0,
                bits: 0,
                nonce: 0,
            },
            transactions: vec![].into(),
        }))
    }

    async fn get_block_header(&self, _hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        Ok(Some(BlockHeader {
            version: 1,
            prev_block_hash: Hash::default(),
            merkle_root: Hash::default(),
            timestamp: 0,
            bits: 0,
            nonce: 0,
        }))
    }

    async fn get_transaction(&self, _hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        use blvm_protocol::Natural;
        Ok(Some(Transaction {
            version: Natural::from(1u64),
            inputs: blvm_protocol::tx_inputs![],
            outputs: blvm_protocol::tx_outputs![],
            lock_time: Natural::from(0u64),
        }))
    }

    async fn has_transaction(&self, _hash: &Hash) -> Result<bool, ModuleError> {
        Ok(true)
    }

    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        Ok(Hash::default())
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        Ok(100)
    }

    async fn get_utxo(&self, _outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        Ok(Some(UTXO {
            value: 1000,
            script_pubkey: vec![].into(),
            height: 100,
            is_coinbase: false,
        }))
    }

    async fn subscribe_events(
        &self,
        _event_types: Vec<EventType>,
    ) -> Result<tokio::sync::mpsc::Receiver<ModuleMessage>, ModuleError> {
        let (_tx, rx) = tokio::sync::mpsc::channel(10);
        Ok(rx)
    }

    async fn get_mempool_transactions(&self) -> Result<Vec<Hash>, ModuleError> {
        Ok(vec![])
    }

    async fn get_mempool_transaction(
        &self,
        _tx_hash: &Hash,
    ) -> Result<Option<Transaction>, ModuleError> {
        Ok(None)
    }

    async fn get_mempool_size(&self) -> Result<MempoolSize, ModuleError> {
        Ok(MempoolSize {
            transaction_count: 0,
            size_bytes: 0,
            total_fee_sats: 0,
        })
    }

    async fn get_network_stats(&self) -> Result<NetworkStats, ModuleError> {
        Ok(NetworkStats {
            peer_count: 0,
            hash_rate: 0.0,
            bytes_sent: 0,
            bytes_received: 0,
        })
    }

    async fn get_network_peers(&self) -> Result<Vec<PeerInfo>, ModuleError> {
        Ok(vec![])
    }

    async fn get_chain_info(&self) -> Result<ChainInfo, ModuleError> {
        Ok(ChainInfo {
            tip_hash: Hash::default(),
            height: 0,
            difficulty: 0,
            chain_work: 0,
            is_synced: false,
        })
    }

    async fn get_block_by_height(&self, _height: u64) -> Result<Option<Block>, ModuleError> {
        Ok(None)
    }

    async fn get_lightning_node_url(&self) -> Result<Option<String>, ModuleError> {
        Ok(None)
    }

    async fn get_lightning_info(&self) -> Result<Option<LightningInfo>, ModuleError> {
        Ok(None)
    }

    async fn get_payment_state(&self, _payment_id: &str) -> Result<Option<PaymentState>, ModuleError> {
        Ok(None)
    }

    async fn check_transaction_in_mempool(&self, _tx_hash: &Hash) -> Result<bool, ModuleError> {
        Ok(false)
    }

    async fn get_fee_estimate(&self, _target_blocks: u32) -> Result<u64, ModuleError> {
        Ok(1)
    }

    async fn register_rpc_endpoint(
        &self,
        _method: String,
        _description: String,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn unregister_rpc_endpoint(&self, _method: &str) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn register_timer(
        &self,
        _interval_seconds: u64,
        _callback: Arc<dyn TimerCallback>,
    ) -> Result<TimerId, ModuleError> {
        Ok(0)
    }

    async fn cancel_timer(&self, _timer_id: TimerId) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn schedule_task(
        &self,
        _delay_seconds: u64,
        _callback: Arc<dyn TaskCallback>,
    ) -> Result<TaskId, ModuleError> {
        Ok(0)
    }

    async fn report_metric(&self, _metric: Metric) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn get_module_metrics(&self, _module_id: &str) -> Result<Vec<Metric>, ModuleError> {
        Ok(vec![])
    }

    async fn get_all_metrics(
        &self,
    ) -> Result<HashMap<String, Vec<Metric>>, ModuleError> {
        Ok(HashMap::new())
    }

    async fn read_file(&self, _path: String) -> Result<Vec<u8>, ModuleError> {
        Ok(vec![])
    }

    async fn write_file(&self, _path: String, _data: Vec<u8>) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn delete_file(&self, _path: String) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn list_directory(&self, _path: String) -> Result<Vec<String>, ModuleError> {
        Ok(vec![])
    }

    async fn create_directory(&self, _path: String) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn get_file_metadata(&self, path: String) -> Result<FileMetadata, ModuleError> {
        Ok(FileMetadata {
            path,
            size: 0,
            is_file: false,
            is_directory: false,
            modified: None,
            created: None,
        })
    }

    async fn initialize_module(
        &self,
        _module_id: String,
        _module_data_dir: std::path::PathBuf,
        _base_data_dir: std::path::PathBuf,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn discover_modules(&self) -> Result<Vec<ModuleInfo>, ModuleError> {
        Ok(vec![])
    }

    async fn get_module_info(&self, _module_id: &str) -> Result<Option<ModuleInfo>, ModuleError> {
        Ok(None)
    }

    async fn is_module_available(&self, _module_id: &str) -> Result<bool, ModuleError> {
        Ok(false)
    }

    async fn publish_event(
        &self,
        _event_type: EventType,
        _payload: EventPayload,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn call_module(
        &self,
        _target_module_id: Option<&str>,
        _method: &str,
        _params: Vec<u8>,
    ) -> Result<Vec<u8>, ModuleError> {
        Ok(vec![])
    }

    async fn register_module_api(&self, _api: Arc<dyn ModuleAPI>) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn unregister_module_api(&self) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn send_mesh_packet_to_module(
        &self,
        _module_id: &str,
        _packet_data: Vec<u8>,
        _peer_addr: String,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn send_mesh_packet_to_peer(
        &self,
        _peer_addr: String,
        _packet_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn send_stratum_v2_message_to_peer(
        &self,
        _peer_addr: String,
        _message_data: Vec<u8>,
    ) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn get_module_health(&self, _module_id: &str) -> Result<Option<ModuleHealth>, ModuleError> {
        Ok(None)
    }

    async fn get_all_module_health(
        &self,
    ) -> Result<Vec<(String, ModuleHealth)>, ModuleError> {
        Ok(vec![])
    }

    async fn report_module_health(&self, _health: ModuleHealth) -> Result<(), ModuleError> {
        Ok(())
    }

    async fn get_block_template(
        &self,
        _rules: Vec<String>,
        _coinbase_script: Option<Vec<u8>>,
        _coinbase_address: Option<String>,
    ) -> Result<blvm_protocol::mining::BlockTemplate, ModuleError> {
        Err(ModuleError::OperationError("stub get_block_template".into()))
    }

    async fn submit_block(&self, _block: Block) -> Result<SubmitBlockResult, ModuleError> {
        Ok(SubmitBlockResult::Accepted)
    }
}
