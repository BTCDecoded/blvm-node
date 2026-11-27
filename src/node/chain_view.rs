//! Live chain reads during a disconnect.
//!
//! A reorg writes restored coins before the mempool has taken the transactions
//! that spent them. `gettxout` waits out that window, then returns
//! [`CHAIN_VIEW_MOVED`] instead of a coin. A later read of the same odd
//! generation does not wait again. Clearing is explicit: a `?` after the
//! disconnect commit must leave the generation odd.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub const CHAIN_VIEW_MOVED: &str = "chain view moved";
pub const CHAIN_VIEW_WAIT: Duration = Duration::from_secs(5);

struct Gate {
    generation: AtomicU64,
    gave_up_on: AtomicU64,
    wait_max: Mutex<Duration>,
    wake: Mutex<()>,
    settled: Condvar,
}

static GATE: Gate = Gate {
    generation: AtomicU64::new(0),
    gave_up_on: AtomicU64::new(0),
    wait_max: Mutex::new(CHAIN_VIEW_WAIT),
    wake: Mutex::new(()),
    settled: Condvar::new(),
};

fn lock_wake() -> std::sync::MutexGuard<'static, ()> {
    GATE.wake.lock().unwrap_or_else(|err| err.into_inner())
}

fn wait_max() -> Duration {
    *GATE.wait_max.lock().unwrap_or_else(|err| err.into_inner())
}

/// The generation is odd while a disconnect is visible and the mempool has not caught up.
pub fn is_odd() -> bool {
    GATE.generation.load(Ordering::SeqCst) % 2 == 1
}

/// Open the window. A second enter while it is already open does not move the generation.
pub fn enter() {
    let _wake = lock_wake();
    let generation = GATE.generation.load(Ordering::SeqCst);
    if generation % 2 == 0 {
        GATE.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// Close the window after the mempool has caught up, or after the coins were restored.
/// Even generations are left alone.
pub fn clear() {
    let _wake = lock_wake();
    let generation = GATE.generation.load(Ordering::SeqCst);
    if generation % 2 == 1 {
        GATE.generation.fetch_add(1, Ordering::SeqCst);
        GATE.settled.notify_all();
    }
}

/// Generation observed for this read. Waits at most [`CHAIN_VIEW_WAIT`] for an open window.
/// A read that already gave up on this generation returns at once.
pub fn wait_for_settle() -> u64 {
    let generation = GATE.generation.load(Ordering::SeqCst);
    if generation % 2 == 0 || GATE.gave_up_on.load(Ordering::SeqCst) == generation {
        return generation;
    }
    let started = Instant::now();
    let max = wait_max();
    let mut wake = lock_wake();
    let generation = loop {
        let generation = GATE.generation.load(Ordering::SeqCst);
        let left = max.saturating_sub(started.elapsed());
        if generation % 2 == 0
            || left.is_zero()
            || GATE.gave_up_on.load(Ordering::SeqCst) == generation
        {
            break generation;
        }
        let (guard, _) = GATE
            .settled
            .wait_timeout(wake, left)
            .unwrap_or_else(|err| err.into_inner());
        wake = guard;
    };
    drop(wake);
    if generation % 2 == 1 && GATE.gave_up_on.swap(generation, Ordering::SeqCst) != generation {
        let _wake = lock_wake();
        GATE.settled.notify_all();
    }
    GATE.generation.load(Ordering::SeqCst)
}

/// True when the window was open for `seen`, or the generation moved during the read.
pub fn refused(seen: u64) -> bool {
    let now = GATE.generation.load(Ordering::SeqCst);
    seen % 2 == 1 || now != seen
}

#[cfg(test)]
pub(crate) fn set_wait_for_test(wait: Duration) {
    *GATE.wait_max.lock().unwrap_or_else(|err| err.into_inner()) = wait;
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    let _wake = lock_wake();
    GATE.generation.store(0, Ordering::SeqCst);
    GATE.gave_up_on.store(0, Ordering::SeqCst);
    *GATE.wait_max.lock().unwrap_or_else(|err| err.into_inner()) = CHAIN_VIEW_WAIT;
    GATE.settled.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            reset_for_test();
        }
    }

    #[test]
    fn an_error_after_enter_leaves_the_generation_odd() {
        let _reset = Reset;
        enter();
        let failed = (|| -> Result<(), ()> { Err(()) })();
        assert!(failed.is_err());
        assert!(is_odd());
        clear();
        assert!(!is_odd());
    }

    #[test]
    fn a_second_wait_on_the_same_generation_does_not_block() {
        let _reset = Reset;
        set_wait_for_test(Duration::from_millis(40));
        enter();
        let started = Instant::now();
        let first = wait_for_settle();
        assert!(
            started.elapsed() >= Duration::from_millis(30),
            "{:?}",
            started.elapsed()
        );
        assert!(refused(first));
        let started = Instant::now();
        let second = wait_for_settle();
        assert!(
            started.elapsed() < Duration::from_millis(30),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(first, second);
        assert!(refused(second));
    }

    #[test]
    fn the_default_wait_is_five_seconds() {
        assert_eq!(CHAIN_VIEW_WAIT, Duration::from_secs(5));
    }
}
