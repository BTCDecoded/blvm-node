//! Storage database backend selection helpers.

use blvm_node::config::DatabaseBackendConfig;
use blvm_node::storage::database::{
    DatabaseBackend, backend_from_config, create_database, default_backend, fallback_backend,
    module_subprocess_database_backend_preference,
};
use tempfile::TempDir;

#[test]
fn database_default_and_fallback_backends() {
    let primary = default_backend();
    #[cfg(feature = "heed3")]
    assert_eq!(primary, DatabaseBackend::Heed3);
    let _fb = fallback_backend(primary);
    assert!(matches!(
        primary,
        DatabaseBackend::Sled
            | DatabaseBackend::RocksDB
            | DatabaseBackend::Redb
            | DatabaseBackend::TidesDB
            | DatabaseBackend::Heed3
    ));
}

#[test]
fn database_backend_from_config_auto() {
    let backend = backend_from_config(DatabaseBackendConfig::Auto).unwrap();
    assert_eq!(backend, default_backend());
}

#[test]
fn database_module_subprocess_preference_respects_override() {
    assert_eq!(
        module_subprocess_database_backend_preference(DatabaseBackend::RocksDB, Some("tidesdb")),
        "tidesdb"
    );
    assert_eq!(
        module_subprocess_database_backend_preference(DatabaseBackend::RocksDB, Some("auto")),
        "sled"
    );
    assert_eq!(
        module_subprocess_database_backend_preference(DatabaseBackend::TidesDB, None),
        "tidesdb"
    );
}

#[test]
fn database_create_instance_smoke() {
    use blvm_node::storage::database::KNOWN_TREE_NAMES;

    let temp_dir = TempDir::new().unwrap();
    let db = create_database(temp_dir.path(), default_backend(), None).unwrap();
    let tree = db.open_tree(KNOWN_TREE_NAMES[0]).unwrap();
    tree.insert(b"key", b"val").unwrap();
    assert_eq!(tree.get(b"key").unwrap(), Some(b"val".to_vec()));
    db.flush().unwrap();
}
