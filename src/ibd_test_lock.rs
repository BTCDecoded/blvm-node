//! Reentrant process lock for IBD unit tests.
//!
//! Parallel `cargo test` shares `BLVM_*` env and tip/export atomics. Production
//! latches env with `OnceLock`; under `cfg(test)` env is re-read, so tests must
//! not interleave. Hold one guard per test thread (reentrant for nested reads).

#[cfg(test)]
mod imp {
    use std::cell::Cell;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    thread_local! {
        static HELD: Cell<bool> = const { Cell::new(false) };
    }

    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    pub struct Guard {
        inner: Option<MutexGuard<'static, ()>>,
    }

    impl Guard {
        pub fn new() -> Self {
            if HELD.with(|h| h.get()) {
                return Self { inner: None };
            }
            let lock = LOCK.get_or_init(|| Mutex::new(()));
            let inner = Some(lock.lock().unwrap_or_else(|e| e.into_inner()));
            HELD.with(|h| h.set(true));
            Self { inner }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.inner.is_some() {
                HELD.with(|h| h.set(false));
            }
        }
    }
}

#[cfg(test)]
pub use imp::Guard;

#[cfg(test)]
pub fn guard() -> Guard {
    Guard::new()
}
