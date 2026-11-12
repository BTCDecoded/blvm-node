//! Test-line QA: an unknown bearer token is rejected.

use std::net::SocketAddr;

use blvm_node::rpc::auth::RpcAuthManager;

#[tokio::test]
async fn authenticate_request_rejects_an_unknown_bearer_token() {
    let manager = RpcAuthManager::new(true);
    let mut headers = hyper::HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer not-a-token".parse().expect("header"),
    );
    let addr: SocketAddr = "127.0.0.1:8332".parse().expect("addr");

    let result = manager.authenticate_request(&headers, addr).await;
    match result.error.as_deref() {
        Some(message) => {
            assert!(
                message.contains("Invalid authentication token"),
                "authenticate_request returned {message}"
            );
            assert!(result.user_id.is_none(), "unknown token was accepted");
        }
        None => panic!("unknown bearer token authenticated: {:?}", result.user_id),
    }
}
