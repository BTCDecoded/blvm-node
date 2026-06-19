//! ckpool / solo-mining pool RPC compatibility tests.
//!
//! ckpool calls `getblocktemplate` with coinbasetxn capabilities, reads `coinbasevalue`,
//! `previousblockhash`, `target`, and `validateaddress` for solo mode.

use blvm_node::node::mempool::MempoolManager;
use blvm_node::rpc::auth::RpcAuthManager;
use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::rpc::mining::MiningRpc;
use blvm_node::storage::Storage;
use hyper::HeaderMap;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

mod common;
use common::{MINING_RPC_CHAIN_BLOCKS, setup_mining_chain};

/// ckpool `gbt_req` from mempool/ckpool `src/bitcoin.c`.
fn ckpool_gbt_params() -> serde_json::Value {
    json!([{
        "capabilities": ["coinbasetxn", "workid", "coinbase/append"],
        "rules": ["segwit"]
    }])
}

#[tokio::test]
async fn ckpool_getblocktemplate_shape() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(storage.clone(), mempool);
    let blockchain = BlockchainRpc::with_dependencies(storage.clone());

    setup_mining_chain(&storage, MINING_RPC_CHAIN_BLOCKS).unwrap();

    let template = mining
        .get_block_template(&ckpool_gbt_params())
        .await
        .expect("ckpool-style getblocktemplate must succeed");

    let tip_hash = blockchain
        .get_best_block_hash()
        .await
        .expect("tip hash")
        .as_str()
        .expect("hex string")
        .to_string();

    assert_eq!(
        template.get("previousblockhash").unwrap().as_str().unwrap(),
        tip_hash,
        "previousblockhash must match getbestblockhash (chain tip)"
    );

    // Regression: old code used tip.prev_block_hash (parent link field), not tip hash.
    let tip_header = storage
        .chain()
        .get_tip_header()
        .unwrap()
        .expect("tip header");
    let parent_rpc = blvm_node::storage::hashing::hash_to_rpc_hex(&tip_header.prev_block_hash);
    assert_ne!(
        template.get("previousblockhash").unwrap().as_str().unwrap(),
        parent_rpc,
        "previousblockhash must be chain tip, not tip.prev_block_hash"
    );

    let coinbaseaux = template.get("coinbaseaux").unwrap().as_object().unwrap();
    assert!(
        coinbaseaux.contains_key("flags"),
        "ckpool reads coinbaseaux.flags"
    );

    for key in [
        "coinbasevalue",
        "target",
        "bits",
        "height",
        "version",
        "curtime",
        "transactions",
        "coinbaseaux",
        "rules",
    ] {
        assert!(template.get(key).is_some(), "missing GBT field: {key}");
    }

    let target = template.get("target").unwrap().as_str().unwrap();
    assert_eq!(target.len(), 64, "target must be 64 hex chars");
    let bits =
        u32::from_str_radix(template.get("bits").unwrap().as_str().unwrap(), 16).expect("bits hex");
    let expected_target = blvm_protocol::pow::expand_target(bits as u64)
        .expect("expand nBits")
        .gbt_target_hex();
    assert_eq!(target, expected_target, "target must match nBits expansion");
    assert!(template.get("coinbasevalue").unwrap().as_u64().unwrap() > 0);

    // ckpool gen_gbtbase rejects null/zero version, curtime, or missing coinbaseaux.
    assert!(template.get("version").unwrap().as_i64().unwrap() > 0);
    assert!(template.get("curtime").unwrap().as_u64().unwrap() > 0);

    // ckpool only rejects mandatory rules prefixed with `!`; optional active rules may be empty
    // on short test chains below mainnet activation heights.
    assert!(template.get("rules").unwrap().is_array());
}

#[tokio::test]
async fn ckpool_validateaddress_solo_payout() {
    let blockchain = BlockchainRpc::new();

    let segwit = blockchain
        .validate_address(&json!(["bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"]))
        .await
        .expect("validateaddress");
    assert!(segwit.get("isvalid").unwrap().as_bool().unwrap());
    assert!(segwit.get("iswitness").unwrap().as_bool().unwrap());
    assert!(!segwit.get("isscript").unwrap().as_bool().unwrap());

    let p2pkh = blockchain
        .validate_address(&json!(["1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"]))
        .await
        .expect("validateaddress");
    assert!(p2pkh.get("isvalid").unwrap().as_bool().unwrap());
    assert!(!p2pkh.get("iswitness").unwrap().as_bool().unwrap());
    assert!(!p2pkh.get("isscript").unwrap().as_bool().unwrap());

    let p2sh = blockchain
        .validate_address(&json!(["3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy"]))
        .await
        .expect("validateaddress");
    assert!(p2sh.get("isvalid").unwrap().as_bool().unwrap());
    assert!(p2sh.get("isscript").unwrap().as_bool().unwrap());

    let bad = blockchain
        .validate_address(&json!(["definitely-not-a-bitcoin-address"]))
        .await
        .expect("validateaddress");
    assert!(!bad.get("isvalid").unwrap().as_bool().unwrap());

    let long_addr = format!("bc1q{}", "x".repeat(500));
    let too_long = blockchain.validate_address(&json!([long_addr])).await;
    assert!(
        too_long.is_err(),
        "validateaddress must reject oversized addresses"
    );
}

