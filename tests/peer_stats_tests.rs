//! Tests for peer statistics tracking

use bllvm_node::network::peer::Peer;
use std::net::SocketAddr;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_peer_stats_initialization() {
    let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();

    // Create a mock stream
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let stream = tokio::net::TcpStream::connect(local_addr).await.unwrap();

    let peer = Peer::new(stream, addr, tx);

    // Check initial stats
    assert!(peer.conntime() > 0);
    assert_eq!(peer.conntime(), peer.last_send());
    assert_eq!(peer.conntime(), peer.last_recv());
    assert_eq!(peer.bytes_sent(), 0);
    assert_eq!(peer.bytes_recv(), 0);
}

#[tokio::test]
async fn test_peer_record_send() {
    let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let stream = tokio::net::TcpStream::connect(local_addr).await.unwrap();

    let mut peer = Peer::new(stream, addr, tx);

    let initial_send = peer.last_send();
    let initial_bytes = peer.bytes_sent();

    // Record a send
    peer.record_send(100);

    assert!(peer.last_send() >= initial_send);
    assert_eq!(peer.bytes_sent(), initial_bytes + 100);
}

#[tokio::test]
async fn test_peer_record_receive() {
    let addr: SocketAddr = "127.0.0.1:8333".parse().unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = listener.local_addr().unwrap();
    let stream = tokio::net::TcpStream::connect(local_addr).await.unwrap();

    let mut peer = Peer::new(stream, addr, tx);

    let initial_recv = peer.last_recv();
    let initial_bytes = peer.bytes_recv();

    // Record a receive
    peer.record_receive(200);

    assert!(peer.last_recv() >= initial_recv);
    assert_eq!(peer.bytes_recv(), initial_bytes + 200);
}
