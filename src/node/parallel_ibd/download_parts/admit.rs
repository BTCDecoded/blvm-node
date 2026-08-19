fn tip_need_from(validation_height: Option<&Arc<AtomicU64>>) -> Option<u64> {
    validation_height.map(|vh| vh.load(Ordering::Relaxed).saturating_add(1))
}

fn block_tx_tip_reserve() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_BLOCK_TX_TIP_RESERVE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(8, 512)
    })
}

/// Wait until the channel has headroom for a non-tip send (keeps slots for tip).
///
/// Unlike `try_send`+break (live ~6 BPS), park briefly so the coordinator can drain while
/// the worker holds the tip-contiguous cursor. Exact tip never waits here.
///
/// Important: do **not** busy-`yield_now` here. Under synth local-disk, a full channel +
/// tip already queued (`GAP_STREAM` at t≈0) left two download workers spinning `yield_now`
/// while the coordinator starved — Validation first block at ~10.8s, wall ~6–10 BPS
/// (burst path had first block ~1.0s and no TIP_RESERVE waits). A 1ms sleep yields the
/// runtime properly so `recv_many` can drain tip.
async fn await_block_tx_tip_reserve(
    tx: &tokio::sync::mpsc::Sender<(u64, SharedBlock, SharedWitnesses)>,
    height: u64,
    tip_need: Option<u64>,
) {
    if tip_need == Some(height) || tip_need.is_none() {
        return;
    }
    // Keep tip-reserve for synth bulk too: disabling it re-starved tip (Validation
    // first block ~11s). Use 1ms sleep — not yield_now — so the coordinator can drain
    // (yield_now spin left first block ~10.8s). Burst ~500 wall had no RESERVE waits
    // because the channel never filled; when it does, reserve protects tip slots.
    let reserve = block_tx_tip_reserve();
    let mut logged = false;
    while tx.capacity() <= reserve {
        if !logged {
            logged = true;
            info!(
                "[IBD_BLOCK_TX_TIP_RESERVE] wait h={} free={} (reserve={})",
                height,
                tx.capacity(),
                reserve
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Disk-backed block source: offline local replay or synthetic WAN harness.
pub(crate) fn is_snapshot_sourced_peer(peer_id: &str) -> bool {
    is_local_disk_peer(peer_id) || super::synthetic_wan::is_synthetic_peer(peer_id)
}

/// Per-worker soft cap on `received` (Arc<Block> bodies). Live W55 jemalloc: 1 MiB class
/// grew to ~4k live objs (~4 GiB) while reorder≪1k — tip-stall + W50 no-ahead-drain left
/// parallel chunk maps holding far-ahead bodies. Persist-then-drop beyond the tip window.
///
/// Default **160** (≥ tip-owner pipe 128). Live W65 genesis: cap=96 + tip pipe 128 +
/// persist lookahead 64 → continuous trim of tip+65.. without successful persist →
/// **106M** drops / 1.6M log lines → ~60–90s coordinator silence plateaus (~0 blk/s).
fn download_received_soft_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_DOWNLOAD_RECEIVED_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(160)
            .clamp(64, 512)
    })
}

/// Hard ceiling: drop even without persist to protect RAM (per worker).
fn download_received_hard_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_DOWNLOAD_RECEIVED_HARD_CAP")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(320)
            .clamp(128, 1024)
            .max(download_received_soft_cap())
    })
}

fn note_received_insert(n: u64) {
    super::memory::DOWNLOAD_RECEIVED_BLOCKS.fetch_add(n, Ordering::Relaxed);
}

fn note_received_remove(n: u64) {
    let _ = super::memory::DOWNLOAD_RECEIVED_BLOCKS.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |v| Some(v.saturating_sub(n)),
    );
}

fn received_put(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
    v: (SharedBlock, SharedWitnesses),
) {
    if received.insert(h, v).is_none() {
        note_received_insert(1);
    }
}

fn received_take(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
) -> Option<(SharedBlock, SharedWitnesses)> {
    received.remove(&h).inspect(|_| note_received_remove(1))
}

