//! UTC cron schedules. The `cron` 0.17 crate parses seven fields:
//! second, minute, hour, day of month, month, day of week, year.
//!
//! `blvm-node` is a std binary, so this does not use a `no_std` parser.
//! The crate is MIT OR Apache-2.0 and is compiled only with `timer-cron`.

#[cfg(feature = "timer-cron")]
use chrono::{TimeZone, Utc};
#[cfg(feature = "timer-cron")]
use cron::Schedule;
#[cfg(feature = "timer-cron")]
use std::str::FromStr;

pub fn parse_expr(expr: &str) -> Result<(), String> {
    #[cfg(not(feature = "timer-cron"))]
    {
        let _ = expr;
        Err("cron requires the timer-cron feature".into())
    }
    #[cfg(feature = "timer-cron")]
    {
        Schedule::from_str(expr)
            .map(|_| ())
            .map_err(|err| err.to_string())
    }
}

#[cfg(feature = "timer-cron")]
pub fn next_after(expr: &str, unix_ms: u64) -> Option<u64> {
    let schedule = Schedule::from_str(expr).ok()?;
    let after = utc_from_unix_ms(unix_ms)?;
    let next = schedule.after(&after).next()?;
    u64::try_from(next.timestamp_millis()).ok()
}

#[cfg(feature = "timer-cron")]
pub fn slots_through(expr: &str, after_ms: u64, until_ms: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cursor = after_ms;
    while out.len() < 10_000 {
        let Some(next) = next_after(expr, cursor) else {
            break;
        };
        if next > until_ms {
            break;
        }
        out.push(next);
        cursor = next;
    }
    out
}

#[cfg(feature = "timer-cron")]
fn utc_from_unix_ms(unix_ms: u64) -> Option<chrono::DateTime<Utc>> {
    let secs = i64::try_from(unix_ms / 1000).ok()?;
    let nsecs = u32::try_from((unix_ms % 1000) * 1_000_000).ok()?;
    Utc.timestamp_opt(secs, nsecs).single()
}

#[cfg(all(test, feature = "timer-cron"))]
mod tests {
    use super::next_after;

    const DAY: u64 = 86_400_000;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> u64 {
        use chrono::TimeZone;
        chrono::Utc
            .with_ymd_and_hms(y, m, d, h, min, s)
            .single()
            .unwrap()
            .timestamp_millis() as u64
    }

    #[test]
    fn next_fire_matches_the_utc_table() {
        let cases = [
            (
                "0 0 3 * * * *",
                at(2024, 1, 1, 0, 0, 0),
                at(2024, 1, 1, 3, 0, 0),
            ),
            (
                "0 30 4 1 * * *",
                at(2024, 1, 1, 0, 0, 0),
                at(2024, 1, 1, 4, 30, 0),
            ),
            (
                "0 0 0 1 1 * *",
                at(2024, 6, 15, 12, 0, 0),
                at(2025, 1, 1, 0, 0, 0),
            ),
            (
                "0 0 12 * * Mon *",
                at(2024, 1, 1, 0, 0, 0),
                at(2024, 1, 1, 12, 0, 0),
            ),
            (
                "0 0 0 * * * *",
                at(2024, 1, 1, 0, 0, 0),
                at(2024, 1, 1, 0, 0, 0) + DAY,
            ),
        ];
        for (expr, after, expect) in cases {
            assert_eq!(next_after(expr, after), Some(expect), "{expr}");
        }
        assert!(super::parse_expr("not a cron").is_err());
    }
}
