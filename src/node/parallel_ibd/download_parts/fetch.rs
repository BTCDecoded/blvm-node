/// Download a chunk of blocks from a peer.
///
/// When block_tx is Some, streams each block immediately so validation doesn't wait for full chunk.
/// blocks_sem: Core-style limit — max 16 blocks in flight per peer across all workers.
/// stall_rx: When coordinator stalls, it broadcasts the needed height; worker aborts if our chunk contains it.
///
/// `confirmed_body_height` — uncapped body-confirmed height at IBD start; used to avoid stall-abort on resume gaps.
///
/// `outer_deadline` — cooperative wall-clock budget (S1). When set, the select loop races
/// `sleep_until(deadline)` and flushes `received` via [`flush_received_on_abort`] before
/// returning Err. Prefer this over wrapping the future in `tokio::timeout`, which drops
/// the task and loses buffered blocks (live: outer deadline at 546964/549065 with no flush).
pub(crate) async fn download_chunk(
    start_height: u64,
    end_height: u64,
    peer_id: &str,
    network: Option<Arc<NetworkManager>>,
    blockstore: &BlockStore,
    config: &ParallelIBDConfig,
    peer_scorer: Arc<crate::network::peer_scoring::PeerScorer>,
    block_tx: Option<tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    blocks_sem: Option<Arc<Semaphore>>,
    mut stall_rx: Option<&mut broadcast::Receiver<u64>>,
    protocol_version: ProtocolVersion,
    validation_height: Option<Arc<AtomicU64>>,
    confirmed_body_height: u64,
    wan_multi_peer: bool,
    outer_deadline: Option<tokio::time::Instant>,
    // W28d: when set, abort ahead partitions that tip has walked into.
    tip_enter: Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<DownloadChunkResult> {
    let streaming = block_tx.is_some();
    // Process-latched registry — avoid per-chunk Vec+String rebuild of feature table.
    let feature_registry = cached_feature_registry(protocol_version);
    let mut blocks = Vec::new();
    let mut streamed_block_count: usize = 0;
    let mut progress = BlockDownloadProgress::new();
    // Used to detect genuinely stuck partial chunks: abort if stall signal arrives
    // and we have been active for >PARTIAL_STALL_ABORT_SECS without delivering the needed block.
    let chunk_start_time = std::time::Instant::now();
    let mut outer_deadline = outer_deadline;
    let mut outer_extends: u32 = 0;
    let mut gap_streams: u64 = 0;

    // Drain stale stall broadcasts accumulated while this worker was finishing its previous chunk.
    // Workers hold stall_rx across the entire worker-task lifetime (one subscription, many chunks).
    // A broadcast sent during the previous chunk's work sits unread in the channel. Without
    // draining, the very first select! poll in the "no first block yet" branch fires the stale
    // signal immediately — "no first block yet" → abort → re-queue → same broadcast fires again.
    // Draining here gives this chunk a clean slate; only broadcasts sent AFTER we start are relevant.
    if let Some(ref mut rx) = stall_rx {
        loop {
            match rx.try_recv() {
                Ok(_) => continue,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break, // Empty or Closed
            }
        }
    }

    info!(
        "Downloading chunk from peer {}: heights {} to {}",
        peer_id, start_height, end_height
    );

    let local_disk = is_snapshot_sourced_peer(peer_id);
    let network = match network {
        Some(n) => n,
        None => {
            warn!("NetworkManager not available, skipping block download");
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: 0,
            });
        }
    };

    // Dummy addr for local-disk (never used for send); real peers must parse.
    let peer_addr = if local_disk {
        SocketAddr::from(([127, 0, 0, 1], 9))
    } else {
        peer_id
            .parse::<SocketAddr>()
            .map_err(|_| anyhow::anyhow!("Invalid peer address: {}", peer_id))?
    };

    if !local_disk {
        let connect_wait = Duration::from_secs(config.download_timeout_secs.max(15));
        wait_for_peer_connected(&network, peer_addr, peer_id, connect_wait, &tip_enter).await?;
        let handshake_wait = Duration::from_secs(15);
        wait_for_peer_ibd_ready(&network, peer_addr, peer_id, handshake_wait, &tip_enter).await?;
    }

    let validated_tip = validation_height
        .as_ref()
        .map(|h| h.load(Ordering::Relaxed))
        .unwrap_or(start_height.saturating_sub(1));
    let resume_from = match resume_download_height(start_height, end_height, validated_tip) {
        Some(h) => h,
        None => {
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: 0,
            });
        }
    };
    if resume_from > start_height {
        info!(
            "[IBD] {} chunk {}-{}: resuming download at height {} (validated tip {})",
            peer_id, start_height, end_height, resume_from, validated_tip
        );
    }

    let mut block_hashes = Vec::new();
    let mut effective_end = end_height;
    for height in start_height..=end_height {
        if let Ok(Some(hash)) = blockstore.get_hash_by_height(height) {
            block_hashes.push((height, hash));
        } else {
            // W35‴-h: clip to contiguous headers instead of failing the whole pipe
            // (live: tip..tip+255 assigned past header tip → 912× instant fails → mass blacklist).
            if height == start_height {
                warn!(
                    "Block hash not found for height {} - header may not be stored yet",
                    height
                );
                return Err(anyhow::anyhow!(
                    "Block hash not found for height {} - headers must be downloaded first",
                    height
                ));
            }
            effective_end = height.saturating_sub(1);
            warn!(
                "[IBD_HEADER_CLIP] chunk {}-{} clipped to {}-{} — missing header at {}",
                start_height, end_height, start_height, effective_end, height
            );
            break;
        }
    }
    let end_height = effective_end;

    if block_hashes.is_empty() {
        return Err(anyhow::anyhow!(
            "No block hashes found for heights {} to {}",
            start_height,
            end_height
        ));
    }

    let base_timeout_secs = config.download_timeout_secs;
    // For single-height (H,H) gap micro-chunks, use a dedicated per-block timeout.
    // W4/W6: default 45s. Do NOT clamp to download_timeout_secs (default 30) — that made the
    // 45s default a no-op (live: bulk+micro still aborted at 30s). Env override still applies.
    // W10: tip-covering / far-ahead chunks override below (12s / 10s) so soft-retry storms
    // cannot pin peers for 3×45s while the validation tip starves.
    let gap_micro_timeout_secs: u64 = std::env::var("BLVM_IBD_GAP_MICRO_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(45)
        .clamp(15, 180);
    let tip_aware_secs = gap_timeout_for_chunk(
        start_height,
        end_height,
        validated_tip,
        gap_micro_timeout_secs,
    );
    // W2: body on disk but witness missing/empty — only need MSG_WITNESS_BLOCK. Give the peer
    // longer than the default micro-timeout (live: 71% of gap timeouts ∩ witness-miss heights;
    // 25s aborts discarded late witness payloads). Default 75s; env BLVM_IBD_WITNESS_HOLE_TIMEOUT_SECS.
    let witness_hole = start_height == end_height
        && block_hashes
            .iter()
            .find(|(h, _)| *h == start_height)
            .map(|(_, hash)| {
                is_local_witness_hole(blockstore, start_height, *hash, protocol_version)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
    let (mut timeout_duration, first_block_wait) = if witness_hole {
        let hole_secs = std::env::var("BLVM_IBD_WITNESS_HOLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(75)
            .clamp(30, 180);
        if hole_secs > tip_aware_secs {
            info!(
                "[IBD_WITNESS_HOLE] height {}: extending micro-timeout {}s → {}s (body on disk, witness missing)",
                start_height, tip_aware_secs, hole_secs
            );
        }
        let d = Duration::from_secs(hole_secs);
        (d, d)
    } else if start_height == end_height {
        let d = Duration::from_secs(tip_aware_secs);
        (d, d)
    } else {
        // W6: multi-block gap/bulk downloads used download_timeout_secs=30 by default.
        // Live WAN: 16-block stall-recovery chunks aborted at 30s on the gap height, flushed
        // 1–8 blocks, then re-queued — timeout storms at ~0.2 BPS. Use ≥ gap micro timeout.
        // W10: tip/far-ahead overrides still apply via tip_aware_secs.
        let secs = if tip_aware_secs < gap_micro_timeout_secs {
            tip_aware_secs
        } else {
            base_timeout_secs.max(gap_micro_timeout_secs)
        };
        let d = Duration::from_secs(secs);
        (d, d)
    };
    if tip_aware_secs != gap_micro_timeout_secs {
        // W175: rate-limit — live W174 logged TIP_GAP 267×/s during mute-reopen spin.
        static LAST_TIP_GAP_LOG_MS: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_TIP_GAP_LOG_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now_ms.saturating_sub(prev) >= 2_000
            && LAST_TIP_GAP_LOG_MS
                .compare_exchange(
                    prev,
                    now_ms,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            info!(
                "[IBD_TIP_GAP] chunk {}-{} tip={} — per-block timeout {}s (default micro {}s)",
                start_height, end_height, validated_tip, tip_aware_secs, gap_micro_timeout_secs
            );
        }
    }

    let pipeline_depth: usize = blocks_sem
        .as_ref()
        .map(|_| config.max_blocks_in_transit_per_peer)
        .unwrap_or(config.max_concurrent_per_peer);

    // W32f: observability for deep WAN tip-owner pipe.
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        warn!(
            "[IBD_TIP_PIPE] chunk {}-{} peer={} pipe_depth={} span={}",
            start_height,
            end_height,
            peer_id,
            pipeline_depth,
            end_height.saturating_sub(start_height).saturating_add(1)
        );
    }

    let chunk_default_secs = timeout_duration.as_secs();

    let mut in_flight: FuturesUnordered<PendingBlockFuture> = FuturesUnordered::new();
    let block_hash_by_height: BTreeMap<u64, [u8; 32]> = block_hashes.into_iter().collect();
    let mut in_flight_heights: HashSet<u64> = HashSet::new();
    // W71/W72: first-seen Instant + shorten-able deadline per in-flight network GetData.
    let mut inflight_started: HashMap<u64, Instant> = HashMap::new();
    let mut inflight_deadlines: HashMap<u64, Arc<AtomicU64>> = HashMap::new();
    // W85: last tip height whose CAP clock was rebased to tip-roll (not pre-roll GetData).
    let mut tip_cap_clock_h: Option<u64> = None;
    // Arc-wrap immediately so downstream pipeline stages never deep-copy block bytes.
    let mut received: BTreeMap<u64, (SharedBlock, SharedWitnesses)> = BTreeMap::new();
    let mut next_to_send = resume_from;
    // P4: soft gap timeouts re-request without aborting; hard deadline still aborts.
    // W10/W12: per-timeout budget from gap_soft_retry_budget (tip/far-ahead aware).
    let mut gap_soft_retries: u32 = 0;
    // W29c: after tip GAP_STREAM succeeds, lengthen per-block timeout for remaining pipe.
    let mut progressive_timeout_applied = false;
    // P1a/P1b: heights loaded via try_load_local (wire_payload=None). Do not credit WAN
    // tip-stream score or arm progressive 45s shelter for these (G-W forensics).
    let mut local_sourced_heights: HashSet<u64> = HashSet::new();
    // Heights that arrived via network GetData (for drain-path tip-stream credit).
    let mut network_body_heights: HashSet<u64> = HashSet::new();
    let mut network_tip_streamed = false;
    // P1d: clock for PIPE_FILL mute — armed when network GetData is outstanding with no
    // network body yet; reset on any network-sourced body.
    let mut pipe_mute_armed_at: Option<Instant> = None;
    let mut saw_network_body = false;
    // Set after initial fill when GetData pipe is full and `received` still empty.
    let mut pipe_fill_recv0 = false;
    // C1c: tip-hole depth — sticky across chunks for same peer; grow only on tip-gap bodies.
    // C1d: hot tip streamers may warm up to pipe cap (128); cold stays at grow_cap (32).
    // C1n: recompute cap each fill/grow — deepen to FAST_CAP only while getdata EWMA is fast.
    let tip_hole_hot = tip_enter
        .as_ref()
        .map(|a| a.peer_is_hot_tip_streamer(peer_id))
        .unwrap_or(false);
    let mut tip_hole_cap = tip_hole_grow_cap_for_peer(tip_hole_hot);
    let mut tip_hole_grown: usize = tip_enter
        .as_ref()
        .map(|a| a.tip_hole_depth_for(peer_id))
        .unwrap_or_else(tip_hole_grow_start)
        .min(tip_hole_cap);
    // C1u: enter with shallow fill if GetData EWMA already slow (sticky may still be 32).
    // Sole ready peer: pin to sole_gd_slow_floor (16) — raise starve-8, lower flood-32.
    // Do not hard-cap FAST_CAP 64 (tc109 tip90≈67.8 used FAST_CAP=64; sole abs-cap regressed).
    let sole_ready = tip_enter
        .as_ref()
        .map(|a| a.ibd_ready_peer_count() <= 1)
        .unwrap_or(false);
    // Sole no-FAST: after GD_SLOW, keep tip-hole at cold grow_cap until gd is fast.
    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_cap);
    if tip_hole_grown > tip_hole_cap {
        let prev = tip_hole_grown;
        tip_hole_grown = tip_hole_cap;
        info!(
            "[IBD_TIP_HOLE_CAP_CLAMP] peer={} height={} depth {}→{} (cap={})",
            peer_id, next_to_send, prev, tip_hole_grown, tip_hole_cap
        );
        if let Some(ref a) = tip_enter {
            a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
        }
    }
    if tip_hole_gd_slow() && sole_ready && tip_hole_sole_floor_applies(next_to_send) {
        let floor = tip_hole_sole_gd_slow_floor();
        super::tip_stage::note_sole_floor_latch();
        maybe_note_sole_no_fast_latch(next_to_send);
        if tip_hole_grown != floor {
            let prev = tip_hole_grown;
            tip_hole_grown = floor;
            info!(
                "[IBD_TIP_HOLE_GD_SLOW_SOLE_FLOOR] peer={} depth {}→{} — sole ready; avoid 32-flood/8-starve",
                peer_id, prev, tip_hole_grown
            );
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    } else if sole_ready
        && tip_hole_sole_floor_applies(next_to_send)
        && tip_hole_sole_floor_blocks_grow()
    {
        let floor = tip_hole_sole_gd_slow_floor();
        if tip_hole_grown > floor {
            tip_hole_grown = floor;
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    } else if tip_hole_gd_slow() && !sole_ready {
        let slow = tip_hole_slow_fill_cap();
        if tip_hole_grown > slow {
            tip_hole_grown = slow;
            if let Some(ref a) = tip_enter {
                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
            }
        }
    }
    super::tip_stage::note_tip_hole_duty(tip_hole_grown);
    // W32d″: after tip streams, remaining pipe uses a long timeout (not 12s) so mid/deep
    // slots do not thrash while validation walks the buffered runway.
    let tip_progressive_secs: u64 = std::env::var("BLVM_IBD_TIP_PROGRESSIVE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(45)
        .clamp(25, 90);
    // A6: empty-witness hits on this peer/chunk before demotion / abort.
    // Live soak: peers that only serve MSG_BLOCK re-request forever (~38 hits/s) and pin
    // validation at the gap. Cap retries then abort so the worker excludes/blacklists the peer.
    let mut empty_witness_hits: u32 = 0;
    let max_empty_witness_hits: u32 = empty_witness_hit_cap();

    let mut first_block_logged = false;
    let initial_outer_secs = outer_deadline
        .map(|d| {
            d.saturating_duration_since(tokio::time::Instant::now())
                .as_secs()
                .max(1)
        })
        .unwrap_or(0);
    let mut outer_deadline_secs = initial_outer_secs;

    async fn fill_pipeline(
        next_to_send: u64,
        end_height: u64,
        pipeline_depth: usize,
        received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
        in_flight: &mut FuturesUnordered<PendingBlockFuture>,
        in_flight_heights: &mut HashSet<u64>,
        inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
        block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
        network: &Arc<NetworkManager>,
        peer_addr: SocketAddr,
        peer_id: &str,
        blockstore: &BlockStore,
        protocol_version: ProtocolVersion,
        validation_tip: u64,
        confirmed_body_height: u64,
        chunk_default_secs: u64,
        blocks_sem: &Option<Arc<Semaphore>>,
        first_block_logged: &mut bool,
        start_height: u64,
        local_sourced_heights: &mut HashSet<u64>,
        tip_hole_grown: usize,
        tip_hole_cap: usize,
    ) -> Result<()> {
        // Batch network block requests into groups of GETDATA_BATCH_SIZE. Local cache hits
        // (blocks already in blockstore) are enqueued immediately without batching.
        let mut net_batch_heights: Vec<(u64, [u8; 32])> = Vec::new();
        let mut net_batch_permits: Vec<Option<tokio::sync::OwnedSemaphorePermit>> = Vec::new();
        let mut scan = next_to_send;
        // W102/W102b: while sequential cursor is still the validation tip and tip is not
        // buffered, cap GetData depth. STREAM + [`sync_next_to_send_after_gap_stream`]
        // disarms the cap.
        // C1/C1d: tip-hole depth is `tip_hole_grown` capped by cold/warm `tip_hole_cap`.
        let tip_needed = validation_tip.saturating_add(1);
        let tip_hole = next_to_send == tip_needed
            && tip_needed <= end_height
            && !received.contains_key(&tip_needed);
        let mut fill_depth = if tip_hole {
            tip_hole_grown.min(pipeline_depth).min(tip_hole_cap)
        } else {
            pipeline_depth
        };
        // C1u: do not hold depth=32 GetDatas while gd_ewma is drip-slow.
        // Keep fill clamp even when ratchet is opt-in (T172520Z REVERT: dropping
        // this over-piped mute stretches and tip60-failed @480k).
        if tip_hole && tip_hole_gd_slow() {
            fill_depth = fill_depth.min(tip_hole_slow_fill_cap());
        }
        // Mode T tip-priority: tip-cover chunks stay glued to tip_needed (never
        // mid-span fill→256 on the tip TCP stream). Ahead chunks on a second
        // loopback peer must NOT be clamped (tc165 sole-only starved tip90).
        let sole_tip_pri = super::sole_tip_priority_enabled();
        let near_tip = next_to_send <= tip_needed.saturating_add(tip_hole_cap as u64);
        let tip_glue = sole_tip_pri && (tip_hole || near_tip);
        if tip_glue {
            let mut sole_fill = tip_hole_grown.min(pipeline_depth).min(tip_hole_cap).max(1);
            if tip_hole_gd_slow() {
                sole_fill = sole_fill.min(tip_hole_slow_fill_cap());
            }
            fill_depth = sole_fill;
            if tip_needed <= end_height {
                scan = tip_needed;
            }
        }
        // M3 C2 keep-pipe: tip-hole underfilled vs grown / pipe_F — refill under same
        // sticky cover (no assign re-roll, no secondaries). Forensic only + fill below.
        if tip_hole && fill_depth > 0 && in_flight.len() < fill_depth {
            let pipe_f = super::tip_stage::pipe_frontier(tip_needed);
            let target = tip_needed.saturating_add((fill_depth as u64).saturating_sub(1));
            if pipe_f < target {
                static C2_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let prev = C2_LAST.load(Ordering::Relaxed);
                if now.saturating_sub(prev) >= 2
                    && C2_LAST
                        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        "[IBD_C2_PREFLIGHT] peer={} tip={} grown={} fill_depth={} in_flight={} pipe_f={} target={} — keep-pipe refill",
                        peer_id,
                        tip_needed,
                        tip_hole_grown,
                        fill_depth,
                        in_flight.len(),
                        pipe_f,
                        target
                    );
                }
            }
        }
        // W2/N11: adapt fill depth to byte budget × recent block size (opt-in).
        let byte_budget = download_byte_budget();
        if let Some(budget) = byte_budget {
            if !tip_glue {
                let est = download_est_block_bytes();
                let by_bytes = (budget / est).max(2) as usize;
                fill_depth = by_bytes.clamp(2, pipeline_depth);
            }
        }
        let sole_scan_end = if tip_glue {
            tip_needed
                .saturating_add((fill_depth as u64).saturating_sub(1))
                .min(end_height)
        } else {
            end_height
        };

        while in_flight.len() + net_batch_heights.len() < fill_depth {
            if let Some(budget) = byte_budget {
                let pending = (in_flight.len() + net_batch_heights.len()) as u64;
                let pending_bytes = pending.saturating_mul(download_est_block_bytes());
                if pending_bytes >= budget {
                    break;
                }
            }
            let Some(height) = (scan..=sole_scan_end)
                .find(|h| !received.contains_key(h) && !in_flight_heights.contains(h))
            else {
                break;
            };
            scan = height + 1;

            let block_hash = *block_hash_by_height
                .get(&height)
                .ok_or_else(|| anyhow::anyhow!("Block hash missing for height {height}"))?;

            // D1: never block on the peer semaphore while in_flight holds permits unpolled.
            let Some(permit) = try_take_blocks_permit(blocks_sem)? else {
                break;
            };

            if super::synthetic_wan::is_synthetic_peer(peer_id) {
                let delay_ms = super::synthetic_wan::getdata_delay_ms();
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }

            if let Some((block, block_witnesses)) =
                try_load_local_ibd_block(blockstore, height, block_hash, protocol_version)?
            {
                // Local block: flush any pending network batch first (preserve order of
                // in_flight_heights insertions — local and network blocks are interleaved).
                if !net_batch_heights.is_empty() {
                    enqueue_network_block_batch(
                        std::mem::take(&mut net_batch_heights),
                        std::mem::take(&mut net_batch_permits),
                        network,
                        peer_addr,
                        peer_id,
                        validation_tip,
                        confirmed_body_height,
                        chunk_default_secs,
                        in_flight,
                        in_flight_heights,
                        inflight_deadlines,
                        first_block_logged,
                        start_height,
                        end_height,
                        blockstore,
                    )
                    .await?;
                }
                if !*first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: local block height {} (hash {})",
                        peer_id,
                        start_height,
                        end_height,
                        height,
                        hex::encode(block_hash)
                    );
                    *first_block_logged = true;
                }
                let request_start = std::time::Instant::now();
                in_flight_heights.insert(height);
                local_sourced_heights.insert(height);
                in_flight.push(Box::pin(async move {
                    let r = Ok(Ok((block, block_witnesses, None)));
                    (height, block_hash, request_start, r, permit)
                }));
            } else if is_snapshot_sourced_peer(peer_id) {
                // Zero-peer local replay: stop at first missing body — never GetData.
                drop(permit);
                break;
            } else {
                net_batch_heights.push((height, block_hash));
                net_batch_permits.push(permit);

                // Flush batch when it reaches GETDATA_BATCH_SIZE.
                if net_batch_heights.len() >= getdata_batch_size() {
                    enqueue_network_block_batch(
                        std::mem::take(&mut net_batch_heights),
                        std::mem::take(&mut net_batch_permits),
                        network,
                        peer_addr,
                        peer_id,
                        validation_tip,
                        confirmed_body_height,
                        chunk_default_secs,
                        in_flight,
                        in_flight_heights,
                        inflight_deadlines,
                        first_block_logged,
                        start_height,
                        end_height,
                        blockstore,
                    )
                    .await?;
                }
            }
        }

        // Flush any remaining network blocks (fewer than GETDATA_BATCH_SIZE).
        if !net_batch_heights.is_empty() {
            enqueue_network_block_batch(
                net_batch_heights,
                net_batch_permits,
                network,
                peer_addr,
                peer_id,
                validation_tip,
                confirmed_body_height,
                chunk_default_secs,
                in_flight,
                in_flight_heights,
                inflight_deadlines,
                first_block_logged,
                start_height,
                end_height,
                blockstore,
            )
            .await?;
        }
        Ok(())
    }

    tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
    fill_pipeline(
        next_to_send,
        end_height,
        pipeline_depth,
        &received,
        &mut in_flight,
        &mut in_flight_heights,
        &mut inflight_deadlines,
        &block_hash_by_height,
        &network,
        peer_addr,
        peer_id,
        blockstore,
        protocol_version,
        validated_tip,
        confirmed_body_height,
        chunk_default_secs,
        &blocks_sem,
        &mut first_block_logged,
        start_height,
        &mut local_sourced_heights,
        tip_hole_grown,
        tip_hole_cap,
    )
    .await?;
    // W35″ observability: how full is the GetData pipe after initial fill?
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        if received.is_empty() && !in_flight.is_empty() {
            pipe_fill_recv0 = true;
            super::tip_stage::note_pipe_fill_recv0();
        }
        warn!(
            "[IBD_PIPE_FILL] peer={} chunk={}-{} in_flight={}/{} received={} tip_hole_grown={}",
            peer_id,
            start_height,
            end_height,
            in_flight.len(),
            pipeline_depth,
            received.len(),
            tip_hole_grown
        );
    }

    // Hard deadline: abort only when the gap height (next_to_send) makes no progress.
    // Out-of-order receives ahead of the gap must not extend the deadline indefinitely.
    // W8: multi-block chunks used the same 45s as micros (W6), aborting after receiving
    // 13/14 blocks (live h=646643). Give multi-block ≥90s (or 3× per-block) so a slow gap
    // peer can finish while ahead blocks stay buffered.
    let chunk_deadline_secs: u64 = wan_deep_tip_pipe_chunk_deadline_secs(
        start_height,
        end_height,
        confirmed_body_height,
        if start_height == end_height {
            timeout_duration.as_secs()
        } else {
            let blocks = end_height.saturating_sub(start_height).saturating_add(1);
            timeout_duration
                .as_secs()
                .saturating_mul(3)
                .max(90)
                .min(timeout_duration.as_secs().saturating_mul(blocks.max(1)))
                .clamp(90, 600)
        },
    );
    let mut last_gap_at = chunk_start_time;
    let mut deadline_poll = tokio::time::interval(Duration::from_secs(1));
    deadline_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so the first poll is ~1s out.
    deadline_poll.tick().await;

    loop {
        // W32d: tiered per-position timeout tracks the live gap cursor.
        let tip_now = validation_height
            .as_ref()
            .map(|h| h.load(Ordering::Relaxed))
            .unwrap_or(validated_tip);
        // W29c/P1b: once network tip STREAM armed progressive, keep it for the chunk
        // (do not overwrite with per-height tier each poll — that made the assign dead).
        if progressive_timeout_applied {
            timeout_duration = Duration::from_secs(tip_progressive_secs);
        } else {
            timeout_duration = Duration::from_secs(block_gap_timeout_secs(
                next_to_send,
                tip_now,
                confirmed_body_height,
                start_height,
                end_height,
                chunk_default_secs,
            ));
        }
        sync_inflight_started(
            &mut inflight_started,
            &mut inflight_deadlines,
            &in_flight_heights,
        );
        // W71/W72: tip walked into a height still waiting on a mid/deep timeout.
        // W72: shorten the in-flight deadline so the existing soft-retry path re-GetData's
        // the tip in place (keep ahead `received`) instead of aborting the whole deep pipe.
        // Live post-W71: CAP abort recover p50≈4s × ~5/min ≈ ⅓ wall — tip60 fell to ~30.
        //
        // W85: rebase CAP clock on tip-roll — deep-pipe GetData often precedes tip by 3–11s
        // (`need→getdata` p50≈-8s). Measuring from pre-roll GetData made CAP=12s fire after
        // ~4s of true tip wait (live W84: `need→body` p50≈14ms but CAP soft-retry thrash →
        // tip60 ~20).
        let tip_needed = tip_now.saturating_add(1);
        rebase_tip_cap_clock(
            tip_needed,
            &mut tip_cap_clock_h,
            &in_flight_heights,
            &mut inflight_started,
        );
        let ahead_buffered = tip_pipe_has_ahead_buffered(&received, tip_needed);
        // W109/W139/W184: CAP — empty deep 16s; empty (H,H) mute 5s; holey pending 16s.
        let tip_cap_secs = tip_gap_timeout_secs_for_chunk(ahead_buffered, start_height, end_height);
        if next_to_send == tip_needed
            && in_flight_heights.contains(&next_to_send)
            && inflight_started
                .get(&next_to_send)
                .is_some_and(|started| tip_gap_inflight_exceeded(*started, tip_cap_secs))
        {
            let waited = inflight_started
                .get(&next_to_send)
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            // W108/W109: if ahead already buffered, allow one soft tip re-GetData (keep pipe).
            // W114: hot tip-STREAM deep pipe gets a longer soft budget (empty/holey).
            let hot_tip_streamer = tip_enter
                .as_ref()
                .is_some_and(|a| a.peer_is_hot_tip_streamer(peer_id));
            let max_soft = gap_soft_retry_budget_for_chunk_ex(
                next_to_send,
                tip_now,
                start_height,
                end_height,
                ahead_buffered,
                hot_tip_streamer,
            );
            if gap_soft_retries < max_soft {
                if let Some(dl) = inflight_deadlines.get(&next_to_send) {
                    let now = wall_ms_now();
                    // Only shorten once (deadline still in the future).
                    let prev = dl.load(Ordering::Relaxed);
                    if prev > now
                        && dl
                            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
                            .is_ok()
                    {
                        // W139: count CAP softs (was log-only → progressive + CAP shared
                        // budget under-counted CAP path).
                        gap_soft_retries = gap_soft_retries.saturating_add(1);
                        warn!(
                            "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — shortening deadline for soft-retry ({}/{})",
                            next_to_send,
                            waited,
                            tip_cap_secs,
                            start_height,
                            end_height,
                            peer_id,
                            ahead_buffered,
                            gap_soft_retries,
                            max_soft
                        );
                    } else if prev <= now && waited >= tip_cap_secs {
                        // W170: deadline already past but soft not counted (select lag /
                        // race). Still arm soft so covering=3 zombies cannot sit mute.
                        gap_soft_retries = gap_soft_retries.saturating_add(1);
                        warn!(
                            "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — deadline already expired, counting soft-retry ({}/{})",
                            next_to_send,
                            waited,
                            tip_cap_secs,
                            start_height,
                            end_height,
                            peer_id,
                            ahead_buffered,
                            gap_soft_retries,
                            max_soft
                        );
                    }
                }
            } else {
                warn!(
                    "[IBD_TIP_TIMEOUT_CAP] gap {} in-flight {}s ≥ tip cap {}s (chunk {}-{}, peer={}, ahead_buf={}) — soft exhausted, aborting for tip rotate",
                    next_to_send,
                    waited,
                    tip_cap_secs,
                    start_height,
                    end_height,
                    peer_id,
                    ahead_buffered
                );
                peer_scorer.record_failure(peer_addr);
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                flush_received_on_abort(
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                    next_to_send,
                    validation_height.as_deref(),
                )
                .await;
                return Err(anyhow::anyhow!(
                    "tip-gap timeout cap: gap {} waited {}s in chunk {}-{}",
                    next_to_send,
                    waited,
                    start_height,
                    end_height
                ));
            }
        }

        // P1d: PIPE_FILL mute eject — tip-band network body starved for MUTE_PIPE_MS after
        // STREAM and/or PIPE_FILL recv=0. Ahead net bodies must not disarm.
        //
        // 2026-07-28 dens-hash160 WAN: every PIPE_FILL recv=0 sat under GAP_PERSIST local
        // tip advances; `tip_buffered` / `tip_is_local_inflight` cleared the clock each
        // height so mute never reached 3s (`pipe_fill_mute=0` / `pipe_fill_recv0=522`).
        // Keep the clock across local tip crawl; only clear on tip-band *network* body
        // (or when the mute episode ends). Defer *fire* while tip is already arriving
        // so we do not cancel an in-flight local load — once local runway ends and tip
        // still needs network, an already-elapsed clock ejects immediately.
        {
            let tip_buffered = received.contains_key(&next_to_send);
            let tip_is_local_inflight = local_sourced_heights.contains(&next_to_send);
            let next_in_chunk = next_to_send >= start_height && next_to_send <= end_height;
            let episode_active = pipe_mute_episode_active(
                gap_streams,
                pipe_fill_recv0,
                saw_network_body,
                next_in_chunk,
            );
            if episode_active {
                let armed = pipe_mute_armed_at.get_or_insert_with(Instant::now);
                if pipe_mute_may_fire(tip_buffered, tip_is_local_inflight)
                    && armed.elapsed() >= Duration::from_millis(mute_pipe_ms())
                {
                    warn!(
                        "[IBD_PIPE_FILL_MUTE] peer={} chunk={}-{} tip={} in_flight={} received={} gap_streams={} pipe_fill_recv0={} after {}ms — mute eject",
                        peer_id,
                        start_height,
                        end_height,
                        next_to_send,
                        in_flight.len(),
                        received.len(),
                        gap_streams,
                        pipe_fill_recv0,
                        mute_pipe_ms()
                    );
                    peer_scorer.record_failure(peer_addr);
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    return Err(anyhow::anyhow!(
                        "PIPE_FILL mute: no network body in {}ms (chunk {}-{})",
                        mute_pipe_ms(),
                        start_height,
                        end_height
                    ));
                }
            } else if pipe_mute_should_clear_clock(
                episode_active,
                gap_streams,
                pipe_fill_recv0,
                saw_network_body,
            ) {
                pipe_mute_armed_at = None;
            }
        }

        if resync_next_to_send_with_validation_tip(
            validation_height.as_ref(),
            &mut received,
            &mut next_to_send,
            end_height,
            &network,
            peer_addr,
            &block_hash_by_height,
            &in_flight_heights,
            &mut last_gap_at,
        ) {
            // Drain immediately-ready in_flight (local blocks) into `received` before exit.
            // Without this, CHUNK_OBSOLETE drops local tip bodies still sitting unpolled in
            // FuturesUnordered (live: 698200 local loaded → obsolete → never GAP_STREAM).
            {
                use std::task::{Context, Poll};
                let waker = futures::task::noop_waker_ref();
                let mut cx = Context::from_waker(waker);
                while let Poll::Ready(Some((height, block_hash, _, block_result, permit))) =
                    in_flight.poll_next_unpin(&mut cx)
                {
                    drop(permit);
                    in_flight_heights.remove(&height);
                    if let Ok(Ok((block, block_witnesses, _wire))) = block_result {
                        let received_hash = blockstore.get_block_hash(&block);
                        if received_hash == block_hash {
                            ARC_BLOCK_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            received_put(
                                &mut received,
                                height,
                                (Arc::new(block), Arc::new(block_witnesses)),
                            );
                        }
                    }
                }
                // Cancel any still-pending network requests left unpolled.
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                in_flight_heights.clear();
            }
            // W13: chunk entirely behind tip — flush anything left and free the peer.
            flush_received_on_abort(
                &mut received,
                block_tx.as_ref(),
                start_height,
                end_height,
                next_to_send,
                validation_height.as_deref(),
            )
            .await;
            received_drain_all(&mut received);
            info!(
                "[IBD_CHUNK_OBSOLETE] chunk {}-{} behind tip — releasing peer {}",
                start_height, end_height, peer_id
            );
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: if streaming { streamed_block_count } else { 0 },
            });
        }
        // W13: also exit cleanly if streaming cursor past end after resync.
        if next_to_send > end_height {
            received_drain_all(&mut received);
            return Ok(DownloadChunkResult {
                blocks,
                streamed_block_count: if streaming { streamed_block_count } else { 0 },
            });
        }
        let next_result = if progress.last_block_hash.is_none() {
            if let Some(ref mut rx) = stall_rx {
                // biased: poll in_flight first so a locally-cached block (resolved immediately)
                // is never preempted by a stall signal that arrived in the broadcast channel
                // before this select runs. Without biased, tokio may pick stall_rx non-deterministically
                // and abort a chunk we're already holding the first block for.
                tokio::select! {
                    biased;
                    r = in_flight.next() => r,
                    stall_res = rx.recv() => {
                        if let Ok(stall_h) = stall_res {
                            if stall_h >= start_height && stall_h <= end_height {
                                // Ignore premature coordinator stalls while the first block is
                                // still in flight (common at height 1 before peer responds).
                                if chunk_start_time.elapsed() < first_block_wait {
                                    continue;
                                }
                                if !ibd_stall_aborts_inflight_gap_fetch(
                                    wan_multi_peer,
                                    confirmed_body_height,
                                    stall_h,
                                ) {
                                    continue;
                                }
                                warn!("Coordinator stall at {}: aborting chunk {}-{} (no first block yet)", stall_h, start_height, end_height);
                                for &h in &in_flight_heights {
                                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                        network.cancel_block_request(peer_addr, h_hash);
                                    }
                                }
                                return Err(anyhow::anyhow!(
                                    "Coordinator stall: aborting chunk {}-{} for retry",
                                    start_height, end_height
                                ));
                            }
                        }
                        continue;
                    }
                    _ = wait_tip_enter_abort(&tip_enter, peer_id, start_height, end_height) => {
                        warn!(
                            "[IBD_TIP_ENTER] aborting ahead chunk {}-{} (peer={}) — tip walked in",
                            start_height, end_height, peer_id
                        );
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "tip-enter walk-in: aborting chunk {}-{} for tip owner reassign",
                            start_height, end_height
                        ));
                    }
                    _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                        warn!(
                            "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                            start_height, end_height, peer_id
                        );
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                            start_height, end_height
                        ));
                    }
                    _ = tokio::time::sleep(first_block_wait) => {
                        let waited = first_block_wait.as_secs();
                        warn!(
                            "Chunk {} to {}: no first block in {}s, failing for retry",
                            start_height, end_height, waited
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {waited}s)"
                        ));
                    }
                    _ = wait_cooperative_outer(outer_deadline) => {
                        if try_soft_extend_outer(
                            &mut outer_deadline,
                            &mut outer_extends,
                            &mut outer_deadline_secs,
                            last_gap_at,
                            gap_streams,
                            chunk_start_time,
                            start_height,
                            end_height,
                            peer_id,
                        ) {
                            continue;
                        }
                        return abort_on_outer_deadline(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                            &in_flight_heights,
                            &block_hash_by_height,
                            &network,
                            peer_addr,
                            &peer_scorer,
                            outer_deadline_secs,
                        )
                        .await;
                    }
                }
            } else {
                tokio::select! {
                    r = in_flight.next() => r,
                    _ = tokio::time::sleep(first_block_wait) => {
                        let waited = first_block_wait.as_secs();
                        warn!(
                            "Chunk {} to {}: no first block in {}s, failing for retry",
                            start_height, end_height, waited
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "Block download stalled (no first block in {waited}s)"
                        ));
                    }
                    _ = wait_cooperative_outer(outer_deadline) => {
                        if try_soft_extend_outer(
                            &mut outer_deadline,
                            &mut outer_extends,
                            &mut outer_deadline_secs,
                            last_gap_at,
                            gap_streams,
                            chunk_start_time,
                            start_height,
                            end_height,
                            peer_id,
                        ) {
                            continue;
                        }
                        return abort_on_outer_deadline(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                            &in_flight_heights,
                            &block_hash_by_height,
                            &network,
                            peer_addr,
                            &peer_scorer,
                            outer_deadline_secs,
                        )
                        .await;
                    }
                }
            }
        } else if let Some(ref mut rx) = stall_rx {
            // We have started receiving blocks. Race in_flight, stall signal, and hard deadline.
            tokio::select! {
                r = in_flight.next() => r,
                stall_res = rx.recv() => {
                    // Coordinator detected validation waiting on our gap height — fail fast so
                    // assigner can requeue a gap micro-chunk to another peer instead of burning
                    // another chunk_deadline_secs on the same slow peer.
                    if let Ok(stall_h) = stall_res {
                        if stall_h >= start_height
                            && stall_h <= end_height
                            && stall_h == next_to_send
                        {
                            if !ibd_stall_aborts_inflight_gap_fetch(
                                wan_multi_peer,
                                confirmed_body_height,
                                stall_h,
                            ) {
                                continue;
                            }
                            warn!(
                                "Coordinator stall at gap height {}: aborting chunk {}-{} for immediate peer retry",
                                stall_h, start_height, end_height
                            );
                            peer_scorer.record_failure(peer_addr);
                            // Flush buffered blocks to coordinator before cancelling so
                            // already-downloaded heights don't need to be re-fetched.
                            flush_received_on_abort(
                                &mut received,
                                block_tx.as_ref(),
                                start_height,
                                end_height,
                                next_to_send,
                                validation_height.as_deref(),
                            )
                            .await;
                            // Cancel all in-flight requests: their rx handles will be
                            // dropped at function exit but tx entries remain in
                            // pending_block_requests.  A late peer response would find
                            // a stale tx, silently discard the block, and stall IBD.
                            for &h in &in_flight_heights {
                                if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                    network.cancel_block_request(peer_addr, h_hash);
                                }
                            }
                            return Err(anyhow::anyhow!(
                                "Coordinator stall at gap height {} - chunk {}-{} needs retry",
                                stall_h, start_height, end_height
                            ));
                        }
                    }
                    continue;
                }
                _ = wait_tip_enter_abort(&tip_enter, peer_id, start_height, end_height) => {
                    warn!(
                        "[IBD_TIP_ENTER] aborting ahead chunk {}-{} (peer={}) — tip walked in",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-enter walk-in: aborting chunk {}-{} for tip owner reassign",
                        start_height, end_height
                    ));
                }
                _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                    warn!(
                        "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                        start_height, end_height
                    ));
                }
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(chunk_deadline_secs) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (next_to_send={}, in_flight={}, received={}) — aborting for retry",
                        start_height, end_height, chunk_deadline_secs,
                        next_to_send, in_flight.len(), received.len()
                    );
                    peer_scorer.record_failure(peer_addr);
                    // Flush blocks we already have so they don't need re-downloading.
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: stuck at height {} after {}s",
                        start_height, end_height, next_to_send, chunk_deadline_secs
                    ));
                }
                _ = wait_cooperative_outer(outer_deadline) => {
                    if try_soft_extend_outer(
                        &mut outer_deadline,
                        &mut outer_extends,
                        &mut outer_deadline_secs,
                        last_gap_at,
                        gap_streams,
                        chunk_start_time,
                        start_height,
                        end_height,
                        peer_id,
                    ) {
                        continue;
                    }
                    return abort_on_outer_deadline(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                        &in_flight_heights,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        &peer_scorer,
                        outer_deadline_secs,
                    )
                    .await;
                }
            }
        } else {
            tokio::select! {
                r = in_flight.next() => r,
                _ = wait_blacklist_abort(&tip_enter, peer_id) => {
                    warn!(
                        "[IBD_TIP_BLACKLIST] aborting chunk {}-{} (peer={}) — tip-SLA rotate",
                        start_height, end_height, peer_id
                    );
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "tip-SLA blacklist: aborting chunk {}-{} for peer rotate",
                        start_height, end_height
                    ));
                }
                _ = deadline_poll.tick() => {
                    if last_gap_at.elapsed() < Duration::from_secs(chunk_deadline_secs) {
                        continue;
                    }
                    warn!(
                        "Chunk {}-{}: hard {}s gap deadline expired (no stall_rx, next_to_send={}) — aborting for retry",
                        start_height, end_height, chunk_deadline_secs, next_to_send
                    );
                    peer_scorer.record_failure(peer_addr);
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Chunk hard deadline {}-{}: no stall_rx, stuck at height {} after {}s",
                        start_height, end_height, next_to_send, chunk_deadline_secs
                    ));
                }
                _ = wait_cooperative_outer(outer_deadline) => {
                    if try_soft_extend_outer(
                        &mut outer_deadline,
                        &mut outer_extends,
                        &mut outer_deadline_secs,
                        last_gap_at,
                        gap_streams,
                        chunk_start_time,
                        start_height,
                        end_height,
                        peer_id,
                    ) {
                        continue;
                    }
                    return abort_on_outer_deadline(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                        &in_flight_heights,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        &peer_scorer,
                        outer_deadline_secs,
                    )
                    .await;
                }
            }
        };

        let Some((height, block_hash, request_start, block_result, permit)) = next_result else {
            break;
        };
        // D0: release the in-flight permit BEFORE fill_pipeline / enqueue_chunk_block.
        // Holding it across acquire_owned while in_flight is at capacity self-deadlocks
        // (WAN tip pipes: pipeline_depth == sem == 128 → 5760s safety-net, 0 soft timeouts).
        drop(permit);
        in_flight_heights.remove(&height);
        match block_result {
            Ok(Ok((block, block_witnesses, wire_payload))) => {
                // P1a: local enqueue inserts height; network GetData does not.
                let from_local = local_sourced_heights.remove(&height);
                if !from_local {
                    network_body_heights.insert(height);
                    // P1d: only tip-band net bodies cancel mute (ahead delivery must not
                    // shelter a mute tip — thermo gd_p50≈2.7s on ahead reset the 3s clock).
                    if height == next_to_send || height <= tip_now.saturating_add(1) {
                        saw_network_body = true;
                        pipe_mute_armed_at = None;
                        super::tip_stage::clear_pipe_fill_recv0_streak();
                    }
                    // C1b/C1d/C1n: deepen only on exact tip-gap body.
                    // Cap: cold=32, gd-fast=48 (EWMA), warm=128 (Mode T only).
                    // C1u: freeze/shrink while GD_SLOW — do not deepen a drip pipe.
                    // C1u′: ratchet down by GROW_STEP (not cliff to fill_cap) so EWMA
                    // flicker ≥gate does not pin depth=8 for the whole tip crawl.
                    if height == next_to_send {
                        let prev = tip_hole_grown;
                        let gd = super::tip_stage::getdata_body_ewma_ms();
                        let sole_ready = tip_enter
                            .as_ref()
                            .map(|a| a.ibd_ready_peer_count() <= 1)
                            .unwrap_or(false);
                        tip_hole_cap = tip_hole_cap_for_sole(
                            sole_ready,
                            tip_hole_grow_cap_for_peer(tip_hole_hot),
                        );
                        if tip_hole_grown > tip_hole_cap {
                            tip_hole_grown = tip_hole_cap;
                            info!(
                                "[IBD_TIP_HOLE_CAP_CLAMP] peer={} height={} depth {}→{} (cap={})",
                                peer_id, height, prev, tip_hole_grown, tip_hole_cap
                            );
                            if let Some(ref a) = tip_enter {
                                a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                            }
                            super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                        }
                        if tip_hole_gd_slow() && sole_ready && tip_hole_sole_floor_applies(height) {
                            let floor = tip_hole_sole_gd_slow_floor();
                            super::tip_stage::note_sole_floor_latch();
                            maybe_note_sole_no_fast_latch(height);
                            if tip_hole_grown != floor {
                                tip_hole_grown = floor;
                                info!(
                                    "[IBD_TIP_HOLE_GD_SLOW_SOLE_FLOOR] peer={} height={} depth {}→{} gd_ewma_ms={:?}",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    gd.map(|(ms, _)| ms)
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else if sole_ready
                            && tip_hole_sole_floor_applies(height)
                            && tip_hole_sole_floor_blocks_grow()
                        {
                            // Hold floor until recover; no-FAST latch separately blocks FAST_CAP.
                            let floor = tip_hole_sole_gd_slow_floor();
                            if tip_hole_grown > floor {
                                tip_hole_grown = floor;
                                info!(
                                    "[IBD_TIP_HOLE_SOLE_FLOOR_HOLD] peer={} height={} depth {}→{} gd_ewma_ms={:?} recover_ms={}",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    gd.map(|(ms, _)| ms),
                                    tip_hole_sole_floor_recover_ms()
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else if tip_hole_gd_slow() && !sole_ready {
                            let slow = tip_hole_slow_fill_cap();
                            let next = tip_hole_gd_slow_next_depth(tip_hole_grown);
                            if next < tip_hole_grown {
                                tip_hole_grown = next;
                                debug!(
                                    "[IBD_TIP_HOLE_GD_SLOW] peer={} height={} depth {}→{} (fill_cap={} ratchet={} gd_ewma_ms={:?} gate_ms={})",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    slow,
                                    tip_hole_gd_slow_ratchet_enabled(),
                                    gd.map(|(ms, _)| ms),
                                    tip_hole_gd_slow_ms()
                                );
                                if let Some(ref a) = tip_enter {
                                    a.clamp_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        } else {
                            tip_hole_grown =
                                tip_hole_grow_on_delivery_capped(tip_hole_grown, tip_hole_cap);
                            if tip_hole_grown > prev {
                                debug!(
                                    "[IBD_TIP_HOLE_GROW] peer={} height={} depth {}→{} (cap={} hot={} gd_ewma_ms={:?})",
                                    peer_id,
                                    height,
                                    prev,
                                    tip_hole_grown,
                                    tip_hole_cap,
                                    tip_hole_hot,
                                    gd.map(|(ms, _)| ms)
                                );
                                if tip_hole_cap > tip_hole_grow_cap() {
                                    debug!(
                                        "[IBD_TIP_HOLE_GD_FAST] peer={} depth={} cap={} gd_ewma_ms={:?} gate_ms={}",
                                        peer_id,
                                        tip_hole_grown,
                                        tip_hole_cap,
                                        gd.map(|(ms, _)| ms),
                                        tip_hole_gd_fast_ms()
                                    );
                                }
                                if let Some(ref a) = tip_enter {
                                    a.note_tip_hole_depth(peer_id, tip_hole_grown);
                                }
                                super::tip_stage::note_tip_hole_duty(tip_hole_grown);
                            }
                        }
                    }
                }
                let received_hash = blockstore.get_block_hash(&block);
                if received_hash != block_hash {
                    // BUGFIX: Previously we continued without inserting; next_to_send stayed at this
                    // height while the pipeline requested later heights, so we could return Ok with a
                    // gap and mark the chunk complete — permanent reorder-buffer stall (e.g. min 547).
                    warn!(
                        "Block hash mismatch for height {} (expected {}, got {}) — failing chunk for retry",
                        height,
                        hex::encode(block_hash),
                        hex::encode(received_hash)
                    );
                    peer_scorer.record_failure(peer_addr);
                    flush_received_on_abort(
                        &mut received,
                        block_tx.as_ref(),
                        start_height,
                        end_height,
                        next_to_send,
                        validation_height.as_deref(),
                    )
                    .await;
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "Block hash mismatch at height {} - chunk needs retry",
                        height
                    ));
                }
                progress.record_progress(received_hash);
                progress.reset_timeout();
                let latency_ms = request_start.elapsed().as_secs_f64() * 1000.0;
                // Rough wire-size for W2 EMA + peer scorer (not consensus-critical).
                let mut block_size = 80u64;
                for tx in &block.transactions {
                    block_size = block_size
                        .saturating_add(40)
                        .saturating_add((tx.inputs.len() * 40 + tx.outputs.len() * 34) as u64);
                }
                for stack in block_witnesses.iter() {
                    for wit in stack.iter() {
                        for item in wit.iter() {
                            block_size = block_size.saturating_add(item.len() as u64);
                        }
                    }
                }
                note_download_block_bytes(block_size);
                peer_scorer.record_block(peer_addr, block_size, latency_ms);
                // W7: empty-witness MSG_BLOCK of a *commitment* block must not enter
                // `received` (stripped payload). Blocks without BIP141 commitment may
                // legitimately have empty stacks (live h=640022) — accept those.
                let segwit_on =
                    feature_registry.is_feature_active("segwit", height, block.header.timestamp);
                if empty_witness_unacceptable(&block, &block_witnesses, segwit_on) {
                    empty_witness_hits = empty_witness_hits.saturating_add(1);
                    // A6: demote peer after repeated empty-witness (likely MSG_BLOCK-only).
                    if empty_witness_hits >= 2 {
                        peer_scorer.record_failure(peer_addr);
                    }
                    // Cap re-requests: same peer looping forever pins validation + holds
                    // reorder/bridge memory. Abort so worker requeues with exclude + blacklist.
                    if empty_witness_hits >= max_empty_witness_hits {
                        warn!(
                            "[IBD_EMPTY_WITNESS_ABORT] height {}: {} empty-witness hits from {} — aborting chunk (max={})",
                            height, empty_witness_hits, peer_id, max_empty_witness_hits
                        );
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        if height == next_to_send {
                            return Err(anyhow::anyhow!(
                                "empty-witness at gap height {} after {} hits - chunk needs retry",
                                height,
                                empty_witness_hits
                            ));
                        }
                        return Err(anyhow::anyhow!(
                            "empty-witness at height {} after {} hits - chunk needs retry",
                            height,
                            empty_witness_hits
                        ));
                    }
                    // Rate-limit spam: first 3 hits + every 8th thereafter.
                    if empty_witness_hits <= 3 || empty_witness_hits % 8 == 0 {
                        warn!(
                            "[IBD_EMPTY_WITNESS] height {}: rejecting empty-witness payload — re-requesting MSG_WITNESS_BLOCK (hit={}/{})",
                            height, empty_witness_hits, max_empty_witness_hits
                        );
                    }
                    enqueue_chunk_block(
                        height,
                        block_hash,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &mut first_block_logged,
                        start_height,
                        end_height,
                        &mut local_sourced_heights,
                    )
                    .await?;
                    tip_hole_cap =
                        tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                    fill_pipeline(
                        next_to_send,
                        end_height,
                        pipeline_depth,
                        &received,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut first_block_logged,
                        start_height,
                        &mut local_sourced_heights,
                        tip_hole_grown,
                        tip_hole_cap,
                    )
                    .await?;
                    continue;
                }
                // KEEP: repair + persist, then received_put (including from_local).
                // Persist short-circuits when the body is already on disk.
                ARC_BLOCK_CREATED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let block_arc = Arc::new(block);
                let wit_arc = Arc::new(block_witnesses);
                if let Err(e) = try_repair_missing_witness(
                    blockstore,
                    height,
                    block_hash,
                    wit_arc.as_ref(),
                    protocol_version,
                    Some(block_arc.as_ref()),
                ) {
                    warn!("[IBD_WITNESS_REPAIR] height {}: repair failed: {e}", height);
                }
                let sync_log = matches!(
                    std::env::var("BLVM_IBD_GAP_PERSIST_SYNC_LOG")
                        .ok()
                        .as_deref()
                        .map(|s| s.trim()),
                    Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
                );
                let t_persist = Instant::now();
                if let Err(e) = try_persist_gap_block_for_local_inject_with_wire(
                    blockstore,
                    validation_height.as_ref(),
                    height,
                    block_hash,
                    block_arc.as_ref(),
                    wit_arc.as_ref(),
                    protocol_version,
                    wire_payload.as_deref(),
                ) {
                    warn!("[IBD_GAP_PERSIST] height {}: persist failed: {e}", height);
                }
                if sync_log {
                    let ms = t_persist.elapsed().as_millis();
                    if ms >= 20 {
                        tracing::info!(
                            "[IBD_GAP_PERSIST_SYNC] height={} persist_ms={} from_local={}",
                            height,
                            ms,
                            from_local
                        );
                    }
                }
                received_put(
                    &mut received,
                    height,
                    (Arc::clone(&block_arc), Arc::clone(&wit_arc)),
                );
                trim_download_received(
                    &mut received,
                    blockstore,
                    validation_height.as_ref(),
                    protocol_version,
                );
                super::tip_stage::mark_body(height);
                // Keep tip-hot window on tip-adjacent body arrival even when coordinator
                // already advanced via LOCAL_GAP inject of GAP_PERSIST (try_stream no-ops).
                // Live 2026-07-15: 15s without GAP_STREAM note → idle floor upgrade mid-pipe
                // → TIP_ENTER abort of a 30+ blk/s owner (since_GS=38–115s while TIP_PIPE live).
                // P1a: never credit WAN tip-stream score for try_load_local bodies.
                // a31: tip-adjacent body credits once, then GAP_STREAM credits again
                // (double-credit sticky tip BPS).
                let tip_adjacent_for_credit =
                    height <= tip_now.saturating_add(1) || height == next_to_send;
                if from_local {
                    if tip_adjacent_for_credit {
                        debug!(
                            "[IBD_TIP_LOCAL_STREAM] height={} peer={} — no tip-stream credit",
                            height, peer_id
                        );
                        // Land E: fresh STREAM arms tip-crawl supply-healthy hold.
                        super::tip_stage::note_tip_local_stream();
                    }
                } else if let Some(ref a) = tip_enter {
                    if tip_adjacent_for_credit {
                        a.note_wan_tip_stream(peer_id);
                    }
                }
                // KEEP: persist on the download path, then STREAM immediately.
                if try_stream_validation_gap(
                    validation_height.as_ref(),
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                )
                .await?
                {
                    last_gap_at = std::time::Instant::now();
                    gap_streams = gap_streams.saturating_add(1);
                    gap_soft_retries = 0;
                    // W102: STREAM advanced DEDUP (tip may still be clone-held) — advance
                    // cursor so tip-hole pipe cap disarms and fill can open full pipeline_depth.
                    sync_next_to_send_after_gap_stream(&mut next_to_send, end_height);
                    if !from_local {
                        network_tip_streamed = true;
                        if let Some(ref a) = tip_enter {
                            a.note_wan_tip_stream(peer_id);
                        }
                    }
                    // Once per chunk — logging every streamed height flooded the log
                    // (~128 lines/ms live 2026-07-15) and competed with tip crawl metrics.
                    if gap_streams == 1 {
                        info!(
                            "[IBD_GAP_STREAM] streamed tip gap from chunk {}-{} to coordinator (peer={})",
                            start_height, end_height, peer_id
                        );
                    }
                    // W29c / P1b: progressive 45s only after a *network*-sourced tip STREAM.
                    // Local tip + mute net pipe must not get shelter (G-W ×483 progressive).
                    if !progressive_timeout_applied
                        && network_tip_streamed
                        && end_height > start_height
                        && tip_progressive_secs > tip_aware_secs
                    {
                        // Armed for subsequent loop polls (see progressive_timeout_applied above).
                        progressive_timeout_applied = true;
                        warn!(
                            "[IBD_TIP_PROGRESSIVE_TIMEOUT] tip streamed from {}-{} — remaining blocks timeout {}s (was {}s)",
                            start_height, end_height, tip_progressive_secs, tip_aware_secs
                        );
                    }
                }
                if !first_block_logged {
                    info!(
                        "[IBD] {} chunk {}-{}: first block received (h={}, {}ms)",
                        peer_id, start_height, end_height, height, latency_ms as u64
                    );
                    first_block_logged = true;
                }
            }
            Ok(Err(_)) => {
                warn!("Block channel closed for height {}", height);
                peer_scorer.record_failure(peer_addr);
                flush_received_on_abort(
                    &mut received,
                    block_tx.as_ref(),
                    start_height,
                    end_height,
                    next_to_send,
                    validation_height.as_deref(),
                )
                .await;
                for &h in &in_flight_heights {
                    if let Some(&h_hash) = block_hash_by_height.get(&h) {
                        network.cancel_block_request(peer_addr, h_hash);
                    }
                }
                return Err(anyhow::anyhow!(
                    "Block channel closed for height {} - chunk needs retry",
                    height
                ));
            }
            Err(_) => {
                if height == next_to_send {
                    // P4: soft-timeout on the gap — re-request in place (keep ahead `received`)
                    // instead of aborting the whole chunk. Hard deadline (W8) still aborts.
                    // W10/W12: budget is tip-aware — far-ahead aborts immediately; tip gap gets
                    // one soft retry then peer rotate (live: 3×45s on same peer → 0.1 BPS).
                    let tip_now = validation_height
                        .as_ref()
                        .map(|h| h.load(Ordering::Relaxed))
                        .unwrap_or(validated_tip);
                    let tip_needed_h = tip_now.saturating_add(1);
                    let ahead_buffered = tip_pipe_has_ahead_buffered(&received, tip_needed_h);
                    let hot_tip_streamer = tip_enter
                        .as_ref()
                        .is_some_and(|a| a.peer_is_hot_tip_streamer(peer_id));
                    let max_soft = gap_soft_retry_budget_for_chunk_ex(
                        height,
                        tip_now,
                        start_height,
                        end_height,
                        ahead_buffered,
                        hot_tip_streamer,
                    );
                    gap_soft_retries = gap_soft_retries.saturating_add(1);
                    network.cancel_block_request(peer_addr, block_hash);
                    if gap_soft_retries > max_soft {
                        warn!(
                            "Block timeout for gap height {} after {}s (soft retries exhausted {}/{}, tip={}, ahead_buf={}) — aborting chunk",
                            height,
                            timeout_duration.as_secs(),
                            gap_soft_retries.saturating_sub(1),
                            max_soft,
                            tip_now,
                            ahead_buffered
                        );
                        peer_scorer.record_failure(peer_addr);
                        for &h in &in_flight_heights {
                            if let Some(&h_hash) = block_hash_by_height.get(&h) {
                                network.cancel_block_request(peer_addr, h_hash);
                            }
                        }
                        flush_received_on_abort(
                            &mut received,
                            block_tx.as_ref(),
                            start_height,
                            end_height,
                            next_to_send,
                            validation_height.as_deref(),
                        )
                        .await;
                        // W110: tag tip-gap soft-exhaust so worker mute-match stays robust
                        // even if string heuristics drift (empty soft=0 → rotate).
                        if height == tip_needed_h {
                            return Err(anyhow::anyhow!(
                                "tip-gap timeout: Block timeout for gap height {} after {}s - chunk needs retry",
                                height,
                                timeout_duration.as_secs()
                            ));
                        }
                        return Err(anyhow::anyhow!(
                            "Block timeout for gap height {} after {}s - chunk needs retry",
                            height,
                            timeout_duration.as_secs()
                        ));
                    }
                    warn!(
                        "[IBD_GAP_SOFT_RETRY] height {} after {}s (limit {}s) — re-requesting (attempt {}/{}, tip={})",
                        height,
                        request_start.elapsed().as_secs(),
                        timeout_duration.as_secs(),
                        gap_soft_retries,
                        max_soft,
                        tip_now
                    );
                    super::tip_stage::mark_soft_retry(height);
                    // W28c: tip soft-retry arms a second covering peer (failover), while this
                    // peer re-requests in place. Cleared when tip arrives in reorder.
                    // W31: never arm failover past body tip — opens 2-fetcher stall recovery on WAN.
                    if height == tip_now.saturating_add(1) {
                        // WAN: arm failover so ChunkAssigner may assign a second tip-height
                        // racer while soft-retry ahead-freeze is latched (W31 exception).
                        // Non-WAN: same as before.
                        super::tip_stage::arm_tip_failover();
                        warn!(
                            "[IBD_TIP_FAILOVER] armed after soft-retry on tip {} (peer={}, wan={})",
                            height,
                            peer_id,
                            confirmed_body_height > 0 && height > confirmed_body_height
                        );
                    }
                    // A6i: do NOT record_failure on soft-retry — expected WAN tip latency.
                    // Live: soft-retry failure penalties floored tip owners at 0.1 and fed
                    // sticky churn; only hard-abort / exhausted soft-retries demote.
                    enqueue_chunk_block(
                        height,
                        block_hash,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &mut first_block_logged,
                        start_height,
                        end_height,
                        &mut local_sourced_heights,
                    )
                    .await?;
                    // W116: do not rebase CAP age here — live W115/W115b: spacing softs to
                    // full tip-cap windows made mute peers sticky (16–24s) and rate-failed
                    // earlier than W114 (soft burn ≈ fast rotate). Keep W114 soft budgets +
                    // W115 hot (H,H) soft=1 only.
                    tip_hole_cap =
                        tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                    fill_pipeline(
                        next_to_send,
                        end_height,
                        pipeline_depth,
                        &received,
                        &mut in_flight,
                        &mut in_flight_heights,
                        &mut inflight_deadlines,
                        &block_hash_by_height,
                        &network,
                        peer_addr,
                        peer_id,
                        blockstore,
                        protocol_version,
                        tip_now,
                        confirmed_body_height,
                        chunk_default_secs,
                        &blocks_sem,
                        &mut first_block_logged,
                        start_height,
                        &mut local_sourced_heights,
                        tip_hole_grown,
                        tip_hole_cap,
                    )
                    .await?;
                    continue;
                }
                // W32d: log the tier actually used (not download_timeout_secs=45).
                let used_secs = block_gap_timeout_secs(
                    height,
                    tip_now,
                    confirmed_body_height,
                    start_height,
                    end_height,
                    chunk_default_secs,
                );
                warn!(
                    "Block timeout for height {} ahead of gap {} after {}s — re-requesting",
                    height, next_to_send, used_secs
                );
                enqueue_chunk_block(
                    height,
                    block_hash,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    tip_now,
                    confirmed_body_height,
                    chunk_default_secs,
                    &blocks_sem,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &mut inflight_deadlines,
                    &mut first_block_logged,
                    start_height,
                    end_height,
                    &mut local_sourced_heights,
                )
                .await?;
                tip_hole_cap =
                    tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
                fill_pipeline(
                    next_to_send,
                    end_height,
                    pipeline_depth,
                    &received,
                    &mut in_flight,
                    &mut in_flight_heights,
                    &mut inflight_deadlines,
                    &block_hash_by_height,
                    &network,
                    peer_addr,
                    peer_id,
                    blockstore,
                    protocol_version,
                    tip_now,
                    confirmed_body_height,
                    chunk_default_secs,
                    &blocks_sem,
                    &mut first_block_logged,
                    start_height,
                    &mut local_sourced_heights,
                    tip_hole_grown,
                    tip_hole_cap,
                )
                .await?;
                continue;
            }
        }

        while let Some((block, block_witnesses)) = received_take(&mut received, next_to_send) {
            if let Some(ref tx) = block_tx {
                let tip_need = tip_need_from(validation_height.as_ref());
                await_block_tx_tip_reserve(tx, next_to_send, tip_need).await;
                let t0 = std::time::Instant::now();
                let send_r = tx.send((next_to_send, block, block_witnesses)).await;
                let wait_ms = t0.elapsed().as_millis() as u64;
                if wait_ms >= 10 {
                    info!(
                        "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                        next_to_send, wait_ms
                    );
                }
                if send_r.is_err() {
                    for &h in &in_flight_heights {
                        if let Some(&h_hash) = block_hash_by_height.get(&h) {
                            network.cancel_block_request(peer_addr, h_hash);
                        }
                    }
                    return Err(anyhow::anyhow!(
                        "block_tx closed during stream - chunk needs retry"
                    ));
                }
                streamed_block_count += 1;
                gap_streams = gap_streams.saturating_add(1);
                // P1a: drain credit only for network-sourced bodies.
                if network_body_heights.contains(&next_to_send) {
                    network_tip_streamed = true;
                    if let Some(ref a) = tip_enter {
                        a.note_wan_tip_stream(peer_id);
                    }
                }
            } else {
                blocks.push((next_to_send, block, block_witnesses));
            }
            next_to_send += 1;
            last_gap_at = std::time::Instant::now();
            gap_soft_retries = 0;
        }

        tip_hole_cap = tip_hole_cap_for_sole(sole_ready, tip_hole_grow_cap_for_peer(tip_hole_hot));
        fill_pipeline(
            next_to_send,
            end_height,
            pipeline_depth,
            &received,
            &mut in_flight,
            &mut in_flight_heights,
            &mut inflight_deadlines,
            &block_hash_by_height,
            &network,
            peer_addr,
            peer_id,
            blockstore,
            protocol_version,
            tip_now,
            confirmed_body_height,
            chunk_default_secs,
            &blocks_sem,
            &mut first_block_logged,
            start_height,
            &mut local_sourced_heights,
            tip_hole_grown,
            tip_hole_cap,
        )
        .await?;
    }

    // C1c: persist tip-hole depth for next chunk on this peer.
    if let Some(ref a) = tip_enter {
        a.note_tip_hole_depth(peer_id, tip_hole_grown);
    }

    while let Some((block, block_witnesses)) = received_take(&mut received, next_to_send) {
        if let Some(ref tx) = block_tx {
            let tip_need = tip_need_from(validation_height.as_ref());
            await_block_tx_tip_reserve(tx, next_to_send, tip_need).await;
            let t0 = std::time::Instant::now();
            let send_r = tx.send((next_to_send, block, block_witnesses)).await;
            let wait_ms = t0.elapsed().as_millis() as u64;
            if wait_ms >= 10 {
                info!(
                    "[IBD_BLOCK_TX_SEND_WAIT] height={} wait_ms={} (download→coordinator channel)",
                    next_to_send, wait_ms
                );
            }
            if send_r.is_err() {
                received_drain_all(&mut received);
                return Err(anyhow::anyhow!(
                    "block_tx closed during stream - chunk needs retry"
                ));
            }
            streamed_block_count += 1;
        } else {
            blocks.push((next_to_send, block, block_witnesses));
        }
        next_to_send += 1;
    }

    // BUGFIX: If in_flight drained but we could not stream start_height..=end_height in order
    // (e.g. hash mismatch previously skipped without Err), we must not report success — that
    // left permanent gaps in the coordinator reorder buffer.
    if next_to_send != end_height + 1 {
        let still = received.len();
        received_drain_all(&mut received);
        return Err(anyhow::anyhow!(
            "Incomplete chunk {}-{}: stuck before height {} ({} heights still buffered) — chunk needs retry",
            start_height,
            end_height,
            next_to_send,
            still
        ));
    }
    // Success path: no leftovers expected; free counter if any remain.
    received_drain_all(&mut received);

    Ok(DownloadChunkResult {
        blocks,
        streamed_block_count: if streaming { streamed_block_count } else { 0 },
    })
}
