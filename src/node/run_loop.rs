//! Main node run loop: block processing, network message poll, health and disk checks (§3).

use anyhow::Result;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::utils::{log_error, with_custom_timeout, HANDSHAKE_POLL_SLEEP};

/// Main node run loop. Called from `Node::start()` after components are started.
pub(crate) async fn run(node: &mut super::Node) -> Result<()> {
        info!("Node running - main loop started");

        // Set up graceful shutdown signal handling
        let shutdown_rx = crate::utils::create_shutdown_receiver();

        // Get initial state for block processing
        let mut current_height = node.storage.chain().get_height()?.unwrap_or(0);
        let mut utxo_set = blvm_protocol::UtxoSet::default();

        // Main node loop - coordinates between all components and handles shutdown signals
        loop {
            // Check for shutdown signal (non-blocking)
            if *shutdown_rx.borrow() {
                info!("Shutdown signal received, stopping node gracefully...");
                break;
            }
            // Process any received blocks (non-blocking)
            while let Some(block_data) = node.network.try_recv_block() {
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
                if let Some(ref event_publisher) = node
                    .module_subsystem
                    .as_ref()
                    .and_then(|s| s.event_publisher.as_ref())
                {
                    event_publisher
                        .publish_block_validation_started(
                            &block_hash_for_validation,
                            current_height,
                        )
                        .await;
                }

                let validation_start_time = std::time::Instant::now();
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

                        // Publish block validation completed event (success)
                        if let Some(ref event_publisher) = node
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

                        // Parse block for governance webhook (need block object, not just block_data)
                        // We'll get it from storage after it's stored
                        let blocks_arc = node.storage.blocks();
                        let block_hash =
                            if let Ok(Some(hash)) = blocks_arc.get_hash_by_height(current_height) {
                                hash
                            } else {
                                warn!("Failed to get block hash for height {}", current_height);
                                [0u8; 32]
                            };

                        // Update chain tip (for chainwork, etc.)
                        if let Ok(Some(block)) = blocks_arc.get_block(&block_hash) {
                            // Capture old tip bits before update (for MiningDifficultyChanged)
                            let old_bits = node
                                .storage
                                .chain()
                                .get_tip_header()
                                .ok()
                                .flatten()
                                .map(|h| h.bits)
                                .unwrap_or(0);

                            log_error(
                                || {
                                    node.storage.chain().update_tip(
                                        &block_hash,
                                        &block.header,
                                        current_height,
                                    )
                                },
                                "Failed to update chain tip",
                            );

                            // Publish MiningDifficultyChanged when bits (difficulty target) changes
                            if block.header.bits != old_bits {
                                if let Some(ref ep) = node
                                    .module_subsystem
                                    .as_ref()
                                    .and_then(|s| s.event_publisher.as_ref())
                                {
                                    ep.publish_mining_difficulty_changed(
                                        old_bits as u32,
                                        block.header.bits as u32,
                                        current_height,
                                    )
                                    .await;
                                }
                            }

                            // Update UTXO stats cache (for fast gettxoutsetinfo RPC)
                            let transaction_count =
                                node.storage.transaction_count().unwrap_or(0) as u64;
                            log_error(
                                || {
                                    node.storage.chain().update_utxo_stats_cache(
                                        &block_hash,
                                        current_height,
                                        &utxo_set,
                                        transaction_count,
                                    )
                                },
                                "Failed to update UTXO stats cache",
                            );

                            // Update network hashrate cache (for fast getmininginfo RPC)
                            log_error(
                                || {
                                    node.storage
                                        .chain()
                                        .calculate_and_cache_network_hashrate(current_height, &blocks_arc)
                                },
                                "Failed to update network hashrate cache",
                            );

                            // Publish NewBlock event to modules
                            if let Some(ref event_publisher) = node
                    .module_subsystem
                    .as_ref()
                    .and_then(|s| s.event_publisher.as_ref())
                {
                                event_publisher
                                    .publish_new_block(&block, &block_hash, current_height)
                                    .await;
                            }

                            // Governance module subscribes to NewBlock events and handles notifications
                            // No direct webhook call needed - handled via event system
                        }

                        // Persist UTXO set to storage after block validation
                        // This is critical for commitment generation and incremental pruning
                        if let Err(e) = node.storage.utxos().store_utxo_set(&utxo_set) {
                            warn!(
                                "Failed to persist UTXO set after block {}: {}",
                                current_height, e
                            );
                        }

                        // Generate UTXO commitment from current state (if enabled)
                        // Use current_height (the block that was just validated) before incrementing
                        #[cfg(feature = "utxo-commitments")]
                        {
                            if let Some(pruning_manager) = node.storage.pruning() {
                                if let (Some(commitment_store), Some(_utxostore)) = (
                                    pruning_manager.commitment_store(),
                                    pruning_manager.utxostore(),
                                ) {
                                    // Get block hash from storage (block was just stored at current_height)
                                    let blocks_arc = node.storage.blocks();
                                    if let Ok(Some(block_hash)) =
                                        blocks_arc.get_hash_by_height(current_height)
                                    {
                                        // Generate commitment from current UTXO set state
                                        if let Err(e) = pruning_manager
                                            .generate_commitment_from_current_state(
                                                &block_hash,
                                                current_height,
                                                &utxo_set,
                                                &commitment_store,
                                            )
                                        {
                                            warn!(
                                                "Failed to generate commitment for block {}: {}",
                                                current_height, e
                                            );
                                        } else {
                                            debug!(
                                                "Generated UTXO commitment for block {}",
                                                current_height
                                            );
                                        }
                                    } else {
                                        warn!("Could not find block hash for height {} to generate commitment", current_height);
                                    }
                                }
                            }
                        }

                        // Increment height after processing
                        current_height += 1;

                        // Check for incremental pruning during IBD
                        // Consider IBD if we're still syncing (height < tip or no recent blocks)
                        let is_ibd = current_height < 1000; // Simple heuristic: consider IBD if < 1000 blocks
                        if let Some(pruning_manager) = node.storage.pruning() {
                            if let Ok(Some(prune_stats)) =
                                pruning_manager.incremental_prune_during_ibd(current_height, is_ibd)
                            {
                                info!("Incremental pruning during IBD: {} blocks pruned, {} bytes freed", 
                                      prune_stats.blocks_pruned, prune_stats.storage_freed);
                                // Flush storage to persist pruning changes
                                if let Err(e) = node.storage.flush() {
                                    warn!(
                                        "Failed to flush storage after incremental pruning: {}",
                                        e
                                    );
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
                                                info!("Automatic pruning completed: {} blocks pruned, {} blocks kept", 
                                                      prune_stats.blocks_pruned, prune_stats.blocks_kept);
                                                // Flush storage to persist pruning changes
                                                use crate::utils::log_error;
                                                log_error(|| node.storage.flush(), "Failed to flush storage after automatic pruning");
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
                        let validation_time_ms = validation_start_time.elapsed().as_millis() as u64;

                        // Publish block validation completed event (failure)
                        if let Some(ref event_publisher) = node
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
                        if let Some(ref event_publisher) = node
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

            tokio::time::sleep(HANDSHAKE_POLL_SLEEP).await;

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
