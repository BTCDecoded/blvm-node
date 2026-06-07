//! Module registry CAS and local cache round-trips.

use blvm_node::module::registry::cache::{CachedModule, LocalCache};
use blvm_node::module::registry::cas::ContentAddressableStorage;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn cas_store_get_verify_and_has() {
    let dir = TempDir::new().unwrap();
    let mut cas = ContentAddressableStorage::new(dir.path()).unwrap();
    let content = b"module-binary-payload";
    let hash = cas.store(content).unwrap();
    assert!(cas.has(&hash));
    assert!(cas.verify(content, &hash));
    assert_eq!(cas.get(&hash).unwrap(), content);
    assert!(!cas.verify(b"tampered", &hash));
}

#[test]
fn cas_get_missing_hash_errors() {
    let dir = TempDir::new().unwrap();
    let cas = ContentAddressableStorage::new(dir.path()).unwrap();
    let missing = [0x99u8; 32];
    assert!(cas.get(&missing).is_err());
}

#[test]
fn local_cache_round_trip_and_expiry() {
    let dir = TempDir::new().unwrap();
    let mut cache = LocalCache::new();
    cache.cache(CachedModule {
        name: "mod-a".into(),
        version: "1.0.0".into(),
        hash: [0x01; 32],
        manifest_hash: [0x02; 32],
        binary_hash: [0x03; 32],
        verified_at: 1,
        verified_by: vec!["mirror".into()],
        local_path: PathBuf::from("/tmp/mod-a"),
        expires_at: None,
    });
    assert!(cache.is_valid("mod-a"));
    assert_eq!(cache.list_modules(), vec!["mod-a".to_string()]);
    cache.remove("mod-a");
    assert!(!cache.is_valid("mod-a"));
    cache.save(dir.path()).unwrap();
    let loaded = LocalCache::load(dir.path()).unwrap();
    assert!(loaded.list_modules().is_empty());
}

#[test]
fn local_cache_clear_expired_drops_stale_entries() {
    let mut cache = LocalCache::new();
    cache.cache(CachedModule {
        name: "stale".into(),
        version: "0.1".into(),
        hash: [0x04; 32],
        manifest_hash: [0x05; 32],
        binary_hash: [0x06; 32],
        verified_at: 0,
        verified_by: vec![],
        local_path: PathBuf::from("/tmp/stale"),
        expires_at: Some(1),
    });
    cache.clear_expired();
    assert!(!cache.is_valid("stale"));
}
