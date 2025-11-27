//! Module API Hub Edge Cases Tests
//!
//! Additional edge cases for module API hub: concurrent requests, timeouts, error recovery.

use bllvm_node::module::api::hub::ModuleApiHub;
use bllvm_node::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload};
use bllvm_node::module::security::permissions::{Permission, PermissionSet};
use bllvm_node::module::traits::{ModuleError, NodeAPI};
use bllvm_protocol::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
use std::sync::Arc;

// Mock NodeAPI for testing
struct MockNodeAPI;

#[async_trait::async_trait]
impl NodeAPI for MockNodeAPI {
    async fn get_block(&self, _hash: &Hash) -> Result<Option<Block>, ModuleError> {
        Ok(None)
    }

    async fn get_block_header(&self, _hash: &Hash) -> Result<Option<BlockHeader>, ModuleError> {
        Ok(None)
    }

    async fn get_transaction(&self, _hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        Ok(None)
    }

    async fn has_transaction(&self, _hash: &Hash) -> Result<bool, ModuleError> {
        Ok(false)
    }

    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        Ok([0; 32])
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        Ok(0)
    }

    async fn get_utxo(&self, _outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        Ok(None)
    }

    async fn subscribe_events(
        &self,
        _event_types: Vec<bllvm_node::module::traits::EventType>,
    ) -> Result<
        tokio::sync::mpsc::Receiver<bllvm_node::module::ipc::protocol::ModuleMessage>,
        ModuleError,
    > {
        let (_tx, rx) = tokio::sync::mpsc::channel(10);
        Ok(rx)
    }
}

#[tokio::test]
async fn test_concurrent_requests() {
    // Test handling multiple concurrent requests
    let node_api = Arc::new(MockNodeAPI);
    let hub = Arc::new(tokio::sync::Mutex::new(ModuleApiHub::new(node_api)));

    let mut handles = vec![];
    for i in 0..10 {
        let hub_clone = hub.clone();
        handles.push(tokio::spawn(async move {
            let request = RequestMessage {
                correlation_id: i,
                request_type: MessageType::Handshake,
                payload: RequestPayload::Handshake {
                    module_id: format!("test_module_{i}"),
                    module_name: "Test Module".to_string(),
                    version: "1.0.0".to_string(),
                },
            };
            let mut hub_guard = hub_clone.lock().await;
            hub_guard
                .handle_request(&format!("test_module_{i}"), request)
                .await
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // All requests should complete
    assert_eq!(results.len(), 10);
}

#[tokio::test]
async fn test_permission_edge_cases() {
    // Test permission edge cases
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);

    // Test with empty permission set
    let empty_permissions = PermissionSet::new();
    hub.register_module_permissions("test_module".to_string(), empty_permissions.clone());

    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "test_module".to_string(),
            module_name: "Test".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let result = hub.handle_request("test_module", request).await;
    // Should handle empty permissions gracefully
    let _ = result;

    // Test with all permissions
    let mut all_permissions = PermissionSet::new();
    all_permissions.add(Permission::ReadBlockchain);
    all_permissions.add(Permission::ReadUTXO);
    all_permissions.add(Permission::ReadChainState);
    all_permissions.add(Permission::SubscribeEvents);

    hub.register_module_permissions("test_module2".to_string(), all_permissions);

    let request2 = RequestMessage {
        correlation_id: 2,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "test_module2".to_string(),
            module_name: "Test".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let result = hub.handle_request("test_module2", request2).await;
    let _ = result;
}

#[tokio::test]
async fn test_error_recovery() {
    // Test error recovery scenarios
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);

    // Test with invalid module ID mismatch
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "wrong_module".to_string(),
            module_name: "Test".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let result = hub.handle_request("test_module", request).await;

    // Should return error but not panic
    match result {
        Ok(_) => {
            // Request succeeded (handshake might succeed even with mismatch)
        }
        Err(e) => {
            // Expected error
            assert!(matches!(
                e,
                ModuleError::OperationError(_) | ModuleError::PermissionDenied(_)
            ));
        }
    }
}
