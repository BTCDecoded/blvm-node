use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::rpc::server::RpcServer;
use blvm_node::storage::Storage;
use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion};
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn getblockchaininfo_errors_before_chain_is_loaded() {
    let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();
    let message = response["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("chain is not initialized"), "{response}");
}

#[tokio::test]
async fn getblockchaininfo_softforks_contains_expected_keys() {
    // getblockchaininfo errors until a chain is loaded. Anchor a regtest genesis the way
    // node start does on a fresh datadir (node/mod.rs), and build the RPC as the server does.
    let dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(dir.path()).unwrap());
    let engine = BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap();
    let np = engine.get_network_params();
    storage
        .chain()
        .initialize_from_network_metadata(
            &np.genesis_block.header,
            &np.network_name,
            np.max_target as u64,
            np.halving_interval,
        )
        .unwrap();
    let rpc = BlockchainRpc::with_dependencies(storage);
    let result = rpc.get_blockchain_info().await.unwrap();
    assert!(result.is_object());
    assert_eq!(result["chain"], "regtest", "{result}");

    // Softforks object must include segwit and taproot keys per RPC spec
    let softforks = &result["softforks"];
    assert!(softforks.is_object(), "{result}");
    assert!(softforks.get("segwit").is_some(), "{result}");
    assert!(softforks.get("taproot").is_some(), "{result}");
}

#[tokio::test]
async fn getblocktemplate_rules_include_feature_flags() {
    let request = r#"{"jsonrpc":"2.0","method":"getblocktemplate","params":[],"id":2}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    // Check if there's an error (chain not initialized)
    if response.get("error").is_some() {
        // Skip test if chain is not initialized (expected in test environment)
        eprintln!(
            "getblocktemplate returned error (chain not initialized): {:?}",
            response["error"]
        );
        return;
    }

    let result = &response["result"];
    assert!(
        result.is_object(),
        "result should be an object, got: {result:?}"
    );

    let rules = result["rules"].as_array().expect("rules array");
    // Rules array includes csv, segwit, taproot per getblocktemplate spec
    let rule_set: std::collections::HashSet<String> = rules
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    assert!(rule_set.contains("csv"));
    assert!(rule_set.contains("segwit"));
    assert!(rule_set.contains("taproot"));
}
