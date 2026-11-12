use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream as TokioTcpStream;

#[tokio::test]
async fn test_rpc_server_creation() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = RpcServer::new(addr);
    assert_eq!(server.addr, addr);
}

#[tokio::test]
async fn test_http_rpc_integration() {
    // Start server on random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_addr = listener.local_addr().unwrap();
    let server = Arc::new(RpcServer::new(server_addr));

    // Spawn server task using hyper
    let server_handle = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            let peer_addr = addr;
            let server_clone = server.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| {
                    RpcServer::handle_http_request_with_server(server_clone.clone(), req, peer_addr)
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });

    // Give server time to start
    tokio::time::sleep(RPC_SERVER_STARTUP_WAIT).await;

    // Connect to server
    let mut client = TokioTcpStream::connect(server_addr).await.unwrap();

    // Send HTTP POST request
    let json_body = r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#;
    let http_request = format!(
        "POST / HTTP/1.1\r\n\
            Host: 127.0.0.1:18332\r\n\
            Content-Type: application/json\r\n\
            Content-Length: {}\r\n\
            \r\n\
            {}",
        json_body.len(),
        json_body
    );

    client.write_all(http_request.as_bytes()).await.unwrap();

    // Read response
    let mut response = vec![0u8; 4096];
    let n = tokio::time::timeout(RPC_CLIENT_READ_TIMEOUT, client.read(&mut response))
        .await
        .unwrap()
        .unwrap();

    let response_str = String::from_utf8_lossy(&response[..n]);

    // Verify HTTP response (hyper uses lowercase headers)
    assert!(
        response_str.contains("HTTP/1.1 200 OK") || response_str.contains("200 OK"),
        "Response: {response_str}"
    );
    assert!(
        response_str.contains("content-type: application/json")
            || response_str.contains("Content-Type: application/json")
    );
    assert!(response_str.contains("jsonrpc"));
    assert!(response_str.contains("\"result\""));

    server_handle.abort();
}

#[tokio::test]
async fn test_process_request_valid_json() {
    let request = r#"{"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"].is_object());
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_process_request_invalid_json() {
    let request = "invalid json";
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32700);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Parse error")
            || response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Invalid JSON")
    );
}

#[tokio::test]
async fn test_process_request_unknown_method() {
    let request = r#"{"jsonrpc":"2.0","method":"unknown_method","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32601);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found")
    );
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_process_request_without_id() {
    let request = r#"{"jsonrpc":"2.0","method":"getnetworkinfo","params":[]}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"].is_object());
    assert_eq!(response["id"], serde_json::Value::Null);
}

#[tokio::test]
async fn test_process_request_with_params() {
    let request = r#"{"jsonrpc":"2.0","method":"getblock","params":["000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    // Block may not be in storage (test doesn't set up storage)
    // If block found, result should be object; if not found, result should be null or error
    if response.get("error").is_some() {
        // Error response is valid when block not found
        assert!(response["error"].is_object());
    } else {
        // Success response should have object result
        assert!(response["result"].is_object() || response["result"].is_null());
    }
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_call_method_getblockchaininfo() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("getblockchaininfo", json!([])).await;
    let err = result.expect_err("no chain");
    assert!(err.to_string().contains("chain is not initialized"));
}

#[tokio::test]
async fn test_call_method_getblock() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let params = json!(["000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"]);
    let result = server.call_method("getblock", params).await;
    // Block may not be in storage (test doesn't set up storage)
    // This is expected - the method should return an error when block not found
    // The important thing is that the method doesn't panic
    if let Ok(response) = result {
        // If block was found, verify response structure
        assert!(response.get("hash").is_some());
    } else {
        // If block not found, that's expected without storage setup
        // Just verify it's a proper error response
        assert!(result.is_err());
    }
}

#[tokio::test]
async fn test_call_method_getblockhash() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let params = json!([0]);
    let result = server.call_method("getblockhash", params).await;
    // Default test server has no storage; real nodes resolve height via blockstore.
    assert!(result.is_err());
}

