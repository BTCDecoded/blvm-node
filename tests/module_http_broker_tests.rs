//! Phase 6 acceptance (a) and (b): a module fetches a local mock only when the
//! origin is allowed, and is refused for an unlisted origin, an off-list
//! redirect, and a private address.

#[path = "stub_node_api.rs"]
mod stub_node_api;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use blvm_node::config::NodeConfig;
use blvm_node::module::api::hub::ModuleApiHub;
use blvm_node::module::http::HttpPolicy;
use blvm_node::module::ipc::protocol::{MessageType, RequestMessage, RequestPayload, ResponsePayload};
use blvm_node::module::registry::manifest::ModuleManifest;
use blvm_node::module::security::permissions::{Permission, PermissionSet};
use stub_node_api::MockNodeAPI;

fn serve(response: &'static [u8]) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(response);
    });
    (port, handle)
}

fn hub_for(module: &str, allow: Vec<String>) -> ModuleApiHub {
    let node_api = Arc::new(MockNodeAPI);
    let mut hub = ModuleApiHub::new(node_api);
    let mut permissions = PermissionSet::new();
    permissions.add(Permission::HttpClient);
    hub.register_module_permissions(module.to_string(), permissions);
    let mut map = HashMap::new();
    map.insert(module.to_string(), allow);
    hub.set_http_policy(HttpPolicy {
        allow: map,
        proxy: None,
    });
    hub
}

fn http_request(url: &str) -> RequestMessage {
    RequestMessage {
        correlation_id: 1,
        request_type: MessageType::HttpRequest,
        payload: RequestPayload::HttpRequest {
            url: url.to_string(),
            accept: Some("application/json".to_string()),
            timeout_ms: 2_000,
        },
    }
}

#[tokio::test]
async fn allowed_local_origin_is_fetched() {
    let body = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}";
    let (port, handle) = serve(body);
    let origin = format!("http://127.0.0.1:{port}");
    let mut hub = hub_for("fees-mod", vec![origin.clone()]);
    let response = hub
        .handle_request("fees-mod", http_request(&format!("{origin}/fees")))
        .await
        .unwrap();
    handle.join().unwrap();
    assert!(response.success);
    match response.payload {
        Some(ResponsePayload::HttpResponse {
            status,
            content_type,
            body,
        }) => {
            assert_eq!(status, 200);
            assert_eq!(content_type.as_deref(), Some("application/json"));
            assert_eq!(body, b"{\"ok\":true}");
        }
        other => panic!("unexpected payload: {other:?}"),
    }
    let audit = hub.get_audit_log(10);
    assert_eq!(audit[0].module_id, "fees-mod");
    assert_eq!(audit[0].origin.as_deref(), Some(origin.as_str()));
    assert_eq!(audit[0].status, Some(200));
    assert!(audit[0].success);
}

#[tokio::test]
async fn unlisted_origin_redirect_and_private_address_are_refused() {
    let hit = Arc::new(AtomicBool::new(false));
    let hit_flag = Arc::clone(&hit);
    let secret = TcpListener::bind("127.0.0.1:0").unwrap();
    secret.set_nonblocking(true).unwrap();
    let secret_port = secret.local_addr().unwrap().port();
    let secret_thread = thread::spawn(move || {
        for _ in 0..40 {
            if let Ok((mut sock, _)) = secret.accept() {
                hit_flag.store(true, Ordering::SeqCst);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret");
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    let redirect = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{secret_port}/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let redirect_bytes = redirect.into_bytes();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let redirect_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(&redirect_bytes);
    });

    let origin = format!("http://127.0.0.1:{port}");
    let mut hub = hub_for("fees-mod", vec![origin.clone()]);

    let unlisted = hub
        .handle_request(
            "fees-mod",
            http_request("https://not-listed.example/fees"),
        )
        .await
        .unwrap();
    assert!(!unlisted.success);
    assert!(unlisted.error.unwrap().contains("not allowed"));

    let private = hub
        .handle_request("fees-mod", http_request("http://10.1.2.3/fees"))
        .await
        .unwrap();
    assert!(!private.success);
    assert!(private.error.unwrap().contains("private address"));

    let redirected = hub
        .handle_request("fees-mod", http_request(&format!("{origin}/go")))
        .await
        .unwrap();
    assert!(!redirected.success);
    let reason = redirected.error.unwrap();
    assert!(
        reason.contains("not allowed") || reason.contains("private address"),
        "{reason}"
    );
    redirect_thread.join().unwrap();
    secret_thread.join().unwrap();
    assert!(!hit.load(Ordering::SeqCst));

    let audit = hub.get_audit_log(10);
    assert!(audit.iter().any(|entry| {
        entry.module_id == "fees-mod" && !entry.success && entry.origin.is_some()
    }));
}

#[test]
fn operator_allow_list_is_not_a_module_config_table() {
    let config: NodeConfig = toml::from_str(
        r#"
[module_network.blvm-fees]
http_allow = ["https://mempool.space"]

[network]
http_proxy = "socks5h://127.0.0.1:9050"
"#,
    )
    .unwrap();
    assert_eq!(
        config.module_network["blvm-fees"].http_allow,
        vec!["https://mempool.space".to_string()]
    );
    assert_eq!(
        config.network.unwrap().http_proxy.as_deref(),
        Some("socks5h://127.0.0.1:9050")
    );
    let modules = config.modules.unwrap();
    assert!(modules.module_configs.get("blvm-fees").is_none());
}

#[test]
fn manifest_http_wants_are_kept_and_not_the_allow_list() {
    let manifest: ModuleManifest = toml::from_str(
        r#"
name = "blvm-fees"
version = "0.1.0"
entry_point = "blvm-fees"

[network]
http_wants = ["https://mempool.space"]
"#,
    )
    .unwrap();
    assert_eq!(
        manifest.network.http_wants,
        vec!["https://mempool.space".to_string()]
    );
    let metadata = manifest.to_metadata();
    assert!(metadata.capabilities.is_empty());
}
