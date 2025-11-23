//! ZeroMQ publisher implementation
//!
//! Publishes blockchain events via ZMQ sockets compatible with standard
//! Bitcoin node notification interfaces.
//!
//! **BLLVM Differentiator**: ZMQ notifications are enabled by default in BLLVM node,
//! making real-time blockchain event notifications available out of the box.

use anyhow::{Context, Result};
use bincode;
use bllvm_protocol::{Block, Hash, Transaction};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

#[cfg(feature = "zmq")]
use zmq::{Context as ZmqContext, Socket, PUB};

/// ZMQ notification publisher
///
/// Publishes blockchain events to ZMQ subscribers using the standard
/// Bitcoin node notification format.
#[cfg(feature = "zmq")]
pub struct ZmqPublisher {
    /// ZMQ context
    context: Arc<ZmqContext>,
    /// Hash block socket (publishes block hashes)
    hashblock_socket: Option<Arc<Socket>>,
    /// Hash transaction socket (publishes transaction hashes)
    hashtx_socket: Option<Arc<Socket>>,
    /// Raw block socket (publishes raw block data)
    rawblock_socket: Option<Arc<Socket>>,
    /// Raw transaction socket (publishes raw transaction data)
    rawtx_socket: Option<Arc<Socket>>,
    /// Sequence socket (publishes sequence notifications)
    sequence_socket: Option<Arc<Socket>>,
    /// Sequence number counter (increments for each notification)
    sequence: Arc<RwLock<u32>>,
}

#[cfg(feature = "zmq")]
impl ZmqPublisher {
    /// Create a new ZMQ publisher
    ///
    /// # Arguments
    ///
    /// * `config` - ZMQ configuration with endpoint URLs
    pub fn new(config: &ZmqConfig) -> Result<Self> {
        let context = Arc::new(ZmqContext::new());
        let sequence = Arc::new(RwLock::new(0u32));

        // Create sockets for enabled notification types
        let hashblock_socket = if let Some(ref endpoint) = config.hashblock {
            Some(Arc::new(Self::create_socket(
                &context,
                endpoint,
                "hashblock",
            )?))
        } else {
            None
        };

        let hashtx_socket = if let Some(ref endpoint) = config.hashtx {
            Some(Arc::new(Self::create_socket(&context, endpoint, "hashtx")?))
        } else {
            None
        };

        let rawblock_socket = if let Some(ref endpoint) = config.rawblock {
            Some(Arc::new(Self::create_socket(
                &context, endpoint, "rawblock",
            )?))
        } else {
            None
        };

        let rawtx_socket = if let Some(ref endpoint) = config.rawtx {
            Some(Arc::new(Self::create_socket(&context, endpoint, "rawtx")?))
        } else {
            None
        };

        let sequence_socket = if let Some(ref endpoint) = config.sequence {
            Some(Arc::new(Self::create_socket(
                &context, endpoint, "sequence",
            )?))
        } else {
            None
        };

        Ok(Self {
            context,
            hashblock_socket,
            hashtx_socket,
            rawblock_socket,
            rawtx_socket,
            sequence_socket,
            sequence,
        })
    }

    /// Create and bind a ZMQ PUB socket
    fn create_socket(context: &ZmqContext, endpoint: &str, topic: &str) -> Result<Socket> {
        let socket = context.socket(PUB)?;
        socket
            .bind(endpoint)
            .with_context(|| format!("Failed to bind ZMQ socket for {} to {}", topic, endpoint))?;
        info!("ZMQ {} socket bound to {}", topic, endpoint);
        Ok(socket)
    }

    /// Publish a block hash notification
    ///
    /// Message format: [topic: "hashblock", block_hash: 32 bytes]
    pub async fn publish_hashblock(&self, block_hash: &Hash) -> Result<()> {
        if let Some(ref socket) = self.hashblock_socket {
            socket.send("hashblock", zmq::SNDMORE)?;
            socket.send(block_hash.as_slice(), 0)?;
            debug!("Published hashblock notification: {:?}", block_hash);
        }
        Ok(())
    }

    /// Publish a transaction hash notification
    ///
    /// Message format: [topic: "hashtx", tx_hash: 32 bytes]
    pub async fn publish_hashtx(&self, tx_hash: &Hash) -> Result<()> {
        if let Some(ref socket) = self.hashtx_socket {
            socket.send("hashtx", zmq::SNDMORE)?;
            socket.send(tx_hash.as_slice(), 0)?;
            debug!("Published hashtx notification: {:?}", tx_hash);
        }
        Ok(())
    }

    /// Publish a raw block notification
    ///
    /// Message format: [topic: "rawblock", block_data: serialized block in Bitcoin wire format]
    pub async fn publish_rawblock(&self, block: &Block) -> Result<()> {
        if let Some(ref socket) = self.rawblock_socket {
            // Serialize block to Bitcoin wire format
            // Note: For full compatibility, this should use Bitcoin P2P wire format
            // For now, using bincode serialization (can be enhanced later)
            let block_data = bincode::serialize(block)?;
            socket.send("rawblock", zmq::SNDMORE)?;
            socket.send(&block_data, 0)?;
            debug!(
                "Published rawblock notification: {} bytes",
                block_data.len()
            );
        }
        Ok(())
    }

    /// Publish a raw transaction notification
    ///
    /// Message format: [topic: "rawtx", tx_data: serialized transaction in Bitcoin wire format]
    pub async fn publish_rawtx(&self, tx: &Transaction) -> Result<()> {
        if let Some(ref socket) = self.rawtx_socket {
            // Serialize transaction to Bitcoin wire format
            // Note: For full compatibility, this should use Bitcoin P2P wire format
            // For now, using bincode serialization (can be enhanced later)
            let tx_data = bincode::serialize(tx)?;
            socket.send("rawtx", zmq::SNDMORE)?;
            socket.send(&tx_data, 0)?;
            debug!("Published rawtx notification: {} bytes", tx_data.len());
        }
        Ok(())
    }

