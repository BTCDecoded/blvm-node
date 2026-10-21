//! AssumeValid functional tests
//!
//! Verifies -assumevalid CLI, config wiring, and per-network defaults.

use blvm_node::config::{
    BlockValidationNodeConfig, core_assume_valid_for_network,
    default_assume_valid_height_for_network, resolve_assume_valid,
};

#[test]
fn test_default_assume_valid_height_per_network() {
    assert_eq!(default_assume_valid_height_for_network("mainnet"), 938_343);
    assert_eq!(default_assume_valid_height_for_network("Mainnet"), 938_343);
    assert_eq!(
        default_assume_valid_height_for_network("bitcoinv1"),
        938_343
    );
    assert_eq!(
        default_assume_valid_height_for_network("testnet"),
        4_842_348
    );
    assert_eq!(
        default_assume_valid_height_for_network("Testnet"),
        4_842_348
    );
    assert_eq!(default_assume_valid_height_for_network("signet"), 293_175);
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

/// Display (RPC) hex to internal byte order.
fn from_display(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out.reverse();
    out
}

fn work(hex: &str) -> u128 {
    let trimmed = hex.trim_start_matches('0');
    u128::from_str_radix(trimmed, 16).unwrap()
}

/// Bitcoin Core v31.1 `src/kernel/chainparams.cpp`, copied verbatim.
#[test]
fn test_assume_valid_anchors_match_core_v31_1() {
    let main = core_assume_valid_for_network("mainnet");
    assert_eq!(main.height, 938_343);
    assert_eq!(
        main.hash,
        Some(from_display(
            "00000000000000000000ccebd6d74d9194d8dcdc1d177c478e094bfad51ba5ac"
        ))
    );
    assert_eq!(
        main.min_chain_work,
        work("0000000000000000000000000000000000000001128750f82f4c366153a3a030")
    );

    let test3 = core_assume_valid_for_network("testnet");
    assert_eq!(test3.height, 4_842_348);
    assert_eq!(
        test3.hash,
        Some(from_display(
            "000000007a61e4230b28ac5cb6b5e5a0130de37ac1faf2f8987d2fa6505b67f4"
        ))
    );
    assert_eq!(
        test3.min_chain_work,
        work("0000000000000000000000000000000000000000000017dde1c649f3708d14b6")
    );

    let test4 = core_assume_valid_for_network("testnet4");
    assert_eq!((test4.height, test4.hash), (0, None));
    assert_eq!(
        test4.min_chain_work,
        work("0000000000000000000000000000000000000000000009a0fe15d0177d086304")
    );

    let sig = core_assume_valid_for_network("signet");
    assert_eq!(sig.height, 293_175);
    assert_eq!(
        sig.hash,
        Some(from_display(
            "00000008414aab61092ef93f1aacc54cf9e9f16af29ddad493b908a01ff5c329"
        ))
    );
    assert_eq!(
        sig.min_chain_work,
        work("00000000000000000000000000000000000000000000000000000b463ea0a4b8")
    );

    let reg = core_assume_valid_for_network("regtest");
    assert_eq!((reg.height, reg.hash, reg.min_chain_work), (0, None, 0));
}

/// A `[block_validation]` that gives only the network's default height and no hash (what
/// the `blvm` binary seeded before blvm 19da720, and what old config files still say) must
/// keep the anchor and the minimum work; before, it reached consensus as height 912683,
/// hash none, minimum work 0.
#[test]
fn test_binary_height_seed_keeps_hash_and_minimum_work() {
    let main = core_assume_valid_for_network("mainnet");
    let seed = BlockValidationNodeConfig {
        assume_valid_height: default_assume_valid_height_for_network("mainnet"),
        assume_valid_hash: None,
    };
    let got = resolve_assume_valid("mainnet", false, Some(&seed), None);
    assert_eq!(got, main);
    assert!(got.hash.is_some());
    assert_ne!(got.min_chain_work, 0);
}

#[test]
fn test_zero_config_uses_network_values() {
    assert_eq!(
        resolve_assume_valid("mainnet", false, None, None),
        core_assume_valid_for_network("mainnet")
    );
    // Regtest must not inherit the mainnet height: any regtest chain would skip scripts.
    let reg = resolve_assume_valid("regtest", false, None, None);
    assert_eq!((reg.height, reg.hash, reg.min_chain_work), (0, None, 0));
}

#[test]
fn test_custom_height_drops_hash_keeps_minimum_work() {
    let cfg = BlockValidationNodeConfig {
        assume_valid_height: 700_000,
        assume_valid_hash: None,
    };
    let got = resolve_assume_valid("mainnet", false, Some(&cfg), None);
    assert_eq!(got.height, 700_000);
    assert_eq!(got.hash, None);
    assert_eq!(
        got.min_chain_work,
        core_assume_valid_for_network("mainnet").min_chain_work
    );
}

#[test]
fn test_env_and_noassumevalid_override_height_not_minimum_work() {
    let env = resolve_assume_valid("mainnet", false, None, Some(0));
    assert_eq!((env.height, env.hash), (0, None));
    assert_ne!(env.min_chain_work, 0);

    let off = BlockValidationNodeConfig {
        assume_valid_height: 0,
        assume_valid_hash: None,
    };
    let got = resolve_assume_valid("mainnet", false, Some(&off), None);
    assert_eq!((got.height, got.hash), (0, None));
}

#[test]
fn test_custom_signet_has_no_assume_valid() {
    let got = resolve_assume_valid("signet", true, None, None);
    assert_eq!((got.height, got.hash, got.min_chain_work), (0, None, 0));
}
