//! `HookManager` fee/mempool cache hooks with timeouts.

use async_trait::async_trait;
use blvm_node::module::hooks::manager::HookManager;
use blvm_node::module::traits::{MempoolSize, ModuleError, ModuleHooks};
use std::sync::Arc;
use std::time::Duration;

struct FeeHook(u64);

#[async_trait]
impl ModuleHooks for FeeHook {
    async fn get_fee_estimate_cached(
        &self,
        _target_blocks: u32,
    ) -> Result<Option<u64>, ModuleError> {
        Ok(Some(self.0))
    }

    async fn get_mempool_stats_cached(&self) -> Result<Option<MempoolSize>, ModuleError> {
        Ok(None)
    }
}

struct MempoolHook(MempoolSize);

#[async_trait]
impl ModuleHooks for MempoolHook {
    async fn get_fee_estimate_cached(
        &self,
        _target_blocks: u32,
    ) -> Result<Option<u64>, ModuleError> {
        Ok(None)
    }

    async fn get_mempool_stats_cached(&self) -> Result<Option<MempoolSize>, ModuleError> {
        Ok(Some(self.0.clone()))
    }
}

struct SlowHook;

#[async_trait]
impl ModuleHooks for SlowHook {
    async fn get_fee_estimate_cached(
        &self,
        _target_blocks: u32,
    ) -> Result<Option<u64>, ModuleError> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(Some(999))
    }

    async fn get_mempool_stats_cached(&self) -> Result<Option<MempoolSize>, ModuleError> {
        Ok(None)
    }
}

struct ErrHook;

#[async_trait]
impl ModuleHooks for ErrHook {
    async fn get_fee_estimate_cached(
        &self,
        _target_blocks: u32,
    ) -> Result<Option<u64>, ModuleError> {
        Err(ModuleError::OperationError("hook fail".into()))
    }

    async fn get_mempool_stats_cached(&self) -> Result<Option<MempoolSize>, ModuleError> {
        Err(ModuleError::OperationError("hook fail".into()))
    }
}

#[tokio::test]
async fn hook_manager_returns_first_cached_fee() {
    let mut mgr = HookManager::new();
    mgr.register_hooks("a".into(), Arc::new(FeeHook(10)));
    mgr.register_hooks("b".into(), Arc::new(FeeHook(20)));
    assert_eq!(mgr.get_fee_estimate_cached(6).await, Some(10));
}

#[tokio::test]
async fn hook_manager_skips_none_and_errors_for_fee() {
    let mut mgr = HookManager::new();
    mgr.register_hooks("err".into(), Arc::new(ErrHook));
    mgr.register_hooks("ok".into(), Arc::new(FeeHook(42)));
    assert_eq!(mgr.get_fee_estimate_cached(3).await, Some(42));
}

#[tokio::test]
async fn hook_manager_skips_slow_hook() {
    let mut mgr = HookManager::new();
    mgr.register_hooks("slow".into(), Arc::new(SlowHook));
    mgr.register_hooks("fast".into(), Arc::new(FeeHook(7)));
    assert_eq!(mgr.get_fee_estimate_cached(2).await, Some(7));
}

#[tokio::test]
async fn hook_manager_mempool_stats_and_unregister() {
    let mut mgr = HookManager::new();
    let stats = MempoolSize {
        transaction_count: 3,
        size_bytes: 900,
        total_fee_sats: 1000,
    };
    mgr.register_hooks("m".into(), Arc::new(MempoolHook(stats)));
    let got = mgr.get_mempool_stats_cached().await.unwrap();
    assert_eq!(got.transaction_count, 3);
    mgr.unregister_hooks("m");
    assert!(mgr.get_mempool_stats_cached().await.is_none());
}
