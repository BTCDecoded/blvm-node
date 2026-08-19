/// After [`try_stream_validation_gap`] pushes tip (+ optional consecutive drain) to the
/// coordinator, advance the download cursor past everything already streamed.
///
/// Tip may still be clone-held in `received` (H5/A1); cursor must still advance past
/// `GAP_STREAM_DEDUP` so W101b tip-hole pipe caps disarm (`next_to_send` past tip).
///
/// Land E: cheese drain can leave DEDUP *ahead* of a still-missing validation gap
/// (STREAM H, drain H+1…, coordinator drops H). Jumping `next_to_send` to DEDUP+1
/// completed the 128-span empty (~170ms), cancelled GetData for H, and re-assigned
/// the same map chunk. When the gap is still missing, only advance when DEDUP is
/// exactly this cursor (we just streamed it).
fn sync_next_to_send_after_gap_stream(next_to_send: &mut u64, end_height: u64) {
    let dedup = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
    if dedup < *next_to_send {
        return;
    }
    let advanced = dedup.saturating_add(1).min(end_height.saturating_add(1));
    if advanced > *next_to_send {
        *next_to_send = advanced;
    }
}

/// Rewind [`GAP_STREAM_DEDUP_HEIGHT`] when cheese-ahead DEDUP covers a hole that is
/// not in `received`. W50 LOCAL_GAP wait never fills WAN-stripped heights.
///
/// Returns the rewind height when DEDUP moved. Caller must already know the tip is
/// not validation-taken / release-latched and not buffered.
#[allow(dead_code)] // kept for unit tests; a31 STREAM path does not call this
pub(crate) fn rewind_gap_stream_dedup_over_missing_hole(
    gap: u64,
    last_streamed: u64,
) -> Option<u64> {
    if last_streamed < gap {
        return None;
    }
    if !super::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed) {
        return None;
    }
    let rewind = gap.saturating_sub(1);
    match super::memory::GAP_STREAM_DEDUP_HEIGHT.compare_exchange(
        last_streamed,
        rewind,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => Some(rewind),
        Err(_) => None,
    }
}

/// When validation races ahead of this worker's sequential `next_to_send`, skip already-
/// validated heights so we fetch the live gap immediately (OOM wedge at h=499231: worker
/// held chunk 499201-499232 while validation sat at 499230 waiting for 499231).
/// W13: returns true when the entire chunk is behind the validation tip (obsolete).
/// Caller should flush and return Ok so the peer can take tip work.
fn resync_next_to_send_with_validation_tip(
    validation_height: Option<&Arc<AtomicU64>>,
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    next_to_send: &mut u64,
    end_height: u64,
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
    in_flight_heights: &HashSet<u64>,
    last_gap_at: &mut std::time::Instant,
) -> bool {
    let Some(vh) = validation_height else {
        return false;
    };
    let tip = vh.load(Ordering::Relaxed);
    let need = tip.saturating_add(1);
    // W13: whole chunk obsolete (common for stall-recovery (H,H) after tip advanced).
    // Old code returned early when need > end_height, leaving next_to_send stuck and
    // soft-retrying forever (live: 30 behind-tip soft retries at 6848xx).
    if tip >= end_height || need > end_height {
        for &h in in_flight_heights.iter() {
            if let Some(&hash) = block_hash_by_height.get(&h) {
                network.cancel_block_request(peer_addr, hash);
            }
        }
        // W55: do NOT `received.clear()` here — caller `flush_received_on_abort` must drain
        // first. Pre-W55 clear dropped hash-verified bodies; flush then saw an empty map
        // (silent re-download). Leave entries for flush; drop leftovers when chunk returns.
        *next_to_send = end_height.saturating_add(1);
        *last_gap_at = std::time::Instant::now();
        return true;
    }
    if need <= *next_to_send {
        return false;
    }
    while *next_to_send < need {
        let h = *next_to_send;
        // G5: preserve the validation gap block (and height `need`) in `received` so
        // `try_stream_validation_gap` can deliver it — silent remove caused multi-minute stalls.
        if h == need || (h == need.saturating_sub(1) && received.contains_key(&h)) {
            if in_flight_heights.contains(&h) {
                if let Some(&hash) = block_hash_by_height.get(&h) {
                    network.cancel_block_request(peer_addr, hash);
                }
            }
            *next_to_send += 1;
            continue;
        }
        let _ = received_take(received, h);
        if in_flight_heights.contains(&h) {
            if let Some(&hash) = block_hash_by_height.get(&h) {
                network.cancel_block_request(peer_addr, hash);
            }
        }
        *next_to_send += 1;
    }
    *last_gap_at = std::time::Instant::now();
    false
}

/// Stream the block validation is waiting on (`validation_height + 1`) as soon as it
/// arrives, even when earlier heights in this chunk are still in flight.  Without this,
/// in-order `next_to_send` streaming traps the gap block behind a slow first block in the
/// chunk — validation sits in a 30s feeder stall while witnesses for the gap height are
/// already on disk (observed h≈489536: chunk 489505-489536 batch-requested, gap waited 69s).
///
/// A6l: after streaming the gap, also drain any **consecutive** heights already buffered in
/// `received` into the feeder. Live A6k vs A6i: inter-GAP_STREAM burst rate (dt<50ms) was
/// **1% vs 8.8%** — out-of-order arrivals left tip+1..tip+N sitting in `received` until the
/// next network event called try_stream again (vh still points at the just-streamed gap, so
/// a single-shot try_stream cannot advance). Draining consecutive fills the feeder without
/// waiting for another block arrival.
///
/// **W100 (reverted 2026-07-18):** tip-only STREAM/RESEND (no consecutive drain) left
/// `GAP_DRAIN=0` but `bridge_min>nn` still ~79% and tip60 collapsed to slow-fail ~31 @330k.
/// Ahead fill is not drain-dominated; RESEND storm (tip lost after STREAM) remains. Restore
/// A6l drain for throughput.
async fn try_stream_validation_gap(
    validation_height: Option<&Arc<AtomicU64>>,
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
) -> Result<bool> {
    let Some(vh) = validation_height else {
        return Ok(false);
    };
    let gap = vh.load(Ordering::Relaxed).saturating_add(1);
    if gap < start_height || gap > end_height {
        return Ok(false);
    }
    // W27/A6l/W38: GAP_STREAM_DEDUP tracks the highest height pushed to the coordinator.
    // Consecutive drain can advance it far ahead of validation. If validation still needs
    // `gap` and we have that height buffered again (local reload / re-download), we MUST
    // re-send — skipping left live 2026-07-16 soft-resume frozen at h=440399 for >5 min
    // while download logged "local block height 440400" thousands of times with no
    // IBD_GAP_STREAM (DEDUP had advanced through chunk 440370-440433).
    let last_streamed = super::memory::GAP_STREAM_DEDUP_HEIGHT.load(Ordering::Relaxed);
    let Some(tx) = block_tx else {
        return Ok(false);
    };
    if gap <= last_streamed {
        // Already STREAM'd once. Skip if validation/latch already owns the tip — clone-held
        // bodies in `received` must not become a 750ms RESEND pump on the healthy path.
        if super::tip_stage::tip_taken_by_validation(gap)
            || super::tip_release::tip_release_holds(gap)
        {
            return Ok(false);
        }
        let Some((block, block_witnesses)) = received_clone(received, gap) else {
            // W50: tip DEDUP'd but not buffered — wait for LOCAL_GAP/reload; do not drain ahead.
            return Ok(false);
        };
        // W42b: at most one resend per height / 750ms (inject races used to hit 599k RESEND).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last_h = super::memory::GAP_STREAM_LAST_RESEND_HEIGHT.load(Ordering::Relaxed);
        let last_ms = super::memory::GAP_STREAM_LAST_RESEND_MS.load(Ordering::Relaxed);
        if gap == last_h && now_ms.saturating_sub(last_ms) < 750 {
            return Ok(false);
        }
        let tip_on_channel = stream_tip_to_coordinator(tx, gap, block, block_witnesses).await?;
        super::memory::GAP_STREAM_LAST_RESEND_HEIGHT.store(gap, Ordering::Relaxed);
        super::memory::GAP_STREAM_LAST_RESEND_MS.store(now_ms, Ordering::Relaxed);
        tracing::warn!(
            "[IBD_GAP_STREAM_RESEND] height {} (last_streamed={}) — tip still missing after prior drain",
            gap,
            last_streamed
        );
        // NOTE: Do NOT set SYNTH_FORCE_TIP_RELOAD here. Live 2026-07-23: RESEND→claim
        // clear every ~10s became the cliff cadence (~6.4 BPS from ignition).
        super::memory::bump_gap_stream_dedup(gap);
        // W6/W50: if tip was latched (channel full), do not drain ahead past a missing tip.
        if tip_on_channel {
            let drained_n =
                drain_consecutive_received_after(received, block_tx, gap, end_height).await?;
            if drained_n > 0 {
                tracing::warn!(
                    "[IBD_GAP_DRAIN] after={} drained={} through={} (chunk {}-{})",
                    gap,
                    drained_n,
                    gap.saturating_add(drained_n),
                    start_height,
                    end_height
                );
            }
        }
        return Ok(true);
    }
    // H5/A1 first STREAM: clone to coordinator; leave tip keyed in `received` until
    // validation advances (trim evicts `h < need`). Removes the reinject/RESEND tax when
    // the tip is dropped from reorder/bridge before apply.
    let Some((block, block_witnesses)) = received_clone(received, gap) else {
        return Ok(false);
    };
    let tip_on_channel = stream_tip_to_coordinator(tx, gap, block, block_witnesses).await?;
    super::memory::bump_gap_stream_dedup(gap);
    // W6/W50: latched tip is not yet in block_tx — do not push tip+N ahead of it.
    if tip_on_channel {
        let drained_n =
            drain_consecutive_received_after(received, block_tx, gap, end_height).await?;
        if drained_n > 0 {
            tracing::warn!(
                "[IBD_GAP_DRAIN] after={} drained={} through={} (chunk {}-{})",
                gap,
                drained_n,
                gap.saturating_add(drained_n),
                start_height,
                end_height
            );
        }
    }
    Ok(true)
}

