//! Test-line QA: a short assume-valid hash is rejected.

use blvm_node::config::assume_valid_hash_from_display_hex;

#[test]
fn assume_valid_hash_rejects_a_short_hex_string() {
    match assume_valid_hash_from_display_hex("abcd") {
        Err(message) => {
            assert!(
                message.contains(
                    "assume_valid_hash must be a 64-character block hash or an array of 32 bytes"
                ),
                "assume_valid_hash_from_display_hex returned {message}"
            );
        }
        Ok(hash) => panic!("short hash was accepted: {hash:?}"),
    }
}
