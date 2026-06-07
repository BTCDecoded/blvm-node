//! `TimerManager` periodic timers and scheduled tasks.

use async_trait::async_trait;
use blvm_node::module::timers::manager::{TaskCallback, TimerCallback, TimerManager};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

struct Inc(Arc<AtomicU32>);

#[async_trait]
impl TimerCallback for Inc {
    async fn call(&self) -> Result<(), String> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[async_trait]
impl TaskCallback for Inc {
    async fn call(&self) -> Result<(), String> {
        self.0.fetch_add(10, Ordering::Relaxed);
        Ok(())
    }
}

#[tokio::test]
async fn timer_manager_fires_timer_and_task() {
    let counter = Arc::new(AtomicU32::new(0));
    let mut mgr = TimerManager::new();
    mgr.start();

    let timer_id = mgr
        .register_timer("mod-a".into(), 0, Arc::new(Inc(Arc::clone(&counter))))
        .await
        .unwrap();
    let task_id = mgr
        .schedule_task("mod-a".into(), 0, Arc::new(Inc(Arc::clone(&counter))))
        .await
        .unwrap();

    sleep(Duration::from_millis(350)).await;

    assert!(counter.load(Ordering::Relaxed) >= 1);
    mgr.cancel_timer(timer_id).await.unwrap();
    let _ = mgr.cancel_task(task_id).await;
    mgr.cancel_module_timers("mod-a").await;
    mgr.cancel_module_tasks("mod-a").await;
    mgr.stop();
}

#[tokio::test]
async fn timer_manager_cancel_missing_returns_error() {
    let mgr = TimerManager::new();
    assert!(mgr.cancel_timer(9999).await.is_err());
    assert!(mgr.cancel_task(9999).await.is_err());
}
