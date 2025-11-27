//! Tests for IPC protocol message serialization and deserialization

use bllvm_node::module::ipc::protocol::{
    CorrelationId, EventMessage, EventPayload, MessageType, ModuleMessage, RequestMessage,
    RequestPayload, ResponseMessage, ResponsePayload,
};
use bllvm_node::module::traits::EventType;
use bllvm_node::{Block, BlockHeader, Hash, OutPoint, Transaction, UTXO};

#[test]
fn test_request_message_serialization() {
    let hash: Hash = [0xab; 32];
    let request = RequestMessage::get_block(1, hash);

    // Serialize
    let bytes = bincode::serialize(&request).unwrap();
    assert!(!bytes.is_empty());

    // Deserialize
    let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.correlation_id, 1);
    assert_eq!(deserialized.request_type, MessageType::GetBlock);
    match deserialized.payload {
        RequestPayload::GetBlock { hash: h } => assert_eq!(h, hash),
        _ => panic!("Expected GetBlock payload"),
    }
}

#[test]
fn test_response_message_serialization() {
    let response = ResponseMessage::success(1, ResponsePayload::Bool(true));

    // Serialize
    let bytes = bincode::serialize(&response).unwrap();
    assert!(!bytes.is_empty());

    // Deserialize
    let deserialized: ResponseMessage = bincode::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.correlation_id, 1);
    assert!(deserialized.success);
    assert!(deserialized.error.is_none());
    match deserialized.payload {
        Some(ResponsePayload::Bool(b)) => assert!(b),
        _ => panic!("Expected Bool(true) payload"),
    }
}

#[test]
fn test_error_response_serialization() {
    let response = ResponseMessage::error(1, "Test error".to_string());

    // Serialize
    let bytes = bincode::serialize(&response).unwrap();
    assert!(!bytes.is_empty());

    // Deserialize
    let deserialized: ResponseMessage = bincode::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.correlation_id, 1);
    assert!(!deserialized.success);
    assert_eq!(deserialized.error, Some("Test error".to_string()));
    assert!(deserialized.payload.is_none());
}

#[test]
fn test_module_message_wrapper() {
    let hash: Hash = [0xcd; 32];
    let request = RequestMessage::get_block(42, hash);
    let message = ModuleMessage::Request(request.clone());

    // Serialize
    let bytes = bincode::serialize(&message).unwrap();

    // Deserialize
    let deserialized: ModuleMessage = bincode::deserialize(&bytes).unwrap();
    match deserialized {
        ModuleMessage::Request(req) => {
            assert_eq!(req.correlation_id, 42);
            assert_eq!(req.request_type, MessageType::GetBlock);
        }
        _ => panic!("Expected Request message"),
    }

    // Test correlation_id method
    assert_eq!(message.correlation_id(), Some(42));
    assert_eq!(message.message_type(), MessageType::GetBlock);
}

#[test]
fn test_all_request_types() {
    let hash: Hash = [0xef; 32];
    let outpoint = OutPoint { hash, index: 0 };

    // Test all request helper methods
    let requests = vec![
        RequestMessage::get_block(1, hash),
        RequestMessage::get_block_header(2, hash),
        RequestMessage::get_transaction(3, hash),
        RequestMessage::has_transaction(4, hash),
        RequestMessage::get_chain_tip(5),
        RequestMessage::get_block_height(6),
        RequestMessage::get_utxo(7, outpoint),
        RequestMessage::subscribe_events(8, vec![EventType::NewBlock, EventType::NewTransaction]),
    ];

    for request in requests {
        let bytes = bincode::serialize(&request).unwrap();
        let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();
        assert_eq!(deserialized.correlation_id, request.correlation_id);
        assert_eq!(deserialized.request_type, request.request_type);
    }
}

#[test]
fn test_handshake_request() {
    let request = RequestMessage {
        correlation_id: 1,
        request_type: MessageType::Handshake,
        payload: RequestPayload::Handshake {
            module_id: "test-module".to_string(),
            module_name: "Test Module".to_string(),
            version: "1.0.0".to_string(),
        },
    };

    let bytes = bincode::serialize(&request).unwrap();
    let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();

    match deserialized.payload {
        RequestPayload::Handshake {
            module_id,
            module_name,
            version,
        } => {
            assert_eq!(module_id, "test-module");
            assert_eq!(module_name, "Test Module");
            assert_eq!(version, "1.0.0");
        }
        _ => panic!("Expected Handshake payload"),
    }
}

#[test]
fn test_response_payload_types() {
    let hash: Hash = [0x12; 32];
    let outpoint = OutPoint { hash, index: 1 };
    let utxo = UTXO {
        value: 1000,
        script_pubkey: vec![0x51], // OP_1
        height: 0,
    };

    let responses = vec![
        ResponseMessage::success(
            1,
            ResponsePayload::HandshakeAck {
                node_version: "1.0.0".to_string(),
            },
        ),
        ResponseMessage::success(2, ResponsePayload::Block(None)),
        ResponseMessage::success(3, ResponsePayload::BlockHeader(None)),
        ResponseMessage::success(4, ResponsePayload::Transaction(None)),
        ResponseMessage::success(5, ResponsePayload::Bool(true)),
        ResponseMessage::success(6, ResponsePayload::Hash(hash)),
        ResponseMessage::success(7, ResponsePayload::U64(12345)),
        ResponseMessage::success(8, ResponsePayload::Utxo(Some(utxo))),
        ResponseMessage::success(9, ResponsePayload::SubscribeAck),
    ];

    for response in responses {
        let bytes = bincode::serialize(&response).unwrap();
        let deserialized: ResponseMessage = bincode::deserialize(&bytes).unwrap();
        assert!(deserialized.success);
        assert!(deserialized.payload.is_some());
    }
}