#[tokio::test]
async fn ckpool_http_basic_auth() {
    let manager = RpcAuthManager::new(true);
    manager
        .set_basic_auth(Some("ckpool".to_string()), "s3cret".to_string())
        .await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic Y2twb29sOnMzY3JldA==".parse().unwrap(), // ckpool:s3cret
    );
    let addr: std::net::SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let ok = manager.authenticate_request(&headers, addr).await;
    assert!(ok.user_id.is_some(), "valid Basic auth must succeed");
    assert!(ok.error.is_none());

    headers.insert(
        "authorization",
        "Basic d3Jvbmc6cGFzcw==".parse().unwrap(), // wrong:pass
    );
    let bad = manager.authenticate_request(&headers, addr).await;
    assert!(bad.user_id.is_none());
    assert!(bad.error.is_some());

    headers.insert(
        "authorization",
        "Basic Y2twb29sOndyb25nX3Bhc3M=".parse().unwrap(), // ckpool:wrong_pass
    );
    let wrong_pass = manager.authenticate_request(&headers, addr).await;
    assert!(wrong_pass.user_id.is_none());
}

#[tokio::test]
async fn ckpool_basic_auth_rejects_wrong_username() {
    let manager = RpcAuthManager::new(true);
    manager
        .set_basic_auth(Some("ckpool".to_string()), "s3cret".to_string())
        .await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic d3Jvbmc6czNjcmV0".parse().unwrap(), // wrong:s3cret
    );
    let addr: std::net::SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let result = manager.authenticate_request(&headers, addr).await;
    assert!(result.user_id.is_none());
}

#[tokio::test]
async fn regtest_getblocktemplate_after_generatetoaddress() {
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();

    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    storage.chain().initialize(&genesis_header).unwrap();

    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol));

    let gen_params = json!([
        3u64,
        "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
        2_000_000u64
    ]);
    mining
        .generate_to_address(&gen_params)
        .await
        .expect("generatetoaddress on regtest");

    let template = mining
        .get_block_template(&ckpool_gbt_params())
        .await
        .expect("regtest GBT must not return Target too large");

    assert!(template.get("coinbasevalue").unwrap().as_u64().unwrap() > 0);
    let target = template.get("target").unwrap().as_str().unwrap();
    assert_eq!(target.len(), 64);
    // Regtest minimum-difficulty nBits after generatetoaddress.
    let expected = blvm_protocol::pow::expand_target(0x207fffff)
        .expect("regtest nBits")
        .gbt_target_hex();
    assert_eq!(target, expected);
    assert!(template.get("version").unwrap().as_i64().unwrap() > 0);
}

