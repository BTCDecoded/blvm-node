//! Tests for protocol adapter

use blvm_node::network::protocol_adapter::ProtocolAdapter;
use blvm_node::network::transport::TransportType;
use blvm_protocol::GetBanListMessage;
use blvm_protocol::network::{
    AddrV2Message, AddressType, NetworkAddress, NetworkAddressV2, NetworkMessage, PingMessage,
    PongMessage, VersionMessage,
};

fn create_test_version_message() -> NetworkMessage {
    NetworkMessage::Version(VersionMessage {
        version: 70015,
        services: 1,
        timestamp: 1234567890,
        addr_recv: NetworkAddress {
            time: 0,
            services: 1,
            ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1],
            port: 8333,
        },
        addr_from: NetworkAddress {
            time: 0,
            services: 1,
            ip: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 127, 0, 0, 1],
            port: 8334,
        },
        nonce: 12345,
        user_agent: "test".to_string(),
        start_height: 0,
        relay: true,
    })
}

fn create_test_ping_message() -> NetworkMessage {
    NetworkMessage::Ping(PingMessage { nonce: 54321 })
}

fn create_test_pong_message() -> NetworkMessage {
    NetworkMessage::Pong(PongMessage { nonce: 54321 })
}

#[test]
fn test_protocol_adapter_serialize_tcp() {
    let msg = create_test_version_message();

    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_ok());

    let serialized = result.unwrap();
    // Should have at least magic bytes (4) + command (12) + length (4) + checksum (4) = 24 bytes
    assert!(serialized.len() >= 24);
}

#[test]
fn test_protocol_adapter_serialize_verack_tcp() {
    let msg = NetworkMessage::VerAck;

    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_ok());

    let serialized = result.unwrap();
    // Verack has no payload, so should be minimal
    assert!(serialized.len() >= 24);
}

#[test]
fn test_protocol_adapter_serialize_ping_tcp() {
    let msg = create_test_ping_message();

    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_ok());
}

#[test]
fn test_protocol_adapter_serialize_pong_tcp() {
    let msg = create_test_pong_message();

    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_ok());
}

#[test]
fn test_protocol_adapter_deserialize_tcp() {
    let msg = create_test_version_message();

    // Serialize first
    let serialized = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp).unwrap();
    assert!(!serialized.is_empty());

    let decoded = ProtocolAdapter::deserialize_message(&serialized, TransportType::Tcp)
        .expect("version round trip");
    assert_eq!(decoded, msg);
}

#[test]
fn test_protocol_adapter_roundtrip_version() {
    let original = create_test_version_message();

    // Serialize
    let serialized = ProtocolAdapter::serialize_message(&original, TransportType::Tcp).unwrap();
    assert!(!serialized.is_empty());

    let deserialized = ProtocolAdapter::deserialize_message(&serialized, TransportType::Tcp)
        .expect("version round trip");
    assert_eq!(deserialized, original);
}

#[test]
fn test_protocol_adapter_roundtrip_ping() {
    let original = create_test_ping_message();

    // Serialize
    let serialized = ProtocolAdapter::serialize_message(&original, TransportType::Tcp).unwrap();
    assert!(!serialized.is_empty());

    let deserialized = ProtocolAdapter::deserialize_message(&serialized, TransportType::Tcp)
        .expect("ping round trip");
    assert_eq!(deserialized, original);
}

#[test]
fn test_protocol_adapter_roundtrip_pong() {
    let original = create_test_pong_message();

    // Serialize
    let serialized = ProtocolAdapter::serialize_message(&original, TransportType::Tcp).unwrap();
    assert!(!serialized.is_empty());

    let deserialized = ProtocolAdapter::deserialize_message(&serialized, TransportType::Tcp)
        .expect("pong round trip");
    assert_eq!(deserialized, original);
}

#[test]
fn test_protocol_adapter_message_to_command() {
    // Test that message_to_command returns correct strings
    // This is tested indirectly through serialization
    let version_msg = create_test_version_message();
    let serialized =
        ProtocolAdapter::serialize_message(&version_msg, TransportType::Tcp).expect("version");
    assert!(serialized[4..16].starts_with(b"version"));
}

#[test]
fn test_protocol_adapter_consensus_to_protocol() {
    let msg = create_test_version_message();

    // This is tested indirectly through serialization
    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_ok());
}

#[test]
fn test_protocol_adapter_protocol_to_consensus() {
    let msg = create_test_version_message();

    // Serialize to test consensus_to_protocol conversion
    let serialized = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(serialized.is_ok());

    let decoded = ProtocolAdapter::deserialize_message(&serialized.unwrap(), TransportType::Tcp)
        .expect("version round trip");
    assert_eq!(decoded, msg);
}

#[test]
fn test_protocol_adapter_roundtrip_addrv2_tcp() {
    let msg = NetworkMessage::AddrV2(AddrV2Message {
        addresses: vec![NetworkAddressV2 {
            time: 1,
            services: 1033,
            address_type: AddressType::IPv4,
            address: vec![10, 0, 0, 1],
            port: 8333,
        }],
    });

    let bytes = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp).unwrap();
    let back = ProtocolAdapter::deserialize_message(&bytes, TransportType::Tcp).unwrap();
    assert_eq!(msg, back);
}

#[test]
fn test_protocol_adapter_unsupported_message() {
    // Commons ban-list request is not mapped in the TCP adapter (bincode path only on node)
    let msg = NetworkMessage::GetBanList(GetBanListMessage {
        request_id: 0,
        min_score: None,
    });

    let result = ProtocolAdapter::serialize_message(&msg, TransportType::Tcp);
    assert!(result.is_err());
}

#[test]
fn test_protocol_adapter_ping_nonces_produce_distinct_tcp_wire() {
    let p1 = NetworkMessage::Ping(PingMessage { nonce: 1 });
    let p2 = NetworkMessage::Ping(PingMessage { nonce: 2 });
    let w1 = ProtocolAdapter::serialize_message(&p1, TransportType::Tcp).unwrap();
    let w2 = ProtocolAdapter::serialize_message(&p2, TransportType::Tcp).unwrap();
    assert_ne!(w1, w2);
    #[cfg(feature = "production")]
    {
        let w1_cached = ProtocolAdapter::serialize_message(&p1, TransportType::Tcp).unwrap();
        assert_eq!(w1, w1_cached);
    }
}