/// Stream validation tip to the coordinator without stranding on admission backpressure.
///
/// Returns `true` when the tip is on `block_tx` (safe to consecutive-drain). Returns `false`
/// when W6 latched the tip because the channel was full (release-side; never `send.await`).
async fn stream_tip_to_coordinator(
    tx: &tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>,
    gap: u64,
    block: SharedBlock,
    witnesses: SharedWitnesses,
) -> Result<bool> {
    if !super::tip_release::release_side_drain_enabled() {
        if tx.send((gap, block, witnesses)).await.is_err() {
            return Err(anyhow::anyhow!(
                "block_tx closed during gap stream - chunk needs retry"
            ));
        }
        return Ok(true);
    }
    match tx.try_send((gap, block, witnesses)) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full((h, b, w))) => {
            let prev = super::tip_release::offer_tip_release(h, b, w);
            warn!(
                "[IBD_TIP_RELEASE_LATCH] h={} channel_full capacity={} prev_latched={:?} — release-side (no send.await)",
                h,
                tx.capacity(),
                prev
            );
            Ok(false)
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(anyhow::anyhow!(
            "block_tx closed during gap stream - chunk needs retry"
        )),
    }
}

/// Push tip+1, tip+2, … already in `received` into the feeder (stops at first hole).
/// Returns the number of consecutive heights drained.
async fn drain_consecutive_received_after(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    after_height: u64,
    end_height: u64,
) -> Result<u64> {
    let Some(tx) = block_tx else {
        return Ok(0);
    };
    let mut next = after_height.saturating_add(1);
    let mut drained = 0u64;
    // Prefer leaving headroom for tip; drain is tip+1… so always non-tip relative to
    // validation tip at stream time (after_height was just streamed as tip).
    while next <= end_height {
        let Some((block, block_witnesses)) = received_take(received, next) else {
            break;
        };
        // after_height was just streamed as tip; validation tip_need is still that height
        // until applied — treat drain heights as non-tip for reserve.
        await_block_tx_tip_reserve(tx, next, Some(after_height)).await;
        if tx.send((next, block, block_witnesses)).await.is_err() {
            return Err(anyhow::anyhow!(
                "block_tx closed during consecutive gap drain - chunk needs retry"
            ));
        }
        super::memory::bump_gap_stream_dedup(next);
        drained = drained.saturating_add(1);
        next = next.saturating_add(1);
    }
    Ok(drained)
}

/// Before aborting a chunk, stream every block already sitting in `received` to the coordinator.
///
/// Without this, blocks already downloaded and hash-verified are silently dropped at chunk abort.
/// The coordinator re-queues and re-downloads them from scratch — visible as repeated
/// `received=N` counts in hard-deadline log lines (e.g. `received=13, next_to_send=H`).
///
/// Drains tip-contiguous blocks from `received` into the coordinator on abort.
///
/// **W60:** only flush `[next_needed, next_needed+1, …]` while contiguous. Dumping sparse
/// ahead (old behavior) hole-fills OrderedReadyBridge — live W59 genesis: `FLUSH_ON_ABORT`
/// 37× by h≈6k with `bmin>nn+1` 71% while tip crawl stalled. Matches W50 GAP_STREAM policy.
/// Returns the number of blocks successfully flushed.
async fn flush_received_on_abort(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
    next_to_send: u64,
    validation_height: Option<&AtomicU64>,
) -> usize {
    let Some(tx) = block_tx else {
        return 0;
    };
    if received.is_empty() {
        return 0;
    }
    let buffered = received.len();
    let mut flushed = 0usize;
    if let Some(vh) = validation_height {
        let tip_needed = vh.load(Ordering::Relaxed).saturating_add(1);
        let min_h = received.keys().next().copied();
        let max_h = received.keys().next_back().copied();
        // W60b:
        // - tip in buffer → flush contiguous from tip (never sparse ahead past a hole)
        // - all buffered < tip → flush all (obsolete behind-tip; persist/reuse, no hole-fill)
        // - all buffered > tip → flush nothing (ahead-only would hole-fill the bridge)
        let ahead_only = min_h.is_some_and(|m| m > tip_needed);
        if ahead_only {
            let skipped = received.len();
            // Keep ahead bodies — clear() was the 409 leak (fetched at +113, discarded,
            // then empty 128 reassign ×35). Caller may still drop the map on abort.
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed 0 tip-contiguous block(s) (kept_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                start_height, end_height, skipped, buffered, next_to_send, tip_needed
            );
        } else if min_h.is_some_and(|m| m == tip_needed) || received.contains_key(&tip_needed) {
            let mut h = tip_needed;
            while let Some((block, witnesses)) = received_take(received, h) {
                if tx.send((h, block, witnesses)).await.is_err() {
                    break;
                }
                flushed += 1;
                h = h.saturating_add(1);
            }
            let skipped = received.len();
            received.clear();
            crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                .fetch_add(flushed as u64, Ordering::Relaxed);
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} tip-contiguous block(s) (skipped_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                start_height, end_height, flushed, skipped, buffered, next_to_send, tip_needed
            );
        } else {
            // Behind tip (max < tip) or tip hole with some behind — drain ascending behind/at tip only.
            let _ = max_h;
            while let Some((&h, _)) = received.iter().next() {
                if h > tip_needed {
                    break;
                }
                let Some((block, witnesses)) = received_take(received, h) else {
                    break;
                };
                if h < tip_needed {
                    // Behind tip: still deliver for store/inject; coordinator drops if obsolete.
                }
                if tx.send((h, block, witnesses)).await.is_err() {
                    break;
                }
                flushed += 1;
            }
            let skipped = received.len();
            received.clear();
            if flushed > 0 || skipped > 0 {
                crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                    .fetch_add(flushed as u64, Ordering::Relaxed);
                info!(
                    "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} block(s) (skipped_ahead={}, buffered={}, next_to_send={}, tip_needed={})",
                    start_height, end_height, flushed, skipped, buffered, next_to_send, tip_needed
                );
            }
        }
    } else {
        // No validation cursor — legacy ascending drain.
        while let Some((&h, _)) = received.iter().next() {
            let Some((block, witnesses)) = received_take(received, h) else {
                break;
            };
            if tx.send((h, block, witnesses)).await.is_err() {
                break;
            }
            flushed += 1;
        }
        if flushed > 0 {
            crate::node::parallel_ibd::memory::GAP_FLUSH_ON_ABORT_BLOCKS
                .fetch_add(flushed as u64, Ordering::Relaxed);
            info!(
                "[IBD_FLUSH_ON_ABORT] chunk {}-{}: flushed {} buffered block(s) to coordinator (next_to_send={})",
                start_height, end_height, flushed, next_to_send
            );
        }
    }
    flushed
}

