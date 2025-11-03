use reference_node::rpc::server::RpcServer;
use serde_json::json;

#[tokio::test]
async fn rpc_invalid_json_returns_parse_error() {
    let response = RpcServer::process_request("invalid json").await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["error"].is_object());
    assert_eq!(response["error"]["code"], -32700);
}

#[tokio::test]
async fn rpc_unknown_method_returns_method_not_found() {
    let request = r#"{"jsonrpc":"2.0","method":"does_not_exist","params":[],"id":1}"#;
    let response = RpcServer::process_request(request).await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["error"].is_object());
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn rpc_getblockcount_returns_number() {
    let request = r#"{"jsonrpc":"2.0","method":"getblockcount","params":[],"id":2}"#;
    let response = RpcServer::process_request(request).await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"].is_number());
    assert_eq!(response["id"], 2);
}

#[tokio::test]
async fn rpc_sendrawtransaction_invalid_hex_returns_invalid_params() {
    let request = json!({
        "jsonrpc": "2.0",
        "method": "sendrawtransaction",
        "params": ["zzzz"],
        "id": 3
    })
    .to_string();

    let response = RpcServer::process_request(&request).await;
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["error"].is_object());
    assert_eq!(response["error"]["code"], -32602); // invalid params
    assert_eq!(response["id"], 3);
}

#[tokio::test]
async fn rpc_getnetworkinfo_has_expected_keys() {
    let request = r#"{"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":4}"#;
    let response = RpcServer::process_request(request).await;
    let result = &response["result"];
    assert!(result.is_object());
    // Check a few stable fields
    assert!(result.get("version").is_some());
    assert!(result.get("protocolversion").is_some());
    assert!(result.get("connections").is_some());
}
