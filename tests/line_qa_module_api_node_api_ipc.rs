//! Test-line QA: a height request fails when the IPC peer closes.

use std::sync::Arc;

use blvm_node::module::api::NodeApiIpc;
use blvm_node::module::ipc::ModuleIpcClient;
use blvm_node::module::traits::NodeAPI;
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::Mutex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_block_height_rejects_a_closed_connection() {
    // AF_UNIX paths must be shorter than SUN_LEN; CI points TMPDIR at a long path.
    let dir = tempfile::TempDir::new_in("/tmp").expect("tempdir");
    let path = dir.path().join("ipc.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        drop(stream);
    });

    let client = ModuleIpcClient::connect(&path).await.expect("connect");
    let api = NodeApiIpc::new(Arc::new(Mutex::new(client)), "qa".to_string());

    match api.get_block_height().await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Connection closed while waiting for response"),
                "get_block_height returned {message}"
            );
        }
        Ok(height) => panic!("closed IPC peer returned height {height}"),
    }
}