/// Cancel in-flight GETDATA, flush buffered blocks, return outer-deadline Err (S1).
async fn abort_on_outer_deadline(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    block_tx: Option<&tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>>,
    start_height: u64,
    end_height: u64,
    next_to_send: u64,
    validation_height: Option<&AtomicU64>,
    in_flight_heights: &HashSet<u64>,
    block_hash_by_height: &BTreeMap<u64, [u8; 32]>,
    network: &NetworkManager,
    peer_addr: SocketAddr,
    peer_scorer: &crate::network::peer_scoring::PeerScorer,
    outer_secs: u64,
) -> Result<DownloadChunkResult> {
    warn!(
        "[IBD] chunk {}-{} cooperative outer deadline ({}s) — flushing {} buffered block(s) before abort",
        start_height,
        end_height,
        outer_secs,
        received.len()
    );
    peer_scorer.record_failure(peer_addr);
    flush_received_on_abort(
        received,
        block_tx,
        start_height,
        end_height,
        next_to_send,
        validation_height,
    )
    .await;
    received_drain_all(received);
    for &h in in_flight_heights {
        if let Some(&h_hash) = block_hash_by_height.get(&h) {
            network.cancel_block_request(peer_addr, h_hash);
        }
    }
    Err(anyhow::anyhow!(
        "Chunk {}-{}: outer deadline {}s",
        start_height,
        end_height,
        outer_secs
    ))
}

/// W35‴ soft outer: extend deadline while tip is still streaming (>1 blk/s or recent GAP_STREAM).
/// Live: 119s cap aborted sticky mid-pipe while GAP_STREAM was live (~2 blk/s).
pub(crate) fn should_extend_outer_while_streaming(
    last_gap_at: std::time::Instant,
    gap_streams: u64,
    chunk_started: std::time::Instant,
    extensions_used: u32,
) -> bool {
    const MAX_EXTENDS: u32 = 4; // +60s × 4 = +240s ceiling
    if extensions_used >= MAX_EXTENDS {
        return false;
    }
    // Never extend before the first tip GAP_STREAM (avoids padding hung first-block waits).
    if gap_streams == 0 {
        return false;
    }
    // Recent tip stream — peer is delivering.
    if last_gap_at.elapsed() < Duration::from_secs(8) {
        return true;
    }
    let secs = chunk_started.elapsed().as_secs_f64().max(1.0);
    gap_streams as f64 / secs >= 1.0
}

fn outer_extend_secs() -> u64 {
    std::env::var("BLVM_IBD_OUTER_EXTEND_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
        .clamp(30, 120)
}

/// Returns true if outer deadline was extended (caller should continue the download loop).
fn try_soft_extend_outer(
    outer_deadline: &mut Option<tokio::time::Instant>,
    outer_extends: &mut u32,
    outer_deadline_secs: &mut u64,
    last_gap_at: std::time::Instant,
    gap_streams: u64,
    chunk_start_time: std::time::Instant,
    start_height: u64,
    end_height: u64,
    peer_id: &str,
) -> bool {
    if !should_extend_outer_while_streaming(
        last_gap_at,
        gap_streams,
        chunk_start_time,
        *outer_extends,
    ) {
        return false;
    }
    let add = outer_extend_secs();
    *outer_extends = outer_extends.saturating_add(1);
    *outer_deadline_secs = outer_deadline_secs.saturating_add(add);
    *outer_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(add));
    warn!(
        "[IBD_OUTER_EXTEND] chunk {}-{} peer={} extend={}s total_budget={}s streams={} extends={}/4",
        start_height, end_height, peer_id, add, *outer_deadline_secs, gap_streams, *outer_extends
    );
    true
}

/// Sleep until cooperative outer deadline, or pending forever when unset.
async fn wait_cooperative_outer(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Max empty-witness re-requests per peer/chunk before abort (A6b).
/// Override via `BLVM_IBD_EMPTY_WITNESS_MAX` (default 4, clamp 2..=32).
pub(crate) fn empty_witness_hit_cap() -> u32 {
    std::env::var("BLVM_IBD_EMPTY_WITNESS_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
        .clamp(2, 32)
}

/// W10: heights beyond `validation_tip + band` are far-ahead (default band 128).
pub(crate) fn far_ahead_band() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_FAR_AHEAD_BAND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(128)
            .clamp(32, 1024)
    })
}

/// W10/W28/P1-T/W80/W89: per-block timeout when the chunk covers the validation tip gap
/// (default **12s**). Soft-retry re-requests in place; shorter tip timeout rotates dead
/// owners faster. Live 2026-07-14 mid-chain: default 25s left getdata→body at 46s.
/// Breakthrough peers finish ~6s (SLA notes); W76 tried default **5** and regresseds
/// pure-WAN tip60 ~40→~7 (CAP thrash / abort-before-delivery). W74/W76 kept **8**,
/// but loop-1 @313–320k had tip `getdata→body` p90≈**11.5s** with CAP=8 →
/// `soft exhausted, aborting` ~2/min (abort-before-delivery). Default **12** sits
/// just above that p90; floor **8** keeps early-height rotate snappy.
///
/// **W89/W89b/W93/W94:** shorter CAP (default **8s**, `BLVM_IBD_TIP_HOLE_GAP_TIMEOUT_SECS`)
/// when tip distress is proven, not on every holey poll.
/// Live W88 @328690: covering=2 / holes=23 waited full 12s → freeze 21s. Live W89:
/// standing ahead-OOO holes kept CAP=5 always → tip60 **69→27**. Trigger at 4s + holes
/// keeps healthy holey pipes on CAP=12 while rotating true tip holes before watcher 20s.
/// **W93:** hole CAP default **5→8**.
/// **W94:** live W93 freeze @314596 had **holes=0 / bridge_pending=0** (pure tip starve)
/// so W89b never shortened CAP — waited full 12s then still missed watcher 20s.
/// Empty-bridge tip gap (`BRIDGE_PENDING_COUNT==0` + `IBD_TIP_GAP_MISSING`) uses the
/// short CAP after a longer trigger (default **8s**) so healthy awaiting≈4–6s pipes
/// stay on base CAP=12.
pub(crate) fn tip_gap_timeout_secs() -> u64 {
    tip_gap_timeout_secs_ex(false)
}

/// Tip-gap CAP. `ahead_buffered` is accepted for call-site symmetry with soft budget
/// (W108/W109) but does **not** lengthen mute CAP — live W109 soak: elevating CAP on
/// ahead made soft-resume mute rotate sluggish; W109b keeps mute at **5s** and relies on
/// soft=1 (keep pipe / re-GetData tip) when ahead is already buffered.
///
/// Prefer [`tip_gap_timeout_secs_for_chunk`] when chunk span is known (W139: empty deep
/// uses 8s; empty `(H,H)` stays mute 5s).
pub(crate) fn tip_gap_timeout_secs_ex(_ahead_buffered: bool) -> u64 {
    tip_gap_timeout_secs_for_chunk(_ahead_buffered, 0, 0)
}

