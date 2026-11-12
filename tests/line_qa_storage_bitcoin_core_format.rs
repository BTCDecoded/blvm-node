//! Test-line QA: a short best-block value is not a block hash.

use blvm_node::storage::bitcoin_core_format::parse_best_block_value;

#[test]
fn parse_best_block_value_rejects_a_truncated_value() {
    match parse_best_block_value(&[0xab]) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Best block value must be 32 bytes, got 1"),
                "parse_best_block_value returned {message}"
            );
        }
        Ok(hash) => panic!("truncated value decoded as {hash:?}"),
    }
}
