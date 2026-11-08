//! A durable timer must still be there after the process is killed with SIGKILL.
//!
//! The child opens a real node database, fires the timer once, and waits until
//! that record is on disk. The parent then kills it with SIGKILL and loads the
//! same tree.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use blvm_node::module::timers::persist::{self, MODULE_TIMERS_TREE};
use blvm_node::module::timers::service::{ChannelTimerTransport, TimerService};
use blvm_node::module::timers::types::{
    monotonic_interval, CreateMode, First, Pacing, Persist, TimerAck, TimerConfig, TimerState,
    TimerTarget,
};
use blvm_node::storage::Storage;

#[tokio::test]
async fn durable_timer_survives_sigkill() {
    if std::env::var_os("BLVM_TIMER_DURABLE_CHILD").is_some() {
        run_child().await;
        std::future::pending::<()>().await;
        return;
    }

    let dir = tempfile::TempDir::new_in("/tmp").expect("temp dir");
    let chain = dir.path().join("chain");
    std::fs::create_dir_all(&chain).unwrap();
    let ready = dir.path().join("ready");
    let exe = std::env::current_exe().unwrap();
    let mut child = std::process::Command::new(exe)
        .args(std::env::args().skip(1))
        .env("BLVM_TIMER_DURABLE_CHILD", "1")
        .env("BLVM_TIMER_DIR", &chain)
        .env("BLVM_TIMER_READY", &ready)
        .spawn()
        .expect("spawn child");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before the record was durable: {status}");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("child did not flush the durable timer");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // `Child::kill` sends SIGKILL.
    child.kill().expect("sigkill");
    let status = child.wait().unwrap();
    assert!(!status.success());

    let storage = Storage::new(&chain).expect("reopen chain");
    let tree = storage.open_tree(MODULE_TIMERS_TREE).unwrap();
    let service = TimerService::spawn_stored(
        TimerConfig::default(),
        std::collections::HashMap::new(),
        Some(tree),
        None,
    );
    let restored = service.list_all(Some("fees")).await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].key.as_deref(), Some("compact"));
    assert_eq!(restored[0].seq, 1);
    assert_eq!(restored[0].state, TimerState::Detached);
    assert!(restored[0].next_due_unix_ms.is_some());
}

async fn run_child() {
    let dir = std::env::var("BLVM_TIMER_DIR").unwrap();
    let ready = std::env::var("BLVM_TIMER_READY").unwrap();
    let storage = Arc::new(Storage::new(Path::new(&dir)).expect("open chain"));
    let tree = storage.open_tree(MODULE_TIMERS_TREE).unwrap();
    let (transport, mut rx) = ChannelTimerTransport::pair();
    let mut config = TimerConfig::default();
    config.min_period_ms = 1;
    let service = TimerService::spawn_stored(
        config,
        std::collections::HashMap::new(),
        Some(Arc::clone(&tree) as Arc<dyn blvm_node::storage::database::Tree>),
        None,
    );
    service.set_transport(transport).await;
    service.attach_module("fees", "fees").await;

    let mut spec = monotonic_interval(Duration::from_secs(1), Pacing::FixedRate);
    spec.first = First::Delay(Duration::from_millis(200));
    spec.key = Some("compact".into());
    spec.persist = Persist::Durable { audit: false };
    service
        .create("fees", spec, CreateMode::CreateOnly)
        .await
        .unwrap();

    let pending = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("fire timed out")
        .expect("channel closed");
    assert_eq!(pending.fire.seq, 1);
    pending.reply.send(Ok(TimerAck::Continue)).unwrap();

    let key = persist::record_key("fees", "compact");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(bytes) = tree.get(key.as_bytes()).unwrap() {
            if let Ok(saved) = persist::decode(&bytes) {
                if saved.seq == 1 {
                    break;
                }
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("durable record did not reach seq 1");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let info = service
        .get("fees", TimerTarget::Key("compact".into()))
        .await
        .unwrap();
    assert_eq!(info.seq, 1);
    std::fs::write(&ready, b"1").unwrap();
}