/// Tip-gap CAP with chunk span (W139).
///
/// Empty-bridge **deep** pipes use `holey_cap` (default **16s**, W184) — live W138
/// @316114: empty mute CAP=5s + soft=1 burned by progressive → mute abort before body.
/// Empty `(H,H)` micros stay on mute **5s**.
pub(crate) fn tip_gap_timeout_secs_for_chunk(
    ahead_buffered: bool,
    chunk_start: u64,
    chunk_end: u64,
) -> u64 {
    let base = latch_env!(u64, {
        std::env::var("BLVM_IBD_TIP_GAP_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            // W184: **12→16**. Live W182 354–362k getdata→body p90≈15.4s under CAP=12
            // → abort-before-delivery → covering=0 → rate-fail @361k.
            .unwrap_or(16)
            .clamp(8, 60)
    });
    let holes = crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
    // W103: hole/empty mute CAP default **5s**.
    let hole_cap = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLE_GAP_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5)
        });
        raw.clamp(3, base)
    };
    // W182: holey **8→12**. W184: **12→16** (same class — mid/late p90 above CAP).
    // Match base CAP when runway is already buffered (pending>0); empty mute stays 5s.
    let holey_cap = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLEY_PENDING_CAP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
        });
        raw.clamp(hole_cap, base)
    };
    let trigger = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_HOLE_CAP_TRIGGER_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
        });
        raw.clamp(2, hole_cap)
    };
    // W104: empty-bridge trigger default **0** (was 5).
    let empty_trigger = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_EMPTY_BRIDGE_CAP_TRIGGER_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        });
        raw.clamp(0, base)
    };
    let awaiting = super::tip_stage::tip_awaiting_secs_for_cap();
    let pending = crate::node::parallel_ibd::memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
    let gap_missing = crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
    let deep = chunk_end > chunk_start;
    // W109: ahead already buffered (this pipe OR coordinator reorder cheese) must not
    // wait holey 16s — that is soak 13/17 tip90≈50–93 (406–411 leftover). Soft-retry
    // re-GetData's tip H in place (W72/W108). Empty deep with no cheese stays 16s (W184).
    let cheese =
        ahead_buffered || crate::node::parallel_ibd::IBD_REORDER_AHEAD.load(Ordering::Relaxed) > 0;
    if holes > 0 {
        // W89b: standing holes without await keep base (do not fall through to empty).
        if awaiting >= trigger {
            // W170: single tip hole with runway already buffered (= empty-hole class).
            // Live W169 @317772: holes=1 pending=61 used holey CAP 8s + mid SLA 90s;
            // mute CAP 5s matches empty tip hole and pairs with floor-SLA rotate.
            if holes == 1 && pending > 0 {
                return hole_cap;
            }
            // W106/W184: holes + bridge pending → holey CAP (default 16s); empty tip
            // hole keeps mute CAP 5s. W176 export floor is a no-op when holey==base.
            if pending > 0 && !cheese {
                return tip_cap_during_export(holey_cap, base);
            }
            return tip_cap_during_export(hole_cap, base);
        }
        return base;
    }
    if gap_missing && pending == 0 && awaiting >= empty_trigger {
        // W139/W184: empty deep → holey CAP (16s) only when no cheese. TIP_HOLE_AHEAD
        // and (H,H) stay mute 5s (W109).
        return if deep && !cheese {
            tip_cap_during_export(holey_cap, base)
        } else {
            tip_cap_during_export(hole_cap, base)
        };
    }
    base
}

/// W176: while checkpoint export holds the compact lock, tip bodies often land after
/// the steady-state holey CAP. Floor tracks holey default (**16s**, W184).
///
/// W179b: do **not** apply this floor during post-local-ahead grace. Live W179 rewind
/// soak: grace+CAP12 held dead tip peers → tip60 stuck ~15–22 while sla5=0. Grace is
/// for tip-SLA (don't 5s-kill cold scores); CAP must still mute-rotate slow peers.
#[inline]
fn tip_cap_during_export(cap: u64, base: u64) -> u64 {
    if !crate::node::parallel_ibd::IBD_CHECKPOINT_EXPORT_ACTIVE.load(Ordering::Relaxed) {
        return cap;
    }
    let floor = {
        let raw = latch_env!(u64, {
            std::env::var("BLVM_IBD_TIP_EXPORT_ACTIVE_CAP_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16)
        });
        raw.clamp(8, base)
    };
    cap.max(floor).min(base)
}

/// W10: per-block timeout for far-ahead chunks (default 10s) — abort fast to free the peer.
pub(crate) fn far_ahead_timeout_secs() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_FAR_AHEAD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10)
            .clamp(5, 30)
    })
}

/// W24/W28/P1-T: soft-retry budget for the exact validation tip gap (default **3**).
/// Deep WAN tip owner pipes need several in-place re-requests before peer rotate.
pub(crate) fn tip_gap_soft_retries() -> u32 {
    latch_env!(u32, {
        std::env::var("BLVM_IBD_TIP_GAP_SOFT_RETRIES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
            .clamp(1, 12)
    })
}

/// W2/N11: Hornet-style download **byte** budget (in addition to block-count caps).
///
/// `BLVM_IBD_DOWNLOAD_BYTE_BUDGET=0` / unset → off (W1b DNA). When set (bytes),
/// GetData fill depth tracks `budget / est_block_bytes` so small-block eras deepen
/// and fat-block eras stay memory-bounded. Clamp 8 MiB..=256 MiB.
pub(crate) fn download_byte_budget() -> Option<u64> {
    let raw = std::env::var("BLVM_IBD_DOWNLOAD_BYTE_BUDGET").ok()?;
    let n: u64 = raw.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(n.clamp(8 * 1024 * 1024, 256 * 1024 * 1024))
}

/// Running estimate of serialized block size for W2 fill-depth (default 1 MiB).
fn download_est_block_bytes() -> u64 {
    static EST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    EST.load(std::sync::atomic::Ordering::Relaxed).max(50_000)
}

fn note_download_block_bytes(nbytes: u64) {
    if nbytes == 0 {
        return;
    }
    static EST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1_000_000);
    let old = EST.load(std::sync::atomic::Ordering::Relaxed).max(50_000);
    // EMA: 7/8 old + 1/8 new
    let next = old.saturating_mul(7).saturating_add(nbytes) / 8;
    EST.store(next.max(50_000), std::sync::atomic::Ordering::Relaxed);
}

/// W10/W12/W14/W24: soft-retry budget for a chunk-local gap height relative to the live tip.
/// - Behind tip (`height < tip+1`): **0** (obsolete — abort; live: 30/43 soft retries)
/// - Far ahead of tip: **0** (abort on first timeout — free peer for tip work)
/// - Exact tip gap: **tip_gap_soft_retries()** (default 3 — rotate after soft retries exhausted)
/// - Otherwise: **3** (existing P4)
///
/// Prefer [`gap_soft_retry_budget_for_chunk`] at the download soft-retry site so tip-gap
/// heights rotate after one soft miss (W69/W70).
#[allow(clippy::if_same_then_else)] // behind-tip and far-ahead are both 0; keep the cases distinct
pub(crate) fn gap_soft_retry_budget(height: u64, validation_tip: u64) -> u32 {
    let tip_needed = validation_tip.saturating_add(1);
    if height < tip_needed {
        0
    } else if height > tip_needed.saturating_add(far_ahead_band()) {
        0
    } else if height == tip_needed {
        tip_gap_soft_retries()
    } else {
        3
    }
}

