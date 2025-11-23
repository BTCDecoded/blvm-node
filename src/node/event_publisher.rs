//! Event publisher for module event notifications
//!
//! Bridges node events to module event system and ZMQ notifications.

use std::sync::Arc;
use tracing::{debug, warn};

use crate::module::api::events::EventManager;
use crate::module::ipc::protocol::EventPayload;
use crate::module::traits::EventType;
#[cfg(feature = "zmq")]
use crate::zmq::ZmqPublisher;
use crate::{Block, Hash, Transaction};

/// Event publisher that publishes node events to modules and ZMQ
pub struct EventPublisher {
    event_manager: Arc<EventManager>,
    #[cfg(feature = "zmq")]
    zmq_publisher: Option<Arc<ZmqPublisher>>,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new(event_manager: Arc<EventManager>) -> Self {
        Self {
            event_manager,
            #[cfg(feature = "zmq")]
            zmq_publisher: None,
        }
    }

    /// Create a new event publisher with ZMQ support
    #[cfg(feature = "zmq")]
    pub fn with_zmq(
        event_manager: Arc<EventManager>,
        zmq_publisher: Option<Arc<ZmqPublisher>>,
    ) -> Self {
        Self {
            event_manager,
            zmq_publisher,
        }
    }

    /// Publish new block event (with block data for ZMQ)
    ///
    /// Publishes to both module event system and ZMQ (if enabled).
    pub async fn publish_new_block(&self, block: &Block, block_hash: &Hash, height: u64) {
        debug!(
            "Publishing NewBlock event for block {:?} at height {}",
            block_hash, height
        );

        let payload = EventPayload::NewBlock {
            block_hash: *block_hash,
            height,
        };

        // Publish to module event system
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NewBlock, payload)
            .await
        {
            warn!("Failed to publish NewBlock event: {}", e);
        }

        // Publish to ZMQ (if enabled)
        #[cfg(feature = "zmq")]
        if let Some(ref zmq) = self.zmq_publisher {
            if let Err(e) = zmq.publish_block(block, block_hash).await {
                warn!("Failed to publish ZMQ block notification: {}", e);
            }
        }
    }

    /// Publish new transaction event (with transaction data for ZMQ)
    ///
    /// Publishes to both module event system and ZMQ (if enabled).
    ///
    /// # Arguments
    ///
    /// * `tx` - Transaction object (for ZMQ rawtx notification)
    /// * `tx_hash` - Transaction hash
    /// * `is_mempool_entry` - true if entering mempool, false if removed
    pub async fn publish_new_transaction(
        &self,
        tx: &Transaction,
        tx_hash: &Hash,
        is_mempool_entry: bool,
    ) {
        debug!("Publishing NewTransaction event for tx {:?}", tx_hash);

        let payload = EventPayload::NewTransaction { tx_hash: *tx_hash };

        // Publish to module event system
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NewTransaction, payload)
            .await
        {
            warn!("Failed to publish NewTransaction event: {}", e);
        }

        // Publish to ZMQ (if enabled)
        #[cfg(feature = "zmq")]
        if let Some(ref zmq) = self.zmq_publisher {
            if let Err(e) = zmq.publish_transaction(tx, tx_hash, is_mempool_entry).await {
                warn!("Failed to publish ZMQ transaction notification: {}", e);
            }
        }
    }

    /// Publish block disconnected event (chain reorg)
    pub async fn publish_block_disconnected(&self, hash: &Hash, height: u64) {
        debug!(
            "Publishing BlockDisconnected event for block {:?} at height {}",
            hash, height
        );

        let payload = EventPayload::BlockDisconnected {
            hash: *hash,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockDisconnected, payload)
            .await
        {
            warn!("Failed to publish BlockDisconnected event: {}", e);
        }
    }

    /// Publish chain reorganization event
    pub async fn publish_chain_reorg(&self, old_tip: &Hash, new_tip: &Hash) {
        debug!(
            "Publishing ChainReorg event: old_tip={:?}, new_tip={:?}",
            old_tip, new_tip
        );

        let payload = EventPayload::ChainReorg {
            old_tip: *old_tip,
            new_tip: *new_tip,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ChainReorg, payload)
            .await
        {
            warn!("Failed to publish ChainReorg event: {}", e);
        }
    }
}
