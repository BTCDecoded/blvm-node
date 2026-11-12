//! Test-line QA: a ping whose checksum does not match the payload is rejected.

use blvm_node::network::protocol::{BITCOIN_MAGIC_MAINNET, ProtocolParser};

#[test]
fn parse_message_rejects_a_bad_checksum() {
    let mut data = [0u8; 32];
    data[0..4].copy_from_slice(&BITCOIN_MAGIC_MAINNET);
    data[4..8].copy_from_slice(b"ping");
    data[16..20].copy_from_slice(&8u32.to_le_bytes());

    match ProtocolParser::parse_message(&data) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid checksum"),
                "parse_message returned {message}"
            );
        }
        Ok(message) => panic!("bad checksum parsed as {message:?}"),
    }
}
