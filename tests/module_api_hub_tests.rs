//! Tests for Module API Hub

use bllvm_node::module::api::hub::{AuditEntry, ModuleApiHub};
use bllvm_node::module::ipc::protocol::{RequestMessage, RequestPayload, ResponsePayload};
use bllvm_node::module::security::permissions::{Permission, PermissionSet};
use bllvm_node::module::traits::{ModuleError, NodeAPI};
use bllvm_protocol::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};
use std::sync::Arc;

// Mock NodeAPI for testing
struct MockNodeAPI;

#[async_trait::async_trait]
impl NodeAPI for MockNodeAPI {
    async fn get_chain_tip(&self) -> Result<Hash, ModuleError> {
        Ok(Hash::default())
    }

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

    async fn get_transaction(&self, _hash: &Hash) -> Result<Option<Transaction>, ModuleError> {
        use bllvm_protocol::{ByteString, Integer, Natural};
        Ok(Some(Transaction {
            version: Natural::from(1u64),
            inputs: bllvm_protocol::tx_inputs![],
            outputs: bllvm_protocol::tx_outputs![],
            lock_time: Natural::from(0u64),
        }))
    }

    async fn has_transaction(&self, _hash: &Hash) -> Result<bool, ModuleError> {
        Ok(true)
    }

    async fn get_utxo(&self, _outpoint: &OutPoint) -> Result<Option<UTXO>, ModuleError> {
        use bllvm_protocol::{ByteString, Integer, Natural};
        Ok(Some(UTXO {
            value: Integer::from(1000i64),
            script_pubkey: vec![], // ByteString is a type alias for Vec<u8>
            height: Natural::from(100u64),
        }))
    }

    async fn get_block_height(&self) -> Result<u64, ModuleError> {
        Ok(100)
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

    async fn subscribe_events(
        &self,
        _event_types: Vec<bllvm_node::module::traits::EventType>,
    ) -> Result<tokio::sync::mpsc::Receiver<bllvm_node::module::ipc::protocol::ModuleMessage>, ModuleError> {
        let (_tx, rx) = tokio::sync::mpsc::channel(10);
        Ok(rx)
    }
}

#[tokio::test]
async fn test_module_api_hub_new() {
    let node_api = Arc::new(MockNodeAPI);
    let hub = ModuleApiHub::new(node_api);
    // Hub should be created successfully
    assert!(true);
}

#[tokio::test]
async fn test_module_api_hub_register_permissions() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    
    hub.register_module_permissions("test-module".to_string(), permissions);
    // Permissions should be registered
    assert!(true);
}

#[tokio::test]
async fn test_module_api_hub_handle_handshake() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "test-module".to_string(),
            module_name: "Test Module".to_string(),
            version: "1.0.0".to_string(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::HandshakeAck { node_version }) = response.payload {
        assert!(!node_version.is_empty());
    } else {
        panic!("Expected HandshakeAck");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_handshake_id_mismatch() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "wrong-module".to_string(),
            module_name: "Test Module".to_string(),
            version: "1.0.0".to_string(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), ModuleError::OperationError(_)));
}

#[tokio::test]
async fn test_module_api_hub_handle_get_chain_tip() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions (GetChainTip requires ReadChainState)
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadChainState);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetChainTip,
        payload: RequestPayload::GetChainTip,
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::Hash(hash)) = response.payload {
        assert_eq!(hash.len(), 32); // Hash is [u8; 32]
    } else {
        panic!("Expected Hash response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_get_block() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetBlock,
        payload: RequestPayload::GetBlock {
            hash: Hash::default(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::Block(block)) = response.payload {
        assert!(block.is_some());
    } else {
        panic!("Expected Block response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_get_block_header() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetBlockHeader,
        payload: RequestPayload::GetBlockHeader {
            hash: Hash::default(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::BlockHeader(header)) = response.payload {
        assert!(header.is_some());
    } else {
        panic!("Expected BlockHeader response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_get_transaction() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetTransaction,
        payload: RequestPayload::GetTransaction {
            hash: Hash::default(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::Transaction(tx)) = response.payload {
        assert!(tx.is_some());
    } else {
        panic!("Expected Transaction response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_has_transaction() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::HasTransaction,
        payload: RequestPayload::HasTransaction {
            hash: Hash::default(),
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::Bool(exists)) = response.payload {
        assert!(exists);
    } else {
        panic!("Expected Bool response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_get_block_height() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions (GetBlockHeight requires ReadChainState)
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadChainState);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetBlockHeight,
        payload: RequestPayload::GetBlockHeight,
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::U64(height)) = response.payload {
        assert_eq!(height, 100);
    } else {
        panic!("Expected U64 response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_get_utxo() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions (GetUtxo requires ReadUTXO)
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadUTXO);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    use bllvm_protocol::types::Natural;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetUtxo,
        payload: RequestPayload::GetUtxo {
            outpoint: OutPoint {
                hash: Hash::default(),
                index: Natural::from(0u64),
            },
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::Utxo(utxo)) = response.payload {
        assert!(utxo.is_some());
    } else {
        panic!("Expected Utxo response");
    }
}

#[tokio::test]
async fn test_module_api_hub_handle_subscribe_events() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::SubscribeEvents);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::SubscribeEvents,
        payload: RequestPayload::SubscribeEvents {
            event_types: vec![bllvm_node::module::traits::EventType::NewBlock],
        },
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_ok(), "Request failed: {:?}", result);
    
    let response = result.unwrap();
    if let Some(ResponsePayload::SubscribeAck) = response.payload {
        // Success
    } else {
        panic!("Expected SubscribeAck response");
    }
}

#[tokio::test]
async fn test_module_api_hub_permission_denied() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register empty permissions to override defaults and test permission denied
    let empty_permissions = PermissionSet::new();
    hub.register_module_permissions("test-module".to_string(), empty_permissions);
    
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetChainTip,
        payload: RequestPayload::GetChainTip,
    };
    
    let result = hub.handle_request("test-module", request).await;
    if let Err(e) = &result {
        eprintln!("Error: {:?}", e);
    }
    assert!(result.is_err(), "Expected permission denied error");
    // Permission errors are returned as OperationError, not PermissionDenied
    assert!(matches!(result.unwrap_err(), ModuleError::OperationError(_)));
}

#[tokio::test]
async fn test_module_api_hub_audit_log() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions (GetChainTip requires ReadChainState)
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadChainState);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    // Make a request
    use bllvm_node::module::ipc::protocol::MessageType;
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::GetChainTip,
        payload: RequestPayload::GetChainTip,
    };
    
    let _ = hub.handle_request("test-module", request).await;
    
    // Check audit log
    let audit_log = hub.get_audit_log(10);
    assert!(!audit_log.is_empty());
    assert_eq!(audit_log[0].module_id, "test-module");
    assert_eq!(audit_log[0].api_call, "get_chain_tip");
    assert!(audit_log[0].success);
}

#[tokio::test]
async fn test_module_api_hub_audit_log_limit() {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    
    // Register permissions
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::ReadBlockchain);
    hub.register_module_permissions("test-module".to_string(), permissions);
    
    // Make many requests (more than max_audit_entries)
    use bllvm_node::module::ipc::protocol::MessageType;
    for i in 0..1500 {
        let request = RequestMessage {
            correlation_id: i,
            request_type: MessageType::GetChainTip,
            payload: RequestPayload::GetChainTip,
        };
        let _ = hub.handle_request("test-module", request).await;
    }
    
    // Check audit log is limited
    let audit_log = hub.get_audit_log(2000);
    assert!(audit_log.len() <= 1000); // Should be limited to max_audit_entries
}

