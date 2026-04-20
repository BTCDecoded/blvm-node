//! Payment Reorg Handler
//!
//! Subscribes to BlockDisconnected and ChainReorg events, downgrades Settled payments
//! when their block is disconnected. Re-broadcasts orphaned txs from cache/storage when possible.

#[cfg(feature = "ctv")]
use crate::node::mempool::MempoolManager;
#[cfg(feature = "ctv")]
use crate::payment::settlement::SettlementMonitor;
#[cfg(feature = "ctv")]
use crate::payment::state_machine::{PaymentState, PaymentStateMachine};
#[cfg(feature = "ctv")]
use crate::payment::tx_cache::PaymentTxCache;
#[cfg(feature = "ctv")]
use crate::storage::Storage;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Buffer size for reorg event channel (avoid dropping events under load)
const REORG_CHANNEL_BUFFER: usize = 1024;

/// Interval for periodic ReorgPending re-check (payor may have re-broadcast)
const REORG_PENDING_CHECK_INTERVAL_SECS: u64 = 90;

/// Handles chain reorg events for payment settlement state.
/// Subscribes to BlockDisconnected and ChainReorg, downgrades Settled payments
/// when their block is disconnected.
#[cfg(feature = "ctv")]
pub struct PaymentReorgHandler {
    #[allow(dead_code)]
    /// Receiving task handle; kept alive so the task runs
    task_handle: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "ctv")]
