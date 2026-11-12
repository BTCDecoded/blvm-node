//! Test-line QA: a Commons payout above the subsidy is rejected.

use std::sync::Arc;

use async_trait::async_trait;
use blvm_node::node::mempool::MempoolManager;
use blvm_node::node::miner::MiningCoordinator;
use blvm_node::rpc::errors::RpcResult;
use blvm_node::rpc::mining::{CommonsGbtCaller, CommonsGbtSlot};

struct OversizePayout;

#[async_trait]
impl CommonsGbtCaller for OversizePayout {
    async fn fetch_commons_gbt_outputs(&self) -> RpcResult<Option<Vec<(i64, Vec<u8>)>>> {
        Ok(Some(vec![(6_000_000_000, vec![0x51])]))
    }
}

#[tokio::test]
async fn generate_block_template_rejects_an_oversize_payout() {
    let mempool = Arc::new(MempoolManager::new());
    let mut coordinator = MiningCoordinator::new(mempool, None);
    let slot = CommonsGbtSlot::default();
    slot.set(Arc::new(OversizePayout));
    coordinator.set_commons_gbt_slot(slot);

    match coordinator.generate_block_template().await {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("exceed subsidy+fees"),
                "generate_block_template returned {message}"
            );
        }
        Ok(template) => panic!(
            "oversize payout was accepted into a template with {} transactions",
            template.transactions.len()
        ),
    }
}