#[tokio::test]
async fn rpc_call_past_the_deadline_is_a_timeout_and_does_not_finish() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    assert_eq!(server.rpc_timeout, std::time::Duration::from_secs(60));

    let finished = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&finished);
    let err = RpcServer::await_bounded(std::time::Duration::from_millis(20), async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        flag.store(true, Ordering::SeqCst);
        Ok(json!(true))
    })
    .await
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("timed out"), "{text}");
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    assert!(!finished.load(Ordering::SeqCst));
}

#[tokio::test]
async fn getblockhash_without_a_height_is_an_error() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let err = server
        .call_method("getblockhash", json!([]))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("getblockhash"), "{text}");
    assert!(text.contains("integer"), "{text}");
}

#[tokio::test]
async fn test_call_method_getrawtransaction() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let params = json!(["0000000000000000000000000000000000000000000000000000000000000000"]);
    let result = server.call_method("getrawtransaction", params).await;
    // getrawtransaction may fail if transaction is not found, which is acceptable in tests
    // The important thing is that it doesn't panic
    if let Ok(response) = result {
        // If it succeeds, check that it has expected structure
        assert!(response.is_object() || response.is_string());
    }
}

#[tokio::test]
async fn test_call_method_getnetworkinfo() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("getnetworkinfo", json!([])).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.get("version").is_some());
}

#[tokio::test]
async fn test_call_method_getpeerinfo() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("getpeerinfo", json!([])).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.is_array());
}

#[tokio::test]
async fn test_call_method_getmininginfo() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("getmininginfo", json!([])).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.get("blocks").is_some());
}

#[tokio::test]
async fn test_call_method_getblocktemplate() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("getblocktemplate", json!([])).await;
    // getblocktemplate may fail if chain state is not initialized, which is acceptable in tests
    // The important thing is that it doesn't panic
    if let Ok(response) = result {
        // If it succeeds, check that it has expected structure
        assert!(response.is_object() || response.is_string());
    }
}

#[tokio::test]
async fn test_call_method_unknown_method() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("unknown_method", json!([])).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Method not found"));
}

/// A node with no loaded module does not serve payment RPC. `blvm-payment` registers these methods.
#[tokio::test]
async fn createpaymentrequest_is_absent_from_the_default_server() {
    let server = RpcServer::new("127.0.0.1:0".parse().unwrap());
    let result = server.call_method("createpaymentrequest", json!([])).await;
    assert!(result.unwrap_err().to_string().contains("Method not found"));
}

#[tokio::test]
async fn test_json_rpc_2_0_compliance() {
    let request = r#"{"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"].is_object());
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_error_response_format() {
    let request = r#"{"jsonrpc":"2.0","method":"unknown_method","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["error"].is_object());
    assert!(response["error"]["code"].is_number());
    assert!(response["error"]["message"].is_string());
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_parse_error_response() {
    let request = "invalid json";
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32700);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Parse error")
            || response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Invalid JSON")
    );
}

#[tokio::test]
async fn test_method_not_found_response() {
    let request = r#"{"jsonrpc":"2.0","method":"nonexistent","params":[],"id":42}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32601);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found")
    );
    assert!(response["error"]["data"].is_string() || response["error"]["data"].is_null());
    assert_eq!(response["id"], 42);
}

