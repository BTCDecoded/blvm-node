//! AssumeValid functional tests
//!
//! Verifies -assumevalid CLI, config wiring, and per-network defaults.

use blvm_node::config::{BlockValidationNodeConfig, default_assume_valid_height_for_network};

#[test]
fn test_default_assume_valid_height_per_network() {
    assert_eq!(default_assume_valid_height_for_network("mainnet"), 912_683);
    assert_eq!(default_assume_valid_height_for_network("Mainnet"), 912_683);
    assert_eq!(
        default_assume_valid_height_for_network("bitcoinv1"),
        912_683
    );
    assert_eq!(
        default_assume_valid_height_for_network("testnet"),
        4_550_000
    );
    assert_eq!(
        default_assume_valid_height_for_network("Testnet"),
        4_550_000
    );
    assert_eq!(default_assume_valid_height_for_network("signet"), 267_665);
    assert_eq!(default_assume_valid_height_for_network("regtest"), 0);
    assert_eq!(default_assume_valid_height_for_network("unknown"), 0);
}

#[test]
fn test_block_validation_config_assume_valid_height() {
    let config = BlockValidationNodeConfig {
        assume_valid_height: 700_000,
        assume_valid_hash: None,
    };
    assert_eq!(config.assume_valid_height, 700_000);
    assert!(config.assume_valid_hash.is_none());
}

#[test]
fn test_block_validation_config_assume_valid_hash() {
    let hash = [0xab; 32];
    let config = BlockValidationNodeConfig {
        assume_valid_height: 0,
        assume_valid_hash: Some(hash),
    };
    assert_eq!(config.assume_valid_height, 0);
    assert_eq!(config.assume_valid_hash, Some(hash));
}

#[test]
fn test_noassumevalid_disables_skip() {
    let config = BlockValidationNodeConfig {
        assume_valid_height: 0,
        assume_valid_hash: None,
    };
    assert_eq!(config.assume_valid_height, 0);
}

#[test]
fn assume_valid_hash_accepts_display_hex_and_byte_array() {
    const DISPLAY: &str = "0000000002368b1e4ee27e2e85676ae6f9f9e69579b29093e9a82c170bf7cf8a";
    let mut internal = hex::decode(DISPLAY).unwrap();
    internal.reverse();

    let from_hex: BlockValidationNodeConfig = toml::from_str(&format!(
        "assume_valid_height = 123613\nassume_valid_hash = \"{DISPLAY}\"\n"
    ))
    .expect("display hash");
    assert_eq!(from_hex.assume_valid_height, 123613);
    assert_eq!(from_hex.assume_valid_hash.unwrap().as_slice(), internal);

    let with_prefix: BlockValidationNodeConfig =
        toml::from_str(&format!("assume_valid_hash = \"0x{DISPLAY}\"\n")).expect("0x display hash");
    assert_eq!(with_prefix.assume_valid_hash.unwrap().as_slice(), internal);

    let bytes = internal
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let from_array: BlockValidationNodeConfig =
        toml::from_str(&format!("assume_valid_hash = [{bytes}]\n")).expect("byte array");
    assert_eq!(from_array.assume_valid_hash.unwrap().as_slice(), internal);

    let absent: BlockValidationNodeConfig =
        toml::from_str("assume_valid_height = 1\n").expect("hash omitted");
    assert!(absent.assume_valid_hash.is_none());

    let err = toml::from_str::<BlockValidationNodeConfig>("assume_valid_hash = \"abcd\"\n");
    assert!(err.is_err());
}
