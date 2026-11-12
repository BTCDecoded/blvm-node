//! Test-line QA: pruning refuses a window smaller than the configured minimum.

use blvm_node::config::{PruningConfig, PruningMode};
use blvm_node::storage::Storage;
use blvm_node::storage::pruning::PruningManager;

#[test]
fn prune_to_height_rejects_a_window_below_the_minimum() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let storage = Storage::new(dir.path()).expect("storage");
    let config = PruningConfig {
        mode: PruningMode::Normal {
            keep_from_height: 0,
            min_recent_blocks: 50,
        },
        auto_prune: false,
        auto_prune_interval: 100,
        min_blocks_to_keep: 50,
        prune_on_startup: false,
        incremental_prune_during_ibd: false,
        prune_window_size: 50,
        min_blocks_for_incremental_prune: 288,
    };
    let manager = PruningManager::new(config, storage.blocks());

    match manager.prune_to_height(80, 100, false) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Pruning would leave 20 blocks, but minimum is 50"),
                "prune_to_height returned {message}"
            );
        }
        Ok(stats) => panic!("short window was pruned: {stats:?}"),
    }
}