/// Soft-retry budget with chunk span awareness (W69/W70/W74/W83/W107/W108/W138).
///
/// **Tip-gap height** (`height == tip+1`):
/// - Deep tip pipe (`chunk_end > chunk_start`): **1** (W83/W138) — one in-place soft then rotate
///   (empty bridge included; W107 soft=0 mute-stormed @310k before STREAM EWMA).
/// - `(H,H)` failover micro: **0** unless hot tip-STREAM (W115) — abort/rotate (W74).
/// - Empty bridge + ahead already buffered in this pipe: **1** (W108) — re-GetData tip,
///   keep ahead `received` (live W107 @355392: abort discarded 25 ahead × peer rotate).
///
/// Prefer [`gap_soft_retry_budget_for_chunk_ex`] at download sites that have `received`.
pub(crate) fn gap_soft_retry_budget_for_chunk(
    height: u64,
    validation_tip: u64,
    chunk_start: u64,
    chunk_end: u64,
) -> u32 {
    gap_soft_retry_budget_for_chunk_ex(height, validation_tip, chunk_start, chunk_end, false, false)
}

/// Like [`gap_soft_retry_budget_for_chunk`] with pipe-local ahead visibility (W108)
/// and hot tip-STREAM tenure (W114).
pub(crate) fn gap_soft_retry_budget_for_chunk_ex(
    height: u64,
    validation_tip: u64,
    chunk_start: u64,
    chunk_end: u64,
    ahead_buffered: bool,
    hot_tip_streamer: bool,
) -> u32 {
    let tip_needed = validation_tip.saturating_add(1);
    if height == tip_needed {
        let pending =
            crate::node::parallel_ibd::memory::BRIDGE_PENDING_COUNT.load(Ordering::Relaxed);
        let gap_missing = crate::node::parallel_ibd::IBD_TIP_GAP_MISSING.load(Ordering::Relaxed);
        if gap_missing && pending == 0 {
            // W158: empty deep soft=**2** always (revert W156/W157 awaiting soft=1).
            // W157@16s reached 338k peak tip60 99 — knife-edge vs W154@346k/89; soft=1
            // awaiting path not a clear win. Keep W146 empty (H,H) soft=2.
            if chunk_end > chunk_start {
                let _ = (ahead_buffered, hot_tip_streamer);
                return 2;
            }
            // W146: empty (H,H) soft=2. Soft=0 mute micro-rotate was W108.
            let _ = (ahead_buffered, hot_tip_streamer);
            return 2;
        }
        if chunk_end > chunk_start {
            // W171: soft=1 @holes≥20 only when bridge pending==0 (no runway). Live W170
            // @319140–154: holes=20–31 pending=33–47 soft=1 → (H,H) tip-step every ~2s
            // covering=2/2 → tip60~14 → rate-fail @321k. Keep soft=1 for holes≥32 always
            // (deep standing). W170 floor-SLA ≤0.20 + holes=1 CAP kept.
            let _ = (ahead_buffered, hot_tip_streamer);
            let holes = crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
            if holes >= 32 || (holes >= 20 && pending == 0) {
                return 1;
            }
            return 2;
        }
        // W171: holey (H,H) — same pending gate as deep.
        let _ = (hot_tip_streamer, ahead_buffered);
        let holes = crate::node::parallel_ibd::IBD_TIP_BRIDGE_HOLES.load(Ordering::Relaxed);
        if holes >= 32 || (holes >= 20 && pending == 0) {
            return 1;
        }
        return 2;
    }
    let _ = (chunk_start, chunk_end, ahead_buffered, hot_tip_streamer);
    gap_soft_retry_budget(height, validation_tip)
}

/// True when this download pipe has buffered any height strictly above the tip gap.
pub(crate) fn tip_pipe_has_ahead_buffered(
    received: &std::collections::BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    tip_needed: u64,
) -> bool {
    received.keys().any(|&h| h > tip_needed)
}

/// W110: tip-covering download failures that should use mute cooldown + failover arm.
///
/// Live W109b @311585: empty soft=0 CAP and "no first block" aborts returned
/// `Block timeout for gap height…` / `no first block in…` — worker only matched
/// `tip-gap timeout` for [`crate::node::parallel_ibd::chunk_assigner::ChunkAssigner::note_tip_owner_failed_mute`].
/// Plain tip fails used 15s cooldown and **skipped** WAN failover arm (W31) →
/// covering=1 mute thrash (~26s) until watcher rate-fail (32.4 < 35).
pub(crate) fn tip_covering_fail_is_mute(err_str: &str) -> bool {
    if err_str.contains("tip-SLA") || err_str.contains("tip-enter walk-in") {
        return false;
    }
    err_str.contains("tip-gap timeout")
        || err_str.contains("Block timeout for gap height")
        || err_str.contains("no first block in")
        // P1d: PIPE_FILL recv=0 mute eject (CAP-independent).
        || err_str.contains("PIPE_FILL mute")
}

/// P1d: after tip pipe is filled with network GetData and no network body arrives, abort.
/// Default **3000** ms. Env `BLVM_IBD_MUTE_PIPE_MS` (clamp 1000–15000).
pub(crate) fn mute_pipe_ms() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_MUTE_PIPE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3000)
            .clamp(1000, 15_000)
    })
}

/// P1d clock policy (unit-tested): keep across local tip advances; clear only on tip-band
/// network body or when the mute episode ends.
pub(crate) fn pipe_mute_episode_active(
    gap_streams: u64,
    pipe_fill_recv0: bool,
    saw_network_body: bool,
    next_in_chunk: bool,
) -> bool {
    (gap_streams > 0 || pipe_fill_recv0) && !saw_network_body && next_in_chunk
}

/// Whether to clear `pipe_mute_armed_at` when the episode is not active.
pub(crate) fn pipe_mute_should_clear_clock(
    episode_active: bool,
    gap_streams: u64,
    pipe_fill_recv0: bool,
    saw_network_body: bool,
) -> bool {
    !episode_active && (saw_network_body || !(gap_streams > 0 || pipe_fill_recv0))
}

/// Whether mute may *fire* (clock elapsed) — defer while tip is already arriving.
pub(crate) fn pipe_mute_may_fire(tip_buffered: bool, tip_is_local_inflight: bool) -> bool {
    !(tip_buffered || tip_is_local_inflight)
}

/// C1: grow tip-hole GetData depth only as network bodies arrive (inverse mute).
/// Default **on**. Opt out: `BLVM_IBD_TIP_HOLE_GROW=0`.
pub(crate) fn tip_hole_grow_enabled() -> bool {
    super::policy::tip_hole_grow()
}

/// Cap for tip-hole pipe (KEEP 128).
pub(crate) fn tip_hole_pipe_cap() -> usize {
    super::policy::tip_hole_pipe()
}

/// C1b: cold max depth under grow-on-delivery (default **32**).
/// Soft peer-day: DNA `TIP_HOLE_PIPE=128` + grow→128 recreated Swiss cheese (iter10k wall≈7.5).
/// Mode T can raise via `BLVM_IBD_TIP_HOLE_GROW_CAP=128`. Clamp 2–pipe_cap.
/// C1n may temporarily raise the *effective* cap when getdata→body EWMA is fast.
pub(crate) fn tip_hole_grow_cap() -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    super::policy::tip_hole_grow_cap_raw()
        .clamp(2, 128)
        .min(tip_hole_pipe_cap())
}

/// C1n: allow deepen past cold grow_cap only while getdata→body EWMA is fast.
/// C1m always-cap64 REGRESS on mute peerday; warm128 REGRESS on public WAN.
/// Default **on** — mute days stay at cold 32 (ewma ≥ gate or cold samples).
pub(crate) fn tip_hole_gd_fast_enabled() -> bool {
    super::policy::tip_hole_gd_fast()
}

