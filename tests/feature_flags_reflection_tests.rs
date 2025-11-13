use bllvm_node::rpc::server::RpcServer;
use serde_json::Value;

#[tokio::test]
async fn getblockchaininfo_softforks_contains_expected_keys() {
    let request = r#"{"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();
    let result = &response["result"];
    assert!(result.is_object());

    // Softforks should be an object, and contain segwit/taproot keys (placeholders for now)
    let softforks = &result["softforks"];
    assert!(softforks.is_object());
    assert!(softforks.get("segwit").is_some());
    assert!(softforks.get("taproot").is_some());
}

#[tokio::test]
async fn getblocktemplate_rules_include_feature_flags() {
    let request = r#"{"jsonrpc":"2.0","method":"getblocktemplate","params":[],"id":2}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();
    let result = &response["result"];
    assert!(result.is_object());

    let rules = result["rules"].as_array().expect("rules array");
    // Placeholders currently include csv, segwit, taproot
    let rule_set: std::collections::HashSet<String> = rules
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();

    assert!(rule_set.contains("csv"));
    assert!(rule_set.contains("segwit"));
    assert!(rule_set.contains("taproot"));
}
