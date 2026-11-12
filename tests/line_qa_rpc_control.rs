//! Test-line QA: getmemoryinfo refuses an unknown mode.

use blvm_node::rpc::control::ControlRpc;
use serde_json::json;

#[tokio::test]
async fn getmemoryinfo_rejects_an_unknown_mode() {
    let rpc = ControlRpc::new();
    match rpc.getmemoryinfo(&json!(["bogus"])).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid mode: bogus. Must be 'stats' or 'mallocinfo'"),
                "getmemoryinfo returned {message}"
            );
        }
        Ok(value) => panic!("unknown mode was accepted: {value}"),
    }
}
