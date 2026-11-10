//! New and tried address buckets used to pick outbound peers.

use std::collections::HashMap;
use std::net::SocketAddr;

pub const NEW_BUCKETS: usize = 1024;
pub const TRIED_BUCKETS: usize = 256;
pub const BUCKET_SIZE: usize = 64;
pub const MAX_ANCHORS: usize = 2;
pub const ANCHOR_KEEP_SECS: u64 = 7 * 24 * 60 * 60;
pub const TERRIBLE_UNSEEN_SECS: u64 = 30 * 24 * 60 * 60;
pub const TERRIBLE_FUTURE_SKEW_SECS: u64 = 10 * 60;
pub const TERRIBLE_FAILURES: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableSide {
    New,
    Tried,
}

/// Which table a selection uses. Both nonempty is the caller's coin.
pub fn choose_side(
    new_nonempty: bool,
    tried_nonempty: bool,
    pick_tried: bool,
) -> Option<TableSide> {
    match (new_nonempty, tried_nonempty) {
        (true, true) => Some(if pick_tried {
            TableSide::Tried
        } else {
            TableSide::New
        }),
        (false, true) => Some(TableSide::Tried),
        (true, false) => Some(TableSide::New),
        (false, false) => None,
    }
}

pub fn is_terrible_record(
    last_seen: u64,
    advertised_time: u32,
    now: u64,
    failures: u32,
    successes: u32,
) -> bool {
    if last_seen == 0 || now.saturating_sub(last_seen) > TERRIBLE_UNSEEN_SECS {
        return true;
    }
    let advertised = advertised_time as u64;
    if advertised_time != 0 && advertised > now.saturating_add(TERRIBLE_FUTURE_SKEW_SECS) {
        return true;
    }
    failures >= TERRIBLE_FAILURES && successes == 0
}

#[derive(Clone, Debug)]
struct Attempt {
    failures: u32,
    successes: u32,
}

#[derive(Clone, Debug)]
pub struct AddressTable {
    new_buckets: Vec<Vec<SocketAddr>>,
    tried_buckets: Vec<Vec<SocketAddr>>,
    attempts: HashMap<SocketAddr, Attempt>,
    anchors: Vec<(SocketAddr, u64)>,
}

impl Default for AddressTable {
    fn default() -> Self {
        Self::new()
    }
}

impl AddressTable {
    pub fn new() -> Self {
        Self {
            new_buckets: vec![Vec::new(); NEW_BUCKETS],
            tried_buckets: vec![Vec::new(); TRIED_BUCKETS],
            attempts: HashMap::new(),
            anchors: Vec::new(),
        }
    }

    pub fn bucket_index(addr: SocketAddr, buckets: usize, tried: bool) -> usize {
        let mut hash = if tried {
            0x9e37_79b9_u64
        } else {
            0x85eb_ca6b_u64
        };
        let ip = match addr.ip() {
            std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
            std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
        };
        for byte in ip {
            hash = hash
                .wrapping_mul(0x0100_0000_01b3)
                .wrapping_add(u64::from(byte));
        }
        hash = hash
            .wrapping_mul(0x0100_0000_01b3)
            .wrapping_add(u64::from(addr.port()));
        (hash as usize) % buckets
    }

    pub fn place_new(&mut self, addr: SocketAddr, is_terrible: impl Fn(SocketAddr) -> bool) {
        if self.contains_tried(addr) || self.contains_new(addr) {
            return;
        }
        let index = Self::bucket_index(addr, NEW_BUCKETS, false);
        Self::insert_capped(&mut self.new_buckets[index], addr, is_terrible);
    }

    pub fn promote_tried(&mut self, addr: SocketAddr, is_terrible: impl Fn(SocketAddr) -> bool) {
        for bucket in &mut self.new_buckets {
            bucket.retain(|stored| *stored != addr);
        }
        if self.contains_tried(addr) {
            return;
        }
        let index = Self::bucket_index(addr, TRIED_BUCKETS, true);
        Self::insert_capped(&mut self.tried_buckets[index], addr, is_terrible);
    }

    pub fn note_attempt(&mut self, addr: SocketAddr, success: bool) {
        let attempt = self.attempts.entry(addr).or_insert(Attempt {
            failures: 0,
            successes: 0,
        });
        if success {
            attempt.successes = attempt.successes.saturating_add(1);
        } else {
            attempt.failures = attempt.failures.saturating_add(1);
        }
    }

    pub fn attempt_counts(&self, addr: SocketAddr) -> (u32, u32) {
        self.attempts
            .get(&addr)
            .map(|attempt| (attempt.failures, attempt.successes))
            .unwrap_or((0, 0))
    }

