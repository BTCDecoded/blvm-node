//! Test-line QA: loadtxoutset rejects a call that omits the snapshot path.

use std::sync::Arc;

use blvm_node::rpc::blockchain::BlockchainRpc;
use blvm_node::storage::Storage;

#[tokio::test]
async fn load_txout_set_rejects_a_missing_path() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Arc::new(Storage::new(dir.path()).expect("storage"));
    let rpc = BlockchainRpc::with_dependencies(storage);

    match rpc.load_txout_set(&serde_json::json!([])).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("loadtxoutset requires path (string)"),
                "load_txout_set returned {message}"
            );
        }
        Ok(value) => panic!("loadtxoutset accepted empty params: {value}"),
    }
}