/// Cheap Arc clone of a buffered height — does **not** remove it from `received`.
///
/// H5/A1: tip STREAM must leave the body keyed until validation advances (`trim` drops
/// `h < need`). Taking on STREAM forced LOCAL_GAP reinject + `GAP_STREAM_RESEND` when the
/// coordinator lost the tip (synth cliff ~315→340k: RESEND≈6k, instant BPS 137→9).
fn received_clone(
    received: &BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    h: u64,
) -> Option<(SharedBlock, SharedWitnesses)> {
    received
        .get(&h)
        .map(|(b, w)| (Arc::clone(b), Arc::clone(w)))
}

fn received_drain_all(received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>) {
    let n = received.len() as u64;
    received.clear();
    if n > 0 {
        note_received_remove(n);
    }
}

/// Hard-cap eviction for per-worker `received` (OOM guard).
///
/// Drops farthest-ahead bodies while `len > hard` and `h > need`.
/// **Never** removes tip-adjacent `h <= need` — deliberate diverge from rbitcoin's soft
/// "never drop already-received" (we drop far-ahead; we still protect the tip window).
/// Phase 0b.1 policy surface (docs/RBITCOIN_VS_BLVM_IBD_ARCHITECTURE.md).
fn hard_trim_download_received_far_ahead(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    need: u64,
    hard: usize,
) -> u64 {
    let mut forced = 0u64;
    while received.len() > hard {
        let Some((&h, _)) = received.iter().next_back() else {
            break;
        };
        if h <= need {
            break;
        }
        let _ = received_take(received, h);
        forced = forced.saturating_add(1);
    }
    forced
}

