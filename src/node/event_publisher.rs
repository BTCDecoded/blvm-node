//! Event publisher for module event notifications
//!
//! Bridges node events to the module event system (e.g. `blvm-zmq` subscribes for ZMQ PUB).

use std::sync::Arc;
use tracing::{debug, warn};

use crate::module::api::events::EventManager;
use crate::module::ipc::protocol::EventPayload;
use crate::module::traits::EventType;
use crate::{Block, Hash};

/// Event publisher that publishes node events to modules over the IPC event bus.
pub struct EventPublisher {
    event_manager: Arc<EventManager>,
    chain_tx: std::sync::Mutex<
        Option<tokio::sync::mpsc::UnboundedSender<crate::module::timers::ChainEvent>>,
    >,
}

impl EventPublisher {
    /// Create a new event publisher
    pub fn new(event_manager: Arc<EventManager>) -> Self {
        Self {
            event_manager,
            chain_tx: std::sync::Mutex::new(None),
        }
    }

    pub fn set_chain_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::module::timers::ChainEvent>,
    ) {
        if let Ok(mut slot) = self.chain_tx.lock() {
            *slot = Some(tx.clone());
        }
        crate::module::timers::clock::install_chain_feed(tx);
    }

    fn note_connected(&self, hash: Hash, height: u64, mtp: u64) {
        let Ok(slot) = self.chain_tx.lock() else {
            return;
        };
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(crate::module::timers::ChainEvent::Connected { height, hash, mtp });
        }
    }

    fn note_disconnected(&self, hash: Hash, height: u64) {
        let Ok(slot) = self.chain_tx.lock() else {
            return;
        };
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(crate::module::timers::ChainEvent::Disconnected { height, hash });
        }
    }

    /// Publish new block event (`blvm-zmq` and other modules consume via `subscribe_events`).
    pub async fn publish_new_block(&self, block: &Block, block_hash: &Hash, height: u64) {
        self.publish_new_block_at(block, block_hash, height, block.header.timestamp)
            .await;
    }

    pub async fn publish_new_block_at(
        &self,
        block: &Block,
        block_hash: &Hash,
        height: u64,
        mtp: u64,
    ) {
        self.note_connected(*block_hash, height, mtp);
        debug!(
            "Publishing NewBlock event for block {:?} at height {}",
            block_hash, height
        );

        let payload = EventPayload::NewBlock {
            block_hash: *block_hash,
            height,
        };

        if let Err(e) = self
            .event_manager
            .publish_event(EventType::NewBlock, payload)
            .await
        {
            warn!("Failed to publish NewBlock event: {}", e);
        }
    }

    /// Publish block disconnected event (chain reorg)
    pub async fn publish_block_disconnected(&self, hash: &Hash, height: u64) {
        self.note_disconnected(*hash, height);
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
            peer_node_id: None,
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

    /// Publish companion UDP endpoint for a P2P peer (`NODE_FIBRE` / port+1). Modules (e.g. `blvm-fibre`) treat it as a FIBRE UDP target.
    pub async fn publish_companion_udp_peer_registered(&self, p2p_peer_addr: &str, udp_addr: &str) {
        debug!(
            "Publishing CompanionUdpPeerRegistered: p2p={} udp={}",
            p2p_peer_addr, udp_addr
        );
        let payload = EventPayload::CompanionUdpPeerRegistered {
            p2p_peer_addr: p2p_peer_addr.to_string(),
            udp_addr: udp_addr.to_string(),
        };
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::CompanionUdpPeerRegistered, payload)
            .await
        {
            warn!("Failed to publish CompanionUdpPeerRegistered: {}", e);
        }
    }

    pub async fn publish_companion_udp_peer_unregistered(&self, p2p_peer_addr: &str) {
        debug!(
            "Publishing CompanionUdpPeerUnregistered: p2p={}",
            p2p_peer_addr
        );
        let payload = EventPayload::CompanionUdpPeerUnregistered {
            p2p_peer_addr: p2p_peer_addr.to_string(),
        };
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::CompanionUdpPeerUnregistered, payload)
            .await
        {
            warn!("Failed to publish CompanionUdpPeerUnregistered: {}", e);
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

    // === Extended module / mining events ===

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

    /// First native table for this tip. The estimator calls this after a recompute.
    pub async fn publish_fee_table_ready(&self, tip_hash: Hash, tip_height: u64, generation: u64) {
        let payload = EventPayload::FeeTableReady {
            tip_hash,
            tip_height,
            generation,
        };
        if let Err(e) = self
            .event_manager
            .publish_event(EventType::FeeTableReady, payload)
            .await
        {
            warn!("Failed to publish FeeTableReady event: {}", e);
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
