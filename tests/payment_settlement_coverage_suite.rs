//! Settlement monitor start/stop and background monitoring smoke.

use blvm_node::config::PaymentConfig;
use blvm_node::payment::processor::PaymentProcessor;
use blvm_node::payment::settlement::SettlementMonitor;
use blvm_node::payment::state_machine::PaymentStateMachine;
use blvm_node::storage::Storage;
use blvm_protocol::payment::PaymentOutput;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

fn settlement_fixture() -> (TempDir, Arc<PaymentStateMachine>, Arc<Storage>) {
    let temp_dir = TempDir::new().unwrap();
    let storage = Arc::new(Storage::new(temp_dir.path()).unwrap());
    let processor =
        Arc::new(PaymentProcessor::new(PaymentConfig::default()).expect("payment processor"));
    let state_machine = Arc::new(PaymentStateMachine::new(processor));
    (temp_dir, state_machine, storage)
}

#[tokio::test]
async fn test_settlement_monitor_start_and_stop() {
    let (_dir, state_machine, storage) = settlement_fixture();
    let monitor = SettlementMonitor::new(state_machine).with_storage(storage);

    let outputs = vec![PaymentOutput {
        script: vec![0x51],
        amount: Some(10_000),
    }];
    monitor
        .start_monitoring("payment-a", outputs, None)
        .await
        .expect("start monitoring");

    tokio::time::sleep(Duration::from_millis(20)).await;
    monitor.stop_monitoring("payment-a").await;
}

#[tokio::test]
async fn test_settlement_monitor_with_expected_tx_hash() {
    let (_dir, state_machine, storage) = settlement_fixture();
    let monitor = SettlementMonitor::new(state_machine).with_storage(storage);

    let outputs = vec![PaymentOutput {
        script: vec![0x52, 0x87],
        amount: None,
    }];
    let expected = [0x11u8; 32];
    monitor
        .start_monitoring("payment-b", outputs, Some(expected))
        .await
        .expect("start with expected hash");

    tokio::time::sleep(Duration::from_millis(20)).await;
    monitor.stop_monitoring("payment-b").await;
}

#[tokio::test]
async fn test_settlement_monitor_restart_same_payment_id() {
    let (_dir, state_machine, _storage) = settlement_fixture();
    let monitor = SettlementMonitor::new(state_machine);

    let outputs = vec![PaymentOutput {
        script: vec![0x53],
        amount: Some(1),
    }];
    monitor
        .start_monitoring("payment-c", outputs.clone(), None)
        .await
        .expect("first start");
    monitor.stop_monitoring("payment-c").await;

    monitor
        .start_monitoring("payment-c", outputs, None)
        .await
        .expect("second start");
    monitor.stop_monitoring("payment-c").await;
}
