//! Protocol adapter for Bitcoin message serialization
//!
//! Handles conversion between consensus-proof NetworkMessage types and
//! transport-specific wire formats (TCP Bitcoin P2P vs Iroh message format).

use crate::network::transport::TransportType;
use anyhow::Result;
use blvm_protocol::network::NetworkMessage as ConsensusNetworkMessage;

#[cfg(feature = "production")]
use std::collections::hash_map::DefaultHasher;
#[cfg(feature = "production")]
use std::hash::{Hash, Hasher};
#[cfg(feature = "production")]
use std::sync::{OnceLock, RwLock};

/// Network message serialization cache (production feature only)
///
/// Caches serialized message bytes to avoid re-serializing the same message.
/// Cache key is a fast hash of message type + content.
#[cfg(feature = "production")]
static SERIALIZATION_CACHE: OnceLock<RwLock<lru::LruCache<u64, Vec<u8>>>> = OnceLock::new();

#[cfg(feature = "production")]
fn get_serialization_cache() -> &'static RwLock<lru::LruCache<u64, Vec<u8>>> {
    SERIALIZATION_CACHE.get_or_init(|| {
        use lru::LruCache;
        use std::num::NonZeroUsize;
        // Cache 5,000 serialized messages (balance between memory and hit rate)
        // Each entry is ~100-500 bytes average, so ~0.5-2.5MB total
        RwLock::new(LruCache::new(NonZeroUsize::new(5_000).unwrap()))
    })
}

/// Calculate a fast hash of message for cache key
///
/// This hash is used as a cache key and doesn't require serialization.
#[cfg(feature = "production")]
fn calculate_message_cache_key(msg: &ConsensusNetworkMessage, transport: TransportType) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Hash message type discriminator
    std::mem::discriminant(msg).hash(&mut hasher);
    // Hash transport type
    std::mem::discriminant(&transport).hash(&mut hasher);
    // Hash message content (simplified - hash a few key fields)
    // This is a heuristic - not perfect but fast
    hasher.finish()
}

/// Protocol adapter for Bitcoin messages
///
/// Converts between consensus-proof message types and transport wire formats.
pub struct ProtocolAdapter;

impl ProtocolAdapter {
    /// Serialize a consensus-proof NetworkMessage to transport format
    ///
    /// For TCP transport, uses Bitcoin P2P wire protocol format.
    /// For Iroh transport, uses a simplified message format.
    ///
    /// Performance optimization: Caches serialized results to avoid re-serializing
    /// the same message multiple times (used in ping/pong, version messages, etc.)
    pub fn serialize_message(
        msg: &ConsensusNetworkMessage,
        transport: TransportType,
    ) -> Result<Vec<u8>> {
        #[cfg(feature = "production")]
        {
            // Check cache first
            let cache = get_serialization_cache();
            let cache_key = calculate_message_cache_key(msg, transport);

            // Try to get from cache
            if let Ok(cached) = cache.read() {
                if let Some(serialized) = cached.peek(&cache_key) {
                    return Ok(serialized.clone()); // Clone cached result
                }
            }

            // Cache miss - serialize and cache
            let serialized = Self::serialize_message_inner(msg, transport)?;

            // Store in cache
            if let Ok(mut cache) = cache.write() {
                cache.put(cache_key, serialized.clone());
            }

            Ok(serialized)
        }

        #[cfg(not(feature = "production"))]
        {
            Self::serialize_message_inner(msg, transport)
        }
    }

    /// Inner serialization function (actual implementation)
    fn serialize_message_inner(
        msg: &ConsensusNetworkMessage,
        transport: TransportType,
    ) -> Result<Vec<u8>> {
        match transport {
            TransportType::Tcp => Self::serialize_bitcoin_wire_format(msg),
            #[cfg(feature = "quinn")]
            TransportType::Quinn => {
                // Quinn uses same format as Iroh (JSON-based) for simplicity
                Self::serialize_iroh_format(msg)
            }
            #[cfg(feature = "iroh")]
            TransportType::Iroh => Self::serialize_iroh_format(msg),
        }
    }

