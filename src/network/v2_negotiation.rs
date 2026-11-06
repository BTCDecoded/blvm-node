//! v2 transport negotiation.
//!
//! An opening whose first 16 bytes are `magic || "version" || five NULs` stays on v1.
//! Anything else, when the offer is on, is the ellswift handshake: garbage, the
//! 16-byte terminator, and a version packet that authenticates that side's garbage.
//! [`offer_v2`] defaults off, so a connection that does not set it never leaves v1.

use anyhow::{Result, anyhow};
use blvm_protocol::v2_transport::{
    V2Handshake, V2Session, V2Transport, append_terminator_and_version, begin_initiator,
    initiator_session, responder_accept, v1_version_prefix,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Ellswift public key length.
pub const ELLSWIFT_LEN: usize = 64;

/// The offer is off. Ordinary connections stay on the v1 reader.
pub fn offer_v2() -> bool {
    false
}

/// What the first bytes of a connection are.
pub enum Opening {
    /// Keep these bytes for the v1 reader.
    V1 { prefix: Vec<u8> },
    /// Responder reply and the transport that decrypts the initiator.
    V2 {
        reply: Vec<u8>,
        transport: V2Transport,
    },
}

/// True when `bytes` begin with the 16-byte v1 version prefix.
pub fn opening_is_v1(bytes: &[u8], magic: [u8; 4]) -> bool {
    let prefix = v1_version_prefix(magic);
    bytes.len() >= prefix.len() && bytes[..prefix.len()] == prefix
}

/// Outbound side. The returned bytes are the 64-byte ellswift, before garbage.
pub fn begin_outbound() -> Result<(Vec<u8>, V2Handshake)> {
    let (opening, handshake) = V2Handshake::new_initiator();
    if opening.len() != ELLSWIFT_LEN {
        return Err(anyhow!("v2 opening is {} bytes", opening.len()));
    }
    Ok((opening, handshake))
}

/// Finish the outbound ellswift exchange with the peer's 64-byte reply.
pub fn finish_outbound(handshake: V2Handshake, reply: &[u8]) -> Result<V2Transport> {
    handshake
        .complete_handshake(reply)
        .map_err(|err| anyhow!("v2 handshake: {err}"))
}

/// Inbound side. `offer_v2` false, or a v1 version prefix, stays on v1 and returns the bytes unchanged.
pub fn accept_inbound(opening: &[u8], magic: [u8; 4], offer_v2: bool) -> Result<Opening> {
    if !offer_v2 || opening_is_v1(opening, magic) {
        return Ok(Opening::V1 {
            prefix: opening.to_vec(),
        });
    }
    if opening.len() != ELLSWIFT_LEN {
        return Err(anyhow!(
            "v2 opening is {} bytes, need {ELLSWIFT_LEN}",
            opening.len()
        ));
    }
    let mut handshake = V2Handshake::new_responder();
    let (reply, transport) = handshake
        .process_initiator_message(opening)
        .map_err(|err| anyhow!("v2 handshake: {err}"))?;
    if reply.len() != ELLSWIFT_LEN {
        return Err(anyhow!("v2 reply is {} bytes", reply.len()));
    }
    Ok(Opening::V2 { reply, transport })
}

/// Run the handshake only when [`offer_v2`] is set. The stream is left unread when it is not.
pub async fn handshake_if_offered<S>(
    stream: &mut S,
    initiating: bool,
    magic: [u8; 4],
    garbage: &[u8],
    version: &[u8],
) -> Result<Option<V2Session>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !offer_v2() {
        return Ok(None);
    }
    let (session, _) = if initiating {
        handshake_initiator(stream, magic, garbage, version).await?
    } else {
        handshake_responder(stream, magic, garbage, version).await?
    };
    Ok(Some(session))
}