#[tokio::test]
async fn ckpool_submitblock_validates_mined_regtest_block() {
    use blvm_protocol::mining::MiningResult;
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::serialization::serialize_block_with_witnesses;
    use blvm_protocol::{BitcoinProtocolEngine, ConsensusProof, ProtocolVersion, UtxoSet};

    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis_header = protocol.get_network_params().genesis_block.header.clone();

    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    storage.chain().initialize(&genesis_header).unwrap();

    let mempool = Arc::new(MempoolManager::new());
    let mining = MiningRpc::with_dependencies(Arc::clone(&storage), mempool)
        .with_protocol_engine(Arc::clone(&protocol));

    mining
        .generate_to_address(&json!([
            2u64,
            "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4",
            2_000_000u64
        ]))
        .await
        .expect("seed regtest chain");

    let template = mining
        .get_block_template(&ckpool_gbt_params())
        .await
        .expect("GBT before submit");

    let prev_rpc = template
        .get("previousblockhash")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();

    let consensus = ConsensusProof::new();
    let prev_header = storage.chain().get_tip_header().unwrap().expect("tip");
    let prev_headers = vec![prev_header.clone(), prev_header.clone()];
    let coinbase_script = vec![0x03u8, 0x01, 0x00, 0x00, 0x00, 0xff];
    let coinbase_address = vec![0x51u8];

    let next_height = storage.blocks().block_count().unwrap() as u64;
    let mut block = consensus
        .create_new_block(
            &UtxoSet::default(),
            &[],
            next_height,
            &prev_header,
            &prev_headers,
            &coinbase_script,
            &coinbase_address,
        )
        .expect("create block");
    block.header.version = 4;

    let (mined, result) = consensus.mine_block(block, 2_000_000).expect("mine_block");
    assert!(matches!(result, MiningResult::Success));

    let tip_internal = storage.chain().get_tip_hash().unwrap().expect("tip hash");
    assert_eq!(
        blvm_node::storage::hashing::hash_to_rpc_hex(&tip_internal),
        prev_rpc
    );
    assert_eq!(mined.header.prev_block_hash, tip_internal);

    let witnesses: Vec<Vec<Witness>> = mined
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect();
    let wire = serialize_block_with_witnesses(&mined, &witnesses, true);
    let hex_block = hex::encode(&wire);

    let submit = mining
        .submit_block(&json!([hex_block]))
        .await
        .expect("submitblock validates mined block");

    assert!(submit.is_null(), "accepted block returns null");
    assert_eq!(storage.blocks().as_ref().get_block_hash(&mined).len(), 32);
    assert_eq!(mined.header.prev_block_hash, tip_internal);
}

#[tokio::test]
async fn ckpool_chain_tip_rpc_methods() {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let blockchain = BlockchainRpc::with_dependencies(storage.clone());

    // Short chain so getblockhash is exercised at a realistic height.
    setup_mining_chain(&storage, 4).unwrap();

    let tip_height = blockchain
        .get_block_count()
        .await
        .expect("getblockcount")
        .as_u64()
        .expect("count");
    assert_eq!(tip_height, 3);

    let tip = blockchain
        .get_best_block_hash()
        .await
        .expect("getbestblockhash")
        .as_str()
        .expect("hex")
        .to_string();

    let hash_at_tip = blockchain
        .get_block_hash(tip_height)
        .await
        .expect("getblockhash")
        .as_str()
        .expect("hex")
        .to_string();
    assert_eq!(tip, hash_at_tip);

    let info = blockchain
        .get_blockchain_info()
        .await
        .expect("getblockchaininfo");
    assert_eq!(
        info.get("bestblockhash").unwrap().as_str().unwrap(),
        tip,
        "getblockchaininfo.bestblockhash must match getbestblockhash"
    );
}

#[tokio::test]
async fn submitblock_rejects_when_chain_has_no_tip_with_network_manager() {
    use blvm_node::network::NetworkManager;
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::serialization::serialize_block_with_witnesses;
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};

    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap();
    let genesis = protocol.get_network_params().genesis_block.clone();
    let witnesses: Vec<Vec<Witness>> = genesis
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect();
    let wire = serialize_block_with_witnesses(&genesis, &witnesses, true);
    let hex_block = hex::encode(&wire);

    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let mempool = Arc::new(MempoolManager::new());
    let nm = Arc::new(NetworkManager::new("127.0.0.1:8333".parse().unwrap()));
    let mining =
        MiningRpc::with_dependencies(Arc::clone(&storage), mempool).with_network_manager(Some(nm));

    let result = mining.submit_block(&json!([hex_block])).await;

    assert!(
        result.is_err(),
        "submitblock must fail without initialized tip"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("chain not initialized") || msg.contains("no tip"),
        "unexpected error: {msg}"
    );
}

#[test]
fn ckpool_block_hash_rpc_display_order() {
    // ckpool hex2bin + swap_256 expects Core GetHex() order (byte-reversed digest).
    let internal = [
        0x6f, 0xe2, 0x8c, 0x0a, 0xb6, 0xf1, 0xb3, 0x72, 0xc1, 0xa6, 0xa2, 0x46, 0xae, 0x63, 0xf7,
        0x4f, 0x93, 0x1e, 0x83, 0x65, 0xe1, 0x5a, 0x08, 0x9c, 0x68, 0xd6, 0x19, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    let rpc = blvm_node::storage::hashing::hash_to_rpc_hex(&internal);
    assert_eq!(
        rpc,
        "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
    );
    assert_eq!(
        blvm_node::storage::hashing::hash_from_rpc_hex(&rpc).unwrap(),
        internal
    );
}