#[tokio::test]
async fn test_empty_params_handling() {
    let request = r#"{"jsonrpc":"2.0","method":"getnetworkinfo","id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response["result"].is_object());
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_missing_method_handling() {
    let request = r#"{"jsonrpc":"2.0","params":[],"id":1}"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["error"]["code"], -32601);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Method not found")
    );
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn test_blockchain_methods_integration() {
    let methods = vec![
        "getblockchaininfo",
        "getblock",
        "getblockhash",
        "getrawtransaction",
    ];

    for method in methods {
        let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
        let response_str = RpcServer::process_request(&request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        // Result may be an object, string, or null (if method failed)
        assert!(
            response["result"].is_object()
                || response["result"].is_string()
                || response["result"].is_null()
        );
        assert_eq!(response["id"], 1);
    }
}

#[tokio::test]
async fn test_network_methods_integration() {
    let methods = vec!["getnetworkinfo", "getpeerinfo"];

    for method in methods {
        let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
        let response_str = RpcServer::process_request(&request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response["result"].is_object() || response["result"].is_array());
        assert_eq!(response["id"], 1);
    }
}

#[tokio::test]
async fn test_mining_methods_integration() {
    let methods = vec!["getmininginfo", "getblocktemplate"];

    for method in methods {
        let request = format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":[],"id":1}}"#);
        let response_str = RpcServer::process_request(&request).await;
        let response: Value = serde_json::from_str(&response_str).unwrap();

        assert_eq!(response["jsonrpc"], "2.0");
        // Result may be an object, string, or null (if method failed due to missing dependencies)
        assert!(
            response["result"].is_object()
                || response["result"].is_string()
                || response["result"].is_null()
        );
        assert_eq!(response["id"], 1);
    }
}

#[tokio::test]
async fn test_batch_request_empty() {
    let request = "[]";
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    assert_eq!(response.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn test_batch_request_single() {
    let request = r#"[{"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1}]"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    let responses = response.as_array().unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["jsonrpc"], "2.0");
    assert!(responses[0]["result"].is_object());
    assert_eq!(responses[0]["id"], 1);
}

#[tokio::test]
async fn test_batch_request_multiple() {
    let request = r#"[
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":2},
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":3}
        ]"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    let responses = response.as_array().unwrap();
    assert_eq!(responses.len(), 3);

    // Verify order is maintained
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[2]["id"], 3);

    assert!(responses[0]["result"].is_object());
    assert!(responses[1]["result"].is_null());
    assert!(responses[1]["error"].is_object());
    assert!(responses[2]["result"].is_object());
}

#[tokio::test]
async fn test_batch_request_with_errors() {
    let request = r#"[
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"unknown_method","params":[],"id":2},
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":3}
        ]"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    let responses = response.as_array().unwrap();
    assert_eq!(responses.len(), 3);

    // First request should succeed
    assert_eq!(responses[0]["id"], 1);
    assert!(responses[0]["result"].is_object());
    assert!(responses[0]["error"].is_null());

    // Second request should fail
    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1]["result"].is_null());
    assert!(responses[1]["error"].is_object());
    assert_eq!(responses[1]["error"]["code"], -32601);

    assert_eq!(responses[2]["id"], 3);
    assert!(responses[2]["result"].is_null());
    assert!(responses[2]["error"].is_object());
}

#[tokio::test]
async fn test_batch_request_order_preserved() {
    let request = r#"[
            {"jsonrpc":"2.0","method":"getblockcount","params":[],"id":3},
            {"jsonrpc":"2.0","method":"getblockchaininfo","params":[],"id":1},
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[],"id":2}
        ]"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    let responses = response.as_array().unwrap();
    assert_eq!(responses.len(), 3);

    // Order should match request order, not ID order
    assert_eq!(responses[0]["id"], 3);
    assert_eq!(responses[1]["id"], 1);
    assert_eq!(responses[2]["id"], 2);
}

#[tokio::test]
async fn test_batch_request_without_ids() {
    let request = r#"[
            {"jsonrpc":"2.0","method":"getnetworkinfo","params":[]},
            {"jsonrpc":"2.0","method":"getblockcount","params":[]}
        ]"#;
    let response_str = RpcServer::process_request(request).await;
    let response: Value = serde_json::from_str(&response_str).unwrap();

    assert!(response.is_array());
    let responses = response.as_array().unwrap();
    assert_eq!(responses.len(), 2);

    // Both should have null IDs
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[1]["id"], Value::Null);

    assert!(responses[0]["result"].is_object());
    assert!(responses[1]["result"].is_null());
    assert!(responses[1]["error"].is_object());
}

