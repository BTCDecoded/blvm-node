//! Event publisher for module event notifications
//! 
//! Bridges node events to module event system.

use std::sync::Arc;
use tracing::{debug, warn};

use crate::module::api::events::EventManager;
use crate::module::traits::EventType;
use crate::module::ipc::protocol::{EventPayload, EventMessage, ModuleMessage};
use crate::Hash;

/// Event publisher that publishes node events to modules
pub struct EventPublisher {
    event_manager: Arc<EventManager>,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new(event_manager: Arc<EventManager>) -> Self {
        Self { event_manager }
    }
    
    /// Publish new block event
    pub async fn publish_new_block(&self, block_hash: &Hash, height: u64) {
        debug!("Publishing NewBlock event for block {:?} at height {}", block_hash, height);
        
        let payload = EventPayload::NewBlock {
            block_hash: *block_hash,
            height,
        };
        
        if let Err(e) = self.event_manager.publish_event(EventType::NewBlock, payload).await {
            warn!("Failed to publish NewBlock event: {}", e);
        }
    }
    
    /// Publish new transaction event
    pub async fn publish_new_transaction(&self, tx_hash: &Hash) {
        debug!("Publishing NewTransaction event for tx {:?}", tx_hash);
        
        let payload = EventPayload::NewTransaction {
            tx_hash: *tx_hash,
        };
        
        if let Err(e) = self.event_manager.publish_event(EventType::NewTransaction, payload).await {
            warn!("Failed to publish NewTransaction event: {}", e);
        }
    }
    
    /// Publish block disconnected event (chain reorg)
    pub async fn publish_block_disconnected(&self, hash: &Hash, height: u64) {
        debug!("Publishing BlockDisconnected event for block {:?} at height {}", hash, height);
        
        let payload = EventPayload::BlockDisconnected {
            hash: *hash,
            height,
        };
        
        if let Err(e) = self.event_manager.publish_event(EventType::BlockDisconnected, payload).await {
            warn!("Failed to publish BlockDisconnected event: {}", e);
        }
    }
    
    /// Publish chain reorganization event
    pub async fn publish_chain_reorg(&self, old_tip: &Hash, new_tip: &Hash) {
        debug!("Publishing ChainReorg event: old_tip={:?}, new_tip={:?}", old_tip, new_tip);
        
        let payload = EventPayload::ChainReorg {
            old_tip: *old_tip,
            new_tip: *new_tip,
        };
        
        if let Err(e) = self.event_manager.publish_event(EventType::ChainReorg, payload).await {
            warn!("Failed to publish ChainReorg event: {}", e);
        }
    }
}

