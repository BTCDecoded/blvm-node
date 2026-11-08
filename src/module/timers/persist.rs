//! Durable timer records in the node tree `module_timers`.
//!
//! Key `{module_name}/{key}`. Value is `bincode` of [`PersistedTimer`].
//! A corrupt value is skipped. It does not stop the others from loading.

use serde::{Deserialize, Serialize};

use super::types::{TimerId, TimerSpec, TimerState};

pub const MODULE_TIMERS_TREE: &str = "module_timers";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedTimer {
    pub id: TimerId,
    pub spec: TimerSpec,
    pub seq: u64,
    pub state: TimerState,
    pub next_due_unix_ms: Option<u64>,
    pub next_due_height: Option<u64>,
    pub fires_remaining: Option<u32>,
    pub created_unix_ms: u64,
}

pub fn record_key(module: &str, key: &str) -> String {
    format!("{module}/{key}")
}

pub fn encode(timer: &PersistedTimer) -> Result<Vec<u8>, bincode::Error> {
    bincode::serialize(timer)
}

pub fn decode(bytes: &[u8]) -> Result<PersistedTimer, bincode::Error> {
    bincode::deserialize(bytes)
}