/// Bound per-worker `received` RAM without the W66 re-download thrash.
///
/// Live W65 genesis: trim dropped far-ahead even when `GAP_PERSIST` refused (height >
/// val+lookahead), while **below-tip** entries were never evicted (`h <= need` break).
/// Maps stayed over soft-cap → every body arrival trimmed 1 → 106M drops + log storm.
///
/// Order:
/// 1. Drop stale `h < need` (already past validation; safe without persist).
/// 2. Soft-cap: persist-then-drop farthest ahead; **put back** if persist fails.
/// 3. Hard-cap: drop farthest anyway (memory guard).
fn trim_download_received(
    received: &mut BTreeMap<u64, (SharedBlock, SharedWitnesses)>,
    blockstore: &BlockStore,
    validation_height: Option<&Arc<AtomicU64>>,
    protocol_version: ProtocolVersion,
) {
    let soft = download_received_soft_cap();
    let hard = download_received_hard_cap();
    let need = validation_height
        .map(|v| v.load(Ordering::Relaxed).saturating_add(1))
        .unwrap_or(0);
    let mut trimmed = 0u64;
    let mut forced = 0u64;

    // (1) Evict below tip — these blocked the old trim loop and kept len > soft forever.
    while let Some((&h, _)) = received.iter().next() {
        if h >= need {
            break;
        }
        let _ = received_take(received, h);
        trimmed = trimmed.saturating_add(1);
    }

    // (2) Soft trim: persist-then-drop only.
    while received.len() > soft {
        let Some((&h, _)) = received.iter().next_back() else {
            break;
        };
        // Never drop the validation tip from the worker buffer.
        if h <= need {
            break;
        }
        let Some((block, witnesses)) = received_take(received, h) else {
            break;
        };
        let hash = blockstore.get_block_hash(block.as_ref());
        let persisted = try_persist_gap_block_for_local_inject(
            blockstore,
            validation_height,
            h,
            hash,
            block.as_ref(),
            witnesses.as_ref(),
            protocol_version,
        )
        .unwrap_or(false);
        if persisted {
            trimmed = trimmed.saturating_add(1);
            drop((block, witnesses));
            continue;
        }
        // Persist refused (outside lookahead / witness) — keep unless hard-capped.
        received_put(received, h, (block, witnesses));
        break;
    }

    // (3) Hard cap: drop farthest ahead even without persist (OOM guard).
    // Never removes tip-adjacent `h <= need` (rbitcoin diverge: we *do* drop far-ahead).
    let hard_forced = hard_trim_download_received_far_ahead(received, need, hard);
    trimmed = trimmed.saturating_add(hard_forced);
    forced = forced.saturating_add(hard_forced);

    if trimmed > 0 {
        super::memory::DOWNLOAD_RECEIVED_TRIM_BLOCKS.fetch_add(trimmed, Ordering::Relaxed);
        // Time-rate-limit: ≤1 warn / 5s (old n%64 still logged ~3.6k/s under thrash).
        static LAST_TRIM_LOG_MS: AtomicU64 = AtomicU64::new(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let prev = LAST_TRIM_LOG_MS.load(Ordering::Relaxed);
        if now_ms.saturating_sub(prev) >= 5_000
            && LAST_TRIM_LOG_MS
                .compare_exchange(prev, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let n = super::memory::DOWNLOAD_RECEIVED_TRIM_BLOCKS.load(Ordering::Relaxed);
            warn!(
                "[IBD_DOWNLOAD_RECEIVED_TRIM] dropped {} block(s) (forced={}, soft={}, hard={}, received={}, need={}, total_trimmed={})",
                trimmed,
                forced,
                soft,
                hard,
                received.len(),
                need,
                n
            );
        }
    }
}

/// Result of [`download_chunk`]: when streaming to the coordinator, `blocks` is empty and
/// `streamed_block_count` holds the number sent; otherwise `blocks` contains the full chunk.
/// Blocks and witnesses are Arc-wrapped so they can be passed cheaply through further pipeline stages.
pub(crate) struct DownloadChunkResult {
    pub blocks: Vec<(u64, SharedBlock, SharedWitnesses)>,
    pub streamed_block_count: usize,
}

impl DownloadChunkResult {
    #[inline]
    pub fn block_count(&self) -> usize {
        if self.blocks.is_empty() {
            self.streamed_block_count
        } else {
            self.blocks.len()
        }
    }
}

/// Tracks block download progress for stalling detection
struct BlockDownloadProgress {
    last_block_hash: Option<Hash>,
    last_progress_time: std::time::Instant,
    current_timeout_seconds: u64,
    disconnected_peers_count: usize,
}

impl BlockDownloadProgress {
    fn new() -> Self {
        Self {
            last_block_hash: None,
            last_progress_time: std::time::Instant::now(),
            current_timeout_seconds: 120,
            disconnected_peers_count: 0,
        }
    }

    fn record_progress(&mut self, block_hash: Hash) {
        self.last_block_hash = Some(block_hash);
        self.last_progress_time = std::time::Instant::now();
    }

    fn reset_timeout(&mut self) {
        self.current_timeout_seconds = 120;
        self.disconnected_peers_count = 0;
    }
}

/// `send_to_peer` can fail transiently (`Peer not found`, disconnected, send channel closed).
/// Failing the entire chunk on first error deadlocks IBD when `BLVM_IBD_PEERS` is a single LAN node:
/// the assigner requeues to the same peer, validation stalls, and `/common` profile runs hit timeout.
async fn send_block_getdata_with_retry(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    wire_msg: Vec<u8>,
    height: u64,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 30;
    const BASE_MS: u64 = 100;
    const MAX_WAIT_MS: u64 = 5_000;
    let mut attempt: u32 = 0;
    let mut reconnect_spawned = false;
    loop {
        match network.send_to_peer(peer_addr, wire_msg.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                let is_gone = msg.contains("not found") || msg.contains("disconnected");
                if !reconnect_spawned && is_gone {
                    reconnect_spawned = true;
                    NetworkManager::spawn_outbound_reconnect_attempt(
                        Arc::clone(&network),
                        peer_addr,
                    );
                }
                attempt += 1;
                let wait_ms = BASE_MS
                    .saturating_mul(1u64 << (attempt - 1).min(6))
                    .min(MAX_WAIT_MS);
                if attempt >= MAX_ATTEMPTS {
                    return Err(e).with_context(|| {
                        format!(
                            "Failed to send GetData for block at height {height} after {MAX_ATTEMPTS} attempts"
                        )
                    });
                }
                if attempt <= 3 || attempt % 5 == 0 {
                    warn!(
                        "GetData send failed for height {} (attempt {}/{}): {} — retrying in {}ms",
                        height, attempt, MAX_ATTEMPTS, e, wait_ms
                    );
                }
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }
}

/// First height to request when retrying a chunk after partial validation progress.
pub(crate) fn resume_download_height(
    start_height: u64,
    end_height: u64,
    validated_tip: u64,
) -> Option<u64> {
    if validated_tip >= end_height {
        return None;
    }
    let resume = start_height.max(validated_tip.saturating_add(1));
    if resume > end_height {
        None
    } else {
        Some(resume)
    }
}

/// Outer wall-clock budget for one chunk download (per-block timeout × remaining blocks).
///
/// Single-height micro-chunks (`start == end`) arise from stall recovery and use a longer
/// minimum (4× per-block timeout, ≥120s) so the block can actually arrive over slow WAN
/// before the outer `tokio::timeout` kills the future.  The standard 35s minimum fires
/// before the inner per-block deadline can even report back, causing the entire retry storm:
/// all 8 racing peers die at 35s simultaneously, re-queue, and repeat.
pub(crate) fn chunk_outer_deadline_secs(
    start_height: u64,
    end_height: u64,
    resume_from: u64,
    per_block_timeout_secs: u64,
) -> u64 {
    if start_height == end_height {
        // Stall-recovery micro-chunk: give the block room to arrive over slow WAN.
        // Inner chunk_deadline_secs still handles genuine stuck peers.
        return per_block_timeout_secs.saturating_mul(4).clamp(120, 7200);
    }
    let blocks_remaining = end_height
        .saturating_sub(resume_from)
        .saturating_add(1)
        .max(1);
    per_block_timeout_secs
        .saturating_mul(blocks_remaining)
        .clamp(35, 7200)
}

/// Worker-level outer budget passed into `download_chunk` (cooperative + safety-net).
///
/// WAN deep tip-owner pipes (`span ≥ 64` past body tip) must not use uncapped
/// `per_block × 128` (live: 5760s safety-net while coordinator shows covering=0).
pub(crate) fn worker_chunk_outer_deadline_secs(
    start_height: u64,
    end_height: u64,
    resume_from: u64,
    per_block_timeout_secs: u64,
    confirmed_body_height: u64,
) -> u64 {
    let base = chunk_outer_deadline_secs(
        start_height,
        end_height,
        resume_from,
        per_block_timeout_secs,
    );
    if confirmed_body_height > 0
        && start_height > confirmed_body_height
        && end_height.saturating_sub(start_height) >= 63
    {
        // Belt with D0: cap cooperative/safety-net outer on WAN tip pipes.
        let cap = wan_deep_tip_pipe_chunk_deadline_secs(
            start_height,
            end_height,
            confirmed_body_height,
            per_block_timeout_secs,
        )
        .saturating_mul(2)
        .clamp(60, 120);
        base.min(cap)
    } else {
        base
    }
}

/// Wait for outbound peer connection, spawning reconnect if needed.
async fn wait_for_peer_connected(
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    max_wait: Duration,
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<()> {
    // Check eviction FIRST — an IP that has been permanently evicted this session
    // must be rejected even if it briefly reconnects.
    {
        let evicted = network.ibd_evicted_ips.read().unwrap();
        if evicted.contains(&peer_addr.ip()) {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} evicted (NODE_NETWORK_LIMITED) — chunk needs retry on another peer"
            ));
        }
    }
    if network.is_peer_connected(peer_addr).await {
        return Ok(());
    }
    NetworkManager::spawn_outbound_reconnect_attempt(Arc::clone(network), peer_addr);
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    while tokio::time::Instant::now() < deadline {
        if tip_enter
            .as_ref()
            .is_some_and(|a| a.is_peer_blacklisted(peer_id))
        {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} blacklisted during connect wait — chunk needs retry"
            ));
        }
        if network.is_peer_connected(peer_addr).await {
            return Ok(());
        }
        poll.tick().await;
    }
    Err(anyhow::anyhow!(
        "Peer {peer_id} not connected after {}s — chunk needs retry",
        max_wait.as_secs()
    ))
}