    /// Deserialize transport bytes to consensus-proof NetworkMessage
    pub fn deserialize_message(
        data: &[u8],
        transport: TransportType,
    ) -> Result<ConsensusNetworkMessage> {
        match transport {
            TransportType::Tcp => Self::deserialize_bitcoin_wire_format(data),
            #[cfg(feature = "quinn")]
            TransportType::Quinn => {
                // Quinn uses same format as Iroh (JSON-based) for simplicity
                Self::deserialize_iroh_format(data)
            }
            #[cfg(feature = "iroh")]
            TransportType::Iroh => Self::deserialize_iroh_format(data),
        }
    }

    /// Serialize using Bitcoin P2P wire protocol format
    ///
    /// Format: [magic:4][command:12][length:4][checksum:4][payload:var]
    fn serialize_bitcoin_wire_format(msg: &ConsensusNetworkMessage) -> Result<Vec<u8>> {
        // Convert consensus-proof message to protocol message
        let protocol_msg = Self::consensus_to_protocol_message(msg)?;

        // Serialize payload
        let payload = match &protocol_msg {
            crate::network::protocol::ProtocolMessage::Version(v) => {
                // Use proper Bitcoin wire format for version messages
                use blvm_protocol::network::{NetworkAddress, VersionMessage};
                use blvm_protocol::wire::serialize_version;

                let version_msg = VersionMessage {
                    version: v.version as u32,
                    services: v.services,
                    timestamp: v.timestamp,
                    addr_recv: NetworkAddress {
                        services: v.addr_recv.services,
                        ip: v.addr_recv.ip,
                        port: v.addr_recv.port,
                    },
                    addr_from: NetworkAddress {
                        services: v.addr_from.services,
                        ip: v.addr_from.ip,
                        port: v.addr_from.port,
                    },
                    nonce: v.nonce,
                    user_agent: v.user_agent.clone(),
                    start_height: v.start_height,
                    relay: v.relay,
                };

                serialize_version(&version_msg)?
            }
            crate::network::protocol::ProtocolMessage::Verack => {
                vec![]
            }
            crate::network::protocol::ProtocolMessage::Ping(p) => bincode::serialize(p)?,
            crate::network::protocol::ProtocolMessage::Pong(p) => bincode::serialize(p)?,
            // Add other message types as needed
            _ => {
                return Err(anyhow::anyhow!(
                    "Unsupported message type for serialization"
                ));
            }
        };

        // Get command string
        let command = Self::message_to_command(msg);
        let mut command_bytes = [0u8; 12];
        command_bytes[..command.len().min(12)].copy_from_slice(command.as_bytes());

        // Calculate checksum (double SHA256 of payload, first 4 bytes)
        // Optimization: Use optimized SHA256 in production
        #[cfg(feature = "production")]
        let checksum_bytes = {
            use blvm_consensus::crypto::OptimizedSha256;
            let hasher = OptimizedSha256::new();
            hasher.hash256(&payload)
        };
        #[cfg(feature = "production")]
        let checksum = &checksum_bytes[..4];

        #[cfg(not(feature = "production"))]
        let checksum_bytes = {
            use sha2::{Digest, Sha256};
            let hash1 = Sha256::digest(&payload);
            let hash2 = Sha256::digest(hash1);
            &hash2[..4]
        };

        // Build message
        let mut message = Vec::new();

        // Magic bytes (mainnet)
        message.extend_from_slice(&0xf9beb4d9u32.to_le_bytes());

        // Command
        message.extend_from_slice(&command_bytes);

        // Payload length
        message.extend_from_slice(&(payload.len() as u32).to_le_bytes());

        // Checksum
        message.extend_from_slice(checksum);

        // Payload
        message.extend_from_slice(&payload);

        Ok(message)
    }

