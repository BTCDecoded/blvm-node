//! Tests for sync coordinator and state machine

use blvm_node::node::sync::{SyncState, SyncStateMachine};
use blvm_protocol::test_utils::create_test_header;

#[test]
fn test_sync_state_machine_initial_state() {
    let machine = SyncStateMachine::new();
    assert!(matches!(machine.state(), &SyncState::Initial));
    assert_eq!(machine.progress(), 0.0);
    assert!(!machine.is_synced());
}

#[test]
fn test_sync_state_machine_transitions() {
    let mut machine = SyncStateMachine::new();

    // Test state transitions
    machine.transition_to(SyncState::Headers);
    assert!(matches!(machine.state(), &SyncState::Headers));

    machine.transition_to(SyncState::Blocks);
    assert!(matches!(machine.state(), &SyncState::Blocks));

    machine.transition_to(SyncState::Synced);
    assert!(matches!(machine.state(), &SyncState::Synced));
    assert!(machine.is_synced());
}

#[test]
fn test_sync_state_machine_error_state() {
    let mut machine = SyncStateMachine::new();

    machine.set_error("Test error".to_string());
    assert!(matches!(machine.state(), &SyncState::Error(_)));
    assert_eq!(machine.progress(), 0.0);
    assert!(!machine.is_synced());
}

#[test]
fn test_sync_state_machine_update_best_header() {
    let mut machine = SyncStateMachine::new();
    let header = create_test_header(1231006505, [0u8; 32]);

    machine.update_best_header(header.clone());
    assert!(machine.best_header().is_some());
    assert_eq!(machine.best_header().unwrap().version, header.version);
}

#[test]
fn test_sync_state_machine_update_chain_tip() {
    let mut machine = SyncStateMachine::new();
    let header = create_test_header(1231006505, [0u8; 32]);

    machine.update_chain_tip(header.clone());
    assert!(machine.chain_tip().is_some());
    assert_eq!(machine.chain_tip().unwrap().version, header.version);
}

#[test]
fn test_sync_state_machine_progress_updates() {
    let mut machine = SyncStateMachine::new();

    // Initial progress should be 0.0
    assert_eq!(machine.progress(), 0.0);

    // Progress should update on state transitions
    machine.transition_to(SyncState::Headers);
    let progress1 = machine.progress();

    machine.transition_to(SyncState::Blocks);
    let progress2 = machine.progress();

    // Progress should increase (or at least not decrease)
    assert!(progress2 >= progress1);

    machine.transition_to(SyncState::Synced);
    // Synced state should have high progress
    assert!(machine.progress() > 0.0);
}

#[test]
fn test_sync_state_all_variants() {
    // Test that all SyncState variants can be created
    let states = vec![
        SyncState::Initial,
        SyncState::Headers,
        SyncState::Blocks,
        SyncState::Synced,
        SyncState::Error("test".to_string()),
    ];

    for state in states {
        let mut machine = SyncStateMachine::new();
        machine.transition_to(state.clone());
        // Verify state was set correctly
        match (&state, machine.state()) {
            (SyncState::Initial, &SyncState::Initial) => {}
            (SyncState::Headers, &SyncState::Headers) => {}
            (SyncState::Blocks, &SyncState::Blocks) => {}
            (SyncState::Synced, &SyncState::Synced) => {}
            (SyncState::Error(_), &SyncState::Error(_)) => {}
            _ => panic!("State mismatch"),
        }
    }
}

#[test]
fn test_sync_state_machine_default() {
    let machine = SyncStateMachine::default();
    assert!(matches!(machine.state(), &SyncState::Initial));
    assert_eq!(machine.progress(), 0.0);
}

#[test]
fn test_sync_state_machine_error_message() {
    let mut machine = SyncStateMachine::new();
    let error_msg = "Connection failed".to_string();

    machine.set_error(error_msg.clone());

    match machine.state() {
        SyncState::Error(msg) => assert_eq!(msg, &error_msg),
        _ => panic!("Expected Error state"),
    }

    // Verify error state has progress reset
    assert_eq!(machine.progress(), 0.0);
}

