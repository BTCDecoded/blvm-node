//! Integration tests for message bridge

use anyhow::Result;
use bllvm_node::network::{
    message_bridge::MessageBridge,
    transport::TransportType,
};
use bllvm_protocol::network::{
    NetworkMessage, NetworkResponse, VersionMessage, NetworkAddress, PingMessage,
};

#[test]
fn test_message_bridge_to_transport_tcp() -> Result<()> {
    let msg = NetworkMessage::VerAck;
    
    let bytes = MessageBridge::to_transport_message(&msg, TransportType::Tcp)?;
    assert!(!bytes.is_empty());
    
    Ok(())
}

#[test]
fn test_message_bridge_from_transport_tcp() -> Result<()> {
    // Create a valid Bitcoin P2P message
    let version_msg = NetworkMessage::Version(VersionMessage {
        version: 70015,
        services: 1,
        timestamp: 1234567890,
        addr_recv: NetworkAddress {
            services: 1,
            ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1],
            port: 8333,
        },
        addr_from: NetworkAddress {
            services: 1,
            ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1],
            port: 8333,
        },
        nonce: 12345,
        user_agent: "test".to_string(),
        start_height: 100,
        relay: true,
    });
    
    // Serialize first
    let bytes = MessageBridge::to_transport_message(&version_msg, TransportType::Tcp)?;
    
    // Then deserialize
    let deserialized = MessageBridge::from_transport_message(&bytes, TransportType::Tcp)?;
    
    match (version_msg, deserialized) {
        (NetworkMessage::Version(v1), NetworkMessage::Version(v2)) => {
            assert_eq!(v1.version, v2.version);
            assert_eq!(v1.nonce, v2.nonce);
        }
        _ => panic!("Message type mismatch"),
    }
    
    Ok(())
}

#[test]
fn test_message_bridge_extract_send_messages_ok() -> Result<()> {
    let response = NetworkResponse::Ok;
    let messages = MessageBridge::extract_send_messages(&response, TransportType::Tcp)?;
    assert_eq!(messages.len(), 0);
    
    Ok(())
}

#[test]
fn test_message_bridge_extract_send_messages_single() -> Result<()> {
    let msg = NetworkMessage::VerAck;
    let response = NetworkResponse::SendMessage(msg);
    let messages = MessageBridge::extract_send_messages(&response, TransportType::Tcp)?;
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].is_empty());
    
    Ok(())
}

#[test]
fn test_message_bridge_extract_send_messages_multiple() -> Result<()> {
    let msg1 = NetworkMessage::VerAck;
    let msg2 = NetworkMessage::Ping(PingMessage { nonce: 12345 });
    let response = NetworkResponse::SendMessages(vec![msg1, msg2]);
    let messages = MessageBridge::extract_send_messages(&response, TransportType::Tcp)?;
    assert_eq!(messages.len(), 2);
    
    Ok(())
}

#[test]
fn test_message_bridge_extract_send_messages_reject() -> Result<()> {
    let response = NetworkResponse::Reject("Test rejection".to_string());
    let messages = MessageBridge::extract_send_messages(&response, TransportType::Tcp)?;
    assert_eq!(messages.len(), 0); // Rejections don't send messages
    
    Ok(())
}