/// Initiator handshake on a byte stream. Returns the session and the responder's version contents.
pub async fn handshake_initiator<S>(
    stream: &mut S,
    magic: [u8; 4],
    garbage: &[u8],
    version: &[u8],
) -> Result<(V2Session, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (opening, pending) =
        begin_initiator(garbage).map_err(|err| anyhow!("v2 handshake: {err}"))?;
    stream
        .write_all(&opening)
        .await
        .map_err(|err| anyhow!("v2 write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| anyhow!("v2 flush: {err}"))?;
    let mut their_ell = [0u8; ELLSWIFT_LEN];
    stream
        .read_exact(&mut their_ell)
        .await
        .map_err(|err| anyhow!("v2 read: {err}"))?;
    let mut session = initiator_session(&pending, &their_ell, magic)
        .map_err(|err| anyhow!("v2 handshake: {err}"))?;
    let their_garbage = read_garbage(stream, &session.recv_garbage_terminator).await?;
    let their_version = read_version_packet(stream, &mut session, &their_garbage).await?;
    let tail = append_terminator_and_version(&mut session, pending.garbage(), version)
        .map_err(|err| anyhow!("v2 handshake: {err}"))?;
    stream
        .write_all(&tail)
        .await
        .map_err(|err| anyhow!("v2 write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| anyhow!("v2 flush: {err}"))?;
    Ok((session, their_version))
}

/// Responder handshake on a byte stream. Returns the session and the initiator's version contents.
pub async fn handshake_responder<S>(
    stream: &mut S,
    magic: [u8; 4],
    garbage: &[u8],
    version: &[u8],
) -> Result<(V2Session, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut their_ell = [0u8; ELLSWIFT_LEN];
    stream
        .read_exact(&mut their_ell)
        .await
        .map_err(|err| anyhow!("v2 read: {err}"))?;
    if opening_is_v1(&their_ell, magic) {
        return Err(anyhow!("v2 opening is a v1 version prefix"));
    }
    let pending = responder_accept(&their_ell, garbage, magic)
        .map_err(|err| anyhow!("v2 handshake: {err}"))?;
    let (wire, mut session) = pending
        .send_version(version)
        .map_err(|err| anyhow!("v2 handshake: {err}"))?;
    stream
        .write_all(&wire)
        .await
        .map_err(|err| anyhow!("v2 write: {err}"))?;
    stream
        .flush()
        .await
        .map_err(|err| anyhow!("v2 flush: {err}"))?;
    let their_garbage = read_garbage(stream, &session.recv_garbage_terminator).await?;
    let their_version = read_version_packet(stream, &mut session, &their_garbage).await?;
    Ok((session, their_version))
}

/// Read one application packet. Empty associated data. `None` when the ignore bit is set.
pub async fn read_application_packet<S>(
    stream: &mut S,
    session: &mut V2Session,
) -> Result<Option<Vec<u8>>>
where
    S: AsyncRead + Unpin,
{
    read_packet_ignore(stream, session, &[]).await
}

async fn read_garbage<S>(stream: &mut S, terminator: &[u8; 16]) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < blvm_protocol::v2_transport::GARBAGE_MAX + terminator.len() {
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|err| anyhow!("v2 garbage: {err}"))?;
        buf.push(byte[0]);
        if buf.len() >= terminator.len() && &buf[buf.len() - terminator.len()..] == terminator {
            buf.truncate(buf.len() - terminator.len());
            return Ok(buf);
        }
    }
    Err(anyhow!("v2 garbage terminator not found"))
}

async fn read_version_packet<S>(
    stream: &mut S,
    session: &mut V2Session,
    garbage: &[u8],
) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    let mut first = true;
    loop {
        let aad: &[u8] = if first { garbage } else { &[] };
        first = false;
        if let Some(contents) = read_packet_ignore(stream, session, aad).await? {
            return Ok(contents);
        }
    }
}

async fn read_packet_ignore<S>(
    stream: &mut S,
    session: &mut V2Session,
    aad: &[u8],
) -> Result<Option<Vec<u8>>>
where
    S: AsyncRead + Unpin,
{
    let mut enc_len = [0u8; 3];
    stream
        .read_exact(&mut enc_len)
        .await
        .map_err(|err| anyhow!("v2 packet: {err}"))?;
    let contents_len = session
        .decrypt_length(&enc_len)
        .map_err(|err| anyhow!("v2 packet: {err}"))?;
    let mut body = vec![0u8; 1 + contents_len + 16];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|err| anyhow!("v2 packet: {err}"))?;
    let (contents, ignore) = session
        .decrypt_body(&body, aad)
        .map_err(|err| anyhow!("v2 packet: {err}"))?;
    if contents.len() != contents_len {
        return Err(anyhow!("v2 packet length does not match"));
    }
    if ignore { Ok(None) } else { Ok(Some(contents)) }
}

#[cfg(test)]
mod tests {
    use super::{
        Opening, accept_inbound, begin_outbound, finish_outbound, handshake_if_offered,
        handshake_initiator, handshake_responder, offer_v2, opening_is_v1, read_application_packet,
    };
    use crate::network::protocol::{BITCOIN_MAGIC_MAINNET, BITCOIN_MAGIC_REGTEST, ProtocolParser};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn a_magic_prefix_stays_on_v1_and_an_offer_that_is_off_does_not_negotiate() {
        let magic = BITCOIN_MAGIC_MAINNET;
        let mut prefixed = magic.to_vec();
        prefixed.extend_from_slice(b"version\0\0\0\0\0");
        assert!(opening_is_v1(&prefixed, magic));
        match accept_inbound(&prefixed, magic, true).unwrap() {
            Opening::V1 { prefix } => assert_eq!(prefix, prefixed),
            Opening::V2 { .. } => panic!("version prefix left v1"),
        }
        let mut magic_only = magic.to_vec();
        magic_only.extend_from_slice(&[0u8; 12]);
        assert!(!opening_is_v1(&magic_only, magic));
        assert!(!offer_v2());
        let (opening, _) = begin_outbound().unwrap();
        match accept_inbound(&opening, magic, false).unwrap() {
            Opening::V1 { prefix } => assert_eq!(prefix, opening),
            Opening::V2 { .. } => panic!("offer off still negotiated v2"),
        }
    }

