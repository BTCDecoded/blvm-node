//! Phase 6 acceptance (c) and (d): an in-repo module serves HTTP and an echo
//! stream on node-bound listeners, then loses those listeners on unload.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use blvm_node::config::{HostedServiceConfig, NodeConfig};
use blvm_node::module::manager::ModuleManager;
use blvm_node::module::registry::manifest::ModuleManifest;
use blvm_node::module::services::{
    BindRequest, HostedModule, ServiceHost, ServiceHttpRequest, ServiceHttpResponse, ServiceKind,
    StreamEvent,
};
use blvm_node::module::validation::manifest_validator::{ManifestValidator, ValidationResult};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct EchoModule {
    conn: Mutex<Option<u64>>,
    seqs: Mutex<Vec<u64>>,
}

#[async_trait]
impl HostedModule for EchoModule {
    async fn http(&self, req: ServiceHttpRequest) -> Result<ServiceHttpResponse, String> {
        Ok(ServiceHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: req.body,
        })
    }

    async fn stream(&self, event: StreamEvent) -> Result<Vec<u8>, String> {
        match event {
            StreamEvent::Open { conn_id, .. } => {
                *self.conn.lock().unwrap() = Some(conn_id);
                Ok(Vec::new())
            }
            StreamEvent::Data { seq, bytes, .. } => {
                self.seqs.lock().unwrap().push(seq);
                Ok(bytes)
            }
            StreamEvent::Close { .. } => Ok(Vec::new()),
        }
    }
}

async fn read_until(sock: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !got.windows(needle.len()).any(|w| w == needle)
    {
        let mut buf = [0u8; 1024];
        match tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) | Ok(Err(_)) => break,
            Ok(Ok(n)) => got.extend_from_slice(&buf[..n]),
        }
    }
    got
}

fn manifest(toml_text: &str) -> ModuleManifest {
    toml::from_str(toml_text).unwrap()
}