#[test]
fn test_event_message_serialization() {
    let hash: Hash = [0x34; 32];
    let event = EventMessage {
        event_type: EventType::NewBlock,
        payload: EventPayload::NewBlock {
            block_hash: hash,
            height: 100,
        },
    };

    let message = ModuleMessage::Event(event.clone());

    // Serialize
    let bytes = bincode::serialize(&message).unwrap();

    // Deserialize
    let deserialized: ModuleMessage = bincode::deserialize(&bytes).unwrap();
    match deserialized {
        ModuleMessage::Event(e) => {
            assert_eq!(e.event_type, EventType::NewBlock);
            match e.payload {
                EventPayload::NewBlock { block_hash, height } => {
                    assert_eq!(block_hash, hash);
                    assert_eq!(height, 100);
                }
                _ => panic!("Expected NewBlock payload"),
            }
        }
        _ => panic!("Expected Event message"),
    }

    // Event messages don't have correlation IDs
    assert_eq!(message.correlation_id(), None);
    assert_eq!(message.message_type(), MessageType::Event);
}

#[test]
fn test_all_event_types() {
    let hash1: Hash = [0x56; 32];
    let hash2: Hash = [0x78; 32];

    let events = vec![
        EventMessage {
            event_type: EventType::NewBlock,
            payload: EventPayload::NewBlock {
                block_hash: hash1,
                height: 100,
            },
        },
        EventMessage {
            event_type: EventType::NewTransaction,
            payload: EventPayload::NewTransaction { tx_hash: hash1 },
        },
        EventMessage {
            event_type: EventType::NewBlock,
            payload: EventPayload::BlockDisconnected {
                hash: hash1,
                height: 99,
            },
        },
        EventMessage {
            event_type: EventType::NewBlock,
            payload: EventPayload::ChainReorg {
                old_tip: hash1,
                new_tip: hash2,
            },
        },
    ];

    for event in events {
        let message = ModuleMessage::Event(event);
        let bytes = bincode::serialize(&message).unwrap();
        let deserialized: ModuleMessage = bincode::deserialize(&bytes).unwrap();
        match deserialized {
            ModuleMessage::Event(_) => {}
            _ => panic!("Expected Event message"),
        }
    }
}

#[test]
fn test_correlation_id_matching() {
    let hash: Hash = [0x9a; 32];
    let request = RequestMessage::get_block(123, hash);
    let response = ResponseMessage::success(123, ResponsePayload::Block(None));

    let req_message = ModuleMessage::Request(request);
    let resp_message = ModuleMessage::Response(response);

    // Both should have the same correlation ID
    assert_eq!(req_message.correlation_id(), Some(123));
    assert_eq!(resp_message.correlation_id(), Some(123));
}

#[test]
fn test_message_type_classification() {
    let hash: Hash = [0xbc; 32];
    let request = RequestMessage::get_transaction(1, hash);
    let response = ResponseMessage::success(1, ResponsePayload::Transaction(None));
    let event = EventMessage {
        event_type: EventType::NewTransaction,
        payload: EventPayload::NewTransaction { tx_hash: hash },
    };

    assert_eq!(
        ModuleMessage::Request(request).message_type(),
        MessageType::GetTransaction
    );
    assert_eq!(
        ModuleMessage::Response(response).message_type(),
        MessageType::Response
    );
    assert_eq!(
        ModuleMessage::Event(event).message_type(),
        MessageType::Event
    );
}

#[test]
fn test_large_correlation_ids() {
    let hash: Hash = [0xde; 32];
    let large_id: CorrelationId = u64::MAX;
    let request = RequestMessage::get_block(large_id, hash);

    let bytes = bincode::serialize(&request).unwrap();
    let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();
    assert_eq!(deserialized.correlation_id, large_id);
}

#[test]
fn test_empty_event_types_subscription() {
    let request = RequestMessage::subscribe_events(1, vec![]);

    let bytes = bincode::serialize(&request).unwrap();
    let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();

    match deserialized.payload {
        RequestPayload::SubscribeEvents { event_types } => {
            assert!(event_types.is_empty());
        }
        _ => panic!("Expected SubscribeEvents payload"),
    }
}

#[test]
fn test_multiple_event_types_subscription() {
    let request = RequestMessage::subscribe_events(
        1,
        vec![
            EventType::NewBlock,
            EventType::NewTransaction,
            EventType::NewBlock, // Duplicate should be allowed
        ],
    );

    let bytes = bincode::serialize(&request).unwrap();
    let deserialized: RequestMessage = bincode::deserialize(&bytes).unwrap();

    match deserialized.payload {
        RequestPayload::SubscribeEvents { event_types } => {
            assert_eq!(event_types.len(), 3);
        }
        _ => panic!("Expected SubscribeEvents payload"),
    }
}
