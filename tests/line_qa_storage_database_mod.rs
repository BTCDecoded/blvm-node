//! Test-line QA: backend_from_config rejects a backend whose feature is off.

use blvm_node::config::DatabaseBackendConfig;
use blvm_node::storage::database::backend_from_config;

#[test]
fn backend_from_config_rejects_redb_when_the_feature_is_off() {
    match backend_from_config(DatabaseBackendConfig::Redb) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Redb backend not available"),
                "backend_from_config returned {message}"
            );
        }
        Ok(backend) => panic!("Redb config was accepted as {backend:?}"),
    }
}
