//! Main node run loop: block processing, network message poll, health and disk checks.

use anyhow::Result;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::utils::{HANDSHAKE_POLL_SLEEP, log_error, with_custom_timeout};

/// Main node run loop. Called from `Node::start()` after components are started.
pub(crate) async fn run(node: &mut super::Node) -> Result<()> {
    // Subscribe before any slow startup work so SIGTERM is not missed.
    let mut shutdown_rx = crate::utils::create_shutdown_receiver();
    if *shutdown_rx.borrow() {
        info!("Shutdown already requested before run loop started");
        return Ok(());
    }

    info!("Node running - main loop started");

    // `current_height` is the height of the next block to connect (chain tip + 1).
    let chain_tip = node.storage.chain().get_height()?.unwrap_or(0);
    let mut current_height = chain_tip.saturating_add(1);
    // Full UTXO load can take minutes at mainnet scale and blocks shutdown; P2P relay
    // validation reloads from disk as needed. Catch-up sync uses parallel IBD + disk store.
    let mut utxo_set = if chain_tip == 0 {
        node.storage.utxos().get_all_utxos().unwrap_or_default()
    } else {
        Default::default()
    };
    node.network.replace_utxo_snapshot(utxo_set.clone()).await;

    // Catch up immediately if peers are ahead (do not wait for the periodic check).
    if let Err(e) = node.try_catch_up_ibd(&mut utxo_set).await {
        warn!("Catch-up sync check failed: {}", e);
    }
    if let Ok(Some(h)) = node.storage.chain().get_height() {
        current_height = h.saturating_add(1);
    }

    // Main node loop - coordinates between all components and handles shutdown signals
    let mut loop_counter: u64 = 0;
    loop {
        // Check for shutdown signal (non-blocking)
        if *shutdown_rx.borrow() {
            info!("Shutdown signal received, stopping node gracefully...");
            break;
        }
        // Process any received blocks (non-blocking)
        while let Some(block_data) = node.network.try_recv_block() {
            if let Ok(Some(h)) = node.storage.chain().get_height() {
                current_height = h.saturating_add(1);
            }
            info!("Processing block from network");
            let blocks_arc = node.storage.blocks();

            // Parse block to get hash for event publishing
            use crate::node::block_processor::parse_block_from_wire;
            let block_hash_for_validation =
                if let Ok((block, _)) = parse_block_from_wire(&block_data) {
                    use crate::storage::blockstore::BlockStore;
                    blocks_arc.get_block_hash(&block)
                } else {
                    [0u8; 32]
                };

            // Publish block validation started event
            if let Some(event_publisher) = node
                .module_subsystem
                .as_ref()
                .and_then(|s| s.event_publisher.as_ref())
            {
                event_publisher
                    .publish_block_validation_started(&block_hash_for_validation, current_height)
                    .await;
            }

            let validation_start_time = std::time::Instant::now();
            let prev_tip_header_bits = node
                .storage
                .chain()
                .get_tip_header()
                .ok()
                .flatten()
                .map(|h| h.bits)
                .unwrap_or(0);
            let prev_tip = node.storage.chain().get_tip_hash_and_height().ok();
            match node.sync_coordinator.process_block(
                &blocks_arc,
                &node.protocol,
                Some(&node.storage),
                &block_data,
                current_height,
                &mut utxo_set,
                Some(Arc::clone(&node.metrics)),
                Some(Arc::clone(&node.profiler)),
            ) {
                Ok(true) => {
                    info!("Block accepted at height {}", current_height);

                    let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;
                    let tip_changed = node
                        .storage
                        .chain()
                        .get_tip_hash_and_height()
                        .ok()
                        .zip(prev_tip.as_ref())
                        .is_some_and(|((new_tip, new_h), (old_tip, old_h))| {
                            new_tip != *old_tip || new_h != *old_h
                        });

                    // Publish block validation completed event (success)
                    if let Some(event_publisher) = node
                        .module_subsystem
                        .as_ref()
                        .and_then(|s| s.event_publisher.as_ref())
                    {
                        event_publisher
                            .publish_block_validation_completed(
                                &block_hash_for_validation,
                                current_height,
                                true,
                                validation_time_ms,
                                None,
                            )
                            .await;
                    }

                    if tip_changed {
                        let blocks_arc = node.storage.blocks();
                        let (block_hash, tip_height) = node
                            .storage
                            .chain()
                            .get_tip_hash_and_height()
                            .unwrap_or(([0u8; 32], current_height));

                        if let Ok(Some(block)) = blocks_arc.get_block(&block_hash) {
                            if block.header.bits != prev_tip_header_bits {
                                if let Some(ep) = node
                                    .module_subsystem
                                    .as_ref()
                                    .and_then(|s| s.event_publisher.as_ref())
                                {
                                    ep.publish_mining_difficulty_changed(
                                        prev_tip_header_bits as u32,
                                        block.header.bits as u32,
                                        tip_height,
                                    )
                                    .await;
                                }
                            }

                            let transaction_count =
                                node.storage.transaction_count().unwrap_or(0) as u64;
                            log_error(
                                || {
                                    node.storage.chain().update_utxo_stats_cache(
                                        &block_hash,
                                        tip_height,
                                        &utxo_set,
                                        transaction_count,
                                    )
                                },
                                "Failed to update UTXO stats cache",
                            );

                            log_error(
                                || {
                                    node.storage.chain().calculate_and_cache_network_hashrate(
                                        tip_height,
                                        &blocks_arc,
                                    )
                                },
                                "Failed to update network hashrate cache",
                            );

                            if let Some(event_publisher) = node
                                .module_subsystem
                                .as_ref()
                                .and_then(|s| s.event_publisher.as_ref())
                            {
                                event_publisher
                                    .publish_new_block(&block, &block_hash, tip_height)
                                    .await;
                            }

                            if let Err(e) = node.storage.utxos().store_utxo_set(&utxo_set) {
                                warn!(
                                    "Failed to persist UTXO set after block {}: {}",
                                    tip_height, e
                                );
                            }
                            node.network.replace_utxo_snapshot(utxo_set.clone()).await;

                            #[cfg(feature = "utxo-commitments")]
                            {
                                if let Some(pruning_manager) = node.storage.pruning() {
                                    if let (Some(commitment_store), Some(_utxostore)) = (
                                        pruning_manager.commitment_store(),
                                        pruning_manager.utxostore(),
                                    ) {
                                        if let Err(e) = pruning_manager
                                            .generate_commitment_from_current_state(
                                                &block_hash,
                                                tip_height,
                                                &utxo_set,
                                                &commitment_store,
                                            )
                                        {
                                            warn!(
                                                "Failed to generate commitment for block {}: {}",
                                                tip_height, e
                                            );
                                        } else {
                                            debug!(
                                                "Generated UTXO commitment for block {}",
                                                tip_height
                                            );
                                        }
                                    }
                                }
                            }

                            current_height = tip_height + 1;
                        } else {
                            warn!("Failed to load block at active tip height {tip_height}");
                        }
                    }

                    // Check for incremental pruning during IBD
                    // Consider IBD if we're still syncing (height < tip or no recent blocks)
                    let is_ibd = current_height < 1000; // Simple heuristic: consider IBD if < 1000 blocks
                    if let Some(pruning_manager) = node.storage.pruning() {
                        if let Ok(Some(prune_stats)) =
                            pruning_manager.incremental_prune_during_ibd(current_height, is_ibd)
                        {
                            info!(
                                "Incremental pruning during IBD: {} blocks pruned, {} bytes freed",
                                prune_stats.blocks_pruned, prune_stats.storage_freed
                            );
                            // Flush storage to persist pruning changes
                            if let Err(e) = node.storage.flush() {
                                warn!("Failed to flush storage after incremental pruning: {}", e);
                            }
                        }
                    }

                    // Check for automatic pruning after block acceptance
                    if let Some(pruning_manager) = node.storage.pruning() {
                        let stats = pruning_manager.get_stats();
                        let should_prune = pruning_manager
                            .should_auto_prune(current_height, stats.last_prune_height);

                        if should_prune {
                            info!("Automatic pruning triggered at height {}", current_height);

                            // Calculate prune height based on configuration
                            let prune_height = match &pruning_manager.config.mode {
                                crate::config::PruningMode::Disabled => None,
                                crate::config::PruningMode::Normal {
                                    keep_from_height, ..
                                } => {
                                    // Prune to keep_from_height, but ensure we keep min_blocks
                                    let min_keep = pruning_manager.config.min_blocks_to_keep;
                                    let effective_keep = (*keep_from_height)
                                        .max(current_height.saturating_sub(min_keep));
                                    Some(effective_keep)
                                }
                                #[cfg(feature = "utxo-commitments")]
                                crate::config::PruningMode::Aggressive {
                                    keep_from_height,
                                    min_blocks,
                                    ..
                                } => {
                                    // Prune to keep_from_height, respecting min_blocks
                                    let sub = current_height.saturating_sub(*min_blocks);
                                    let effective_keep = (*keep_from_height).max(sub);
                                    Some(effective_keep)
                                }
                                #[cfg(not(feature = "utxo-commitments"))]
                                crate::config::PruningMode::Aggressive { .. } => {
                                    // Aggressive pruning requires utxo-commitments feature
                                    // Fall back to no pruning if feature is disabled
                                    None
                                }
                                crate::config::PruningMode::Custom {
                                    keep_bodies_from_height,
                                    ..
                                } => {
                                    // Prune to keep_bodies_from_height, respecting min_blocks
                                    let min_keep = pruning_manager.config.min_blocks_to_keep;
                                    let effective_keep = (*keep_bodies_from_height)
                                        .max(current_height.saturating_sub(min_keep));
                                    Some(effective_keep)
                                }
                            };

                            if let Some(prune_to_height) = prune_height {
                                if prune_to_height < current_height {
                                    match pruning_manager.prune_to_height(
                                        prune_to_height,
                                        current_height,
                                        false,
                                    ) {
                                        Ok(prune_stats) => {
                                            info!(
                                                "Automatic pruning completed: {} blocks pruned, {} blocks kept",
                                                prune_stats.blocks_pruned, prune_stats.blocks_kept
                                            );
                                            // Flush storage to persist pruning changes
                                            use crate::utils::log_error;
                                            log_error(
                                                || node.storage.flush(),
                                                "Failed to flush storage after automatic pruning",
                                            );
                                        }
                                        Err(e) => {
                                            warn!("Automatic pruning failed: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(false) => {
                    warn!("Block rejected at height {}", current_height);
                    if let Err(e) = node.try_catch_up_ibd(&mut utxo_set).await {
                        warn!("Catch-up after block rejection failed: {}", e);
                    }
                    if let Ok(Some(h)) = node.storage.chain().get_height() {
                        current_height = h.saturating_add(1);
                    }
                    let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                    // Publish block validation completed event (failure)
                    if let Some(event_publisher) = node
                        .module_subsystem
                        .as_ref()
                        .and_then(|s| s.event_publisher.as_ref())
                    {
                        event_publisher
                            .publish_block_validation_completed(
                                &block_hash_for_validation,
                                current_height,
                                false,
                                validation_time_ms,
                                Some("Block validation failed"),
                            )
                            .await;
                    }
                }
                Err(e) => {
                    warn!("Error processing block: {}", e);
                    let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                    // Publish block validation completed event (error)
                    if let Some(event_publisher) = node
                        .module_subsystem
                        .as_ref()
                        .and_then(|s| s.event_publisher.as_ref())
                    {
                        event_publisher
                            .publish_block_validation_completed(
                                &block_hash_for_validation,
                                current_height,
                                false,
                                validation_time_ms,
                                Some(&format!("Block processing error: {e}")),
                            )
                            .await;
                    }
                }
            }
        }

        // Peer traffic is drained by the background task spawned in start_components
        // (`NetworkManager::process_messages`). Do not call it here: it never returns until
        // the channel closes, which would starve this loop and leave `pending_blocks` undrained.

        tokio::select! {
            biased;
            Ok(()) = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("Shutdown signal received, stopping node gracefully...");
                    break;
                }
            }
            _ = tokio::time::sleep(HANDSHAKE_POLL_SLEEP) => {}
        }

        loop_counter = loop_counter.saturating_add(1);

        // Peers advance the tip while we idle; re-check every ~5s and resume parallel IBD when behind.
        if loop_counter % 50 == 0 {
            if let Err(e) = node.try_catch_up_ibd(&mut utxo_set).await {
                warn!("Catch-up sync check failed: {}", e);
            }
            if let Ok(Some(h)) = node.storage.chain().get_height() {
                current_height = h.saturating_add(1);
            }
        }

        // Check node health periodically
        node.check_health().await?;

        // Check disk space periodically (every 10 iterations = ~1 second)
        // Use timeout to prevent hanging on slow disk operations
        let counter = node
            .disk_check_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if counter % 10 == 0 {
            let timeout_dur = node.storage_timeout();
            use crate::utils::with_custom_timeout;
            match with_custom_timeout(async { node.check_disk_space().await }, timeout_dur).await {
                Ok(Ok(())) => {
                    // Disk check succeeded
                }
                Ok(Err(e)) => {
                    warn!("Disk space check failed: {}", e);
                    // Continue - disk errors don't stop the node
                }
                Err(_) => {
                    warn!("Disk space check timed out");
                    // Continue - timeout doesn't stop the node
                }
            }
        }
    }

    // Graceful shutdown - stop all components
    info!("Initiating graceful shutdown...");
    node.stop().await?;
    Ok(())
}
