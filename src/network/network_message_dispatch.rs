//! Network message dispatch for process_messages loop.
//!
//! Handles all NetworkMessage variants received from the peer channel.
//! Extracted from network_manager.rs to reduce file size.

use crate::module::ipc::protocol::EventPayload;
use crate::module::traits::EventType;
use crate::network::network_manager::NetworkManager;
use crate::network::protocol::{NetworkAddress, ProtocolMessage, ProtocolParser};
use crate::network::transport::TransportAddr;
use crate::network::NetworkMessage;
use anyhow::Result;
use crate::utils::{current_timestamp, ignore_error};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Publish a network event if an event publisher is configured. Logs at debug on failure.
async fn publish_network_event_if_configured(
    event_publisher: &Arc<tokio::sync::Mutex<Option<Arc<crate::node::event_publisher::EventPublisher>>>>,
    event_type: EventType,
    payload: EventPayload,
    log_context: &str,
) {
    let guard = event_publisher.lock().await;
    if let Some(ref ep) = *guard {
        ignore_error(|| ep.publish_event(event_type, payload), log_context).await;
    }
}

/// Handle a single network message (full dispatch for process_messages loop)
pub(crate) async fn handle_network_message(nm: &NetworkManager, message: NetworkMessage) -> Result<()> {
    match message {
        NetworkMessage::PeerConnected(addr) => {
            handle_peer_connected(nm, addr).await;
        }
        NetworkMessage::PeerDisconnected(addr) => {
            handle_peer_disconnected(nm, addr).await;
        }
        NetworkMessage::RawMessageReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_incoming_wire_tcp(peer_addr, data),
                &format!("Error processing raw message from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::BlockReceived(data) => {
            nm.queue_block(data);
        }
        NetworkMessage::TransactionReceived(data) => {
            if let Ok(ProtocolMessage::Tx(tx_msg)) = ProtocolParser::parse_message(&data) {
                let txs = [tx_msg.transaction.clone()];
                ignore_error(
                    || nm.submit_transactions_to_mempool(&txs),
                    "Error processing transaction",
                )
                .await;
            }
        }
        NetworkMessage::InventoryReceived(_data) => {
            // Inventory relay - no-op for now (inv processing in protocol layer)
            debug!("InventoryReceived");
        }
        NetworkMessage::GetCfiltersReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_getcfilters_request(data, peer_addr),
                &format!("Error handling GetCfilters from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::GetCfheadersReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_getcfheaders_request(data, peer_addr),
                &format!("Error handling GetCfheaders from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::GetCfcheckptReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_getcfcheckpt_request(data, peer_addr),
                &format!("Error handling GetCfcheckpt from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::PkgTxnReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_pkgtxn_request(data, peer_addr),
                &format!("Error handling PkgTxn from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::SendPkgTxnReceived(_data, _peer_addr) => {
            // SendPkgTxn is a notification - no response needed
            debug!("SendPkgTxnReceived");
        }
        NetworkMessage::GetModuleReceived(data, peer_addr) => {
            let protocol_msg = match ProtocolParser::parse_message(&data) {
                Ok(ProtocolMessage::GetModule(msg)) => msg,
                _ => {
                    debug!("Invalid GetModule message from {}", peer_addr);
                    return Ok(());
                }
            };
            ignore_error(
                || nm.handle_get_module(peer_addr, protocol_msg),
                &format!("Error handling GetModule from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::ModuleReceived(data, _peer_addr) => {
            if let Ok(ProtocolMessage::Module(msg)) = ProtocolParser::parse_message(&data) {
                nm.complete_request(msg.request_id, data);
            }
        }
        NetworkMessage::GetModuleByHashReceived(data, peer_addr) => {
            let protocol_msg = match ProtocolParser::parse_message(&data) {
                Ok(ProtocolMessage::GetModuleByHash(msg)) => msg,
                _ => {
                    debug!("Invalid GetModuleByHash message from {}", peer_addr);
                    return Ok(());
                }
            };
            ignore_error(
                || nm.handle_get_module_by_hash(peer_addr, protocol_msg),
                &format!("Error handling GetModuleByHash from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::ModuleByHashReceived(data, _peer_addr) => {
            if let Ok(ProtocolMessage::ModuleByHash(msg)) = ProtocolParser::parse_message(&data) {
                nm.complete_request(msg.request_id, data);
            }
        }
        NetworkMessage::GetModuleListReceived(data, peer_addr) => {
            let protocol_msg = match ProtocolParser::parse_message(&data) {
                Ok(ProtocolMessage::GetModuleList(msg)) => msg,
                _ => {
                    debug!("Invalid GetModuleList message from {}", peer_addr);
                    return Ok(());
                }
            };
            ignore_error(
                || nm.handle_get_module_list(peer_addr, protocol_msg),
                &format!("Error handling GetModuleList from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::ModuleListReceived(_data, _peer_addr) => {
            // ModuleList has no request_id; direct response, no completion needed
            debug!("ModuleListReceived (no request matching)");
        }
        NetworkMessage::GetPaymentRequestReceived(data, peer_addr) => {
            let protocol_msg = match ProtocolParser::parse_message(&data) {
                Ok(ProtocolMessage::GetPaymentRequest(msg)) => msg,
                _ => {
                    debug!("Invalid GetPaymentRequest message from {}", peer_addr);
                    return Ok(());
                }
            };
            ignore_error(
                || nm.handle_get_payment_request(peer_addr, protocol_msg),
                &format!("Error handling GetPaymentRequest from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::PaymentReceived(data, peer_addr) => {
            let protocol_msg = match ProtocolParser::parse_message(&data) {
                Ok(ProtocolMessage::Payment(msg)) => msg,
                _ => {
                    debug!("Invalid Payment message from {}", peer_addr);
                    return Ok(());
                }
            };
            ignore_error(
                || nm.handle_payment(peer_addr, protocol_msg),
                &format!("Error handling Payment from {}", peer_addr),
            )
            .await;
        }
        NetworkMessage::PaymentRequestReceived(_data, _peer_addr)
        | NetworkMessage::PaymentACKReceived(_data, _peer_addr) => {
            debug!("Payment response received (handled via complete_request if applicable)");
        }
        NetworkMessage::HeadersReceived(_data, _peer_addr) => {
            // Headers are handled in wire_dispatch via complete_headers_request
            debug!("HeadersReceived (unexpected in queue - normally handled in wire_dispatch)");
        }
        NetworkMessage::MeshPacketReceived(data, peer_addr) => {
            let payload = EventPayload::MeshPacketReceived {
                packet_data: data,
                peer_addr: peer_addr.to_string(),
            };
            publish_network_event_if_configured(
                nm.event_publisher(),
                EventType::MeshPacketReceived,
                payload,
                "Failed to publish MeshPacketReceived",
            )
            .await;
        }
        #[cfg(feature = "stratum-v2")]
        NetworkMessage::StratumV2MessageReceived(data, peer_addr) => {
            let payload = EventPayload::StratumV2MessageReceived {
                message_data: data,
                peer_addr: peer_addr.to_string(),
            };
            publish_network_event_if_configured(
                nm.event_publisher(),
                EventType::StratumV2MessageReceived,
                payload,
                "Failed to publish StratumV2MessageReceived",
            )
            .await;
        }
        NetworkMessage::SettlementNotificationReceived(_data, _peer_addr) => {
            debug!("SettlementNotificationReceived");
        }
        #[cfg(feature = "ctv")]
        NetworkMessage::PaymentProofReceived(_data, _peer_addr) => {
            debug!("PaymentProofReceived");
        }
        #[cfg(feature = "utxo-commitments")]
        NetworkMessage::UTXOSetReceived(data, _peer_addr) => {
            if let Ok(ProtocolMessage::UTXOSet(msg)) = ProtocolParser::parse_message(&data) {
                nm.complete_request(msg.request_id, data);
            }
        }
        #[cfg(feature = "utxo-commitments")]
        NetworkMessage::FilteredBlockReceived(data, _peer_addr) => {
            if let Ok(ProtocolMessage::FilteredBlock(msg)) = ProtocolParser::parse_message(&data) {
                nm.complete_request(msg.request_id, data);
            }
        }
        #[cfg(feature = "utxo-commitments")]
        NetworkMessage::GetUTXOSetReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_get_utxo_set_request(data, peer_addr),
                &format!("Error handling GetUTXOSet from {}", peer_addr),
            )
            .await;
        }
        #[cfg(feature = "utxo-commitments")]
        NetworkMessage::GetFilteredBlockReceived(data, peer_addr) => {
            ignore_error(
                || nm.handle_get_filtered_block_request(data, peer_addr),
                &format!("Error handling GetFilteredBlock from {}", peer_addr),
            )
            .await;
        }
    }
    Ok(())
}

async fn handle_peer_connected(nm: &NetworkManager, addr: TransportAddr) {
    info!("Peer connected: {:?}", addr);

    let socket_addr = match &addr {
        TransportAddr::Tcp(sock) => Some(*sock),
        #[cfg(feature = "quinn")]
        TransportAddr::Quinn(sock) => Some(*sock),
        #[cfg(feature = "iroh")]
        TransportAddr::Iroh(_) => None,
    };

    if let Some(peer_socket) = socket_addr {
        let start_height = nm
            .storage()
            .as_ref()
            .and_then(|s| s.chain().get_height().ok().flatten())
            .unwrap_or(0) as i32;

        let peer_ip = match peer_socket.ip() {
            std::net::IpAddr::V4(ip) => {
                let mut addr_bytes = [0u8; 16];
                addr_bytes[10] = 0xff;
                addr_bytes[11] = 0xff;
                addr_bytes[12..16].copy_from_slice(&ip.octets());
                addr_bytes
            }
            std::net::IpAddr::V6(ip) => ip.octets(),
        };
        let addr_recv = NetworkAddress {
            services: 0,
            ip: peer_ip,
            port: peer_socket.port(),
        };
        let addr_from = NetworkAddress {
            services: 0,
            ip: [0u8; 16],
            port: 0,
        };

        let version_msg = nm.create_version_message(
            70015,
            0,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            addr_recv,
            addr_from,
            rand::random::<u64>(),
            format!("/Bitcoin Commons:{}/", env!("CARGO_PKG_VERSION")),
            start_height,
            true,
        );

        match ProtocolParser::serialize_message(&ProtocolMessage::Version(version_msg)) {
            Ok(wire_msg) => {
                if let Err(e) = nm.send_to_peer_by_transport(addr.clone(), wire_msg).await {
                    warn!("Failed to send Version message to {:?}: {}", addr, e);
                } else {
                    info!("Sent Version message to {:?}", addr);
                }
            }
            Err(e) => {
                warn!("Failed to serialize Version message for {:?}: {}", addr, e);
            }
        }
    }

    let event_publisher_guard = nm.event_publisher().lock().await;
    if let Some(ref event_publisher) = *event_publisher_guard {
        let addr_str = format!("{addr:?}");
        let transport_type = match &addr {
            TransportAddr::Tcp(_) => "tcp",
            #[cfg(feature = "quinn")]
            TransportAddr::Quinn(_) => "quinn",
            #[cfg(feature = "iroh")]
            TransportAddr::Iroh(_) => "iroh",
        };

        let pm = nm.peer_manager_mutex().lock().await;
        let (services, version) = if let Some(peer) = pm.get_peer(&addr) {
            (peer.services(), peer.version())
        } else {
            (0, 0)
        };
        drop(pm);

        let event_pub_clone = Arc::clone(event_publisher);
        tokio::spawn(async move {
            event_pub_clone
                .publish_peer_connected(&addr_str, transport_type, services, version)
                .await;
        });
    }
}

#[allow(irrefutable_let_patterns)] // TransportAddr is TCP-only without quinn/iroh features
async fn handle_peer_disconnected(nm: &NetworkManager, addr: TransportAddr) {
    info!("Peer disconnected: {:?}", addr);
    let mut pm = nm.peer_manager_mutex().lock().await;
    pm.remove_peer(&addr);
    drop(pm);

    // Enqueue TCP persistent peers for automatic reconnect (see `start_peer_reconnection_task`).
    // The periodic task used to bail out when `current_peers >= min_peers`, so a LAN node could
    // stay gone while many WAN peers kept the count high.
    if let TransportAddr::Tcp(sock) = addr {
        let persistent = nm.get_persistent_peers().await;
        if persistent.contains(&sock) {
            let mut q = nm.peer_reconnection_queue().lock().await;
            q.entry(sock).or_insert((0, current_timestamp(), 1.0));
            info!(
                "Persistent peer {} queued for automatic TCP reconnection",
                sock
            );
        }
    }

    let event_publisher_guard = nm.event_publisher().lock().await;
    if let Some(ref event_publisher) = *event_publisher_guard {
        let addr_str = format!("{addr:?}");
        let event_pub_clone = Arc::clone(event_publisher);
        tokio::spawn(async move {
            event_pub_clone
                .publish_peer_disconnected(&addr_str, "disconnected")
                .await;
        });
    }
}