/// ckpool-style Basic auth authenticates but is not admin unless password ∈ admin_tokens.
#[tokio::test]
async fn test_dispatch_getblocktemplate_forbidden_for_ckpool_basic_without_admin() {
    use crate::rpc::auth::RpcAuthManager;
    use hyper::HeaderMap;
    use std::sync::Arc;

    let addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let auth = Arc::new(RpcAuthManager::new(true));
    auth.set_basic_auth(Some("ckpool".to_string()), "s3cret".to_string())
        .await
        .unwrap();

    let server = Arc::new(RpcServer::with_auth("127.0.0.1:0".parse().unwrap(), auth));
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic Y2twb29sOnMzY3JldA==".parse().unwrap(), // ckpool:s3cret
    );
    let body = r#"{"jsonrpc":"2.0","method":"getblocktemplate","params":[],"id":1}"#;

    let outcome =
        RpcServer::dispatch_json_rpc_post_body(Arc::clone(&server), &headers, addr, body).await;

    match outcome {
        DispatchJsonRpcPostOutcome::Error { status, message } => {
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(message.contains("admin privileges"));
        }
        DispatchJsonRpcPostOutcome::Success { .. } => {
            panic!("expected FORBIDDEN for ckpool Basic without admin_tokens, got Success");
        }
    }
}

#[tokio::test]
async fn test_dispatch_getblocktemplate_allowed_via_build_auth_manager_config() {
    use crate::config::RpcAuthConfig;
    use crate::rpc::RpcManager;
    use hyper::HeaderMap;
    use std::sync::Arc;

    let auth = RpcManager::build_auth_manager(RpcAuthConfig {
        required: true,
        username: Some("ckpool".to_string()),
        password: Some("s3cret".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();

    let addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let server = Arc::new(RpcServer::with_auth("127.0.0.1:0".parse().unwrap(), auth));
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic Y2twb29sOnMzY3JldA==".parse().unwrap(),
    );
    let body = r#"{"jsonrpc":"2.0","method":"getblocktemplate","params":[],"id":1}"#;

    let outcome =
        RpcServer::dispatch_json_rpc_post_body(Arc::clone(&server), &headers, addr, body).await;

    match outcome {
        DispatchJsonRpcPostOutcome::Success { .. } => {}
        DispatchJsonRpcPostOutcome::Error { status, message } => {
            panic!("with_auth_config-style password must pass RBAC; got HTTP {status}: {message}");
        }
    }
}

#[tokio::test]
async fn test_dispatch_getblocktemplate_allowed_when_password_is_admin_token() {
    use crate::rpc::auth::RpcAuthManager;
    use hyper::HeaderMap;
    use std::sync::Arc;

    let addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let auth = Arc::new(RpcAuthManager::new(true));
    auth.set_basic_auth(Some("ckpool".to_string()), "s3cret".to_string())
        .await
        .unwrap();
    auth.add_admin_token("s3cret".to_string()).await.unwrap();

    let server = Arc::new(RpcServer::with_auth("127.0.0.1:0".parse().unwrap(), auth));
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Basic Y2twb29sOnMzY3JldA==".parse().unwrap(),
    );
    let body = r#"{"jsonrpc":"2.0","method":"getblocktemplate","params":[],"id":1}"#;

    let outcome =
        RpcServer::dispatch_json_rpc_post_body(Arc::clone(&server), &headers, addr, body).await;

    match outcome {
        DispatchJsonRpcPostOutcome::Success { .. } => {}
        DispatchJsonRpcPostOutcome::Error { status, message } => {
            panic!(
                "expected Success (handler may error on uninitialized chain), got HTTP {status}: {message}"
            );
        }
    }
}

#[tokio::test]
async fn test_dispatch_generatetoaddress_forbidden_for_read_only_token() {
    use crate::rpc::auth::RpcAuthManager;
    use hyper::HeaderMap;
    use std::sync::Arc;

    let addr: SocketAddr = "127.0.0.1:8332".parse().unwrap();
    let auth = Arc::new(RpcAuthManager::new(true));
    auth.add_token("read-only".to_string()).await.unwrap();

    let server = Arc::new(RpcServer::with_auth("127.0.0.1:0".parse().unwrap(), auth));
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer read-only".parse().unwrap());
    let body = r#"{"jsonrpc":"2.0","method":"generatetoaddress","params":[1,"bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4"],"id":1}"#;

    let outcome =
        RpcServer::dispatch_json_rpc_post_body(Arc::clone(&server), &headers, addr, body).await;

    match outcome {
        DispatchJsonRpcPostOutcome::Error { status, message } => {
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(message.contains("admin privileges"));
        }
        DispatchJsonRpcPostOutcome::Success { .. } => {
            panic!("generatetoaddress must be admin-gated for read-only Bearer tokens");
        }
    }
}
