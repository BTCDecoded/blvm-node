//! Core block-file reuse: which network's reader `open_core_block_reader_for_store`
//! opens. Lives in the crate because that function is `pub(crate)`.
//!
//! Network rule (see `BitcoinCoreDetection::resolve_reuse_network`): migration
//! marker, then Core folder name, then the node's network; every known source
//! must agree, and custom signet (`signet_<hash>`) folders are always refused.

use super::Storage;
use super::bitcoin_core_migrate::{MigrationMarker, migration_marker_path};
use super::bitcoin_detection::CoreDataNetwork;
use crate::config::StorageConfig;
use blvm_protocol::types::Network;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn create_core_layout(dir: &Path) {
    let chainstate = dir.join("chainstate");
    std::fs::create_dir_all(&chainstate).unwrap();
    std::fs::write(chainstate.join("CURRENT"), "MANIFEST-000001\n").unwrap();
    std::fs::write(chainstate.join("MANIFEST-000001"), b"").unwrap();
    std::fs::create_dir_all(dir.join("blocks")).unwrap();
}

/// A Core datadir at `temp/<rel>` plus an empty BLVM store next to it.
fn setup(temp: &TempDir, rel: &str) -> (PathBuf, PathBuf) {
    let core = temp.path().join(rel);
    create_core_layout(&core);
    let store = temp.path().join("blvm_store");
    std::fs::create_dir_all(&store).unwrap();
    (core, store)
}

fn write_marker(store: &Path, core: &Path, network: &str) {
    let marker = MigrationMarker {
        source: core.to_path_buf(),
        destination: store.to_path_buf(),
        network: network.to_string(),
        tip_hash: String::new(),
        height: 0,
        muhash: None,
        reuse_core_blocks: Some(true),
        migrated_at: "0".to_string(),
    };
    let path = migration_marker_path(store);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string(&marker).unwrap()).unwrap();
}

fn reuse_on() -> StorageConfig {
    StorageConfig {
        reuse_core_block_files: true,
        ..Default::default()
    }
}

fn open(store: &Path, core: &Path, node: Option<Network>) -> Option<CoreDataNetwork> {
    let config = reuse_on();
    Storage::open_core_block_reader_for_store(store, Some(core), Some(&config), node)
        .map(|r| r.network())
}

#[test]
fn custom_signet_core_dir_does_not_produce_a_reader() {
    // Moved from tests/bitcoin_core_tests.rs (it called a pub(crate) fn, E0624).
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "signet_abc123def456");
    assert_eq!(open(&store, &core, None), None);
    // Even with the node on (standard) signet: the custom signet magic differs.
    assert_eq!(open(&store, &core, Some(Network::Signet)), None);
}

#[test]
fn nonstandard_mainnet_names_reuse_with_node_network_mainnet() {
    // B2: Core's default mainnet folders on macOS/Windows ("Bitcoin"), a plain
    // "bitcoin", and arbitrary paths like /srv/btc-data must keep block reuse.
    for rel in ["bitcoin", "Bitcoin", "srv/btc-data"] {
        let temp = TempDir::new().unwrap();
        let (core, store) = setup(&temp, rel);
        assert_eq!(
            open(&store, &core, Some(Network::Mainnet)),
            Some(CoreDataNetwork::Mainnet),
            "{rel}"
        );
    }
}

#[test]
fn nonstandard_name_without_node_network_or_marker_is_not_mainnet_by_default() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "btc-data");
    assert_eq!(open(&store, &core, None), None);
}

#[test]
fn nonstandard_name_follows_node_network() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "chain-data");
    assert_eq!(
        open(&store, &core, Some(Network::Testnet4)),
        Some(CoreDataNetwork::Testnet4)
    );
}

#[test]
fn mainnet_marker_on_standard_signet_folder_is_refused() {
    // S3: the marker used to win unchecked and open a mainnet reader here.
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, ".bitcoin/signet");
    write_marker(&store, &core, "mainnet");
    assert_eq!(open(&store, &core, None), None);
    assert_eq!(open(&store, &core, Some(Network::Mainnet)), None);
}

#[test]
fn signet_marker_on_custom_signet_folder_is_refused() {
    // S3: a signet marker on signet_<hash> used to open a reader with the
    // standard signet magic.
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "signet_0f9188f13cb7b2c71f2a335e3a4fc328");
    write_marker(&store, &core, "signet");
    assert_eq!(open(&store, &core, None), None);
    assert_eq!(open(&store, &core, Some(Network::Signet)), None);
}

#[test]
fn marker_and_folder_disagreement_is_refused() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, ".bitcoin/testnet3");
    write_marker(&store, &core, "testnet4");
    assert_eq!(open(&store, &core, None), None);
}

#[test]
fn marker_and_node_disagreement_is_refused() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "btc-data");
    write_marker(&store, &core, "mainnet");
    assert_eq!(open(&store, &core, Some(Network::Regtest)), None);
}

#[test]
fn folder_and_node_disagreement_is_refused() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, ".bitcoin/testnet3");
    assert_eq!(open(&store, &core, Some(Network::Mainnet)), None);
}

#[test]
fn agreeing_sources_open_the_reader() {
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, ".bitcoin/signet");
    write_marker(&store, &core, "signet");
    assert_eq!(
        open(&store, &core, Some(Network::Signet)),
        Some(CoreDataNetwork::Signet)
    );

    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "btc-data");
    write_marker(&store, &core, "mainnet");
    assert_eq!(open(&store, &core, None), Some(CoreDataNetwork::Mainnet));
    assert_eq!(
        open(&store, &core, Some(Network::Mainnet)),
        Some(CoreDataNetwork::Mainnet)
    );
}

#[test]
fn open_for_node_passes_the_node_network_to_core_reuse() {
    // End to end through the public constructor: a mainnet Core datadir with a
    // non-standard name opened with the node network must keep reuse; the same
    // datadir with no network supplied must not silently become mainnet.
    use crate::storage::database::DatabaseBackend;
    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "btc-data");
    let config = reuse_on();
    let mainnet = Storage::with_backend_pruning_and_indexing(
        &store,
        DatabaseBackend::RocksDB,
        None,
        None,
        Some(&config),
        Some(&core),
        Some(Network::Mainnet),
    )
    .unwrap();
    assert_eq!(
        mainnet.blocks().bitcoin_core_reader_network(),
        Some(CoreDataNetwork::Mainnet)
    );
    drop(mainnet);

    let temp = TempDir::new().unwrap();
    let (core, store) = setup(&temp, "btc-data");
    let unknown = Storage::with_backend_pruning_and_indexing(
        &store,
        DatabaseBackend::RocksDB,
        None,
        None,
        Some(&config),
        Some(&core),
        None,
    )
    .unwrap();
    assert_eq!(unknown.blocks().bitcoin_core_reader_network(), None);
}