#[test]
fn test_sync_coordinator_start_sync_reaches_synced() {
    use blvm_node::node::sync::{SyncCoordinator, SyncState};

    let mut coordinator = SyncCoordinator::new();
    assert!(!coordinator.is_synced());
    coordinator.start_sync().unwrap();
    assert!(coordinator.is_synced());
    assert!(matches!(
        coordinator.current_sync_state(),
        SyncState::Synced
    ));
    assert!(coordinator.progress() > 0.0);
}

#[test]
fn test_sync_state_machine_ibd_phase_event_strings() {
    let mut machine = SyncStateMachine::new();
    machine.transition_to(SyncState::Headers);
    assert_eq!(SyncState::Headers.as_event_str(), "Headers");
    machine.transition_to(SyncState::Blocks);
    assert_eq!(SyncState::Blocks.as_event_str(), "Blocks");
    machine.transition_to(SyncState::Synced);
    assert!(machine.is_synced());
}

#[test]
fn test_sync_coordinator_progress_before_start() {
    use blvm_node::node::sync::SyncCoordinator;

    let coordinator = SyncCoordinator::new();
    assert_eq!(coordinator.progress(), 0.0);
    assert!(!coordinator.is_synced());
}

#[test]
fn test_sync_coordinator_current_state_before_start() {
    use blvm_node::node::sync::{SyncCoordinator, SyncState};

    let coordinator = SyncCoordinator::new();
    assert!(matches!(
        coordinator.current_sync_state(),
        SyncState::Initial
    ));
}

#[test]
fn test_sync_coordinator_process_block_invalid_wire_rejects() {
    use blvm_node::node::sync::SyncCoordinator;
    use blvm_node::storage::Storage;
    use blvm_protocol::{BitcoinProtocolEngine, ProtocolVersion, UtxoSet};
    use std::sync::Arc;
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let mut coordinator = SyncCoordinator::new();
    let protocol = BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap();
    let mut utxo_set = UtxoSet::default();
    let result = coordinator.process_block(
        storage.blocks().as_ref(),
        &protocol,
        Some(&storage),
        &[0xff, 0xfe, 0xfd],
        0,
        &mut utxo_set,
        None,
        None,
    );
    assert!(result.is_err());
}

#[test]
fn test_sync_coordinator_process_block_valid_regtest_block() {
    use blvm_node::node::metrics::MetricsCollector;
    use blvm_node::node::performance::PerformanceProfiler;
    use blvm_node::node::sync::SyncCoordinator;
    use blvm_node::storage::Storage;
    use blvm_protocol::mining::MiningResult;
    use blvm_protocol::segwit::Witness;
    use blvm_protocol::serialization::serialize_block_with_witnesses;
    use blvm_protocol::{BitcoinProtocolEngine, ConsensusProof, ProtocolVersion, UtxoSet};
    use std::sync::Arc;
    use tempfile::TempDir;

    let protocol = Arc::new(BitcoinProtocolEngine::new(ProtocolVersion::Regtest).unwrap());
    let genesis = protocol.get_network_params().genesis_block.clone();
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    storage.chain().initialize(&genesis.header).unwrap();

    let mut coordinator = SyncCoordinator::new();
    let mut utxo_set = UtxoSet::default();
    let consensus = ConsensusProof::new();
    let prev_header = genesis.header.clone();
    let prev_headers = vec![prev_header.clone(); 2016];

    let mut block = consensus
        .create_new_block(
            &utxo_set,
            &[],
            0,
            &prev_header,
            &prev_headers,
            &vec![0x00, 0xff],
            &vec![0x51],
        )
        .unwrap();
    block.header.version = 4;

    let (mined, result) = consensus.mine_block(block, 2_000_000).unwrap();
    assert!(matches!(result, MiningResult::Success));

    let witnesses: Vec<Vec<Witness>> = mined
        .transactions
        .iter()
        .map(|tx| tx.inputs.iter().map(|_| Witness::default()).collect())
        .collect();
    let wire = serialize_block_with_witnesses(&mined, &witnesses, true);

    let metrics = Arc::new(MetricsCollector::new());
    let profiler = Arc::new(PerformanceProfiler::new(1000));
    let accepted = coordinator
        .process_block(
            storage.blocks().as_ref(),
            protocol.as_ref(),
            Some(&storage),
            &wire,
            0,
            &mut utxo_set,
            Some(metrics),
            Some(profiler),
        )
        .unwrap();
    assert!(accepted);
    assert!(storage.blocks().block_count().unwrap() >= 1);
}
