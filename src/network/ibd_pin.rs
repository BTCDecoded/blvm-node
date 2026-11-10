//! IBD peer-set pin (`BLVM_IBD_PIN_PEERS`).
//!
//! R-291: DNS seeds draw a new lottery every dest. When this env is set, skip
//! DNS entirely and connect exactly that host:port list, marked manual so
//! extra-outbound eviction leaves them alone. Not `BLVM_IBD_PEERS` (download
//! pin / LAN archive tip-now — that still DNS-seeds the mesh).

use std::net::SocketAddr;
use std::str::FromStr;
use tracing::warn;

/// True when `BLVM_IBD_PIN_PEERS` is a non-empty list.
pub fn ibd_peers_pinned() -> bool {
    std::env::var("BLVM_IBD_PIN_PEERS")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Parse `host:port,host:port,…`. Invalid tokens are skipped (logged).
pub fn parse_ibd_pin_peers(s: &str) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tok in s.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        match SocketAddr::from_str(tok) {
            Ok(addr) => {
                if seen.insert(addr) {
                    out.push(addr);
                }
            }
            Err(e) => {
                warn!(
                    "[IBD_PINNED_PEERS] skip invalid addr {:?} ({e}) — expected host:port",
                    tok
                );
            }
        }
    }
    out
}

/// Current pin list (empty = unset).
pub fn ibd_pin_peers() -> Vec<SocketAddr> {
    std::env::var("BLVM_IBD_PIN_PEERS")
        .ok()
        .map(|s| parse_ibd_pin_peers(&s))
        .unwrap_or_default()
}

pub fn ibd_pin_contains(addr: SocketAddr) -> bool {
    ibd_pin_peers().contains(&addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r291_pin_parse_skips_junk_and_dedupes() {
        let v = parse_ibd_pin_peers(
            "1.2.3.4:8333, 1.2.3.4:8333, not-an-addr, [2001:db8::1]:8333, 5.6.7.8:8333",
        );
        assert_eq!(
            v.len(),
            3,
            "R-291 pin list is comma host:port; junk skipped, dupes dropped. got {v:?}"
        );
        assert_eq!(v[0], "1.2.3.4:8333".parse::<SocketAddr>().unwrap());
        assert_eq!(v[2], "5.6.7.8:8333".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn r291_pin_empty_is_unpinned() {
        assert!(parse_ibd_pin_peers("").is_empty());
        assert!(parse_ibd_pin_peers("   ,  ,").is_empty());
    }
}
