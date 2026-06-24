//! Network RPC methods with real `NetworkManager`.

use blvm_node::network::NetworkManager;
use blvm_node::rpc::network::NetworkRpc;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;

fn rpc_with_network() -> NetworkRpc {
    let addr: SocketAddr = "127.0.0.1:18410".parse().unwrap();
    NetworkRpc::with_dependencies(Arc::new(NetworkManager::new(addr)))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_network_info_and_connection_count() {
    let rpc = rpc_with_network();
    let info = rpc.get_network_info().await.unwrap();
    assert_eq!(info.get("connections").unwrap().as_u64(), Some(0));
    assert!(info.get("version").is_some());

    let count = rpc.get_connection_count(&json!([])).await.unwrap();
    assert_eq!(count.as_u64(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_get_peer_info_and_net_totals() {
    let rpc = rpc_with_network();
    let peers = rpc.get_peer_info().await.unwrap();
    assert!(peers.is_array());
    assert!(peers.as_array().unwrap().is_empty());

    let totals = rpc.get_net_totals(&json!([])).await.unwrap();
    assert!(totals.get("totalbytesrecv").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_ping_and_dos_protection_smoke() {
    let rpc = rpc_with_network();
    assert!(rpc.ping(&json!([])).await.is_ok());
    let dos = rpc.get_dos_protection_info(&json!([])).await.unwrap();
    assert!(dos.is_object());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_banned_and_clear_banned() {
    let rpc = rpc_with_network();
    let banned = rpc.list_banned(&json!([])).await.unwrap();
    assert!(banned.is_array());
    assert!(rpc.clear_banned(&json!([])).await.is_ok());
}

#[tokio::test]
async fn test_network_rpc_without_manager_degrades() {
    let rpc = NetworkRpc::new();
    let info = rpc.get_network_info().await.unwrap();
    assert!(info.get("connections").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_set_ban_and_getnodeaddresses() {
    let rpc = rpc_with_network();
    rpc.set_ban(&json!(["127.0.0.1:8333", "add", 3600]))
        .await
        .unwrap();
    let banned = rpc.list_banned(&json!([])).await.unwrap();
    assert!(banned.as_array().unwrap().len() >= 1);
    let addrs = rpc.getnodeaddresses(&json!([1])).await.unwrap();
    assert!(addrs.is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_addnode_and_disconnect_smoke() {
    let rpc = rpc_with_network();
    rpc.add_node(&json!(["127.0.0.1:8333", "add"]))
        .await
        .unwrap();
    rpc.disconnect_node(&json!(["127.0.0.1:8333"]))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_addnode_invalid_address_errors() {
    let rpc = rpc_with_network();
    assert!(
        rpc.add_node(&json!(["not-an-address", "add"]))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_getaddednodeinfo_reports_peer_connection_state() {
    let rpc = rpc_with_network();
    let result = rpc
        .getaddednodeinfo(&json!(["127.0.0.1:8333"]))
        .await
        .unwrap();
    let entry = &result.as_array().unwrap()[0];
    assert_eq!(entry.get("connected").unwrap().as_bool(), Some(false));
    let addr_entry = &entry.get("addresses").unwrap().as_array().unwrap()[0];
    assert_eq!(addr_entry.get("connected").unwrap().as_bool(), Some(false));
}