impl PaymentReorgHandler {
    /// Create and start the reorg handler.
    ///
    /// Subscribes to BlockDisconnected and ChainReorg from the event manager,
    /// spawns a task to process events.
    pub fn new(
        state_machine: Arc<PaymentStateMachine>,
        storage: Option<Arc<Storage>>,
        mempool_manager: Option<Arc<MempoolManager>>,
        tx_cache: Option<Arc<PaymentTxCache>>,
        settlement_monitor: Option<Arc<SettlementMonitor>>,
        event_manager: Arc<crate::module::api::events::EventManager>,
    ) -> Self {
        let (tx, mut rx) =
            mpsc::channel::<crate::module::ipc::protocol::ModuleMessage>(REORG_CHANNEL_BUFFER);

        let module_id = "payment_reorg_internal".to_string();
        let event_types = vec![
            crate::module::traits::EventType::BlockDisconnected,
            crate::module::traits::EventType::ChainReorg,
        ];

        let task_handle = tokio::spawn(async move {
            // Subscribe first
            if let Err(e) = event_manager
                .subscribe_module(module_id.clone(), event_types, tx)
                .await
            {
                warn!("PaymentReorgHandler failed to subscribe: {}", e);
                return;
            }
            info!("PaymentReorgHandler subscribed to BlockDisconnected and ChainReorg");

            // Spawn periodic ReorgPending re-check (payor may re-broadcast tx)
            let state_machine_for_periodic = Arc::clone(&state_machine);
            let mempool_for_periodic = mempool_manager.clone();
            let settlement_for_periodic = settlement_monitor.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
                    REORG_PENDING_CHECK_INTERVAL_SECS,
                ));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    Self::recheck_reorg_pending(
                        &state_machine_for_periodic,
                        mempool_for_periodic.as_deref(),
                        settlement_for_periodic.as_deref(),
                    )
                    .await;
                }
            });

            while let Some(msg) = rx.recv().await {
                if let crate::module::ipc::protocol::ModuleMessage::Event(event_msg) = msg {
                    use crate::module::ipc::protocol::EventPayload;
                    match &event_msg.payload {
                        EventPayload::BlockDisconnected { hash, height } => {
                            Self::on_block_disconnected(
                                &state_machine,
                                storage.as_deref(),
                                mempool_manager.as_deref(),
                                tx_cache.as_deref(),
                                settlement_monitor.as_deref(),
                                hash,
                                *height,
                            )
                            .await;
                        }
                        EventPayload::ChainReorg { old_tip, new_tip } => {
                            debug!(
                                "PaymentReorgHandler: ChainReorg old={:?} new={:?}",
                                old_tip, new_tip
                            );
                            // BlockDisconnected is published per block; we rely on that for now
                        }
                        _ => {}
                    }
                }
            }
            info!("PaymentReorgHandler: event channel closed");
        });

        Self { task_handle }
    }

    async fn on_block_disconnected(
        state_machine: &PaymentStateMachine,
        storage: Option<&Storage>,
        mempool_manager: Option<&MempoolManager>,
        tx_cache: Option<&PaymentTxCache>,
        settlement_monitor: Option<&SettlementMonitor>,
        block_hash: &crate::Hash,
        _height: u64,
    ) {
        let states = state_machine.list_payment_states();
        let affected: Vec<_> = states
            .into_iter()
            .filter_map(|(id, state)| {
                if let PaymentState::Settled {
                    request_id,
                    tx_hash,
                    block_hash: b,
                    expected_outputs,
                    ..
                } = state
                {
                    if &b == block_hash {
                        Some((request_id, tx_hash, expected_outputs))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        let affected_count = affected.len();
        for (payment_id, tx_hash, expected_outputs) in affected {
            // 0. Chain re-verification: tx might still be in best chain (different block)
            if let Some(storage) = storage {
                if let Ok(Some(metadata)) = storage.transactions().get_metadata(&tx_hash) {
                    if storage
                        .chain()
                        .is_invalid(&metadata.block_hash)
                        .unwrap_or(true)
                        == false
                    {
                        debug!(
                            "Reorg: payment {} tx {} still in valid chain (block {:?}), keeping Settled",
                            payment_id,
                            hex::encode(tx_hash),
                            metadata.block_hash
                        );
                        continue;
                    }
                }
            }

            // 1. Check mempool: if tx is there, downgrade to InMempool
            let in_mempool = mempool_manager
                .map(|m| m.get_transaction(&tx_hash).is_some())
                .unwrap_or(false);

            if in_mempool {
                info!(
                    "Reorg: payment {} tx {} in mempool, downgrading to InMempool",
                    payment_id,
                    hex::encode(tx_hash)
                );
                let _ = state_machine.mark_in_mempool(&payment_id, tx_hash).await;
                Self::restart_monitoring_if_needed(
                    settlement_monitor,
                    &payment_id,
                    expected_outputs.as_deref(),
                    tx_hash,
                )
                .await;
                continue;
            }

            // 2. Try to get tx from cache or storage for re-broadcast
            let tx_opt = tx_cache.and_then(|c| c.get(&tx_hash)).or_else(|| {
                storage.and_then(|s| s.transactions().get_transaction(&tx_hash).ok().flatten())
            });

            if let Some(tx) = tx_opt {
                // 3. Attempt re-broadcast to mempool
                if let Some(mempool) = mempool_manager {
                    match mempool.add_transaction(tx.clone()) {
                        Ok(true) => {
                            if let Some(cache) = tx_cache {
                                cache.store(tx_hash, tx);
                            }
                            info!(
                                "Reorg: payment {} tx {} re-broadcast to mempool, downgrading to InMempool",
                                payment_id,
                                hex::encode(tx_hash)
                            );
                            let _ = state_machine.mark_in_mempool(&payment_id, tx_hash).await;
                            Self::restart_monitoring_if_needed(
                                settlement_monitor,
                                &payment_id,
                                expected_outputs.as_deref(),
                                tx_hash,
                            )
                            .await;
                            continue;
                        }
                        Ok(false) => {
                            debug!(
                                "Reorg: payment {} tx {} re-broadcast rejected by mempool",
                                payment_id,
                                hex::encode(tx_hash)
                            );
                        }
                        Err(e) => {
                            warn!(
                                "Reorg: payment {} tx {} re-broadcast failed: {}",
                                payment_id,
                                hex::encode(tx_hash),
                                e
                            );
                        }
                    }
                }
            }

            // 4. No tx or re-broadcast failed → ReorgPending
            info!(
                "Reorg: payment {} tx {} not in mempool, marking ReorgPending",
                payment_id,
                hex::encode(tx_hash)
            );
            let _ = state_machine
                .mark_reorg_pending(
                    &payment_id,
                    tx_hash,
                    "Block disconnected, transaction not in mempool".to_string(),
                    expected_outputs,
                )
                .await;
        }

        if affected_count > 0 {
            debug!(
                "PaymentReorgHandler: processed {} affected payment(s) for block {:?}",
                affected_count, block_hash
            );
        }
    }

    /// Restart settlement monitoring when downgrading to InMempool (re-track to confirmation)
    async fn restart_monitoring_if_needed(
        settlement_monitor: Option<&SettlementMonitor>,
        payment_id: &str,
        expected_outputs: Option<&[blvm_protocol::payment::PaymentOutput]>,
        tx_hash: crate::Hash,
    ) {
        if let (Some(monitor), Some(outputs)) = (settlement_monitor, expected_outputs) {
            if !outputs.is_empty() {
                if let Err(e) = monitor
                    .start_monitoring(payment_id, outputs.to_vec(), Some(tx_hash))
                    .await
                {
                    warn!(
                        "Reorg: failed to restart monitoring for payment {}: {}",
                        payment_id, e
                    );
                } else {
                    debug!(
                        "Reorg: restarted settlement monitoring for payment {}",
                        payment_id
                    );
                }
            }
        }
    }

    /// Periodic re-check of ReorgPending payments (payor may have re-broadcast to mempool)
    async fn recheck_reorg_pending(
        state_machine: &PaymentStateMachine,
        mempool_manager: Option<&MempoolManager>,
        settlement_monitor: Option<&SettlementMonitor>,
    ) {
        let Some(mempool) = mempool_manager else {
            return;
        };

        let states = state_machine.list_payment_states();
        let reorg_pending: Vec<_> = states
            .into_iter()
            .filter_map(|(id, state)| {
                if let PaymentState::ReorgPending {
                    request_id,
                    tx_hash,
                    expected_outputs,
                    ..
                } = state
                {
                    Some((request_id, tx_hash, expected_outputs))
                } else {
                    None
                }
            })
            .collect();

        for (payment_id, tx_hash, expected_outputs) in reorg_pending {
            if mempool.get_transaction(&tx_hash).is_some() {
                info!(
                    "ReorgPending re-check: payment {} tx {} now in mempool, downgrading to InMempool",
                    payment_id,
                    hex::encode(tx_hash)
                );
                let _ = state_machine.mark_in_mempool(&payment_id, tx_hash).await;
                Self::restart_monitoring_if_needed(
                    settlement_monitor,
                    &payment_id,
                    expected_outputs.as_deref(),
                    tx_hash,
                )
                .await;
            }
        }
    }
}

/// Stub when CTV is disabled
#[cfg(not(feature = "ctv"))]
pub struct PaymentReorgHandler;

#[cfg(not(feature = "ctv"))]
impl PaymentReorgHandler {
    pub fn new(
        _state_machine: Arc<crate::payment::state_machine::PaymentStateMachine>,
        _storage: Option<Arc<crate::storage::Storage>>,
        _mempool_manager: Option<Arc<crate::node::mempool::MempoolManager>>,
        _tx_cache: Option<Arc<()>>,
        _settlement_monitor: Option<Arc<crate::payment::SettlementMonitor>>,
        _event_manager: Arc<crate::module::api::events::EventManager>,
    ) -> Self {
        Self
    }
}