#[tokio::test]
async fn http_and_echo_stream_close_on_unload() {
    let temp = tempfile::tempdir().unwrap();
    let mut manager = ModuleManager::new(temp.path(), temp.path(), temp.path());
    let echo = Arc::new(EchoModule {
        conn: Mutex::new(None),
        seqs: Mutex::new(Vec::new()),
    });
    let addrs = manager
        .service_host()
        .bind(
            "idx",
            vec![
                BindRequest {
                    name: "esplora".to_string(),
                    kind: ServiceKind::Http,
                    listen: "127.0.0.1:0".parse().unwrap(),
                    max_connections: 0,
                    tls_cert: None,
                },
                BindRequest {
                    name: "electrum".to_string(),
                    kind: ServiceKind::Stream,
                    listen: "127.0.0.1:0".parse().unwrap(),
                    max_connections: 0,
                    tls_cert: None,
                },
            ],
            echo.clone() as Arc<dyn HostedModule>,
        )
        .await
        .unwrap();
    let http_addr = addrs[0];
    let stream_addr = addrs[1];

    let mut http = TcpStream::connect(http_addr).await.unwrap();
    http.write_all(
        b"POST /echo?x=1 HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nping",
    )
    .await
    .unwrap();
    let http_body = read_until(&mut http, b"ping").await;
    let http_text = String::from_utf8_lossy(&http_body);
    assert!(
        http_text.contains("200"),
        "http response missing status: {http_text}"
    );
    assert!(
        http_text.contains("ping"),
        "http response missing body: {http_text}"
    );

    let mut stream = TcpStream::connect(stream_addr).await.unwrap();
    let conn_id = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(id) = *echo.conn.lock().unwrap() {
                break id;
            }
            if tokio::time::Instant::now() > deadline {
                panic!("stream did not open");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    stream.write_all(b"ping").await.unwrap();
    let echoed = read_until(&mut stream, b"ping").await;
    assert!(echoed.windows(4).any(|w| w == b"ping"), "echo was {echoed:?}");
    let seqs = echo.seqs.lock().unwrap().clone();
    assert!(!seqs.is_empty());
    assert_eq!(seqs[0], 1);
    for pair in seqs.windows(2) {
        assert!(pair[1] == pair[0] + 1);
    }

    manager
        .service_host()
        .write("idx", conn_id, b"pong")
        .await
        .unwrap();
    let pushed = read_until(&mut stream, b"pong").await;
    assert!(
        pushed.windows(4).any(|w| w == b"pong"),
        "stream send was {pushed:?}"
    );

    manager.remember_stopped_module("idx").await;
    manager.unload_module("idx").await.unwrap();
    assert!(manager.service_host().bound_addrs("idx").await.is_empty());

    let mut refused = false;
    for _ in 0..50 {
        let http_down = TcpStream::connect(http_addr).await.is_err();
        let stream_down = TcpStream::connect(stream_addr).await.is_err();
        if http_down && stream_down {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "listeners still accepted after unload");
}

#[tokio::test]
async fn tls_cert_refuses_plaintext_and_unset_listen_does_not_bind() {
    let host = ServiceHost::new();
    let err = host
        .bind(
            "idx",
            vec![BindRequest {
                name: "esplora".to_string(),
                kind: ServiceKind::Http,
                listen: "127.0.0.1:0".parse().unwrap(),
                max_connections: 256,
                tls_cert: Some("cert.pem".to_string()),
            }],
            Arc::new(EchoModule {
                conn: Mutex::new(None),
                seqs: Mutex::new(Vec::new()),
            }),
        )
        .await
        .unwrap_err();
    assert!(err.contains("tls_cert"), "{err}");
    assert!(host.bound_addrs("idx").await.is_empty());

    let temp = tempfile::tempdir().unwrap();
    let mut manager = ModuleManager::new(temp.path(), temp.path(), temp.path());
    let mut configs = std::collections::HashMap::new();
    configs.insert(
        "esplora".to_string(),
        HostedServiceConfig {
            module: "idx".to_string(),
            listen: None,
            tls_cert: None,
            max_connections: None,
        },
    );
    manager.set_service_configs(configs);
    let declared = manifest(
        r#"
name = "idx"
version = "0.1.0"
entry_point = "idx"
capabilities = ["host_service"]
min_node_version = "0.1.*"

[[services]]
name = "esplora"
kind = "http"
"#,
    );
    manager.bind_declared_services(&declared).await.unwrap();
    assert!(manager.service_host().bound_addrs("idx").await.is_empty());
}

#[test]
fn services_and_http_wants_require_a_matching_node_version() {
    let validator = ManifestValidator::new();
    let missing = manifest(
        r#"
name = "idx"
version = "0.1.0"
entry_point = "idx"
capabilities = ["host_service"]

[[services]]
name = "esplora"
kind = "http"
"#,
    );
    match validator.validate(&missing) {
        ValidationResult::Invalid(errors) => {
            assert!(
                errors.iter().any(|err| err.contains("min_node_version")),
                "{errors:?}"
            );
        }
        ValidationResult::Valid => panic!("services without min_node_version were accepted"),
    }

    let wants = manifest(
        r#"
name = "fees"
version = "0.1.0"
entry_point = "fees"

[network]
http_wants = ["https://example.test"]
"#,
    );
    assert!(matches!(
        validator.validate(&wants),
        ValidationResult::Invalid(_)
    ));

    let ok = manifest(
        r#"
name = "idx"
version = "0.1.0"
entry_point = "idx"
capabilities = ["host_service"]
min_node_version = "0.1.*"
replaces = ["blvm-wallet-index"]
provides_apis = ["blvm.upstream.v1"]
requires_apis = ["blvm.upstream.v1"]

[[services]]
name = "esplora"
kind = "http"

[[services]]
name = "electrum"
kind = "stream"

[[fee_estimators]]
name = "native"
"#,
    );
    match validator.validate(&ok) {
        ValidationResult::Valid => {}
        ValidationResult::Invalid(errors) => panic!("{errors:?}"),
    }
    assert_eq!(ok.fee_estimators.len(), 1);
    assert_eq!(
        ok.fee_estimators[0].get("name").and_then(|v| v.as_str()),
        Some("native")
    );

    let future = manifest(
        r#"
name = "idx"
version = "0.1.0"
entry_point = "idx"
min_node_version = "9.0.0"

[[services]]
name = "esplora"
kind = "http"
"#,
    );
    match validator.validate(&future) {
        ValidationResult::Invalid(errors) => {
            assert!(
                errors.iter().any(|err| err.contains("not satisfied")),
                "{errors:?}"
            );
        }
        ValidationResult::Valid => panic!("a future node constraint was accepted"),
    }
}

#[test]
fn replaces_renames_the_old_data_dir_once() {
    let temp = tempfile::tempdir().unwrap();
    let manager = ModuleManager::new(temp.path(), temp.path(), temp.path());
    let old = temp.path().join("blvm-wallet-index");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("keep"), b"blocks").unwrap();
    let declared = manifest(
        r#"
name = "blvm-index"
version = "0.1.0"
entry_point = "blvm-index"
replaces = ["blvm-wallet-index"]
"#,
    );
    manager.adopt_replaced_data(&declared).unwrap();
    let moved = temp.path().join("blvm-index").join("keep");
    assert_eq!(std::fs::read(&moved).unwrap(), b"blocks");
    assert!(!old.exists());
    manager.adopt_replaced_data(&declared).unwrap();
    assert_eq!(std::fs::read(moved).unwrap(), b"blocks");
}

#[tokio::test]
async fn requires_apis_match_a_loaded_provider() {
    let temp = tempfile::tempdir().unwrap();
    let manager = ModuleManager::new(temp.path(), temp.path(), temp.path());
    let provider = manifest(
        r#"
name = "blvm-upstream"
version = "0.1.0"
entry_point = "blvm-upstream"
provides_apis = ["blvm.upstream.v1"]
"#,
    );
    let consumer = manifest(
        r#"
name = "blvm-fees"
version = "0.1.0"
entry_point = "blvm-fees"
requires_apis = ["blvm.upstream.v1"]
"#,
    );
    let missing = manager.check_requires_apis(&consumer).await.unwrap_err();
    assert!(missing.to_string().contains("blvm.upstream.v1"));
    manager.record_declaration(&provider).await;
    manager.check_requires_apis(&consumer).await.unwrap();
}

#[test]
fn service_table_is_not_a_module_config() {
    let config: NodeConfig = toml::from_str(
        r#"
[services.esplora]
module = "blvm-index"
listen = "127.0.0.1:3000"
max_connections = 256

[services.electrum]
module = "blvm-index"
"#,
    )
    .unwrap();
    let esplora = &config.services["esplora"];
    assert_eq!(esplora.module, "blvm-index");
    assert_eq!(esplora.listen.as_deref(), Some("127.0.0.1:3000"));
    assert_eq!(esplora.max_connections, Some(256));
    assert!(config.services["electrum"].listen.is_none());
    let modules = config.modules.unwrap();
    assert!(modules.module_configs.get("esplora").is_none());
    assert!(modules.module_configs.get("electrum").is_none());
}
