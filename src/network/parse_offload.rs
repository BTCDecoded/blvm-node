//! R-302: inbound `block` deserialize off the shared message loop.
//!
//! Default **off** (`BLVM_IBD_PARSE_OFFLOAD` unset) = R-298 DNA: parse stays
//! inline on `process_messages`. This is the **inbound** BLOCK path only.
//! Do not touch getdata serve (the comment on that loop about a prior spawn
//! collapse is outbound).
//!
//! Pool size: 6 (clamp 4..=8). This host has other work; keep it modest.

use crate::network::NetworkManager;
use crate::network::protocol::{ProtocolMessage, ProtocolParser, cmd};
use anyhow::Result;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

static PEER_RX_DEPTH: AtomicUsize = AtomicUsize::new(0);
static PEER_RX_MAX: AtomicUsize = AtomicUsize::new(0);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("BLVM_IBD_PARSE_OFFLOAD") {
        Ok(v) => {
            let t = v.trim();
            t == "1" || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("on")
        }
        Err(_) => false,
    })
}

/// Concurrent deserialize workers. 6 sits in the asked 4..=8 band.
fn pool_n() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("BLVM_IBD_PARSE_OFFLOAD_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6)
            .clamp(4, 8)
    })
}

fn permits() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(pool_n()))
}

/// Bitcoin frame command is 12 bytes at offset 4, NUL-padded.
pub(crate) fn is_block_frame(data: &[u8]) -> bool {
    if data.len() < 16 {
        return false;
    }
    let cmd = data[4..16].split(|&b| b == 0).next().unwrap_or(&[]);
    cmd == cmd::BLOCK.as_bytes()
}

pub(crate) fn note_peer_rx_enqueue() {
    let d = PEER_RX_DEPTH.fetch_add(1, Ordering::Relaxed) + 1;
    loop {
        let max = PEER_RX_MAX.load(Ordering::Relaxed);
        if d <= max {
            break;
        }
        if PEER_RX_MAX
            .compare_exchange_weak(max, d, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
    }
}

pub(crate) fn note_peer_rx_dequeue() {
    PEER_RX_DEPTH.fetch_sub(1, Ordering::Relaxed);
}

pub(crate) fn peer_rx_depth() -> usize {
    PEER_RX_DEPTH.load(Ordering::Relaxed)
}

pub(crate) fn peer_rx_max() -> usize {
    PEER_RX_MAX.load(Ordering::Relaxed)
}

/// If this is an inbound BLOCK and offload is on, spawn parse+oneshot and
/// return `None` (caller must not dispatch inline). Otherwise `Some(message)`.
pub(crate) fn take_inbound_block(
    nm: &Arc<NetworkManager>,
    message: crate::network::NetworkMessage,
) -> Option<crate::network::NetworkMessage> {
    use crate::network::NetworkMessage;
    if !enabled() {
        return Some(message);
    }
    match message {
        NetworkMessage::RawMessageReceived(data, peer_addr) if is_block_frame(&data) => {
            let nm = Arc::clone(nm);
            tokio::spawn(async move {
                if let Err(e) = offload_block(nm, peer_addr, data).await {
                    debug!("parse-offload block from {peer_addr}: {e:#}");
                }
            });
            None
        }
        other => Some(other),
    }
}

async fn offload_block(
    nm: Arc<NetworkManager>,
    peer_addr: std::net::SocketAddr,
    data: Vec<u8>,
) -> Result<()> {
    nm.track_bytes_received(data.len() as u64).await;
    if nm.is_banned(peer_addr) {
        return Ok(());
    }
    if let Some(hash) =
        crate::node::parallel_ibd::wire_hash_gate::try_skip_obsolete_block_frame(&data)
    {
        nm.cancel_block_request_force(peer_addr, hash);
        return Ok(());
    }
    let _permit = permits()
        .acquire()
        .await
        .expect("parse-offload semaphore closed");
    let t0 = Instant::now();
    let join = tokio::task::spawn_blocking(move || {
        let parsed = ProtocolParser::parse_message(&data);
        (parsed, data)
    })
    .await;
    let parse_ms = t0.elapsed().as_millis() as u64;
    crate::node::parallel_ibd::note_block_parse(parse_ms, true);
    match join {
        Ok((Ok(ProtocolMessage::Block(block_msg)), data)) => {
            nm.handle_block_wire_message(peer_addr, block_msg, data)
                .await?;
        }
        Ok((Ok(_other), _)) => {
            debug!("parse-offload: frame looked like block but parsed as other cmd from {peer_addr}");
        }
        Ok((Err(e), _)) => {
            debug!("parse-offload deserialize from {peer_addr}: {e}");
        }
        Err(e) => {
            warn!("parse-offload join from {peer_addr}: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r302_parse_offload_defaults_off_so_unset_is_r298_dna() {
        assert!(
            !enabled(),
            "R-302: BLVM_IBD_PARSE_OFFLOAD unset must be today's inline parse (R-298 DNA); got on"
        );
    }

    #[test]
    fn r302_is_block_frame_only_matches_nul_padded_block_command() {
        let mut block = vec![0u8; 24];
        block[4..9].copy_from_slice(b"block");
        assert!(is_block_frame(&block), "12-byte NUL-padded 'block' is cmd::BLOCK");
        let mut inv = vec![0u8; 24];
        inv[4..7].copy_from_slice(b"inv");
        assert!(!is_block_frame(&inv), "inv must stay on the message loop");
        let mut getdata = vec![0u8; 24];
        getdata[4..11].copy_from_slice(b"getdata");
        assert!(
            !is_block_frame(&getdata),
            "getdata is the SERVE path — offload must not take it"
        );
        assert!(!is_block_frame(&[0u8; 8]), "short frame is not a block");
    }

    #[test]
    fn r302_pool_n_is_modest_four_to_eight() {
        let n = pool_n();
        assert!(
            (4..=8).contains(&n),
            "R-302: parse pool must stay 4..=8 (chose 6); got {n}"
        );
    }
}