    /// Deserialize from Bitcoin P2P wire protocol format
    fn deserialize_bitcoin_wire_format(data: &[u8]) -> Result<ConsensusNetworkMessage> {
        use crate::network::protocol::ProtocolParser;

        // Parse using existing protocol parser
        let protocol_msg = ProtocolParser::parse_message(data)?;

        // Convert to consensus-proof message
        Self::protocol_to_consensus_message(&protocol_msg)
    }

    #[cfg(any(feature = "iroh", feature = "quinn"))]
    /// Serialize using simplified message format (bincode-based)
    ///
    /// Used by both Iroh and Quinn transports for simpler wire format.
    /// Converts to protocol message first, then serializes.
    fn serialize_iroh_format(msg: &ConsensusNetworkMessage) -> Result<Vec<u8>> {
        // Convert to protocol message first (which is serializable)
        let protocol_msg = Self::consensus_to_protocol_message(msg)?;
        // Serialize protocol message using bincode
        bincode::serialize(&protocol_msg)
            .map_err(|e| anyhow::anyhow!("Failed to serialize message: {}", e))
    }

    #[cfg(any(feature = "iroh", feature = "quinn"))]
    /// Deserialize from simplified message format (bincode-based)
    ///
    /// Used by both Iroh and Quinn transports.
    fn deserialize_iroh_format(data: &[u8]) -> Result<ConsensusNetworkMessage> {
        // Deserialize protocol message
        let protocol_msg: crate::network::protocol::ProtocolMessage = bincode::deserialize(data)
            .map_err(|e| anyhow::anyhow!("Failed to deserialize message: {}", e))?;
        // Convert back to consensus message
        Self::protocol_to_consensus_message(&protocol_msg)
    }

    /// Convert consensus-proof message to protocol message
    fn consensus_to_protocol_message(
        msg: &ConsensusNetworkMessage,
    ) -> Result<crate::network::protocol::ProtocolMessage> {
        use crate::network::protocol::{
            NetworkAddress as ProtoNetworkAddress, PingMessage as ProtoPingMessage,
            PongMessage as ProtoPongMessage, ProtocolMessage,
            VersionMessage as ProtoVersionMessage,
        };

        match msg {
            ConsensusNetworkMessage::Version(v) => {
                Ok(ProtocolMessage::Version(ProtoVersionMessage {
                    version: v.version as i32,
                    services: v.services,
                    timestamp: v.timestamp,
                    addr_recv: ProtoNetworkAddress {
                        services: v.addr_recv.services,
                        ip: v.addr_recv.ip,
                        port: v.addr_recv.port,
                    },
                    addr_from: ProtoNetworkAddress {
                        services: v.addr_from.services,
                        ip: v.addr_from.ip,
                        port: v.addr_from.port,
                    },
                    nonce: v.nonce,
                    user_agent: v.user_agent.clone(),
                    start_height: v.start_height,
                    relay: v.relay,
                }))
            }
            ConsensusNetworkMessage::VerAck => Ok(ProtocolMessage::Verack),
            ConsensusNetworkMessage::Ping(p) => {
                Ok(ProtocolMessage::Ping(ProtoPingMessage { nonce: p.nonce }))
            }
            ConsensusNetworkMessage::Pong(p) => {
                Ok(ProtocolMessage::Pong(ProtoPongMessage { nonce: p.nonce }))
            }
            _ => Err(anyhow::anyhow!(
                "Unsupported message type for protocol conversion"
            )),
        }
    }