    /// Publish a sequence notification
    ///
    /// Sequence notifications indicate mempool events (entry/removal).
    /// Message format: [topic: "sequence", data: 33 bytes (1 byte type + 32 bytes hash)]
    ///
    /// # Arguments
    ///
    /// * `tx_hash` - Transaction hash
    /// * `is_mempool_entry` - true if transaction entered mempool, false if removed
    pub async fn publish_sequence(&self, tx_hash: &Hash, is_mempool_entry: bool) -> Result<()> {
        if let Some(ref socket) = self.sequence_socket {
            // Increment sequence number
            let mut seq = self.sequence.write().await;
            *seq = seq.wrapping_add(1);
            let sequence_num = *seq;

            // Create sequence message: [1 byte type, 32 bytes hash]
            // Type: 0x01 = mempool entry, 0x02 = mempool removal
            let mut data = Vec::with_capacity(33);
            data.push(if is_mempool_entry { 0x01 } else { 0x02 });
            data.extend_from_slice(tx_hash.as_slice());

            socket.send("sequence", zmq::SNDMORE)?;
            socket.send(&data, 0)?;
            debug!(
                "Published sequence notification: seq={}, tx={:?}, entry={}",
                sequence_num, tx_hash, is_mempool_entry
            );
        }
        Ok(())
    }

    /// Publish all block notifications (hash and raw)
    ///
    /// Convenience method that publishes both hashblock and rawblock if enabled.
    pub async fn publish_block(&self, block: &Block, block_hash: &Hash) -> Result<()> {
        // Publish hashblock
        if let Err(e) = self.publish_hashblock(block_hash).await {
            warn!("Failed to publish hashblock notification: {}", e);
        }

        // Publish rawblock
        if let Err(e) = self.publish_rawblock(block).await {
            warn!("Failed to publish rawblock notification: {}", e);
        }

        Ok(())
    }

    /// Publish all transaction notifications (hash, raw, and sequence)
    ///
    /// Convenience method that publishes hashtx, rawtx, and sequence if enabled.
    ///
    /// # Arguments
    ///
    /// * `tx` - Transaction
    /// * `tx_hash` - Transaction hash
    /// * `is_mempool_entry` - true if entering mempool, false if removed
    pub async fn publish_transaction(
        &self,
        tx: &Transaction,
        tx_hash: &Hash,
        is_mempool_entry: bool,
    ) -> Result<()> {
        // Publish hashtx
        if let Err(e) = self.publish_hashtx(tx_hash).await {
            warn!("Failed to publish hashtx notification: {}", e);
        }

        // Publish rawtx
        if let Err(e) = self.publish_rawtx(tx).await {
            warn!("Failed to publish rawtx notification: {}", e);
        }

        // Publish sequence
        if let Err(e) = self.publish_sequence(tx_hash, is_mempool_entry).await {
            warn!("Failed to publish sequence notification: {}", e);
        }

        Ok(())
    }
}

/// ZMQ configuration
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "zmq", derive(serde::Serialize, serde::Deserialize))]
pub struct ZmqConfig {
    /// Endpoint for hashblock notifications (e.g., "tcp://127.0.0.1:28332")
    pub hashblock: Option<String>,
    /// Endpoint for hashtx notifications (e.g., "tcp://127.0.0.1:28333")
    pub hashtx: Option<String>,
    /// Endpoint for rawblock notifications (e.g., "tcp://127.0.0.1:28334")
    pub rawblock: Option<String>,
    /// Endpoint for rawtx notifications (e.g., "tcp://127.0.0.1:28335")
    pub rawtx: Option<String>,
    /// Endpoint for sequence notifications (e.g., "tcp://127.0.0.1:28336")
    pub sequence: Option<String>,
}

impl ZmqConfig {
    /// Check if any ZMQ notifications are enabled
    pub fn is_enabled(&self) -> bool {
        self.hashblock.is_some()
            || self.hashtx.is_some()
            || self.rawblock.is_some()
            || self.rawtx.is_some()
            || self.sequence.is_some()
    }
}

#[cfg(not(feature = "zmq"))]
/// Stub implementation when ZMQ feature is disabled
///
/// To disable ZMQ, build with: `--no-default-features --features <other-features>`
/// Or simply don't configure any ZMQ endpoints (ZMQ won't initialize if no endpoints are configured)
pub struct ZmqPublisher;

#[cfg(not(feature = "zmq"))]
impl ZmqPublisher {
    pub fn new(_config: &ZmqConfig) -> Result<Self> {
        Ok(Self)
    }

    pub async fn publish_hashblock(&self, _block_hash: &Hash) -> Result<()> {
        Ok(())
    }

    pub async fn publish_hashtx(&self, _tx_hash: &Hash) -> Result<()> {
        Ok(())
    }

    pub async fn publish_rawblock(&self, _block: &Block) -> Result<()> {
        Ok(())
    }

    pub async fn publish_rawtx(&self, _tx: &Transaction) -> Result<()> {
        Ok(())
    }

    pub async fn publish_sequence(&self, _tx_hash: &Hash, _is_mempool_entry: bool) -> Result<()> {
        Ok(())
    }

    pub async fn publish_block(&self, _block: &Block, _block_hash: &Hash) -> Result<()> {
        Ok(())
    }

    pub async fn publish_transaction(
        &self,
        _tx: &Transaction,
        _tx_hash: &Hash,
        _is_mempool_entry: bool,
    ) -> Result<()> {
        Ok(())
    }
}
