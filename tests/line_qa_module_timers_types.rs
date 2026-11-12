//! Test-line QA: reorg refire is rejected on the monotonic clock.

use std::time::Duration;

use blvm_node::module::timers::types::{
    Pacing, ReorgPolicy, TimerConfig, ValidateCtx, monotonic_interval, validate_spec,
};

#[test]
fn validate_spec_rejects_reorg_refire_on_the_monotonic_clock() {
    let mut spec = monotonic_interval(Duration::from_secs(60), Pacing::FixedRate);
    spec.reorg = ReorgPolicy::Refire;
    let limits = TimerConfig::default();
    let ctx = ValidateCtx {
        now_unix_ms: 0,
        tip_height: None,
    };

    match validate_spec(&spec, &limits, &ctx) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("reorg refire requires the chain clock"),
                "validate_spec returned {message}"
            );
        }
        Ok(()) => panic!("monotonic reorg refire was accepted"),
    }
}
