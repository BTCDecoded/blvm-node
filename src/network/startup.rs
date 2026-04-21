//! Transport initialization and listener startup for NetworkManager.
//!
//! Handles Quinn/Iroh transport init, TCP/Quinn/Iroh listener setup,
//! and connection accept loops with DoS protection.

use crate::network::network_manager::NetworkManager;
use crate::network::peer;
use crate::network::transport::{Transport, TransportAddr, TransportListener};
use crate::network::NetworkMessage;
use crate::utils::current_timestamp;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Initialize Quinn transport if enabled and store in NetworkManager
#[cfg(feature = "quinn")]
pub(crate) async fn init_quinn_transport(nm: &NetworkManager) -> Result<()> {
    use crate::network::transport::TransportPreference;

    if !nm.transport_preference().allows_quinn() {
        return Ok(());
    }
    let max_msg_len = nm.protocol_limits().max_protocol_message_length;
    match crate::network::quinn_transport::QuinnTransport::with_max_message_length(max_msg_len) {
        Ok(quinn) => {
            if let Ok(mut guard) = nm.quinn_transport().lock() {
                *guard = Some(Arc::new(quinn));
            }
            info!("Quinn transport initialized");
        }
        Err(e) => {
            warn!("Failed to initialize Quinn transport: {}", e);
            if nm.transport_preference() == TransportPreference::QUINN_ONLY {
                return Err(anyhow::anyhow!("Quinn-only mode requires Quinn transport"));
            }
        }
    }
    Ok(())
}

/// Initialize Iroh transport if enabled and store in NetworkManager
#[cfg(feature = "iroh")]
pub(crate) async fn init_iroh_transport(nm: &NetworkManager) -> Result<()> {
    use crate::network::transport::TransportPreference;

    if !nm.transport_preference().allows_iroh() {
        return Ok(());
    }
    let max_msg_len = nm.protocol_limits().max_protocol_message_length;
    match crate::network::iroh_transport::IrohTransport::with_max_message_length(max_msg_len).await
    {
        Ok(iroh) => {
            if let Ok(mut guard) = nm.iroh_transport().lock() {
                *guard = Some(iroh);
            }
            info!("Iroh transport initialized");
        }
        Err(e) => {
            warn!("Failed to initialize Iroh transport: {}", e);
            if nm.transport_preference() == TransportPreference::IROH_ONLY {
                return Err(anyhow::anyhow!("Iroh-only mode requires Iroh transport"));
            }
        }
    }
    Ok(())
}

/// Start TCP listener and accept loop
pub(crate) async fn start_tcp_listener(nm: &NetworkManager, listen_addr: SocketAddr) -> Result<()> {
    use crate::network::transport::TransportPreference;

    if !nm.transport_preference().allows_tcp() {
        return Ok(());
    }

    let mut tcp_listener = nm.tcp_transport().listen(listen_addr).await?;
    info!("TCP listener started on {}", listen_addr);

    let peer_tx = nm.peer_tx().clone();
    let dos_protection = Arc::clone(nm.dos_protection());
    let peer_manager_clone = Arc::clone(nm.peer_manager_mutex());
    let ban_list = Arc::clone(nm.ban_list());
    let max_message_length = nm.protocol_limits().max_protocol_message_length;

    tokio::spawn(async move {
        loop {
            match tcp_listener.accept_stream().await {
                Ok((stream, socket_addr)) => {
                    info!("New TCP connection from {:?}", socket_addr);

                    let ip = socket_addr.ip();
                    if !dos_protection.check_connection(ip).await {
                        warn!(
                            "Connection rate limit exceeded for IP {}, rejecting connection",
                            ip
                        );
                        if dos_protection.should_auto_ban(ip).await {
                            warn!(
                                "Auto-banning IP {} for repeated connection rate violations",
                                ip
                            );
                            let ban_duration = dos_protection.ban_duration_seconds();
                            let unban_timestamp = crate::utils::current_timestamp() + ban_duration;
                            let mut ban_list_guard = ban_list.write().await;
                            ban_list_guard.insert(socket_addr, unban_timestamp);
                        }
                        drop(stream);
                        continue;
                    }

                    let current_connections = {
                        let pm = peer_manager_clone.lock().await;
                        pm.peer_count()
                    };
                    if !dos_protection
                        .check_active_connections(current_connections)
                        .await
                    {
                        warn!(
                            "Active connection limit exceeded, rejecting connection from {}",
                            socket_addr
                        );
                        drop(stream);
                        continue;
                    }

                    let transport_addr_tcp = TransportAddr::Tcp(socket_addr);
                    let _ = peer_tx.send(NetworkMessage::PeerConnected(transport_addr_tcp.clone()));

                    let peer_tx_clone = peer_tx.clone();
                    let peer_manager_for_peer = Arc::clone(&peer_manager_clone);
                    let transport_addr_for_peer = transport_addr_tcp;
                    tokio::spawn(async move {
                        let peer = peer::Peer::from_tcp_stream_split(
                            stream,
                            socket_addr,
                            peer_tx_clone.clone(),
                            max_message_length,
                        );

                        let mut pm = peer_manager_for_peer.lock().await;
                        if let Err(e) = pm.add_peer(transport_addr_for_peer.clone(), peer) {
                            warn!("Failed to add peer {}: {}", socket_addr, e);
                            let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                transport_addr_for_peer.clone(),
                            ));
                            return;
                        }
                        info!(
                            "Successfully added peer {} (transport: {:?})",
                            socket_addr, transport_addr_for_peer
                        );
                        drop(pm);
                    });
                }
                Err(e) => {
                    error!("Failed to accept TCP connection: {}", e);
                }
            }
        }
    });

    Ok(())
}

