//! Test-line QA: a truncated wire-body blob does not decode.

use blvm_node::storage::blockstore::{decode_wire_body_blob, encode_wire_body_blob};

#[test]
fn truncated_wire_body_blob_does_not_decode() {
    let payload = b"block-bytes";
    let full = encode_wire_body_blob(payload);
    assert_eq!(decode_wire_body_blob(&full), Some(payload.as_slice()));

    match decode_wire_body_blob(&full[..full.len() - 1]) {
        None => {}
        Some(decoded) => panic!("truncated wire body decoded as {decoded:?}"),
    }
}
