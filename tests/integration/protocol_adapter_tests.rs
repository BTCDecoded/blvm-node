//! Integration tests for protocol adapter

use anyhow::Result;
use bllvm_node::network::{
    protocol_adapter::ProtocolAdapter,
    transport::TransportType,
};
use bllvm_protocol::network::{
    NetworkMessage, VersionMessage, NetworkAddress, PingMessage, PongMessage,
};

#[test]
fn test_protocol_adapter_serialize_version_message() -> Result<()> {
    let version = NetworkMessage::Version(VersionMessage {
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
    
    // Serialize for TCP
    let tcp_bytes = ProtocolAdapter::serialize_message(&version, TransportType::Tcp)?;
    assert!(!tcp_bytes.is_empty());
    assert!(tcp_bytes.len() > 24); // At least header + payload
    
    Ok(())
}

#[test]
fn test_protocol_adapter_serialize_verack() -> Result<()> {
    let verack = NetworkMessage::VerAck;
    
    let tcp_bytes = ProtocolAdapter::serialize_message(&verack, TransportType::Tcp)?;
    assert!(!tcp_bytes.is_empty());
    
    Ok(())
}

#[test]
fn test_protocol_adapter_serialize_ping() -> Result<()> {
    let ping = NetworkMessage::Ping(PingMessage { nonce: 54321 });
    
    let tcp_bytes = ProtocolAdapter::serialize_message(&ping, TransportType::Tcp)?;
    assert!(!tcp_bytes.is_empty());
    
    Ok(())
}

#[test]
fn test_protocol_adapter_serialize_pong() -> Result<()> {
    let pong = NetworkMessage::Pong(PongMessage { nonce: 54321 });
    
    let tcp_bytes = ProtocolAdapter::serialize_message(&pong, TransportType::Tcp)?;
    assert!(!tcp_bytes.is_empty());
    
    Ok(())
}

#[cfg(feature = "iroh")]
#[test]
fn test_protocol_adapter_iroh_format() -> Result<()> {
    let version = NetworkMessage::Version(VersionMessage {
        version: 70015,
        services: 1,
        timestamp: 1234567890,
        addr_recv: NetworkAddress {
            services: 1,
            ip: [0; 16],
            port: 8333,
        },
        addr_from: NetworkAddress {
            services: 1,
            ip: [0; 16],
            port: 8333,
        },
        nonce: 12345,
        user_agent: "test".to_string(),
        start_height: 100,
        relay: true,
    });
    
    // Serialize for Iroh
    let iroh_bytes = ProtocolAdapter::serialize_message(&version, TransportType::Iroh)?;
    assert!(!iroh_bytes.is_empty());
    
    // Deserialize should work
    let deserialized = ProtocolAdapter::deserialize_message(&iroh_bytes, TransportType::Iroh)?;
    match (version, deserialized) {
        (NetworkMessage::Version(v1), NetworkMessage::Version(v2)) => {
            assert_eq!(v1.version, v2.version);
            assert_eq!(v1.nonce, v2.nonce);
        }
        _ => panic!("Message type mismatch"),
    }
    
    Ok(())
}

#[test]
fn test_protocol_adapter_tcp_roundtrip() -> Result<()> {
    let original = NetworkMessage::VerAck;
    
    // Serialize
    let bytes = ProtocolAdapter::serialize_message(&original, TransportType::Tcp)?;
    
    // Deserialize
    let deserialized = ProtocolAdapter::deserialize_message(&bytes, TransportType::Tcp)?;
    
    // Should match
    match (original, deserialized) {
        (NetworkMessage::VerAck, NetworkMessage::VerAck) => {
            // Success
        }
        _ => panic!("Message type mismatch"),
    }
    
    Ok(())
}

