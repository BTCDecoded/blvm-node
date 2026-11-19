//! Logging utilities for consistent logging across node and modules
//!
//! Provides simple, non-overengineered logging initialization that:
//! - Supports centralized node logging
//! - Supports module-specific logging with different filters
//! - Respects RUST_LOG environment variable
//! - Allows configuration via config file
//!
//! # Usage
//!
//! ## Main Node
//! ```no_run
//! use blvm_node::utils::init_logging;
//!
//! init_logging(None); // Uses RUST_LOG or defaults to "info"
//! ```
//!
//! ## Module
//! ```no_run
//! use blvm_node::utils::init_module_logging;
//!
//! init_module_logging("my_module", None); // Module gets its own filter
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Live log size that `scripts/ibd/wan-bench-common.sh` rotates at start.
pub(crate) const LOG_ROTATE_BYTES: u64 = 200 * 1024 * 1024;

/// Initialize logging for the main node
///
/// Uses standard Rust logging practices:
/// - Respects RUST_LOG environment variable (standard practice)
/// - Falls back to config filter if provided
/// - Defaults to "info" level
/// - Uses `EnvFilter::from_default_env()` for proper RUST_LOG handling
///
/// # Arguments
/// * `filter` - Optional log filter from config (e.g., "info", "debug", "blvm_node=debug,network=trace")
///   If None, uses RUST_LOG environment variable or defaults to "info"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_logging;
///
/// // Use RUST_LOG (standard) or default to "info"
/// init_logging(None);
///
/// // Override with config filter (RUST_LOG still takes precedence)
/// init_logging(Some("debug"));
/// ```
pub fn init_logging(filter: Option<&str>) {
    let resolved = current_node_log_filter(filter, false);
    install_text_subscriber(&resolved);
}

/// Filter string for the node subscriber.
///
/// `RUST_LOG`, then `BLVM_LOG_LEVEL`, then the config filter, then `--verbose`
/// (`blvm=debug,blvm_node=debug`), then `blvm=info,blvm_node=info`.
pub fn node_log_filter(
    rust_log: Option<&str>,
    blvm_log_level: Option<&str>,
    config_filter: Option<&str>,
    verbose: bool,
) -> String {
    if let Some(value) = rust_log.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if let Some(value) = blvm_log_level.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if let Some(value) = config_filter.map(str::trim).filter(|s| !s.is_empty()) {
        return value.to_string();
    }
    if verbose {
        return "blvm=debug,blvm_node=debug".to_string();
    }
    "blvm=info,blvm_node=info".to_string()
}

fn current_node_log_filter(config_filter: Option<&str>, verbose: bool) -> String {
    let rust_log = std::env::var("RUST_LOG").ok();
    let blvm_log_level = std::env::var("BLVM_LOG_LEVEL").ok();
    node_log_filter(
        rust_log.as_deref(),
        blvm_log_level.as_deref(),
        config_filter,
        verbose,
    )
}

fn install_text_subscriber(filter: &str) {
    let env_filter = EnvFilter::new(filter);
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true)
                .with_thread_ids(false)
                .with_ansi(std::env::var("NO_COLOR").is_err()),
        )
        .with(env_filter)
        .try_init();
    start_log_rotation();
}

/// Install the node subscriber once.
///
/// `verbose` is the `--verbose` flag: `blvm=debug,blvm_node=debug` when
/// `RUST_LOG`, `BLVM_LOG_LEVEL`, and `[logging].filter` are all unset.
pub fn init_node_logging(config: Option<&crate::config::LoggingConfig>, verbose: bool) {
    let config_filter = config.and_then(|c| c.filter.as_deref());
    let resolved = current_node_log_filter(config_filter, verbose);
    let want_json = config.map(|c| c.json_format).unwrap_or(false);
    if want_json {
        #[cfg(feature = "json-logging")]
        {
            init_json_logging(Some(&resolved));
            return;
        }
        #[cfg(not(feature = "json-logging"))]
        {
            install_text_subscriber(&resolved);
            tracing::warn!(
                "logging.json_format is set but the json-logging feature is off; using text logs"
            );
            return;
        }
    }
    install_text_subscriber(&resolved);
}

