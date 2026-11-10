//! Process-latched env parses for IBD hot-path knobs.
//!
//! Production: first-read `OnceLock` (rematch sets env before process start).
//! `cfg(test)`: always re-read so unit tests may `set_var` mid-process.

/// Latch a `Copy` env parse. Each expansion gets its own `OnceLock` static.
macro_rules! latch_env {
    ($t:ty, $body:block) => {{
        #[cfg(test)]
        {
            $body
        }
        #[cfg(not(test))]
        {
            static CACHED: ::std::sync::OnceLock<$t> = ::std::sync::OnceLock::new();
            *CACHED.get_or_init(|| $body)
        }
    }};
}
pub(crate) use latch_env;
