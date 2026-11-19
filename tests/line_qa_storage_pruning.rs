//! Test-line QA: pruning refuses a window smaller than the configured minimum.

use blvm_node::config::{PruningConfig, PruningMode};
use blvm_node::storage::Storage;
use blvm_node::storage::pruning::PruningManager;

#[test]
fn prune_to_height_rejects_a_window_below_the_minimum() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Storage::new(dir.path()).expect("storage");
    let config = PruningConfig {
        mode: PruningMode::Normal,
        keep_blocks: 50,
        auto_prune: false,
        ..PruningConfig::default()
    };
    let manager = PruningManager::new(config, storage.blocks());

    match manager.prune_to_height(80, 100, false) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Pruning would leave 20 blocks, but keep_blocks is 50"),
                "prune_to_height returned {message}"
            );
        }
        Ok(stats) => panic!("short window was pruned: {stats:?}"),
    }
}
