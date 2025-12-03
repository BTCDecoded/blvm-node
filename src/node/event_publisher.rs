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

    /// Publish mempool transaction added event
    pub async fn publish_mempool_transaction_added(
        &self,
        tx_hash: &Hash,
        fee_rate: f64,
        mempool_size: usize,
    ) {
        debug!(
            "Publishing MempoolTransactionAdded event for tx {:?}",
            tx_hash
        );

        let payload = EventPayload::MempoolTransactionAdded {
            tx_hash: *tx_hash,
            fee_rate,
            mempool_size,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MempoolTransactionAdded, payload)
            .await
        {
            warn!("Failed to publish MempoolTransactionAdded event: {}", e);
        }
    }

    /// Publish mempool transaction removed event
    pub async fn publish_mempool_transaction_removed(
        &self,
        tx_hash: &Hash,
        reason: String,
        mempool_size: usize,
    ) {
        debug!(
            "Publishing MempoolTransactionRemoved event for tx {:?}",
            tx_hash
        );

        let payload = EventPayload::MempoolTransactionRemoved {
            tx_hash: *tx_hash,
            reason,
            mempool_size,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MempoolTransactionRemoved, payload)
            .await
        {
            warn!("Failed to publish MempoolTransactionRemoved event: {}", e);
        }
    }

    /// Publish fee rate changed event
    pub async fn publish_fee_rate_changed(
        &self,
        old_fee_rate: f64,
        new_fee_rate: f64,
        mempool_size: usize,
    ) {
        debug!(
            "Publishing FeeRateChanged event: {} -> {}",
            old_fee_rate, new_fee_rate
        );

        let payload = EventPayload::FeeRateChanged {
            old_fee_rate,
            new_fee_rate,
            mempool_size,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::FeeRateChanged, payload)
            .await
        {
            warn!("Failed to publish FeeRateChanged event: {}", e);
        }
    }

    /// Publish peer connected event
    pub async fn publish_peer_connected(
        &self,
        peer_addr: &str,
        transport_type: &str,
        services: u64,
        version: u32,
    ) {
        debug!("Publishing PeerConnected event for peer {}", peer_addr);

        let payload = EventPayload::PeerConnected {
            peer_addr: peer_addr.to_string(),
            transport_type: transport_type.to_string(),
            services,
            version,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::PeerConnected, payload)
            .await
        {
            warn!("Failed to publish PeerConnected event: {}", e);
        }
    }

    /// Publish peer disconnected event
    pub async fn publish_peer_disconnected(&self, peer_addr: &str, reason: &str) {
        debug!("Publishing PeerDisconnected event for peer {}", peer_addr);

        let payload = EventPayload::PeerDisconnected {
            peer_addr: peer_addr.to_string(),
            reason: reason.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::PeerDisconnected, payload)
            .await
        {
            warn!("Failed to publish PeerDisconnected event: {}", e);
        }
    }

    /// Publish block mined event
    pub async fn publish_block_mined(
        &self,
        block_hash: &Hash,
        height: u64,
        miner_id: Option<String>,
    ) {
        debug!(
            "Publishing BlockMined event for block {:?} at height {}",
            block_hash, height
        );

        let payload = EventPayload::BlockMined {
            block_hash: *block_hash,
            height,
            miner_id,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockMined, payload)
            .await
        {
            warn!("Failed to publish BlockMined event: {}", e);
        }
    }

    /// Publish block template updated event
    pub async fn publish_block_template_updated(
        &self,
        prev_hash: &Hash,
        height: u64,
        tx_count: usize,
    ) {
        debug!("Publishing BlockTemplateUpdated event at height {}", height);

        let payload = EventPayload::BlockTemplateUpdated {
            prev_hash: *prev_hash,
            height,
            tx_count,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockTemplateUpdated, payload)
            .await
        {
            warn!("Failed to publish BlockTemplateUpdated event: {}", e);
        }
    }

    // === Network Events ===

    /// Publish peer banned event
    pub async fn publish_peer_banned(
        &self,
        peer_addr: &str,
        reason: &str,
        ban_duration_seconds: u64,
    ) {
        debug!("Publishing PeerBanned event for peer {}", peer_addr);

        let payload = EventPayload::PeerBanned {
            peer_addr: peer_addr.to_string(),
            reason: reason.to_string(),
            ban_duration_seconds,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::PeerBanned, payload)
            .await
        {
            warn!("Failed to publish PeerBanned event: {}", e);
        }
    }

    /// Publish peer unbanned event
    pub async fn publish_peer_unbanned(&self, peer_addr: &str) {
        debug!("Publishing PeerUnbanned event for peer {}", peer_addr);

        let payload = EventPayload::PeerUnbanned {
            peer_addr: peer_addr.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::PeerUnbanned, payload)
            .await
        {
            warn!("Failed to publish PeerUnbanned event: {}", e);
        }
    }

    /// Publish message received event
    pub async fn publish_message_received(
        &self,
        peer_addr: &str,
        message_type: &str,
        message_size: usize,
        protocol_version: u32,
    ) {
        let payload = EventPayload::MessageReceived {
            peer_addr: peer_addr.to_string(),
            message_type: message_type.to_string(),
            message_size,
            protocol_version,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MessageReceived, payload)
            .await
        {
            warn!("Failed to publish MessageReceived event: {}", e);
        }
    }

    /// Publish message sent event
    pub async fn publish_message_sent(
        &self,
        peer_addr: &str,
        message_type: &str,
        message_size: usize,
    ) {
        let payload = EventPayload::MessageSent {
            peer_addr: peer_addr.to_string(),
            message_type: message_type.to_string(),
            message_size,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MessageSent, payload)
            .await
        {
            warn!("Failed to publish MessageSent event: {}", e);
        }
    }

    /// Publish broadcast started event
    pub async fn publish_broadcast_started(&self, message_type: &str, target_peers: usize) {
        let payload = EventPayload::BroadcastStarted {
            message_type: message_type.to_string(),
            target_peers,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BroadcastStarted, payload)
            .await
        {
            warn!("Failed to publish BroadcastStarted event: {}", e);
        }
    }

    /// Publish broadcast completed event
    pub async fn publish_broadcast_completed(
        &self,
        message_type: &str,
        successful: usize,
        failed: usize,
    ) {
        let payload = EventPayload::BroadcastCompleted {
            message_type: message_type.to_string(),
            successful,
            failed,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BroadcastCompleted, payload)
            .await
        {
            warn!("Failed to publish BroadcastCompleted event: {}", e);
        }
    }

    /// Publish route discovered event
    pub async fn publish_route_discovered(
        &self,
        destination: &[u8],
        route_path: &[String],
        route_cost: u64,
    ) {
        let payload = EventPayload::RouteDiscovered {
            destination: destination.to_vec(),
            route_path: route_path.to_vec(),
            route_cost,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::RouteDiscovered, payload)
            .await
        {
            warn!("Failed to publish RouteDiscovered event: {}", e);
        }
    }

    /// Publish route failed event
    pub async fn publish_route_failed(&self, destination: &[u8], reason: &str) {
        let payload = EventPayload::RouteFailed {
            destination: destination.to_vec(),
            reason: reason.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::RouteFailed, payload)
            .await
        {
            warn!("Failed to publish RouteFailed event: {}", e);
        }
    }

    /// Publish connection attempt event
    pub async fn publish_connection_attempt(
        &self,
        peer_addr: &str,
        success: bool,
        error: Option<&str>,
    ) {
        let payload = EventPayload::ConnectionAttempt {
            peer_addr: peer_addr.to_string(),
            success,
            error: error.map(|s| s.to_string()),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ConnectionAttempt, payload)
            .await
        {
            warn!("Failed to publish ConnectionAttempt event: {}", e);
        }
    }

    /// Publish address discovered event
    pub async fn publish_address_discovered(&self, peer_addr: &str, source: &str) {
        let payload = EventPayload::AddressDiscovered {
            peer_addr: peer_addr.to_string(),
            source: source.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::AddressDiscovered, payload)
            .await
        {
            warn!("Failed to publish AddressDiscovered event: {}", e);
        }
    }

    /// Publish address expired event
    pub async fn publish_address_expired(&self, peer_addr: &str) {
        let payload = EventPayload::AddressExpired {
            peer_addr: peer_addr.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::AddressExpired, payload)
            .await
        {
            warn!("Failed to publish AddressExpired event: {}", e);
        }
    }

    /// Publish network partition event
    pub async fn publish_network_partition(
        &self,
        partition_id: &[u8],
        disconnected_peers: &[String],
        partition_size: usize,
    ) {
        let payload = EventPayload::NetworkPartition {
            partition_id: partition_id.to_vec(),
            disconnected_peers: disconnected_peers.to_vec(),
            partition_size,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NetworkPartition, payload)
            .await
        {
            warn!("Failed to publish NetworkPartition event: {}", e);
        }
    }

    /// Publish network reconnected event
    pub async fn publish_network_reconnected(
        &self,
        partition_id: &[u8],
        reconnected_peers: &[String],
    ) {
        let payload = EventPayload::NetworkReconnected {
            partition_id: partition_id.to_vec(),
            reconnected_peers: reconnected_peers.to_vec(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NetworkReconnected, payload)
            .await
        {
            warn!("Failed to publish NetworkReconnected event: {}", e);
        }
    }

    /// Publish DoS attack detected event
    pub async fn publish_dos_attack_detected(
        &self,
        peer_addr: &str,
        attack_type: &str,
        severity: &str,
    ) {
        let payload = EventPayload::DoSAttackDetected {
            peer_addr: peer_addr.to_string(),
            attack_type: attack_type.to_string(),
            severity: severity.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::DoSAttackDetected, payload)
            .await
        {
            warn!("Failed to publish DoSAttackDetected event: {}", e);
        }
    }

    /// Publish rate limit exceeded event
    pub async fn publish_rate_limit_exceeded(
        &self,
        peer_addr: &str,
        limit_type: &str,
        current_rate: f64,
        limit: f64,
    ) {
        let payload = EventPayload::RateLimitExceeded {
            peer_addr: peer_addr.to_string(),
            limit_type: limit_type.to_string(),
            current_rate,
            limit,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::RateLimitExceeded, payload)
            .await
        {
            warn!("Failed to publish RateLimitExceeded event: {}", e);
        }
    }

    // === Mempool Events ===

    /// Publish mempool threshold exceeded event
    pub async fn publish_mempool_threshold_exceeded(&self, current_size: usize, threshold: usize) {
        let payload = EventPayload::MempoolThresholdExceeded {
            current_size,
            threshold,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MempoolThresholdExceeded, payload)
            .await
        {
            warn!("Failed to publish MempoolThresholdExceeded event: {}", e);
        }
    }

    /// Publish mempool cleared event
    pub async fn publish_mempool_cleared(&self, cleared_count: usize) {
        let payload = EventPayload::MempoolCleared { cleared_count };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MempoolCleared, payload)
            .await
        {
            warn!("Failed to publish MempoolCleared event: {}", e);
        }
    }

    // === Consensus Events ===

    /// Publish block validation started event
    pub async fn publish_block_validation_started(&self, block_hash: &Hash, height: u64) {
        let payload = EventPayload::BlockValidationStarted {
            block_hash: *block_hash,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockValidationStarted, payload)
            .await
        {
            warn!("Failed to publish BlockValidationStarted event: {}", e);
        }
    }

    /// Publish block validation completed event
    pub async fn publish_block_validation_completed(
        &self,
        block_hash: &Hash,
        height: u64,
        success: bool,
        validation_time_ms: u64,
        error: Option<&str>,
    ) {
        let payload = EventPayload::BlockValidationCompleted {
            block_hash: *block_hash,
            height,
            success,
            validation_time_ms,
            error: error.map(|s| s.to_string()),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockValidationCompleted, payload)
            .await
        {
            warn!("Failed to publish BlockValidationCompleted event: {}", e);
        }
    }

    /// Publish script verification started event
    pub async fn publish_script_verification_started(&self, tx_hash: &Hash, input_index: usize) {
        let payload = EventPayload::ScriptVerificationStarted {
            tx_hash: *tx_hash,
            input_index,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ScriptVerificationStarted, payload)
            .await
        {
            warn!("Failed to publish ScriptVerificationStarted event: {}", e);
        }
    }

    /// Publish script verification completed event
    pub async fn publish_script_verification_completed(
        &self,
        tx_hash: &Hash,
        input_index: usize,
        success: bool,
        verification_time_ms: u64,
    ) {
        let payload = EventPayload::ScriptVerificationCompleted {
            tx_hash: *tx_hash,
            input_index,
            success,
            verification_time_ms,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ScriptVerificationCompleted, payload)
            .await
        {
            warn!("Failed to publish ScriptVerificationCompleted event: {}", e);
        }
    }

    /// Publish UTXO validation started event
    pub async fn publish_utxo_validation_started(&self, block_hash: &Hash, height: u64) {
        let payload = EventPayload::UTXOValidationStarted {
            block_hash: *block_hash,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::UTXOValidationStarted, payload)
            .await
        {
            warn!("Failed to publish UTXOValidationStarted event: {}", e);
        }
    }

    /// Publish UTXO validation completed event
    pub async fn publish_utxo_validation_completed(
        &self,
        block_hash: &Hash,
        height: u64,
        success: bool,
    ) {
        let payload = EventPayload::UTXOValidationCompleted {
            block_hash: *block_hash,
            height,
            success,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::UTXOValidationCompleted, payload)
            .await
        {
            warn!("Failed to publish UTXOValidationCompleted event: {}", e);
        }
    }

    /// Publish difficulty adjusted event
    pub async fn publish_difficulty_adjusted(
        &self,
        old_difficulty: u32,
        new_difficulty: u32,
        height: u64,
    ) {
        let payload = EventPayload::DifficultyAdjusted {
            old_difficulty,
            new_difficulty,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::DifficultyAdjusted, payload)
            .await
        {
            warn!("Failed to publish DifficultyAdjusted event: {}", e);
        }
    }

    /// Publish soft fork activated event
    pub async fn publish_soft_fork_activated(&self, fork_name: &str, height: u64) {
        let payload = EventPayload::SoftForkActivated {
            fork_name: fork_name.to_string(),
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::SoftForkActivated, payload)
            .await
        {
            warn!("Failed to publish SoftForkActivated event: {}", e);
        }
    }

    /// Publish soft fork locked in event
    pub async fn publish_soft_fork_locked_in(&self, fork_name: &str, height: u64) {
        let payload = EventPayload::SoftForkLockedIn {
            fork_name: fork_name.to_string(),
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::SoftForkLockedIn, payload)
            .await
        {
            warn!("Failed to publish SoftForkLockedIn event: {}", e);
        }
    }

    /// Publish consensus rule violation event
    pub async fn publish_consensus_rule_violation(
        &self,
        rule_name: &str,
        block_hash: Option<&Hash>,
        tx_hash: Option<&Hash>,
        error: &str,
    ) {
        let payload = EventPayload::ConsensusRuleViolation {
            rule_name: rule_name.to_string(),
            block_hash: block_hash.copied(),
            tx_hash: tx_hash.copied(),
            error: error.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ConsensusRuleViolation, payload)
            .await
        {
            warn!("Failed to publish ConsensusRuleViolation event: {}", e);
        }
    }

    // === Sync Events ===

    /// Publish headers sync started event
    pub async fn publish_headers_sync_started(&self, start_height: u64) {
        let payload = EventPayload::HeadersSyncStarted { start_height };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::HeadersSyncStarted, payload)
            .await
        {
            warn!("Failed to publish HeadersSyncStarted event: {}", e);
        }
    }

    /// Publish headers sync progress event
    pub async fn publish_headers_sync_progress(
        &self,
        current_height: u64,
        target_height: u64,
        progress_percent: f64,
    ) {
        let payload = EventPayload::HeadersSyncProgress {
            current_height,
            target_height,
            progress_percent,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::HeadersSyncProgress, payload)
            .await
        {
            warn!("Failed to publish HeadersSyncProgress event: {}", e);
        }
    }

    /// Publish headers sync completed event
    pub async fn publish_headers_sync_completed(&self, final_height: u64, duration_seconds: u64) {
        let payload = EventPayload::HeadersSyncCompleted {
            final_height,
            duration_seconds,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::HeadersSyncCompleted, payload)
            .await
        {
            warn!("Failed to publish HeadersSyncCompleted event: {}", e);
        }
    }

    /// Publish block sync started event
    pub async fn publish_block_sync_started(&self, start_height: u64, target_height: u64) {
        let payload = EventPayload::BlockSyncStarted {
            start_height,
            target_height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockSyncStarted, payload)
            .await
        {
            warn!("Failed to publish BlockSyncStarted event: {}", e);
        }
    }

    /// Publish block sync progress event
    pub async fn publish_block_sync_progress(
        &self,
        current_height: u64,
        target_height: u64,
        progress_percent: f64,
        blocks_per_second: f64,
    ) {
        let payload = EventPayload::BlockSyncProgress {
            current_height,
            target_height,
            progress_percent,
            blocks_per_second,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockSyncProgress, payload)
            .await
        {
            warn!("Failed to publish BlockSyncProgress event: {}", e);
        }
    }

    /// Publish block sync completed event
    pub async fn publish_block_sync_completed(&self, final_height: u64, duration_seconds: u64) {
        let payload = EventPayload::BlockSyncCompleted {
            final_height,
            duration_seconds,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::BlockSyncCompleted, payload)
            .await
        {
            warn!("Failed to publish BlockSyncCompleted event: {}", e);
        }
    }

    /// Publish sync state changed event
    pub async fn publish_sync_state_changed(&self, old_state: &str, new_state: &str) {
        let payload = EventPayload::SyncStateChanged {
            old_state: old_state.to_string(),
            new_state: new_state.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::SyncStateChanged, payload)
            .await
        {
            warn!("Failed to publish SyncStateChanged event: {}", e);
        }
    }

    // === Mining Events ===

    /// Publish mining difficulty changed event
    pub async fn publish_mining_difficulty_changed(
        &self,
        old_difficulty: u32,
        new_difficulty: u32,
        height: u64,
    ) {
        let payload = EventPayload::MiningDifficultyChanged {
            old_difficulty,
            new_difficulty,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MiningDifficultyChanged, payload)
            .await
        {
            warn!("Failed to publish MiningDifficultyChanged event: {}", e);
        }
    }

    /// Publish mining job created event
    pub async fn publish_mining_job_created(&self, job_id: &str, prev_hash: &Hash, height: u64) {
        let payload = EventPayload::MiningJobCreated {
            job_id: job_id.to_string(),
            prev_hash: *prev_hash,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MiningJobCreated, payload)
            .await
        {
            warn!("Failed to publish MiningJobCreated event: {}", e);
        }
    }

    /// Publish share submitted event
    pub async fn publish_share_submitted(
        &self,
        job_id: &str,
        share_hash: &Hash,
        miner_id: Option<&str>,
    ) {
        let payload = EventPayload::ShareSubmitted {
            job_id: job_id.to_string(),
            share_hash: *share_hash,
            miner_id: miner_id.map(|s| s.to_string()),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ShareSubmitted, payload)
            .await
        {
            warn!("Failed to publish ShareSubmitted event: {}", e);
        }
    }

    /// Publish merge mining reward event
    pub async fn publish_merge_mining_reward(
        &self,
        secondary_chain: &str,
        reward_amount: u64,
        block_hash: &Hash,
    ) {
        let payload = EventPayload::MergeMiningReward {
            secondary_chain: secondary_chain.to_string(),
            reward_amount,
            block_hash: *block_hash,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MergeMiningReward, payload)
            .await
        {
            warn!("Failed to publish MergeMiningReward event: {}", e);
        }
    }

    /// Publish mining pool connected event
    pub async fn publish_mining_pool_connected(&self, pool_url: &str, pool_id: Option<&str>) {
        let payload = EventPayload::MiningPoolConnected {
            pool_url: pool_url.to_string(),
            pool_id: pool_id.map(|s| s.to_string()),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MiningPoolConnected, payload)
            .await
        {
            warn!("Failed to publish MiningPoolConnected event: {}", e);
        }
    }

    /// Publish mining pool disconnected event
    pub async fn publish_mining_pool_disconnected(&self, pool_url: &str, reason: &str) {
        let payload = EventPayload::MiningPoolDisconnected {
            pool_url: pool_url.to_string(),
            reason: reason.to_string(),
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::MiningPoolDisconnected, payload)
            .await
        {
            warn!("Failed to publish MiningPoolDisconnected event: {}", e);
        }
    }

    /// Generic event publishing method (for any event type)
    /// Publish configuration loaded/changed event
    ///
    /// Modules can subscribe to this to react to config changes.
    /// This is called when node configuration is loaded or updated.
    pub async fn publish_config_loaded(
        &self,
        changed_sections: Vec<String>,
        config_json: Option<String>,
    ) {
        debug!(
            "Publishing ConfigLoaded event for sections: {:?}",
            changed_sections
        );

        let payload = EventPayload::ConfigLoaded {
            changed_sections,
            config_json,
        };

        // Publish to module event system
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::ConfigLoaded, payload)
            .await
        {
            warn!("Failed to publish ConfigLoaded event: {}", e);
        }
    }

    /// Publish node shutdown event
    ///
    /// Modules should clean up gracefully when receiving this event.
    pub async fn publish_node_shutdown(&self, reason: String, timeout_seconds: u64) {
        debug!(
            "Publishing NodeShutdown event: reason={}, timeout={}s",
            reason, timeout_seconds
        );

        let payload = EventPayload::NodeShutdown {
            reason,
            timeout_seconds,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NodeShutdown, payload)
            .await
        {
            warn!("Failed to publish NodeShutdown event: {}", e);
        }
    }

    /// Publish data maintenance event (unified cleanup/flush)
    ///
    /// Modules should perform the requested maintenance operation:
    /// - "flush": Flush pending writes (for shutdown, urgent operations)
    /// - "cleanup": Delete old data (for periodic maintenance, low disk)
    /// - "both": Both flush and cleanup
    ///
    /// Urgency levels:
    /// - "low": Periodic maintenance, can be done asynchronously
    /// - "medium": Scheduled maintenance, should complete soon
    /// - "high": Urgent (shutdown, low disk), must complete quickly
    pub async fn publish_data_maintenance(
        &self,
        operation: String, // "flush", "cleanup", or "both"
        urgency: String,   // "low", "medium", or "high"
        reason: String,    // "periodic", "shutdown", "low_disk", "manual"
        target_age_days: Option<u64>,
        timeout_seconds: Option<u64>,
    ) {
        debug!(
            "Publishing DataMaintenance event: operation={}, urgency={}, reason={}",
            operation, urgency, reason
        );

        let payload = EventPayload::DataMaintenance {
            operation,
            urgency,
            reason,
            target_age_days,
            timeout_seconds,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::DataMaintenance, payload)
            .await
        {
            warn!("Failed to publish DataMaintenance event: {}", e);
        }
    }

    /// Publish disk space low event
    ///
    /// Modules should clean up data when disk space is low.
    pub async fn publish_disk_space_low(
        &self,
        available_bytes: u64,
        total_bytes: u64,
        percent_free: f64,
        disk_path: String,
    ) {
        debug!(
            "Publishing DiskSpaceLow event: available={} bytes, total={} bytes, percent_free={:.2}%",
            available_bytes, total_bytes, percent_free
        );

        let payload = EventPayload::DiskSpaceLow {
            available_bytes,
            total_bytes,
            percent_free,
            disk_path,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::DiskSpaceLow, payload)
            .await
        {
            warn!("Failed to publish DiskSpaceLow event: {}", e);
        }
    }

    /// Publish health check event
    ///
    /// Modules can report their health status when receiving this event.
    pub async fn publish_health_check(
        &self,
        check_type: String,
        node_healthy: bool,
        health_report: Option<String>,
    ) {
        debug!(
            "Publishing HealthCheck event: type={}, healthy={}",
            check_type, node_healthy
        );

        let payload = EventPayload::HealthCheck {
            check_type,
            node_healthy,
            health_report,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::HealthCheck, payload)
            .await
        {
            warn!("Failed to publish HealthCheck event: {}", e);
        }
    }

    /// Generic event publisher (for custom events)
    pub async fn publish_event(
        &self,
        event_type: EventType,
        payload: EventPayload,
    ) -> Result<(), crate::module::traits::ModuleError> {
        self.event_manager.publish_event(event_type, payload).await
    }
}

// Safety: EventPublisher is safe to share across threads (Sync) because:
// - EventManager is already Sync (uses Arc internally)
// - ZmqPublisher's Socket is only accessed through async methods which are Send
// - All operations are async and don't require synchronous access to the Socket
// This is a workaround for ZMQ's Socket type not being Sync/Send, but the actual usage is safe.
#[cfg(feature = "zmq")]
unsafe impl Sync for EventPublisher {}
#[cfg(feature = "zmq")]
unsafe impl Send for EventPublisher {}
