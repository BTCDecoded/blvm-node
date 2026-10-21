//! validateaddress network checks with production-style construction.
//!
//! The JSON-RPC and REST servers build `BlockchainRpc::with_dependencies(storage)` with no
//! protocol (rpc/mod.rs), so the chain must come from the network stored in the datadir.
//! Regression tests from Review's probes on blvm-node#30 @ 09a6526.

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

fn storage_initialized_like_node_start(v: ProtocolVersion) -> (TempDir, Arc<Storage>) {
    // Mirrors node/mod.rs (fresh datadir genesis anchor).
    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let p = BitcoinProtocolEngine::new(v).unwrap();
    let np = p.get_network_params();
    storage
        .chain()
        .initialize_from_network_metadata(
            &np.genesis_block.header,
            &np.network_name,
            np.max_target as u64,
            np.halving_interval,
        )
        .unwrap();
    (dir, storage)
}

async fn isvalid(rpc: &BlockchainRpc, addr: &str) -> bool {
    let r = rpc.validate_address(&json!([addr])).await.unwrap();
    r["isvalid"].as_bool().unwrap_or_else(|| panic!("{r}"))
}

/// rv_p1: mainnet datadir + production construction accepts a mainnet bech32 address.
#[tokio::test]
async fn rv_p1_mainnet_node_production_construction_accepts_bc1() {
    let (_d, storage) = storage_initialized_like_node_start(ProtocolVersion::BitcoinV1);
    let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    let info = rpc.get_blockchain_info().await.unwrap();
    assert_eq!(info["chain"], "main");
    let r = rpc
        .validate_address(&json!(["bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"]))
        .await
        .unwrap();
    assert_eq!(r["isvalid"].as_bool(), Some(true), "{r}");
    // Taproot (BIP-350 test vector).
    assert!(
        isvalid(
            &rpc,
            "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0"
        )
        .await
    );
}

/// rv_p1b: regtest datadir + production construction accepts bcrt1 and rejects bc1.
#[tokio::test]
async fn rv_p1b_regtest_node_production_construction() {
    let (_d, storage) = storage_initialized_like_node_start(ProtocolVersion::Regtest);
    let rpc = BlockchainRpc::with_dependencies(Arc::clone(&storage));
    assert!(!isvalid(&rpc, "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4").await);
    assert!(isvalid(&rpc, "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080").await);
}

const MAIN_P2PKH: &str = "1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2";
const MAIN_P2SH: &str = "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy";
const TEST_P2PKH: &str = "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn";
const TEST_P2SH: &str = "2MzQwSSnBHWHqSAqtTVQ6v47XtaisrJa1Vc";

#[tokio::test]
async fn base58_network_checked_on_mainnet_node() {
    let (_d, storage) = storage_initialized_like_node_start(ProtocolVersion::BitcoinV1);
    let rpc = BlockchainRpc::with_dependencies(storage);
    assert!(isvalid(&rpc, MAIN_P2PKH).await);
    assert!(isvalid(&rpc, MAIN_P2SH).await);
    assert!(!isvalid(&rpc, TEST_P2PKH).await);
    assert!(!isvalid(&rpc, TEST_P2SH).await);
}

#[tokio::test]
async fn base58_network_checked_on_regtest_node() {
    let (_d, storage) = storage_initialized_like_node_start(ProtocolVersion::Regtest);
    let rpc = BlockchainRpc::with_dependencies(storage);
    assert!(!isvalid(&rpc, MAIN_P2PKH).await);
    assert!(!isvalid(&rpc, MAIN_P2SH).await);
    assert!(isvalid(&rpc, TEST_P2PKH).await);
    assert!(isvalid(&rpc, TEST_P2SH).await);
}

// Review's probes on blvm-node#30 @ 26c4d5e: full address matrix per network, so testnet3,
// signet and testnet4 are covered by both the base58 version check and the stored-network
// fallback in `get_chain_name()`.
const BC1: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
const BC1P: &str = "bc1p0xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqzk5jj0";
const TB1: &str = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";
const TB1P: &str = "tb1pqqqqp399et2xygdj5xreqhjjvcmzhxw4aywxecjdzew6hylgvsesf3hn0c";
const BCRT1: &str = "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080";
const M1: &str = MAIN_P2PKH;
const M3: &str = MAIN_P2SH;
const TM: &str = TEST_P2PKH;
const T2: &str = TEST_P2SH;

async fn matrix(v: ProtocolVersion) -> Vec<(&'static str, bool)> {
    let (_d, s) = storage_initialized_like_node_start(v);
    let rpc = BlockchainRpc::with_dependencies(s);
    let mut out = vec![];
    for a in [BC1, BC1P, TB1, TB1P, BCRT1, M1, M3, TM, T2] {
        out.push((a, isvalid(&rpc, a).await));
    }
    let st = rpc.get_blockchain_state().await.unwrap();
    let info = rpc.get_blockchain_info().await.unwrap();
    eprintln!(
        "{v:?}: getblockchainstate.chain={} getblockchaininfo.chain={} {:?}",
        st["chain"], info["chain"], out
    );
    out
}
fn exp(out: &[(&str, bool)], valid: &[&str]) {
    for (a, got) in out {
        assert_eq!(*got, valid.contains(a), "{a}");
    }
}
#[tokio::test]
async fn rv_q2_testnet3() {
    exp(
        &matrix(ProtocolVersion::Testnet3).await,
        &[TB1, TB1P, TM, T2],
    );
}
#[tokio::test]
async fn rv_q3_signet() {
    exp(&matrix(ProtocolVersion::Signet).await, &[TB1, TB1P, TM, T2]);
}
#[tokio::test]
async fn rv_q4_testnet4() {
    exp(
        &matrix(ProtocolVersion::Testnet4).await,
        &[TB1, TB1P, TM, T2],
    );
}

/// rv_w1 (Review's probe on blvm-node#30 @ cafc63c): production construction (storage, no
/// protocol) on a fresh datadir whose chain_info is not yet written. node/mod.rs starts RPC
/// before it writes the genesis chain_info, so the chain is unknown; validateaddress must fail
/// closed and report no network's address as valid.
#[tokio::test]
async fn rv_w1_uninitialized_datadir_production_construction() {
    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    assert!(storage.chain().load_chain_info().unwrap().is_none());
    let rpc = BlockchainRpc::with_dependencies(storage);
    let mut out = vec![];
    for a in [BC1, BC1P, TB1, TB1P, BCRT1, M1, M3, TM, T2] {
        let r = rpc.validate_address(&json!([a])).await;
        out.push((a, r.ok().and_then(|r| r["isvalid"].as_bool())));
    }
    eprintln!("RV_W1 uninit storage: {out:?}");
    let accepted: Vec<_> = out
        .iter()
        .filter(|(_, v)| *v == Some(true))
        .map(|(a, _)| *a)
        .collect();
    assert!(
        accepted.is_empty(),
        "accepted with unknown chain: {accepted:?}"
    );
}
