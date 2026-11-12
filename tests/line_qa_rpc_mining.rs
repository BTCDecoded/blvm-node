//! Test-line QA: estimatesmartfee rejects an unknown estimate mode.

use blvm_node::rpc::mining::MiningRpc;

#[tokio::test]
async fn estimate_smart_fee_rejects_an_unknown_mode() {
    let rpc = MiningRpc::new();
    match rpc
        .estimate_smart_fee(&serde_json::json!([6, "bogus"]))
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid estimate_mode: bogus"),
                "estimate_smart_fee returned {message}"
            );
        }
        Ok(value) => panic!("estimatesmartfee accepted mode bogus: {value}"),
    }
}
