//! Test-line QA: a hosted service rejects a write to an unknown connection.

use blvm_node::module::services::host::ServiceHost;

#[tokio::test]
async fn write_rejects_an_unknown_connection() {
    let host = ServiceHost::new();
    match host.write("demo", 7, b"ping").await {
        Err(message) => {
            assert!(
                message.contains("unknown connection 7"),
                "write returned {message}"
            );
        }
        Ok(()) => panic!("unknown connection accepted a write"),
    }
}
