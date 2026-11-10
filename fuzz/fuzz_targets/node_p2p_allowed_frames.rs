#![no_main]
//! Valid P2P frames for [`ALLOWED_COMMANDS`]: checksum-correct payloads through node `ProtocolParser`.
use blvm_node::network::protocol::{ALLOWED_COMMANDS, ProtocolParser};
use blvm_protocol::p2p_frame::{build_p2p_frame, parse_p2p_frame};
use blvm_protocol::p2p_framing::{BITCOIN_MAGIC_MAINNET, BITCOIN_P2P_MAGIC_MAINNET_LE};
use blvm_protocol::wire::MAX_MESSAGE_PAYLOAD;
use libfuzzer_sys::fuzz_target;

fn allowed(c: &str) -> bool {
    ALLOWED_COMMANDS.iter().any(|&a| a == c)
}

fuzz_target!(|data: &[u8]| {
    let _ = ProtocolParser::calculate_checksum(data);
    if data.len() > 4 {
        let _ = ProtocolParser::calculate_checksum(&data[..data.len() / 2]);
    }

    if data.is_empty() {
        return;
    }

    let idx = (data[0] as usize) % ALLOWED_COMMANDS.len();
    let cmd = ALLOWED_COMMANDS[idx];
    let payload = &data[1..];
    let payload = if payload.len() > MAX_MESSAGE_PAYLOAD {
        &payload[..MAX_MESSAGE_PAYLOAD]
    } else {
        payload
    };

    if let Ok(frame) = build_p2p_frame(BITCOIN_MAGIC_MAINNET, cmd, payload) {
        let _ = parse_p2p_frame(&frame, BITCOIN_P2P_MAGIC_MAINNET_LE, allowed);
        let _ = ProtocolParser::parse_message(&frame);
        if let Ok(msg) = ProtocolParser::parse_message(&frame) {
            if let Ok(bytes) = ProtocolParser::serialize_message(&msg) {
                let _ = ProtocolParser::parse_message(&bytes);
            }
        }
    }

    if data.len() > 2 {
        let idx2 = (data[1] as usize) % ALLOWED_COMMANDS.len();
        let payload2 = &data[2..];
        let payload2 = if payload2.len() > MAX_MESSAGE_PAYLOAD {
            &payload2[..MAX_MESSAGE_PAYLOAD]
        } else {
            payload2
        };
        if let Ok(frame2) = build_p2p_frame(BITCOIN_MAGIC_MAINNET, ALLOWED_COMMANDS[idx2], payload2)
        {
            let _ = ProtocolParser::parse_message(&frame2);
        }
    }
});