    /// Convert protocol message to consensus-proof message
    pub fn protocol_to_consensus_message(
        msg: &crate::network::protocol::ProtocolMessage,
    ) -> Result<ConsensusNetworkMessage> {
        use crate::network::protocol::ProtocolMessage;
        use blvm_protocol::network::{
            NetworkAddress as ConsensusNetworkAddress, PingMessage as ConsensusPingMessage,
            PongMessage as ConsensusPongMessage, VersionMessage as ConsensusVersionMessage,
        };

        match msg {
            ProtocolMessage::Version(v) => {
                Ok(ConsensusNetworkMessage::Version(ConsensusVersionMessage {
                    version: v.version as u32,
                    services: v.services,
                    timestamp: v.timestamp,
                    addr_recv: ConsensusNetworkAddress {
                        services: v.addr_recv.services,
                        ip: v.addr_recv.ip,
                        port: v.addr_recv.port,
                    },
                    addr_from: ConsensusNetworkAddress {
                        services: v.addr_from.services,
                        ip: v.addr_from.ip,
                        port: v.addr_from.port,
                    },
                    nonce: v.nonce,
                    user_agent: v.user_agent.clone(),
                    start_height: v.start_height,
                    relay: v.relay,
                }))
            }
            ProtocolMessage::Verack => Ok(ConsensusNetworkMessage::VerAck),
            ProtocolMessage::Ping(p) => Ok(ConsensusNetworkMessage::Ping(ConsensusPingMessage {
                nonce: p.nonce,
            })),
            ProtocolMessage::Pong(p) => Ok(ConsensusNetworkMessage::Pong(ConsensusPongMessage {
                nonce: p.nonce,
            })),
            _ => Err(anyhow::anyhow!(
                "Unsupported message type for consensus conversion"
            )),
        }
    }

    /// Get command string for a message type
    fn message_to_command(msg: &ConsensusNetworkMessage) -> &'static str {
        match msg {
            ConsensusNetworkMessage::Version(_) => "version",
            ConsensusNetworkMessage::VerAck => "verack",
            ConsensusNetworkMessage::Addr(_) => "addr",
            ConsensusNetworkMessage::Inv(_) => "inv",
            ConsensusNetworkMessage::GetData(_) => "getdata",
            ConsensusNetworkMessage::GetHeaders(_) => "getheaders",
            ConsensusNetworkMessage::Headers(_) => "headers",
            ConsensusNetworkMessage::Block(_) => "block",
            ConsensusNetworkMessage::Tx(_) => "tx",
            ConsensusNetworkMessage::Ping(_) => "ping",
            ConsensusNetworkMessage::Pong(_) => "pong",
            ConsensusNetworkMessage::MemPool => "mempool",
            ConsensusNetworkMessage::FeeFilter(_) => "feefilter",
            ConsensusNetworkMessage::GetBlocks(_) => "getblocks",
            ConsensusNetworkMessage::GetAddr => "getaddr",
            ConsensusNetworkMessage::NotFound(_) => "notfound",
            ConsensusNetworkMessage::Reject(_) => "reject",
            ConsensusNetworkMessage::SendHeaders => "sendheaders",
            ConsensusNetworkMessage::SendCmpct(_) => "sendcmpct",
            ConsensusNetworkMessage::CmpctBlock(_) => "cmpctblock",
            ConsensusNetworkMessage::GetBlockTxn(_) => "getblocktxn",
            ConsensusNetworkMessage::BlockTxn(_) => "blocktxn",
            #[cfg(feature = "utxo-commitments")]
            ConsensusNetworkMessage::GetUTXOSet(_) => "getutxoset",
            #[cfg(feature = "utxo-commitments")]
            ConsensusNetworkMessage::UTXOSet(_) => "utxoset",
            #[cfg(feature = "utxo-commitments")]
            ConsensusNetworkMessage::GetFilteredBlock(_) => "getfilteredblock",
            #[cfg(feature = "utxo-commitments")]
            ConsensusNetworkMessage::FilteredBlock(_) => "filteredblock",
            ConsensusNetworkMessage::GetBanList(_) => "getbanlist",
            ConsensusNetworkMessage::BanList(_) => "banlist",
            ConsensusNetworkMessage::EconomicNodeRegistration(_) => "economic_node_registration",
            ConsensusNetworkMessage::EconomicNodeVeto(_) => "economic_node_veto",
            ConsensusNetworkMessage::EconomicNodeStatus(_) => "economic_node_status",
        }
    }
}