/// C1n/C1p: getdata→body EWMA must be **below** this (ms) to use fast cap (default **150**).
/// C1o@64 with gate=200 false-armed on mute day (EWMA briefly 196 while gd_p50≈2.2s).
pub(crate) fn tip_hole_gd_fast_ms() -> u64 {
    super::policy::tip_hole_gd_fast_ms()
}

/// C1p: min EWMA samples before tip-hole may elevate (default **16**, match A6m).
/// C1o early n=8 let a short delivery burst arm FAST_CAP on a mute peerday.
pub(crate) fn tip_hole_gd_fast_n() -> u64 {
    super::policy::tip_hole_gd_fast_n()
}

/// C1n KEEP: fast grow cap when EWMA qualifies (default **48**).
/// C1n@48: wall≈294 / bursts~500 on good peerday. C1o@64 REGRESS on soft peerday
/// (false-positive EWMA arm). Stay at 48 on public WAN; Mode T may override.
pub(crate) fn tip_hole_grow_fast_cap() -> usize {
    let cold = tip_hole_grow_cap();
    super::policy::tip_hole_grow_fast_cap_raw()
        .clamp(cold, 96)
        .min(tip_hole_pipe_cap())
        .max(cold)
}

/// Min tracked tip height before GD_FAST may elevate tip-hole cap (default **0**).
/// KEEP opened FAST @~405825 after surviving 403–405k at depth≤32. Rematch with
/// early FAST (tc293/298) dens-peaks then cliffs @~403.7k. Set e.g. **405000**.
pub(crate) fn tip_hole_gd_fast_min_h() -> u64 {
    super::policy::tip_hole_gd_fast_min_h()
}

/// C1n/C1o: cold grow_cap, or fast_cap when getdata EWMA is warm+fast.
pub(crate) fn tip_hole_grow_cap_effective() -> usize {
    let cold = tip_hole_grow_cap();
    if !tip_hole_grow_enabled() || !tip_hole_gd_fast_enabled() {
        return cold;
    }
    let min_h = tip_hole_gd_fast_min_h();
    if min_h > 0 && super::tip_stage::tracked_tip_height() < min_h {
        return cold;
    }
    let fast = tip_hole_grow_fast_cap();
    if fast <= cold {
        return cold;
    }
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_fast_n()) {
        Some((ms, _n)) if ms < tip_hole_gd_fast_ms() => fast,
        _ => cold,
    }
}

/// C1u: freeze tip-hole deepen + clamp fill while getdata→body EWMA is slow.
/// Default **on**. Post-DEDUP GPU ignition cliffs: grow 8→32 continued at
/// `gd_ewma≈5–8s` → `in_flight≈32` + ~10 BPS (`depth/gd`). Soft EWMA dips must
/// not thrash sticky (C1s); this gate matches A6m GD_SLOW (**800** ms).
pub(crate) fn tip_hole_gd_slow_enabled() -> bool {
    super::policy::tip_hole_gd_slow()
}

/// C1u: EWMA ≥ this (ms) → slow clamp (default **800**, same as `A6M_MAX_GETDATA_MS`).
pub(crate) fn tip_hole_gd_slow_ms() -> u64 {
    super::policy::tip_hole_gd_slow_ms()
}

/// C1u: min EWMA samples before slow clamp (default **16**).
pub(crate) fn tip_hole_gd_slow_n() -> u64 {
    super::policy::tip_hole_gd_slow_n()
}

/// C1u: tip-hole fill / grown ceiling while GD_SLOW (default = grow_start **8**).
pub(crate) fn tip_hole_slow_fill_cap() -> usize {
    super::policy::tip_hole_slow_fill_cap_raw()
        .unwrap_or_else(tip_hole_grow_start)
        .clamp(2, tip_hole_grow_cap())
}

/// C1u′ ratchet (opt-in): step tip-hole depth down while GD_SLOW instead of cliff
/// `32→8`. Soak `T172520Z` with default-on + fill-clamp removed: tip60 FAIL @480k,
/// past-body **71.8** &lt; prior **81.1** — REVERT default. Opt in:
/// `BLVM_IBD_TIP_HOLE_GD_SLOW_RATCHET=1` (fill-time slow clamp stays on).
pub(crate) fn tip_hole_gd_slow_ratchet_enabled() -> bool {
    super::policy::tip_hole_gd_slow_ratchet()
}

/// Next tip-hole grown under GD_SLOW (ratchet or legacy cliff).
/// TPP L0b REVERT (300→320 L0b-20260801T032132Z): wall 378&lt;C0 390 — manual undo.
pub(crate) fn tip_hole_gd_slow_next_depth(grown: usize) -> usize {
    let slow = tip_hole_slow_fill_cap();
    if grown <= slow {
        return grown;
    }
    if !tip_hole_gd_slow_ratchet_enabled() {
        return slow;
    }
    grown.saturating_sub(tip_hole_grow_step()).max(slow)
}

/// Mode T sole archive: GD_SLOW arms the sole-floor path (see
/// [`tip_hole_sole_gd_slow_floor`]) instead of the multi-peer 32→8 cliff.
pub(crate) fn tip_hole_gd_slow_sole_keep(ibd_ready: usize) -> bool {
    ibd_ready <= 1 && tip_hole_gd_slow()
}

/// Sole-peer GD_SLOW depth floor (default **16**). Full keep-at-32 floods a slow
/// archive (tc105+ tip30≈3–20); cliff-to-8 starves. Env: `BLVM_IBD_TIP_HOLE_SOLE_FLOOR`.
pub(crate) fn tip_hole_sole_gd_slow_floor() -> usize {
    super::policy::tip_hole_sole_floor().clamp(tip_hole_slow_fill_cap(), tip_hole_grow_cap())
}

/// After sole GD_SLOW floor, require EWMA below this (ms) before deepen above floor.
/// Default = **gd-slow gate** (800): once not slow, allow cold CAP deepen.
/// FAST stays blocked separately by [`tip_hole_sole_no_fast_active`] until gd-fast.
/// Override lower only if you want a stricter floor hold (≤ gd-slow).
pub(crate) fn tip_hole_sole_floor_recover_ms() -> u64 {
    super::policy::tip_hole_sole_floor_recover_ms_raw()
        .unwrap_or_else(tip_hole_gd_slow_ms)
        .clamp(50, tip_hole_gd_slow_ms())
}

/// True while sole-floor latch is armed and EWMA has not left the slow band.
/// Clears when EWMA &lt; recover_ms (default = gd-slow). Mid-band (e.g. 359–765)
/// may then grow to cold CAP; [`tip_hole_sole_no_fast_active`] still blocks FAST.
pub(crate) fn tip_hole_sole_floor_blocks_grow() -> bool {
    if !super::tip_stage::sole_floor_latched() {
        return false;
    }
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_slow_n()) {
        Some((ms, _)) if ms < tip_hole_sole_floor_recover_ms() => {
            super::tip_stage::clear_sole_floor_latch();
            false
        }
        _ => true,
    }
}

/// Consecutive tip-hole checks with EWMA &lt; gd-fast required to clear no-FAST.
/// Default **16** (match gd-fast N). tc152: single blip &lt;150ms re-armed FAST_CAP.
pub(crate) fn tip_hole_sole_no_fast_clear_n() -> u32 {
    super::policy::tip_hole_sole_no_fast_clear_n()
}

/// Min wall-ms no-FAST stays armed after sole GD_SLOW before streak clear may fire.
/// Default **120_000** (covers a tip90 cell). tc155: 10s hold expired → FAST @+10.2s
/// → dens tip30 then gd_max≈5s cheese. Re-arming each sole floor resets this clock.
pub(crate) fn tip_hole_sole_no_fast_min_hold_ms() -> u64 {
    // Default **120s** covers tip90 cell. tc173 tried 15s → FAST re-armed mid-window
    // (grown=64) and tip90 fell to ≈28.9. Keep FAST blocked after sole floor.
    super::policy::tip_hole_sole_no_fast_min_hold_ms()
}