    pub fn remove(&mut self, addr: SocketAddr) {
        for bucket in &mut self.new_buckets {
            bucket.retain(|stored| *stored != addr);
        }
        for bucket in &mut self.tried_buckets {
            bucket.retain(|stored| *stored != addr);
        }
        self.attempts.remove(&addr);
        self.anchors.retain(|(stored, _)| *stored != addr);
    }

    pub fn addrs(&self, side: TableSide) -> Vec<SocketAddr> {
        let buckets = match side {
            TableSide::New => &self.new_buckets,
            TableSide::Tried => &self.tried_buckets,
        };
        buckets.iter().flatten().copied().collect()
    }

    pub fn remember_anchor(&mut self, addr: SocketAddr, now: u64) {
        self.anchors.retain(|(stored, seen)| {
            *stored != addr && now.saturating_sub(*seen) <= ANCHOR_KEEP_SECS
        });
        self.anchors.insert(0, (addr, now));
        self.anchors.truncate(MAX_ANCHORS);
    }

    pub fn anchors(&mut self, now: u64) -> Vec<SocketAddr> {
        self.anchors
            .retain(|(_, seen)| now.saturating_sub(*seen) <= ANCHOR_KEEP_SECS);
        self.anchors.iter().map(|(addr, _)| *addr).collect()
    }

    fn insert_capped(
        bucket: &mut Vec<SocketAddr>,
        addr: SocketAddr,
        is_terrible: impl Fn(SocketAddr) -> bool,
    ) {
        if bucket.contains(&addr) {
            return;
        }
        if bucket.len() < BUCKET_SIZE {
            bucket.push(addr);
            return;
        }
        if let Some(slot) = bucket.iter().position(|stored| is_terrible(*stored)) {
            bucket[slot] = addr;
        }
    }

    fn contains_new(&self, addr: SocketAddr) -> bool {
        self.new_buckets.iter().any(|bucket| bucket.contains(&addr))
    }

    fn contains_tried(&self, addr: SocketAddr) -> bool {
        self.tried_buckets
            .iter()
            .any(|bucket| bucket.contains(&addr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 8333))
    }

    #[test]
    fn both_tables_follow_the_coin_and_one_table_does_not() {
        assert_eq!(choose_side(true, true, true), Some(TableSide::Tried));
        assert_eq!(choose_side(true, true, false), Some(TableSide::New));
        assert_eq!(choose_side(true, false, true), Some(TableSide::New));
        assert_eq!(choose_side(false, true, false), Some(TableSide::Tried));
        assert_eq!(choose_side(false, false, true), None);
    }

    #[test]
    fn terrible_is_unseen_future_or_three_failures_without_success() {
        let now = 3_000_000_u64;
        assert!(is_terrible_record(0, 1, now, 0, 0));
        assert!(is_terrible_record(
            now - TERRIBLE_UNSEEN_SECS - 1,
            1,
            now,
            0,
            1
        ));
        assert!(!is_terrible_record(
            now - TERRIBLE_UNSEEN_SECS,
            1,
            now,
            0,
            0
        ));
        let future = (now + TERRIBLE_FUTURE_SKEW_SECS + 1) as u32;
        assert!(is_terrible_record(now, future, now, 0, 1));
        assert!(is_terrible_record(now, 1, now, TERRIBLE_FAILURES, 0));
        assert!(!is_terrible_record(now, 1, now, TERRIBLE_FAILURES, 1));
    }

    #[test]
    fn a_full_bucket_keeps_sixty_four_and_replaces_a_terrible_slot() {
        let mut table = AddressTable::new();
        for n in 1..=64 {
            table.new_buckets[0].push(addr(n));
        }
        AddressTable::insert_capped(&mut table.new_buckets[0], addr(65), |_| false);
        assert_eq!(table.new_buckets[0].len(), BUCKET_SIZE);
        assert!(!table.new_buckets[0].contains(&addr(65)));
        AddressTable::insert_capped(&mut table.new_buckets[0], addr(65), |stored| {
            stored == addr(1)
        });
        assert_eq!(table.new_buckets[0].len(), BUCKET_SIZE);
        assert!(table.new_buckets[0].contains(&addr(65)));
        assert!(!table.new_buckets[0].contains(&addr(1)));
    }

    #[test]
    fn anchors_keep_two_for_seven_days() {
        let mut table = AddressTable::new();
        table.remember_anchor(addr(1), 1_000);
        table.remember_anchor(addr(2), 1_000);
        table.remember_anchor(addr(3), 1_000);
        assert_eq!(table.anchors(1_000), vec![addr(3), addr(2)]);
        let later = 1_000 + ANCHOR_KEEP_SECS + 1;
        assert!(table.anchors(later).is_empty());
    }
}