/// Start Quinn listener if available
#[cfg(feature = "quinn")]
pub(crate) async fn start_quinn_listener(
    nm: &NetworkManager,
    listen_addr: SocketAddr,
) -> Result<()> {
    let quinn_transport = nm
        .quinn_transport()
        .lock()
        .ok()
        .and_then(|g| g.as_ref().cloned());

    let Some(quinn_transport) = quinn_transport else {
        return Ok(());
    };

    match quinn_transport.listen(listen_addr).await {
        Ok(mut quinn_listener) => {
            info!("Quinn listener started on {}", listen_addr);
            let peer_tx = nm.peer_tx().clone();
            let peer_manager = Arc::clone(nm.peer_manager_mutex());
            let dos_protection = Arc::clone(nm.dos_protection());
            let ban_list = Arc::clone(nm.ban_list());

            tokio::spawn(async move {
                loop {
                    match quinn_listener.accept().await {
                        Ok((conn, addr)) => {
                            info!("New Quinn connection from {:?}", addr);
                            let socket_addr = match addr {
                                TransportAddr::Quinn(addr) => addr,
                                _ => {
                                    error!("Invalid transport address for Quinn");
                                    continue;
                                }
                            };

                            let ip = socket_addr.ip();
                            if !dos_protection.check_connection(ip).await {
                                warn!(
                                    "Connection rate limit exceeded for IP {}, rejecting Quinn connection",
                                    ip
                                );
                                if dos_protection.should_auto_ban(ip).await {
                                    warn!(
                                        "Auto-banning IP {} for repeated connection rate violations",
                                        ip
                                    );
                                    let ban_duration = dos_protection.ban_duration_seconds();
                                    let unban_timestamp = current_timestamp() + ban_duration;
                                    let mut ban_list_guard = ban_list.write().await;
                                    ban_list_guard.insert(socket_addr, unban_timestamp);
                                }
                                drop(conn);
                                continue;
                            }

                            let current_connections = {
                                let pm = peer_manager.lock().await;
                                pm.peer_count()
                            };
                            if !dos_protection
                                .check_active_connections(current_connections)
                                .await
                            {
                                warn!(
                                    "Active connection limit exceeded, rejecting Quinn connection from {}",
                                    socket_addr
                                );
                                drop(conn);
                                continue;
                            }

                            let quinn_transport_addr = TransportAddr::Quinn(socket_addr);
                            let _ = peer_tx
                                .send(NetworkMessage::PeerConnected(quinn_transport_addr.clone()));

                            let peer_tx_clone = peer_tx.clone();
                            let peer_manager_clone = Arc::clone(&peer_manager);
                            tokio::spawn(async move {
                                let quinn_addr = TransportAddr::Quinn(socket_addr);
                                let quinn_addr_clone = quinn_addr.clone();
                                let peer = peer::Peer::from_transport_connection(
                                    conn,
                                    socket_addr,
                                    quinn_addr,
                                    peer_tx_clone.clone(),
                                );

                                let mut pm = peer_manager_clone.lock().await;
                                if let Err(e) = pm.add_peer(quinn_addr_clone.clone(), peer) {
                                    warn!("Failed to add Quinn peer {}: {}", socket_addr, e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        quinn_addr_clone.clone(),
                                    ));
                                    return;
                                }
                                info!("Successfully added Quinn peer {}", socket_addr);
                                drop(pm);
                            });
                        }
                        Err(e) => {
                            warn!("Failed to accept Quinn connection (continuing): {}", e);
                        }
                    }
                }
            });
        }
        Err(e) => {
            warn!(
                "Failed to start Quinn listener (graceful degradation): {}",
                e
            );
        }
    }

    Ok(())
}