/// P0-A: wait for Bitcoin handshake (VerAck) before sending GetData.
async fn wait_for_peer_ibd_ready(
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    max_wait: Duration,
    tip_enter: &Option<Arc<super::chunk_assigner::ChunkAssigner>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + max_wait;
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await;
    while tokio::time::Instant::now() < deadline {
        if tip_enter
            .as_ref()
            .is_some_and(|a| a.is_peer_blacklisted(peer_id))
        {
            return Err(anyhow::anyhow!(
                "Peer {peer_id} blacklisted during handshake wait — chunk needs retry"
            ));
        }
        if network.peer_ibd_ready(peer_addr).await {
            return Ok(());
        }
        poll.tick().await;
    }
    Err(anyhow::anyhow!(
        "Peer {peer_id} handshake not complete after {}s — chunk needs retry",
        max_wait.as_secs()
    ))
}

/// `MSG_BLOCK` only when a module skip-witness policy verifies; fail-open = full witness.
fn ibd_getdata_inv_type(blockstore: &BlockStore, height: u64, block_hash: Hash) -> u32 {
    let merkle_root = blockstore
        .get_header(&block_hash)
        .ok()
        .flatten()
        .map(|h| h.merkle_root)
        .unwrap_or([0u8; 32]);
    if crate::module::pipeline::try_filter_block_download_policy(height, block_hash, merkle_root) {
        MSG_BLOCK
    } else {
        MSG_WITNESS_BLOCK
    }
}