/// Min height before sole GD_SLOW arms the no-FAST latch (default **0** = always).
/// Rematch: READY-crawl SOLE_FLOOR @~400.3–401.0k re-armed 30s hold and blocked
/// dens FAST_CAP 64 (KEEP opened FAST @~405.8k). Set e.g. **401000** so cold
/// crawl still floors depth but does not block later dens deepen.
pub(crate) fn tip_hole_sole_no_fast_arm_min_h() -> u64 {
    super::policy::tip_hole_sole_no_fast_arm_min_h()
}

fn maybe_note_sole_no_fast_latch(height: u64) {
    let min_h = tip_hole_sole_no_fast_arm_min_h();
    if min_h > 0 && height < min_h {
        return;
    }
    super::tip_stage::note_sole_no_fast_latch();
}

/// Skip GD_SLOW sole-floor (32→16) at/after this height. Default **0** = KEEP
/// (always apply). Cold READY still needs the floor (tc105 / #8 SOLE_FLOOR=32
/// flood). Dens death at 406787 is a poisoned getdata→body EWMA on a still-fed
/// pipe — do not re-clamp there. Env: `BLVM_IBD_TIP_HOLE_SOLE_FLOOR_MAX_H`.
pub(crate) fn tip_hole_sole_floor_max_h() -> u64 {
    super::policy::tip_hole_sole_floor_max_h()
}

/// Whether sole-floor / floor-hold may run at `height`. Crossing max_h clears
/// a leftover 400.3k latch so HOLD cannot 32→16 on the dens pipe.
pub(crate) fn tip_hole_sole_floor_applies(height: u64) -> bool {
    let max_h = tip_hole_sole_floor_max_h();
    if max_h == 0 {
        return true;
    }
    if height >= max_h {
        if super::tip_stage::sole_floor_latched() {
            super::tip_stage::clear_sole_floor_latch();
        }
        return false;
    }
    true
}

/// True while sole no-FAST latch is armed and getdata is not *sustainably* gd-fast.
/// Clears only after min wall hold **and** [`tip_hole_sole_no_fast_clear_n`]
/// consecutive healthy checks (signal-only; no height cliff).
pub(crate) fn tip_hole_sole_no_fast_active() -> bool {
    if !super::tip_stage::sole_no_fast_latched() {
        return false;
    }
    let held_long_enough = super::tip_stage::sole_no_fast_armed_age_ms()
        .map(|age| age >= tip_hole_sole_no_fast_min_hold_ms())
        .unwrap_or(false);
    match super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_fast_n()) {
        Some((ms, _)) if ms < tip_hole_gd_fast_ms() => {
            if !held_long_enough {
                // Still inside hold — do not accumulate clear streak yet.
                let _ = super::tip_stage::sole_no_fast_note_clear_sample(false);
                return true;
            }
            let streak = super::tip_stage::sole_no_fast_note_clear_sample(true);
            if streak >= tip_hole_sole_no_fast_clear_n() {
                super::tip_stage::clear_sole_no_fast_latch();
                false
            } else {
                true
            }
        }
        _ => {
            let _ = super::tip_stage::sole_no_fast_note_clear_sample(false);
            true
        }
    }
}

/// Sole peer: while no-FAST latch is active, clamp tip-hole cap to cold GROW_CAP
/// so GD_SLOW → brief not-slow flicker cannot re-arm FAST_CAP.
pub(crate) fn tip_hole_cap_for_sole(sole_ready: bool, cap: usize) -> usize {
    if sole_ready && tip_hole_sole_no_fast_active() {
        cap.min(tip_hole_grow_cap())
    } else {
        cap
    }
}

/// C1u: true when grow is enabled and getdata→body EWMA is warm+slow.
pub(crate) fn tip_hole_gd_slow() -> bool {
    if !tip_hole_grow_enabled() || !tip_hole_gd_slow_enabled() {
        return false;
    }
    matches!(
        super::tip_stage::getdata_body_ewma_ms_min_n(tip_hole_gd_slow_n()),
        Some((ms, _n)) if ms >= tip_hole_gd_slow_ms()
    )
}

/// C1d: warm peers (hot tip streamer) may deepen to pipe cap (default **off**).
/// iter10k: warm→128 regressed wall≈22 &lt; C1b floor 40 (`tip_hole_grown_p50=128`).
/// Opt in: `BLVM_IBD_TIP_HOLE_WARM=1` (Mode T / serving peer only).
pub(crate) fn tip_hole_warm_enabled() -> bool {
    super::policy::tip_hole_warm()
}

fn tip_hole_warm_cap_raw() -> usize {
    super::policy::tip_hole_warm_cap_raw()
        .unwrap_or_else(tip_hole_pipe_cap)
        .clamp(2, 128)
        .min(tip_hole_pipe_cap())
}

pub(crate) fn tip_hole_grow_cap_for_peer(hot_tip_streamer: bool) -> usize {
    // C1n: base is gd-gated effective (cold 32, or 48 when EWMA fast).
    let cold = tip_hole_grow_cap_effective();
    if !tip_hole_grow_enabled() || !tip_hole_warm_enabled() || !hot_tip_streamer {
        return cold;
    }
    tip_hole_warm_cap_raw()
        .clamp(cold, 128)
        .min(tip_hole_pipe_cap())
        .max(cold)
}

/// C1: initial tip-hole depth when grow-on-delivery is on (default **8**).
pub(crate) fn tip_hole_grow_start() -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    super::policy::tip_hole_grow_start()
        .clamp(2, 32)
        .min(tip_hole_grow_cap())
}

/// C1/C1r: deepen by this many slots per tip-band network body (default **8**).
/// C1r: while gd-fast effective cap is elevated, step at least **16** (bake often
/// sets `GROW_STEP=8` — that must not disable the fast step).
pub(crate) fn tip_hole_grow_step() -> usize {
    let configured = super::policy::tip_hole_grow_step();
    if tip_hole_gd_fast_enabled()
        && tip_hole_grow_enabled()
        && tip_hole_grow_cap_effective() > tip_hole_grow_cap()
    {
        return configured.max(16);
    }
    configured
}

/// Absolute sticky ceiling — independent of live EWMA (C1s).
/// Fill still mins against `tip_hole_grow_cap_effective()` each request.
pub(crate) fn tip_hole_sticky_abs_cap(hot_tip_streamer: bool) -> usize {
    let cold = tip_hole_grow_cap();
    let mut cap = if tip_hole_gd_fast_enabled() {
        tip_hole_grow_fast_cap().max(cold)
    } else {
        cold
    };
    if tip_hole_warm_enabled() && hot_tip_streamer {
        let warm = tip_hole_warm_cap_raw()
            .clamp(cap, 128)
            .min(tip_hole_pipe_cap());
        cap = cap.max(warm);
    }
    cap.min(tip_hole_pipe_cap())
}

/// C1/C1n: deepen tip-hole fill after tip-gap network body; returns new depth.
pub(crate) fn tip_hole_grow_on_delivery(current: usize) -> usize {
    tip_hole_grow_on_delivery_capped(current, tip_hole_grow_cap_effective())
}

pub(crate) fn tip_hole_grow_on_delivery_capped(current: usize, cap: usize) -> usize {
    if !tip_hole_grow_enabled() {
        return tip_hole_pipe_cap();
    }
    current.saturating_add(tip_hole_grow_step()).min(cap)
}

/// W10: pick per-block timeout from chunk position vs validation tip.
pub(crate) fn gap_timeout_for_chunk(
    start_height: u64,
    end_height: u64,
    validation_tip: u64,
    default_secs: u64,
) -> u64 {
    let tip_needed = validation_tip.saturating_add(1);
    if start_height <= tip_needed && tip_needed <= end_height {
        // W139: empty deep 8s / empty (H,H) 5s — match CAP.
        tip_gap_timeout_secs_for_chunk(false, start_height, end_height)
    } else if start_height > tip_needed.saturating_add(far_ahead_band()) {
        far_ahead_timeout_secs()
    } else {
        default_secs
    }
}

