//! Test-line QA: getmempoolentry refuses a call with no transaction id.

use blvm_node::rpc::mempool::MempoolRpc;
use serde_json::json;

#[tokio::test]
async fn getmempoolentry_rejects_a_missing_txid() {
    let rpc = MempoolRpc::new();
    match rpc.getmempoolentry(&json!([])).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Expected string at params[0] for getmempoolentry"),
                "getmempoolentry returned {message}"
            );
        }
        Ok(value) => panic!("missing txid was accepted: {value}"),
    }
}