/// Register a block download and send GetData. Fails fast when the peer is gone (avoids orphan
/// pending requests that time out and blacklist the whole chunk).
async fn register_and_request_block(
    network: Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    block_hash: Hash,
    height: u64,
    blockstore: &BlockStore,
) -> Result<tokio::sync::oneshot::Receiver<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>)>> {
    if !network.is_peer_connected(peer_addr).await {
        return Err(anyhow::anyhow!(
            "Peer {peer_id} not connected — cannot request block at height {height}"
        ));
    }
    let block_rx = network.register_block_request(peer_addr, block_hash);
    let inventory = vec![InventoryVector {
        inv_type: ibd_getdata_inv_type(blockstore, height, block_hash),
        hash: block_hash,
    }];
    let wire_msg =
        ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage { inventory }))?;
    if let Err(e) =
        send_block_getdata_with_retry(Arc::clone(&network), peer_addr, wire_msg, height).await
    {
        network.cancel_block_request(peer_addr, block_hash);
        return Err(e);
    }
    super::tip_stage::mark_getdata(height);
    Ok(block_rx)
}

type PendingBlockResult = (
    u64,
    [u8; 32],
    std::time::Instant,
    Result<
        Result<(Block, Vec<Vec<Witness>>, Option<Vec<u8>>), tokio::sync::oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
    Option<tokio::sync::OwnedSemaphorePermit>,
);

type PendingBlockFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = PendingBlockResult> + Send>>;

/// Maximum block hashes per `getdata` message for batched IBD download.
///
/// Default **64** matches a31 peak (`GETDATA_BATCH_SIZE`). Override
/// `BLVM_IBD_GETDATA_BATCH` (1..=64).
fn getdata_batch_size() -> usize {
    super::policy::getdata_batch()
}

/// Enqueue a batch of network block downloads using a single `getdata` message.
///
/// All heights in the batch must not be locally available (call the standard
/// `enqueue_chunk_block` for heights where `try_load_local_ibd_block` succeeds).
///
/// On any error all registered receivers for this batch are cancelled before returning.
#[allow(clippy::too_many_arguments)]
async fn enqueue_network_block_batch(
    heights_and_hashes: Vec<(u64, [u8; 32])>,
    permits: Vec<Option<tokio::sync::OwnedSemaphorePermit>>,
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_default_secs: u64,
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    first_block_logged: &mut bool,
    start_height: u64,
    end_height: u64,
    blockstore: &BlockStore,
) -> Result<()> {
    debug_assert_eq!(heights_and_hashes.len(), permits.len());
    if heights_and_hashes.is_empty() {
        return Ok(());
    }

    let hashes: Vec<[u8; 32]> = heights_and_hashes.iter().map(|(_, h)| *h).collect();
    let mut rxs = network.register_block_requests_batch(peer_addr, &hashes);

    // Build a single GetData message for all blocks in the batch.
    let inventory: Vec<InventoryVector> = heights_and_hashes
        .iter()
        .map(|&(height, hash)| InventoryVector {
            inv_type: ibd_getdata_inv_type(blockstore, height, hash),
            hash,
        })
        .collect();
    let wire_msg =
        ProtocolParser::serialize_message(&ProtocolMessage::GetData(GetDataMessage { inventory }))?;

    let first_height = heights_and_hashes[0].0;
    if let Err(e) =
        send_block_getdata_with_retry(Arc::clone(network), peer_addr, wire_msg, first_height).await
    {
        // Cancel all registered receivers before propagating the error.
        for &hash in &hashes {
            network.cancel_block_request(peer_addr, hash);
        }
        return Err(e);
    }
    for &(height, _) in &heights_and_hashes {
        super::tip_stage::mark_getdata(height);
    }

    if !*first_block_logged {
        info!(
            "[IBD] {} chunk {}-{}: batch-requested {} blocks starting at height {} (hash {})",
            peer_id,
            start_height,
            end_height,
            heights_and_hashes.len(),
            first_height,
            hex::encode(hashes[0])
        );
        *first_block_logged = true;
    }

    for (((height, block_hash), permit), rx) in heights_and_hashes
        .into_iter()
        .zip(permits.into_iter())
        .zip(rxs.drain(..))
    {
        let secs = block_gap_timeout_secs(
            height,
            validation_tip,
            confirmed_body_height,
            start_height,
            end_height,
            chunk_default_secs,
        );
        push_network_inflight(
            in_flight,
            in_flight_heights,
            inflight_deadlines,
            height,
            block_hash,
            rx,
            permit,
            secs,
        );
    }
    Ok(())
}