/// W32d″: tiered per-position timeout on WAN deep tip pipe.
///
/// Tip gap stays short (soft-retry / rotate). Mid/deep must be **≥ tip soft-retry window**
/// so ahead GetData does not expire while tip is soft-retrying.
/// Live W32d′ regression (~13→4.7 blk/s): short 8/12s mid/deep + park emptied the pipe.
/// Defaults: tip=`tip_gap_timeout_secs()`, +1..31 → **30s**, +32.. → **45s**.
pub(crate) fn wan_deep_pipe_timeout_secs(
    height: u64,
    validation_tip: u64,
    confirmed_body_height: u64,
) -> Option<u64> {
    if confirmed_body_height == 0 || height <= confirmed_body_height {
        return None;
    }
    let tip_needed = validation_tip.saturating_add(1);
    if height < tip_needed {
        return None;
    }
    let offset = height.saturating_sub(tip_needed);
    let secs = match offset {
        0 => tip_gap_timeout_secs(),
        1..=31 => latch_env!(u64, {
            std::env::var("BLVM_IBD_PIPE_MID_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30)
                .clamp(15, 60)
        }),
        _ => latch_env!(u64, {
            std::env::var("BLVM_IBD_PIPE_DEEP_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(45)
                .clamp(25, 90)
        }),
    };
    Some(secs)
}

/// Per-height gap timeout: W32d WAN pipe tiers override chunk-level defaults.
pub(crate) fn block_gap_timeout_secs(
    height: u64,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_start: u64,
    chunk_end: u64,
    default_secs: u64,
) -> u64 {
    wan_deep_pipe_timeout_secs(height, validation_tip, confirmed_body_height).unwrap_or_else(|| {
        gap_timeout_for_chunk(chunk_start, chunk_end, validation_tip, default_secs)
    })
}

/// W71: true when an in-flight tip-gap GetData has waited ≥ tip-gap timeout.
///
/// Tokio timeouts were historically baked at enqueue (`timeout(secs, rx)`). Mid/deep pipe
/// tiers use 30–45s, so when the validation tip walks into a previously-ahead height the
/// in-flight future kept waiting the long timeout. Live 2026-07-17 tip=322456: soft-retry
/// logged `after 45s (limit 10s)`. W72 uses a shorten-able deadline atomic instead.
pub(crate) fn tip_gap_inflight_exceeded(started: Instant, tip_gap_secs: u64) -> bool {
    started.elapsed().as_secs() >= tip_gap_secs.max(1)
}

/// W85: when validation tip first walks onto `tip_needed`, reset that height's CAP
/// clock so the 12s budget measures **post-roll** wait only.
///
/// Deep tip pipes GetData heights seconds before they become the tip (`need→getdata`
/// often largely negative). Without rebase, CAP=12s expires from pre-roll age and
/// soft-retries / rotates peers that would deliver within true tip SLA.
pub(crate) fn rebase_tip_cap_clock(
    tip_needed: u64,
    tip_cap_clock_h: &mut Option<u64>,
    in_flight_heights: &HashSet<u64>,
    inflight_started: &mut HashMap<u64, Instant>,
) -> bool {
    if Some(tip_needed) == *tip_cap_clock_h {
        return false;
    }
    *tip_cap_clock_h = Some(tip_needed);
    if !in_flight_heights.contains(&tip_needed) {
        return false;
    }
    if let Some(started) = inflight_started.get_mut(&tip_needed) {
        *started = Instant::now();
        return true;
    }
    false
}

fn wall_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Keep `inflight_started` / deadline map aligned with `in_flight_heights`.
fn sync_inflight_started(
    inflight_started: &mut HashMap<u64, Instant>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    in_flight_heights: &HashSet<u64>,
) {
    inflight_started.retain(|h, _| in_flight_heights.contains(h));
    inflight_deadlines.retain(|h, _| in_flight_heights.contains(h));
    for &h in in_flight_heights {
        inflight_started.entry(h).or_insert_with(Instant::now);
    }
}

/// Await a block oneshot until the shared deadline (ms since epoch) elapses.
///
/// W72: tip-cap can store `now` into `deadline_ms` to force a tip-gap timeout without
/// aborting the whole deep pipe (in-place soft-retry keeps ahead `received`).
async fn await_block_with_deadline(
    mut rx: tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>,
    deadline_ms: Arc<AtomicU64>,
) -> Result<
    Result<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>), tokio::sync::oneshot::error::RecvError>,
    tokio::time::error::Elapsed,
> {
    loop {
        let now = wall_ms_now();
        let dl = deadline_ms.load(Ordering::Relaxed);
        if now >= dl {
            return Err(timeout(Duration::ZERO, std::future::pending::<()>())
                .await
                .unwrap_err());
        }
        let slice = Duration::from_millis((dl - now).min(500));
        tokio::select! {
            biased;
            r = &mut rx => return Ok(r),
            _ = tokio::time::sleep(slice) => {}
        }
    }
}

fn push_network_inflight(
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    height: u64,
    block_hash: [u8; 32],
    rx: tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    timeout_secs: u64,
) {
    let deadline = Arc::new(AtomicU64::new(
        wall_ms_now().saturating_add(timeout_secs.saturating_mul(1000)),
    ));
    inflight_deadlines.insert(height, Arc::clone(&deadline));
    in_flight_heights.insert(height);
    let request_start = Instant::now();
    in_flight.push(Box::pin(async move {
        let r = await_block_with_deadline(rx, deadline).await;
        (height, block_hash, request_start, r, permit)
    }));
}

/// W28d: poll until tip walks into this chunk's range and it is not a tip-cover claim.
async fn wait_tip_enter_abort(
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
    peer_id: &str,
    start_height: u64,
    end_height: u64,
) {
    let Some(assigner) = tip_enter.as_ref() else {
        std::future::pending::<()>().await;
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if assigner.should_abort_tip_walk_in(peer_id, start_height, end_height) {
            return;
        }
    }
}

/// W36: abort when coordinator blacklists this peer (tip-SLA rotate) so in-flight
/// does not remain a covering zombie for minutes.
async fn wait_blacklist_abort(
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
    peer_id: &str,
) {
    let Some(assigner) = tip_enter.as_ref() else {
        std::future::pending::<()>().await;
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if assigner.is_peer_blacklisted(peer_id) {
            return;
        }
    }
}

/// W33/A6g: WAN deep tip-owner pipe uses a bounded gap deadline — ahead blocks must not
/// extend to 600s. Floor tracks tip-SLA (default 90s) so soft-retries are not aborted by
/// the outer chunk deadline before SLA can rotate.
pub(crate) fn wan_deep_tip_pipe_chunk_deadline_secs(
    start_height: u64,
    end_height: u64,
    confirmed_body_height: u64,
    default_secs: u64,
) -> u64 {
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        super::tip_stage::tip_sla_secs()
            .saturating_mul(2)
            .clamp(90, 180)
    } else {
        default_secs
    }
}

/// WAN tip-stream score credits for one received body (assigner present).
///
/// Tip-adjacent arrival credits once (covers GAP_STREAM no-op). Successful GAP_STREAM
/// credits only when the body was not tip-adjacent — never both (avoids ~2× sticky tip BPS).
pub(crate) fn wan_tip_stream_credit_count(
    from_local: bool,
    tip_adjacent: bool,
    gap_streamed: bool,
) -> u8 {
    if from_local {
        return 0;
    }
    let mut n = 0u8;
    if tip_adjacent {
        n = n.saturating_add(1);
    }
    if gap_streamed && !tip_adjacent {
        n = n.saturating_add(1);
    }
    n
}
