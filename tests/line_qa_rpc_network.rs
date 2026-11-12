//! Test-line QA: setban refuses a CIDR prefix.

use blvm_node::rpc::network::NetworkRpc;
use serde_json::json;

#[tokio::test]
async fn set_ban_rejects_a_subnet() {
    let rpc = NetworkRpc::new();
    match rpc.set_ban(&json!(["10.0.0.0/8"])).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("setban: a subnet is not accepted until a prefix parser exists"),
                "set_ban returned {message}"
            );
        }
        Ok(value) => panic!("subnet was banned: {value}"),
    }
}
