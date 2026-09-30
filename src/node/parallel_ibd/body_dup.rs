//! R-303: read-only body receive / discard accounting. No control-flow change
//! at the call sites beyond counting.

use dashmap::DashMap;
use dashmap::DashSet;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscardReason {
    /// Far-ahead reject in `insert_reorder_gap_aware` (`mod.rs` ~1790).
    AdmitDrop,
    /// Height already in the per-pipe `received` map (overwrite).
    AlreadyPresent,
    /// `h < next_needed` at admit — validation already passed this height.
    AlreadyValidated,
    /// Pending GetData gone (requeue / timeout / cancelled receiver).
    RequeueLoser,
    /// Late body admitted into reorder instead of discarded (R-326).
    LateAdmit,
    /// `evict_reorder_gap_pressure` dropped the body.
    ReorderEvict,
    /// Per-pipe `received` trim (soft/hard cap).
    RecvTrim,
    /// Empty-witness MSG_BLOCK rejected.
    EmptyWitness,
}

struct Acc {
    n: AtomicU64,
    bytes: AtomicU64,
}

impl Acc {
    const fn new() -> Self {
        Self {
            n: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }
    fn add(&self, bytes: u64) {
        self.n.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
    fn get(&self) -> (u64, u64) {
        (
            self.n.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

struct Counters {
    rx: Acc,
    distinct: AtomicU64,
    wire: Acc,
    admit: Acc,
    present: Acc,
    validated: Acc,
    loser: Acc,
    late: Acc,
    /// Key missing in `pending_block_requests` (request already gone).
    loser_miss: AtomicU64,
    /// Key present but every receiver was dropped.
    loser_drop: AtomicU64,
    evict: Acc,
    trim: Acc,
    empty: Acc,
    seen: DashSet<u64>,
    discarded: DashSet<u64>,
    last_bytes: DashMap<u64, u64>,
}

impl Counters {
    fn new() -> Self {
        Self {
            rx: Acc::new(),
            distinct: AtomicU64::new(0),
            wire: Acc::new(),
            admit: Acc::new(),
            present: Acc::new(),
            validated: Acc::new(),
            loser: Acc::new(),
            late: Acc::new(),
            loser_miss: AtomicU64::new(0),
            loser_drop: AtomicU64::new(0),
            evict: Acc::new(),
            trim: Acc::new(),
            empty: Acc::new(),
            seen: DashSet::new(),
            discarded: DashSet::new(),
            last_bytes: DashMap::new(),
        }
    }
}

fn ctr() -> &'static Counters {
    static C: OnceLock<Counters> = OnceLock::new();
    C.get_or_init(Counters::new)
}

/// Every deserialized inbound `block` frame (the R-302 `offload_n` analogue).
pub(crate) fn note_wire_block(bytes: u64) {
    ctr().wire.add(bytes);
}

/// IBD GetData body that reached the download worker (`!from_local`).
pub(crate) fn note_body_rx(height: u64, bytes: u64) {
    let c = ctr();
    c.rx.add(bytes);
    c.last_bytes.insert(height, bytes);
    if c.seen.insert(height) {
        c.distinct.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn last_bytes(height: u64) -> u64 {
    ctr()
        .last_bytes
        .get(&height)
        .map(|v| *v)
        .unwrap_or(0)
}

pub(crate) fn note_discard(reason: DiscardReason, height: Option<u64>, bytes: u64) {
    let c = ctr();
    let b = match height {
        Some(h) if bytes == 0 => last_bytes(h),
        _ => bytes,
    };
    match reason {
        DiscardReason::AdmitDrop => c.admit.add(b),
        DiscardReason::AlreadyPresent => c.present.add(b),
        DiscardReason::AlreadyValidated => c.validated.add(b),
        DiscardReason::RequeueLoser => c.loser.add(b),
        DiscardReason::LateAdmit => c.late.add(b),
        DiscardReason::ReorderEvict => c.evict.add(b),
        DiscardReason::RecvTrim => c.trim.add(b),
        DiscardReason::EmptyWitness => c.empty.add(b),
    }
    if let Some(h) = height {
        c.discarded.insert(h);
    }
}

/// Which RequeueLoser branch ran. Counted even when the body is late-admitted.
pub(crate) fn note_loser_branch_missing() {
    ctr().loser_miss.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn note_loser_branch_dropped() {
    ctr().loser_drop.fetch_add(1, Ordering::Relaxed);
}

/// Anatomy: had any body for H been received and then thrown away earlier?
pub(crate) fn height_was_discarded(h: u64) -> bool {
    ctr().discarded.contains(&h)
}

#[derive(Clone, Copy, Default)]
struct Snap {
    rx_n: u64,
    rx_b: u64,
    distinct: u64,
    wire_n: u64,
    wire_b: u64,
    admit_n: u64,
    admit_b: u64,
    present_n: u64,
    present_b: u64,
    validated_n: u64,
    validated_b: u64,
    loser_n: u64,
    loser_b: u64,
    late_n: u64,
    late_b: u64,
    loser_miss_n: u64,
    loser_drop_n: u64,
    evict_n: u64,
    evict_b: u64,
    trim_n: u64,
    trim_b: u64,
    empty_n: u64,
    empty_b: u64,
}

fn snap_now() -> Snap {
    let c = ctr();
    let (rx_n, rx_b) = c.rx.get();
    let (wire_n, wire_b) = c.wire.get();
    let (admit_n, admit_b) = c.admit.get();
    let (present_n, present_b) = c.present.get();
    let (validated_n, validated_b) = c.validated.get();
    let (loser_n, loser_b) = c.loser.get();
    let (late_n, late_b) = c.late.get();
    let (evict_n, evict_b) = c.evict.get();
    let (trim_n, trim_b) = c.trim.get();
    let (empty_n, empty_b) = c.empty.get();
    Snap {
        rx_n,
        rx_b,
        distinct: c.distinct.load(Ordering::Relaxed),
        wire_n,
        wire_b,
        admit_n,
        admit_b,
        present_n,
        present_b,
        validated_n,
        validated_b,
        loser_n,
        loser_b,
        late_n,
        late_b,
        loser_miss_n: c.loser_miss.load(Ordering::Relaxed),
        loser_drop_n: c.loser_drop.load(Ordering::Relaxed),
        evict_n,
        evict_b,
        trim_n,
        trim_b,
        empty_n,
        empty_b,
    }
}

fn sub(a: Snap, b: Snap) -> Snap {
    Snap {
        rx_n: a.rx_n.saturating_sub(b.rx_n),
        rx_b: a.rx_b.saturating_sub(b.rx_b),
        distinct: a.distinct.saturating_sub(b.distinct),
        wire_n: a.wire_n.saturating_sub(b.wire_n),
        wire_b: a.wire_b.saturating_sub(b.wire_b),
        admit_n: a.admit_n.saturating_sub(b.admit_n),
        admit_b: a.admit_b.saturating_sub(b.admit_b),
        present_n: a.present_n.saturating_sub(b.present_n),
        present_b: a.present_b.saturating_sub(b.present_b),
        validated_n: a.validated_n.saturating_sub(b.validated_n),
        validated_b: a.validated_b.saturating_sub(b.validated_b),
        loser_n: a.loser_n.saturating_sub(b.loser_n),
        loser_b: a.loser_b.saturating_sub(b.loser_b),
        late_n: a.late_n.saturating_sub(b.late_n),
        late_b: a.late_b.saturating_sub(b.late_b),
        loser_miss_n: a.loser_miss_n.saturating_sub(b.loser_miss_n),
        loser_drop_n: a.loser_drop_n.saturating_sub(b.loser_drop_n),
        evict_n: a.evict_n.saturating_sub(b.evict_n),
        evict_b: a.evict_b.saturating_sub(b.evict_b),
        trim_n: a.trim_n.saturating_sub(b.trim_n),
        trim_b: a.trim_b.saturating_sub(b.trim_b),
        empty_n: a.empty_n.saturating_sub(b.empty_n),
        empty_b: a.empty_b.saturating_sub(b.empty_b),
    }
}

fn emit_one(tag: &str, h: u64, s: Snap) {
    let ratio = if s.distinct == 0 {
        0.0
    } else {
        s.rx_n as f64 / s.distinct as f64
    };
    info!(
        "[IBD_BODY_DUP] {} h={} rx_n={} rx_B={} distinct_n={} ratio={:.2} wire_n={} wire_B={} admit_n={} admit_B={} present_n={} present_B={} validated_n={} validated_B={} loser_n={} loser_B={} late_n={} late_B={} loser_miss_n={} loser_drop_n={} evict_n={} evict_B={} trim_n={} trim_B={} empty_n={} empty_B={}",
        tag,
        h,
        s.rx_n,
        s.rx_b,
        s.distinct,
        ratio,
        s.wire_n,
        s.wire_b,
        s.admit_n,
        s.admit_b,
        s.present_n,
        s.present_b,
        s.validated_n,
        s.validated_b,
        s.loser_n,
        s.loser_b,
        s.late_n,
        s.late_b,
        s.loser_miss_n,
        s.loser_drop_n,
        s.evict_n,
        s.evict_b,
        s.trim_n,
        s.trim_b,
        s.empty_n,
        s.empty_b,
    );
}

pub(crate) fn emit(h: u64) {
    static LAST: OnceLock<std::sync::Mutex<Snap>> = OnceLock::new();
    let last = LAST.get_or_init(|| std::sync::Mutex::new(Snap::default()));
    let now = snap_now();
    let win = {
        let mut g = last.lock().unwrap_or_else(|e| e.into_inner());
        let w = sub(now, *g);
        *g = now;
        w
    };
    emit_one("win", h, win);
    emit_one("cum", h, now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r303_second_body_same_height_is_not_a_new_distinct() {
        // Isolated: use a high sentinel height so this does not collide with other tests
        // if they share process-global counters (OnceLock lives for the test bin).
        let h = u64::MAX - 303;
        let before = snap_now();
        note_body_rx(h, 1000);
        note_body_rx(h, 1000);
        let after = snap_now();
        assert_eq!(after.rx_n.saturating_sub(before.rx_n), 2, "two receives");
        assert_eq!(
            after.distinct.saturating_sub(before.distinct),
            1,
            "one distinct height"
        );
    }

    #[test]
    fn r303_discard_marks_height_for_anatomy() {
        let h = u64::MAX - 304;
        assert!(!height_was_discarded(h));
        note_body_rx(h, 500);
        note_discard(DiscardReason::AdmitDrop, Some(h), 500);
        assert!(height_was_discarded(h));
    }

    #[test]
    fn r326_late_admit_bucket_is_not_a_loser() {
        let before = snap_now();
        note_loser_branch_missing();
        note_discard(DiscardReason::LateAdmit, None, 400);
        note_loser_branch_dropped();
        note_discard(DiscardReason::RequeueLoser, None, 100);
        let after = snap_now();
        assert_eq!(after.late_n.saturating_sub(before.late_n), 1);
        assert_eq!(after.late_b.saturating_sub(before.late_b), 400);
        assert_eq!(after.loser_n.saturating_sub(before.loser_n), 1);
        assert_eq!(after.loser_miss_n.saturating_sub(before.loser_miss_n), 1);
        assert_eq!(after.loser_drop_n.saturating_sub(before.loser_drop_n), 1);
    }
}
