//! One accepted TCP peer must not queue unbounded max-sized frames, and a bad
//! checksum or the wrong network magic must disconnect and ban that peer.

use std::net::SocketAddr;
use std::time::Duration;

use blvm_node::network::peer::Peer;
use blvm_node::network::protocol::ProtocolParser;
use blvm_node::network::{NetworkManager, NetworkMessage};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const FRAME_PAYLOAD: usize = 64 * 1024;

fn frame(command: &str, payload: &[u8]) -> Vec<u8> {
    ProtocolParser::serialize_command_payload(command, payload).expect("frame")
}

fn public_peer() -> SocketAddr {
    "203.0.113.10:8333".parse().unwrap()
}

async fn pair() -> (TcpStream, TcpStream, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).await.unwrap();
    let (server, peer_addr) = listener.accept().await.unwrap();
    (client, server, peer_addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_peer_cannot_queue_a_second_max_sized_frame_until_the_first_is_dropped() {
    let (mut client, server, peer_addr) = pair().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let max_message_length = FRAME_PAYLOAD + 24;
    let _peer = Peer::from_tcp_stream_split(server, peer_addr, tx, max_message_length);

    let payload = vec![0x11u8; FRAME_PAYLOAD];
    let bytes = frame("ping", &payload);
    client.write_all(&bytes).await.unwrap();
    client.write_all(&bytes).await.unwrap();
    client.flush().await.unwrap();

    let first = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first frame")
        .expect("channel open");
    match &first {
        NetworkMessage::RawMessageReceived(data, _) => {
            assert_eq!(data.len(), bytes.len());
        }
        other => panic!("expected raw frame, got {other:?}"),
    }

    let queued_while_held = timeout(Duration::from_millis(400), rx.recv()).await;
    assert!(
        queued_while_held.is_err(),
        "a second max-sized frame was queued while the first was still held"
    );

    drop(first);
    let second = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("budget released")
        .expect("channel open");
    match second {
        NetworkMessage::RawMessageReceived(data, _) => assert_eq!(data.len(), bytes.len()),
        other => panic!("expected second raw frame, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_magic_is_not_queued_in_full_and_bans_the_peer() {
    let (mut client, server, peer_addr) = pair().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _peer = Peer::from_tcp_stream_split(server, peer_addr, tx, FRAME_PAYLOAD + 24);

    let mut bytes = frame("ping", &vec![0x22u8; FRAME_PAYLOAD]);
    bytes[0] ^= 0xff;
    client.write_all(&bytes).await.unwrap();
    client.flush().await.unwrap();

    let queued = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("reader signals the bad frame")
        .expect("channel open");
    match queued {
        NetworkMessage::RawMessageReceived(data, _) => {
            assert!(
                data.len() <= 24,
                "bad magic queued {} bytes; payload must not be buffered",
                data.len()
            );
        }
        NetworkMessage::PeerDisconnected(_) => {}
        other => panic!("unexpected message {other:?}"),
    }

    let nm = NetworkManager::new("127.0.0.1:0".parse().unwrap());
    let peer = public_peer();
    let err = nm
        .handle_incoming_wire_tcp(peer, bytes[..24].to_vec())
        .await
        .expect_err("bad magic disconnects");
    assert!(
        err.to_string()
            .contains("bitcoin framing: bad network magic"),
        "{err}"
    );
    assert!(nm.is_banned(peer), "bad network magic does not ban");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_checksum_is_not_queued_in_full_and_bans_the_peer() {
    let (mut client, server, peer_addr) = pair().await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _peer = Peer::from_tcp_stream_split(server, peer_addr, tx, FRAME_PAYLOAD + 24);

    let mut bytes = frame("ping", &vec![0x33u8; FRAME_PAYLOAD]);
    bytes[20] ^= 0xff;
    client.write_all(&bytes).await.unwrap();
    client.flush().await.unwrap();

    let queued = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("reader signals the bad checksum")
        .expect("channel open");
    match queued {
        NetworkMessage::RawMessageReceived(data, _) => {
            assert!(
                data.len() <= 24,
                "bad checksum queued {} bytes; payload must not stay queued",
                data.len()
            );
        }
        NetworkMessage::PeerDisconnected(_) => {}
        other => panic!("unexpected message {other:?}"),
    }

    let nm = NetworkManager::new("127.0.0.1:0".parse().unwrap());
    let peer = public_peer();
    let mut small = frame("verack", &[]);
    small[20] ^= 0xff;
    let err = nm
        .handle_incoming_wire_tcp(peer, small)
        .await
        .expect_err("bad checksum disconnects");
    assert!(
        err.to_string().contains("bitcoin framing: bad checksum"),
        "{err}"
    );
    assert!(nm.is_banned(peer), "bad checksum does not ban");

    let local: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let mut local_frame = frame("verack", &[]);
    local_frame[20] ^= 0xff;
    let err = nm
        .handle_incoming_wire_tcp(local, local_frame)
        .await
        .expect_err("local peer is still disconnected");
    assert!(
        err.to_string().contains("bitcoin framing: bad checksum"),
        "{err}"
    );
    assert!(
        !nm.is_banned(local),
        "loopback stays unbanned, matching other protocol violations"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_frame_and_unknown_command_do_not_ban() {
    let nm = NetworkManager::new("127.0.0.1:0".parse().unwrap());
    let peer = public_peer();

    let err = nm
        .handle_incoming_wire_tcp(peer, vec![0xff])
        .await
        .expect_err("short frame");
    assert!(
        !err.to_string().contains("bitcoin framing:"),
        "short frame must stay a parse error, got {err}"
    );
    assert!(!nm.is_banned(peer));

    let unknown = frame("zzzz", &[1, 2, 3, 4]);
    let err = nm
        .handle_incoming_wire_tcp(peer, unknown)
        .await
        .expect_err("unknown command");
    assert!(
        !err.to_string().contains("bitcoin framing:"),
        "unknown command must not be treated as bad magic or checksum, got {err}"
    );
    assert!(!nm.is_banned(peer));
}
