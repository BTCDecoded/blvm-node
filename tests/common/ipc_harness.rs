//! Unix IPC test helpers: short socket paths (sockaddr_un limit) and bind/error surfacing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration, Instant};

use blvm_node::module::ipc::client::ModuleIpcClient;
use blvm_node::module::ipc::server::ModuleIpcServer;
use blvm_node::module::traits::{ModuleError, NodeAPI};

pub fn setup_ipc_socket() -> (TempDir, PathBuf) {
    // Linux `sockaddr_un` paths are short (~108 bytes). CI may set a long TMPDIR/CARGO_TARGET_DIR;
    // `TempDir::new()` there can make Unix socket bind fail with no obvious client error.
    let tmp = std::path::Path::new("/tmp");
    let temp_dir = TempDir::new_in(tmp).expect("temp dir under /tmp");
    let socket_path = temp_dir.path().join("s.sock");
    (temp_dir, socket_path)
}

pub fn spawn_ipc_server<A: NodeAPI + Send + Sync + 'static>(
    server_path: PathBuf,
    node_api: Arc<A>,
) -> JoinHandle<Result<(), ModuleError>> {
    spawn_ipc_server_with(server_path, node_api, |s| s)
}

/// Build the server (e.g. `with_event_manager`) before `start`.
pub fn spawn_ipc_server_with<A, F>(
    server_path: PathBuf,
    node_api: Arc<A>,
    configure: F,
) -> JoinHandle<Result<(), ModuleError>>
where
    A: NodeAPI + Send + Sync + 'static,
    F: FnOnce(ModuleIpcServer) -> ModuleIpcServer + Send + 'static,
{
    tokio::spawn(async move {
        let mut server = configure(ModuleIpcServer::new(&server_path));
        server.start(node_api).await
    })
}

/// Wait until `bind` created the socket file, then connect. If the server task exits first,
/// surface `ModuleIpcServer::start` errors instead of retrying connect until timeout.
pub async fn wait_bound_then_connect(
    socket_path: &Path,
    server: &mut JoinHandle<Result<(), ModuleError>>,
) -> ModuleIpcClient {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if socket_path.exists() {
            return ModuleIpcClient::connect(socket_path)
                .await
                .unwrap_or_else(|e| {
                    panic!("connect to {socket_path:?} after bind failed: {e:?}");
                });
        }
        if server.is_finished() {
            match server.await {
                Ok(Ok(())) => panic!("server returned before binding (unexpected)"),
                Ok(Err(e)) => panic!("IPC server failed to start: {e:?}"),
                Err(j) => panic!("IPC server task panicked: {j}"),
            }
        }
        if Instant::now() > deadline {
            panic!(
                "timeout waiting for socket file {socket_path:?} (path.len={}, dir={:?})",
                socket_path.as_os_str().len(),
                socket_path.parent()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}