/// Start Iroh listener if available
#[cfg(feature = "iroh")]
pub(crate) async fn start_iroh_listener(
    nm: &NetworkManager,
    listen_addr: SocketAddr,
) -> Result<()> {
    let listen_result = {
        let guard = match nm.iroh_transport().lock() {
            Ok(g) => g,
            Err(_) => return Ok(()),
        };
        let Some(transport) = guard.as_ref() else {
            return Ok(());
        };
        transport.listen(listen_addr).await
    };

    let mut iroh_listener = match listen_result {
        Ok(l) => l,
        Err(e) => {
            warn!(
                "Failed to start Iroh listener (graceful degradation): {}",
                e
            );
            return Ok(());
        }
    };

    info!("Iroh listener started on {}", listen_addr);
            let peer_tx = nm.peer_tx().clone();
            let peer_manager = Arc::clone(nm.peer_manager_mutex());
            let dos_protection = Arc::clone(nm.dos_protection());
            let address_database = Arc::clone(nm.address_database());
            let socket_to_transport = Arc::clone(nm.socket_to_transport());

            tokio::spawn(async move {
                loop {
                    match iroh_listener.accept().await {
                        Ok((conn, addr)) => {
                            info!("New Iroh connection from {:?}", addr);
                            let iroh_addr = match &addr {
                                TransportAddr::Iroh(key) => {
                                    if key.is_empty() {
                                        warn!("Invalid Iroh public key: empty");
                                        continue;
                                    }
                                    addr.clone()
                                }
                                _ => {
                                    error!("Invalid transport address for Iroh");
                                    continue;
                                }
                            };

                            let current_connections = {
                                let pm = peer_manager.lock().await;
                                pm.peer_count()
                            };
                            if !dos_protection
                                .check_active_connections(current_connections)
                                .await
                            {
                                warn!(
                                    "Active connection limit exceeded, rejecting Iroh connection"
                                );
                                drop(conn);
                                continue;
                            }

                            let _ = peer_tx.send(NetworkMessage::PeerConnected(iroh_addr.clone()));

                            let peer_tx_clone = peer_tx.clone();
                            let peer_manager_clone = Arc::clone(&peer_manager);
                            let iroh_addr_clone = iroh_addr.clone();
                            let socket_to_transport_clone = Arc::clone(&socket_to_transport);
                            let address_database_clone = Arc::clone(&address_database);
                            tokio::spawn(async move {
                                let placeholder_socket = if let TransportAddr::Iroh(ref key) =
                                    iroh_addr_clone
                                {
                                    let ip_bytes = if key.len() >= 4 {
                                        [key[0], key[1], key[2], key[3]]
                                    } else {
                                        [0, 0, 0, 0]
                                    };
                                    let port = if key.len() >= 6 {
                                        u16::from_be_bytes([key[key.len() - 2], key[key.len() - 1]])
                                    } else {
                                        0
                                    };
                                    std::net::SocketAddr::from((ip_bytes, port))
                                } else {
                                    std::net::SocketAddr::from(([0, 0, 0, 0], 0))
                                };

                                let peer = peer::Peer::from_transport_connection(
                                    conn,
                                    placeholder_socket,
                                    iroh_addr_clone.clone(),
                                    peer_tx_clone.clone(),
                                );

                                let mut pm = peer_manager_clone.lock().await;
                                if let Err(e) = pm.add_peer(iroh_addr_clone.clone(), peer) {
                                    warn!("Failed to add Iroh peer: {}", e);
                                    let _ = peer_tx_clone.send(NetworkMessage::PeerDisconnected(
                                        iroh_addr_clone.clone(),
                                    ));
                                    return;
                                }
                                drop(pm);

                                socket_to_transport_clone
                                    .lock()
                                    .await
                                    .insert(placeholder_socket, iroh_addr_clone.clone());

                                if let TransportAddr::Iroh(ref node_id_bytes) = iroh_addr_clone {
                                    if node_id_bytes.len() == 32 {
                                        use iroh::PublicKey;
                                        let mut key_array = [0u8; 32];
                                        key_array.copy_from_slice(node_id_bytes);
                                        if let Ok(public_key) = PublicKey::from_bytes(&key_array) {
                                            let address_db_clone = address_database_clone.clone();
                                            tokio::spawn(async move {
                                                let mut db = address_db_clone.write().await;
                                                db.add_iroh_address(public_key, 0);
                                            });
                                        }
                                    }
                                }

                                info!(
                                    "Successfully added Iroh peer (transport: {:?})",
                                    iroh_addr_clone
                                );
                            });
                        }
                        Err(e) => {
                            warn!("Failed to accept Iroh connection (continuing): {}", e);
                        }
                    }
                }
            });

    Ok(())
}
