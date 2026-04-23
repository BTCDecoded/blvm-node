//! Stratum V2 TCP listener
//!
//! Accepts miner connections on a dedicated port (e.g. 3333), reads TLV frames,
//! and forwards them to the network message queue as StratumV2MessageReceived.
//! Responses are sent via the stratum_connections map in NetworkManager.

use crate::network::network_manager::NetworkManager;
use crate::network::NetworkMessage;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Stratum V2 TLV frame: [4-byte length (LE)][2-byte tag][4-byte payload_len][payload]
/// First 4 bytes = total TLV size (2 + 4 + payload_len)
const MAX_FRAME_SIZE: usize = 1024 * 1024; // 1 MiB

/// Start Stratum V2 TCP listener on the given address.
/// When stratum-v2 feature is enabled and config has listen_addr.
pub(crate) async fn start_stratum_v2_listener(
    nm: Arc<NetworkManager>,
    listen_addr: SocketAddr,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!("Stratum V2 listener started on {}", listen_addr);

    let peer_tx = nm.peer_tx().clone();
    let stratum_connections = Arc::clone(nm.stratum_connections());

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    info!("Stratum V2 connection from {}", peer_addr);

                    let peer_tx = peer_tx.clone();
                    let stratum_connections = Arc::clone(&stratum_connections);

                    let (send_tx, mut send_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    {
                        let mut conns = stratum_connections.write().await;
                        conns.insert(peer_addr, send_tx);
                    }

                    let (mut reader, mut writer) = stream.into_split();

                    // Write loop: send responses to miner
                    let stratum_connections_write = Arc::clone(&stratum_connections);
                    tokio::spawn(async move {
                        while let Some(data) = send_rx.recv().await {
                            if writer.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        let mut conns = stratum_connections_write.write().await;
                        conns.remove(&peer_addr);
                        debug!("Stratum V2 connection closed: {}", peer_addr);
                    });

                    // Read loop: parse TLV frames and forward to message queue
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 4];
                        loop {
                            if reader.read_exact(&mut buf).await.is_err() {
                                break;
                            }
                            let frame_len =
                                u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                            if frame_len < 6 || frame_len > MAX_FRAME_SIZE {
                                warn!(
                                    "Invalid Stratum V2 frame length {} from {}",
                                    frame_len, peer_addr
                                );
                                break;
                            }
                            let mut frame = vec![0u8; 4 + frame_len];
                            frame[0..4].copy_from_slice(&buf);
                            if reader.read_exact(&mut frame[4..]).await.is_err() {
                                break;
                            }
                            // Forward TLV part only (handle_message uses decode_raw, expects [tag][len][payload])
                            if peer_tx
                                .send(NetworkMessage::StratumV2MessageReceived(
                                    frame[4..].to_vec(),
                                    peer_addr,
                                ))
                                .is_err()
                            {
                                break;
                            }
                        }
                        let mut conns = stratum_connections.write().await;
                        conns.remove(&peer_addr);
                        debug!("Stratum V2 read loop ended: {}", peer_addr);
                    });
                }
                Err(e) => {
                    error!("Stratum V2 accept error: {}", e);
                }
            }
        }
    });

    Ok(())
}
