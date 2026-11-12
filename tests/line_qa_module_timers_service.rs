//! Test-line QA: looking up a timer key that was never created fails.

use std::collections::HashMap;

use blvm_node::module::timers::{TimerConfig, TimerService, TimerTarget};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_rejects_a_missing_timer() {
    let service = TimerService::spawn(TimerConfig::default(), HashMap::new());

    match service
        .get("mod", TimerTarget::Key("missing".to_string()))
        .await
    {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("timer_not_found"),
                "TimerService::get returned {message}"
            );
        }
        Ok(info) => panic!("missing timer was returned: {info:?}"),
    }
}