    #[test]
    fn two_local_parties_complete_the_handshake_and_one_packet() {
        let (opening, initiator_hs) = begin_outbound().unwrap();
        let Opening::V2 {
            reply,
            transport: mut responder,
        } = accept_inbound(&opening, BITCOIN_MAGIC_MAINNET, true).unwrap()
        else {
            panic!("64-byte opening stayed on v1");
        };
        let mut initiator = finish_outbound(initiator_hs, &reply).unwrap();
        let plaintext = b"version";
        let packet = initiator.encrypt(plaintext).unwrap();
        let out = responder.decrypt(&packet).unwrap();
        assert_eq!(out, plaintext);
    }

    #[tokio::test]
    async fn an_offer_that_is_off_does_not_read_the_socket() {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"version-bytes-stay").await.unwrap();
        writer.flush().await.unwrap();
        let session = handshake_if_offered(&mut reader, true, BITCOIN_MAGIC_REGTEST, b"", b"")
            .await
            .unwrap();
        assert!(session.is_none());
        let mut buf = [0u8; 18];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"version-bytes-stay");
    }

    #[tokio::test]
    async fn an_offered_socket_runs_the_handshake_and_one_later_frame() {
        let magic = BITCOIN_MAGIC_REGTEST;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (session, version) = handshake_responder(&mut sock, magic, b"rg", b"ver-r")
                .await
                .unwrap();
            assert_eq!(version, b"ver-i");
            assert_ne!(session.session_id, [0u8; 32]);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let peer =
                crate::network::peer::Peer::from_tcp_stream_v2(sock, addr, tx, 4_000_000, session);
            let incoming = rx.recv().await.unwrap();
            let reply = v1_frame(magic, b"verack\0\0\0\0\0\0", b"");
            peer.send_message(reply).await.unwrap();
            incoming
        });
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut session, version) = handshake_initiator(&mut sock, magic, b"ig", b"ver-i")
            .await
            .unwrap();
        assert_eq!(version, b"ver-r");
        let outbound = v1_frame(BITCOIN_MAGIC_MAINNET, b"ping\0\0\0\0\0\0\0\0", &[7u8; 8]);
        let packet = session.encrypt(&outbound).unwrap();
        sock.write_all(&packet).await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("handshake timed out")
            .unwrap();
        match got {
            crate::network::NetworkMessage::RawMessageReceived(data, _) => {
                assert_eq!(data, outbound);
            }
            other => panic!("unexpected peer message: {other:?}"),
        }
        let reply = read_application_packet(&mut sock, &mut session)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply, v1_frame(magic, b"verack\0\0\0\0\0\0", b""));
    }

    #[tokio::test]
    async fn a_decoy_before_the_version_packet_is_skipped() {
        use blvm_protocol::v2_transport::{
            append_handshake, begin_initiator, initiator_session, responder_accept,
        };
        let magic = BITCOIN_MAGIC_REGTEST;
        let (mut initiator, mut responder) = tokio::io::duplex(64 * 1024);
        let (opening, pending) = begin_initiator(b"ig").unwrap();
        initiator.write_all(&opening).await.unwrap();
        let mut ell = [0u8; 64];
        responder.read_exact(&mut ell).await.unwrap();
        let accepted = responder_accept(&ell, b"rg", magic).unwrap();
        let (wire, mut responder_session) =
            accepted.send_with_decoys(&[b"\x00"], b"ver-r").unwrap();
        responder.write_all(&wire).await.unwrap();
        let mut their = [0u8; 64];
        initiator.read_exact(&mut their).await.unwrap();
        let mut session = initiator_session(&pending, &their, magic).unwrap();
        let garbage = super::read_garbage(&mut initiator, &session.recv_garbage_terminator)
            .await
            .unwrap();
        assert_eq!(garbage, b"rg");
        let version = super::read_version_packet(&mut initiator, &mut session, &garbage)
            .await
            .unwrap();
        assert_eq!(version, b"ver-r");
        let tail = append_handshake(&mut session, b"ig", &[b"\x00"], b"ver-i").unwrap();
        initiator.write_all(&tail).await.unwrap();
        let garbage =
            super::read_garbage(&mut responder, &responder_session.recv_garbage_terminator)
                .await
                .unwrap();
        assert_eq!(garbage, b"ig");
        let version = super::read_version_packet(&mut responder, &mut responder_session, &garbage)
            .await
            .unwrap();
        assert_eq!(version, b"ver-i");
    }

    fn v1_frame(magic: [u8; 4], command: &[u8; 12], payload: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 24];
        header[..4].copy_from_slice(&magic);
        header[4..16].copy_from_slice(command);
        header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[20..24].copy_from_slice(&ProtocolParser::calculate_checksum(payload));
        let mut frame = header.to_vec();
        frame.extend_from_slice(payload);
        frame
    }
}