/// Non-blocking take of a per-peer in-flight permit.
///
/// Returns `Ok(None)` when the semaphore is exhausted — caller must return to `select!` and
/// poll `in_flight` (those futures hold the permits). Blocking `acquire_owned` here while
/// `in_flight` is unpolled self-deadlocks when `pipeline_depth == sem capacity` (live WAN
/// tip pipes: 128-deep, 5760s safety-net, 0 soft timeouts).
fn try_take_blocks_permit(
    blocks_sem: &Option<Arc<Semaphore>>,
) -> Result<Option<Option<tokio::sync::OwnedSemaphorePermit>>> {
    match blocks_sem {
        None => Ok(Some(None)),
        Some(sem) => match sem.clone().try_acquire_owned() {
            Ok(p) => Ok(Some(Some(p))),
            Err(tokio::sync::TryAcquireError::NoPermits) => Ok(None),
            Err(tokio::sync::TryAcquireError::Closed) => {
                Err(anyhow::anyhow!("blocks semaphore closed"))
            }
        },
    }
}

/// Enqueue one block download (local replay or network GetData). Skips if already in flight.
async fn enqueue_chunk_block(
    height: u64,
    block_hash: [u8; 32],
    network: &Arc<NetworkManager>,
    peer_addr: SocketAddr,
    peer_id: &str,
    blockstore: &BlockStore,
    protocol_version: ProtocolVersion,
    validation_tip: u64,
    confirmed_body_height: u64,
    chunk_default_secs: u64,
    blocks_sem: &Option<Arc<Semaphore>>,
    in_flight: &mut FuturesUnordered<PendingBlockFuture>,
    in_flight_heights: &mut HashSet<u64>,
    inflight_deadlines: &mut HashMap<u64, Arc<AtomicU64>>,
    first_block_logged: &mut bool,
    start_height: u64,
    end_height: u64,
    local_sourced_heights: &mut HashSet<u64>,
) -> Result<()> {
    if in_flight_heights.contains(&height) {
        return Ok(());
    }
    let Some(permit) = try_take_blocks_permit(blocks_sem)? else {
        // Pipe full — leave height free for a later fill_pipeline after in_flight drains.
        return Ok(());
    };
    if let Some((block, block_witnesses)) =
        try_load_local_ibd_block(blockstore, height, block_hash, protocol_version)?
    {
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
    } else {
        if !*first_block_logged {
            info!(
                "[IBD] {} chunk {}-{}: registered block height {} (hash {})",
                peer_id,
                start_height,
                end_height,
                height,
                hex::encode(block_hash)
            );
            *first_block_logged = true;
        }
        let block_rx = register_and_request_block(
            Arc::clone(network),
            peer_addr,
            peer_id,
            block_hash,
            height,
            blockstore,
        )
        .await?;
        let secs = block_gap_timeout_secs(
            height,
            validation_tip,
            confirmed_body_height,
            start_height,
            end_height,
            chunk_default_secs,
        );
        push_network_inflight(
            in_flight,
            in_flight_heights,
            inflight_deadlines,
            height,
            block_hash,
            block_rx,
            permit,
            secs,
        );
    }
    Ok(())
}