/// Initialize logging for a module
///
/// Modules get their own logging context with sensible defaults:
/// - Module name is included in filter
/// - Node module communication logs are visible
/// - Can be overridden with RUST_LOG (standard practice)
///
/// Uses standard Rust logging practices with module-specific defaults.
///
/// # Arguments
/// * `module_name` - Name of the module (e.g., "lightning", "lnbits")
/// * `filter` - Optional log filter from config. If None, uses:
///   - RUST_LOG if set (standard, takes precedence)
///   - Otherwise: "{module_name}=info,blvm_node::module=debug"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_module_logging;
///
/// // Default module logging (respects RUST_LOG)
/// init_module_logging("lightning", None);
///
/// // Custom filter from config (RUST_LOG still takes precedence)
/// init_module_logging("lightning", Some("lightning=debug"));
/// ```
pub fn init_module_logging(module_name: &str, filter: Option<&str>) {
    // Standard practice: Use EnvFilter::from_default_env() for RUST_LOG
    let mut env_filter = EnvFilter::from_default_env();

    // Default filter for modules: module at info, node module communication at debug
    let default_filter = format!("{}={},blvm_node::module=debug", module_name, "info");

    // If RUST_LOG is not set, use config filter or default
    if std::env::var("RUST_LOG").is_err() {
        env_filter = filter
            .map(EnvFilter::new)
            .unwrap_or_else(|| EnvFilter::new(&default_filter));
    }
    // If RUST_LOG is set, it takes precedence (standard practice)

    // Module logging setup (consistent with node):
    // - Same formatter as node for consistency
    // - Include target (module path) for better debugging
    // - Module-specific filter for isolation
    // - Can still see node module communication logs
    let _ = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_target(true) // Include module path - useful for module debugging
                .with_thread_ids(false) // Disable by default
                .with_ansi(std::env::var("NO_COLOR").is_err()), // Respect NO_COLOR standard
        )
        .with(env_filter)
        .try_init();
}

/// Initialize logging with JSON output (for production/monitoring)
///
/// Useful when logs need to be parsed by log aggregation systems.
///
/// # Arguments
/// * `filter` - Optional log filter. If None, uses RUST_LOG or defaults to "info"
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_json_logging;
///
/// init_json_logging(None);
/// ```
#[cfg(feature = "json-logging")]
pub fn init_json_logging(filter: Option<&str>) {
    // Standard practice: Use EnvFilter::from_default_env() for RUST_LOG
    let mut env_filter = EnvFilter::from_default_env();

    // If config provides a filter and RUST_LOG is not set, use config filter
    if filter.is_some() && std::env::var("RUST_LOG").is_err() {
        if let Some(f) = filter {
            env_filter = EnvFilter::new(f);
        }
    }

    // If neither RUST_LOG nor config filter is set, default to "info"
    if std::env::var("RUST_LOG").is_err() && filter.is_none() {
        env_filter = EnvFilter::new("info");
    }

    // JSON logging for structured logging (production/monitoring):
    // - JSON format for log aggregation systems
    // - Include target, spans, and span lists for full context
    // - Standard structured logging practice
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().json().with_target(true))
        .with(env_filter)
        .try_init();
    start_log_rotation();
}

/// Why the live log should be closed.
///
/// A new UTC day rotates on its own. A file larger than [`LOG_ROTATE_BYTES`]
/// rotates on the same rule as the IBD bench script, including in the middle
/// of a day. The returned day is the date stamped on the closed file.
pub(crate) fn rotation_stamp_day(open_day: i64, now_day: i64, size: u64) -> Option<i64> {
    if now_day != open_day {
        return Some(open_day);
    }
    if size > LOG_ROTATE_BYTES {
        return Some(now_day);
    }
    None
}

fn utc_unix_day() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0)
}

pub(crate) fn ymd_from_unix_day(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d)
}

