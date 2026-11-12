//! Test-line QA: a disabled fee estimator refuses a quote.

use blvm_node::config::FeeEstimatorConfig;
use blvm_node::node::fee_estimator::{FeeEstimator, FeeMode, FeeUnavailable, SourceView};

#[tokio::test]
async fn quote_rejects_a_disabled_estimator() {
    let mut cfg = FeeEstimatorConfig::default();
    cfg.enabled = false;
    let estimator = FeeEstimator::new(cfg, true);

    match estimator
        .quote(6, FeeMode::Conservative, SourceView::History)
        .await
    {
        Err(FeeUnavailable::Disabled) => {}
        Err(other) => panic!("quote returned {other:?}"),
        Ok(quote) => panic!("disabled estimator quoted {} sat/kvB", quote.sat_per_kvb),
    }
}
