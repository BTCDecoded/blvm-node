//! Test-line QA: a block template is refused before the chain is initialized.

use std::sync::Arc;

use blvm_node::module::api::NodeApiImpl;
use blvm_node::module::traits::NodeAPI;
use blvm_node::storage::Storage;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_block_template_rejects_an_uninitialized_chain() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Arc::new(Storage::new(dir.path()).expect("storage"));
    let api = NodeApiImpl::new(storage);

    match api.get_block_template(Vec::new(), None, None).await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Chain not initialized"),
                "get_block_template returned {message}"
            );
        }
        Ok(template) => panic!("template built with no chain: {template:?}"),
    }
}