pub(crate) fn format_unix_day(day: i64) -> String {
    let (y, m, d) = ymd_from_unix_day(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD` or `YYYY-MM-DD.N` written by this rotator. Script names
/// (`.prev`, `.stall-*`, `.pre-oom-restart-*`) do not match.
fn rotation_date(rest: &str) -> Option<&str> {
    let (date, tail) = if rest.len() == 10 {
        (rest, "")
    } else if rest.len() > 11 && rest.as_bytes().get(10) == Some(&b'.') {
        (&rest[..10], &rest[11..])
    } else {
        return None;
    };
    if !tail.is_empty() && !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let b = date.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !b[..4]
        .iter()
        .chain(b[5..7].iter())
        .chain(b[8..10].iter())
        .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some(date)
}

fn rotation_dest(live: &Path, day: i64) -> PathBuf {
    let stamp = format_unix_day(day);
    let stem = live
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ibd.log");
    let mut dest = live.with_file_name(format!("{stem}.{stamp}"));
    for n in 1..1000u32 {
        if !dest.exists() {
            return dest;
        }
        dest = live.with_file_name(format!("{stem}.{stamp}.{n}"));
    }
    dest
}

/// Drop dated rotations from before yesterday. `.prev` and restart copies stay.
pub(crate) fn prune_dated_rotations(live: &Path, today: i64) {
    let Some(dir) = live.parent() else {
        return;
    };
    let Some(name) = live.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let prefix = format!("{name}.");
    let keep_from = format_unix_day(today - 1);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let Some(fname) = fname.to_str() else {
            continue;
        };
        let Some(rest) = fname.strip_prefix(&prefix) else {
            continue;
        };
        let Some(date) = rotation_date(rest) else {
            continue;
        };
        if date < keep_from.as_str() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn stderr_log_path() -> Option<PathBuf> {
    let link = std::fs::read_link("/proc/self/fd/2").ok()?;
    if !link.is_absolute() {
        return None;
    }
    std::fs::metadata(&link).ok().filter(|m| m.is_file())?;
    Some(link)
}

fn start_log_rotation() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.get().is_some() {
        return;
    }
    let Some(path) = stderr_log_path() else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("log-rotate".to_string())
        .spawn(move || {
            let mut open_day = utc_unix_day();
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let now_day = utc_unix_day();
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let Some(stamp) = rotation_stamp_day(open_day, now_day, size) else {
                    continue;
                };
                if rotate_live_log(&path, stamp, now_day) {
                    open_day = now_day;
                }
            }
        })
        .ok();
}

fn rotate_live_log(live: &Path, stamp_day: i64, today: i64) -> bool {
    let _ = std::io::stderr().flush();
    let _ = std::io::stdout().flush();
    let dest = rotation_dest(live, stamp_day);
    if std::fs::rename(live, &dest).is_err() {
        return false;
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(live)
    {
        Ok(file) => file,
        Err(_) => {
            let _ = std::fs::rename(&dest, live);
            return false;
        }
    };
    if !retarget_stdio(&file) {
        let _ = std::fs::rename(&dest, live);
        return false;
    }
    prune_dated_rotations(live, today);
    true
}

fn retarget_stdio(file: &std::fs::File) -> bool {
    #[cfg(unix)]
    {
        let fd = std::os::fd::AsRawFd::as_raw_fd(file);
        unsafe { dup2(fd, 1) >= 0 && dup2(fd, 2) >= 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        false
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn dup2(oldfd: i32, newfd: i32) -> i32;
}

/// Initialize logging from NodeConfig
///
/// Integrates logging configuration from config file with standard RUST_LOG handling.
/// RUST_LOG always takes precedence (standard practice).
///
/// # Arguments
/// * `config` - Optional LoggingConfig from NodeConfig
///
/// # Example
/// ```no_run
/// use blvm_node::utils::init_logging_from_config;
/// use blvm_node::config::NodeConfig;
///
/// let config = NodeConfig::default();
/// init_logging_from_config(config.logging.as_ref());
/// ```
pub fn init_logging_from_config(config: Option<&crate::config::LoggingConfig>) {
    init_node_logging(config, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_filter_uses_config_when_env_unset() {
        let filter = node_log_filter(None, None, Some("blvm=debug"), false);
        assert_eq!(filter, "blvm=debug");
    }

    #[test]
    fn logging_filter_prefers_rust_log_then_blvm_level() {
        assert_eq!(
            node_log_filter(Some("trace"), Some("warn"), Some("blvm=debug"), true),
            "trace"
        );
        assert_eq!(
            node_log_filter(None, Some("warn"), Some("blvm=debug"), true),
            "warn"
        );
    }

    #[test]
    fn logging_filter_verbose_when_nothing_else_set() {
        assert_eq!(
            node_log_filter(None, None, None, true),
            "blvm=debug,blvm_node=debug"
        );
        assert_eq!(
            node_log_filter(None, None, None, false),
            "blvm=info,blvm_node=info"
        );
    }

    #[test]
    fn test_logging_initialization() {
        // Test that logging functions exist and have correct signatures
        // We can't actually call init_logging() because tracing_subscriber
        // can only be initialized once per process, and other tests may have
        // already initialized it. Instead, we verify the functions compile
        // and can be referenced.

        // Verify function signatures exist
        let _init_logging_fn: fn(Option<&str>) = init_logging;
        let _init_module_logging_fn: fn(&str, Option<&str>) = init_module_logging;
        let _init_logging_from_config_fn: fn(Option<&crate::config::LoggingConfig>) =
            init_logging_from_config;

        #[cfg(feature = "json-logging")]
        {
            let _init_json_logging_fn: fn(Option<&str>) = init_json_logging;
        }

        // If we can reference the functions, they compile correctly
        // This test will fail if function signatures change
    }

    /// One event, captured in a buffer. Does not call `init_json_logging`
    /// (`try_init` installs once per process and discards the line).
    #[cfg(feature = "json-logging")]
    #[test]
    fn json_format_emits_a_level_field() {
        use std::io::{self, Write};
        use std::sync::{Arc, Mutex};

        struct Buf(Arc<Mutex<Vec<u8>>>);

        impl Write for Buf {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(data);
                Ok(data.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_target(true)
            .with_writer(move || Buf(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("json probe");
        });
        let bytes = captured.lock().expect("log buffer").clone();
        let text = String::from_utf8(bytes).expect("log line is utf-8");
        let line = text
            .lines()
            .find(|line| !line.is_empty())
            .expect("one log line");
        let value: serde_json::Value = serde_json::from_str(line).expect("json log line");
        assert!(
            value.get("level").is_some(),
            "json log missing level: {line}"
        );
    }

    #[test]
    fn log_rotation_unix_day_is_utc_date() {
        assert_eq!(ymd_from_unix_day(0), (1970, 1, 1));
        assert_eq!(format_unix_day(20_774), "2026-11-17");
    }

    #[test]
    fn log_rotation_new_day_or_script_size() {
        assert_eq!(rotation_stamp_day(10, 10, 0), None);
        assert_eq!(rotation_stamp_day(10, 10, LOG_ROTATE_BYTES), None);
        assert_eq!(rotation_stamp_day(10, 10, LOG_ROTATE_BYTES + 1), Some(10));
        assert_eq!(rotation_stamp_day(10, 11, 0), Some(10));
    }

    #[test]
    fn log_rotation_dest_skips_a_taken_name() {
        let dir = std::env::temp_dir().join(format!(
            "blvm-log-rot-dest-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("temp dir");
        let live = dir.join("ibd.log");
        let taken = dir.join("ibd.log.2026-11-17");
        std::fs::write(&taken, b"x").expect("taken rotation");
        let dest = rotation_dest(&live, 20_774);
        assert_eq!(dest, dir.join("ibd.log.2026-11-17.1"));
        let _ = std::fs::remove_file(&taken);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn log_rotation_prune_keeps_yesterday_and_script_archives() {
        let dir = std::env::temp_dir().join(format!(
            "blvm-log-rot-prune-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("temp dir");
        let live = dir.join("ibd.log");
        let today = 20_774_i64;
        let keep = dir.join(format!("ibd.log.{}", format_unix_day(today - 1)));
        let drop_day = dir.join(format!("ibd.log.{}", format_unix_day(today - 2)));
        let drop_n = dir.join(format!("ibd.log.{}.2", format_unix_day(today - 2)));
        let prev = dir.join("ibd.log.prev");
        let stall = dir.join("ibd.log.stall-1");
        let restart = dir.join("ibd.log.pre-oom-restart-20261115T000000Z");
        for path in [&live, &keep, &drop_day, &drop_n, &prev, &stall, &restart] {
            std::fs::write(path, b"x").expect("fixture");
        }
        prune_dated_rotations(&live, today);
        assert!(keep.exists());
        assert!(!drop_day.exists());
        assert!(!drop_n.exists());
        assert!(prev.exists());
        assert!(stall.exists());
        assert!(restart.exists());
        assert!(live.exists());
        for path in [&live, &keep, &prev, &stall, &restart] {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&dir);
    }
}
