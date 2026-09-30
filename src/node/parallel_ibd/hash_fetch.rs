//! Stage 2 missing-set scheduler (`BLVM_IBD_HASH_FETCH=1`).
//!
//! Startup mode, not per-peer selection. Default off — C1g / cheese / STICKY_HOLD /
//! FORCE stay on the flag-off assigner path (untouched). Deletion is Stage 3.
//!
//! Do not bind engine `contiguous_length` to body-store max.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::storage::blockstore::BlockStore;

use super::latch_env;

const DEFAULT_FETCH_AHEAD: u64 = 2048;
const DEFAULT_INFLIGHT_PER_PEER: usize = 16;
/// Deadline band only (8s). Assignment lane is [`lane_width`] (default 16).
const FRONTIER: u64 = 8;
const MID: u64 = 256;
/// B-6: near-frontier primary lane. Fast peers only. Default 16, env 8–32.
const DEFAULT_LANE: u64 = 16;
/// Peer is a lane holder when `ema >= best * this` (or no rates yet).
const LANE_FAST_FRAC: f64 = 0.7;
/// WAN GetData for one height is seconds, not 800ms. The 800ms/3s pair
/// expired HASH_FETCH inflight and re-issued GetData on the same peers
/// (HF-5 8s FAIL load). Match assigner hole CAP, not fixture-local RTT.
const FRONTIER_DEADLINE: Duration = Duration::from_secs(8);
const MID_DEADLINE: Duration = Duration::from_secs(16);
const DEEP_DEADLINE: Duration = Duration::from_secs(20);
/// S-7.2: `next_validation_height` (val+1) is the apply hole. 8s + same-peer
/// requeue left 88591 on one dead GetData for 237s. Tight, then **dup**.
const TIP_DEADLINE: Duration = Duration::from_secs(2);
/// S-9.3: S-8 escalate-to-5 collapsed 10k 499→151 on the same mesh class.
/// One extra only (S-7.2). No n=3/4/5.
const TIP_PEER_CAP: usize = 2;
const IDLE_DECAY: Duration = Duration::from_secs(5);
/// S-14: one frontier rule. If `val+1` is neither takeable nor progressing
/// for this long, put it in `missing`. Live holders keep racing (S-8.1).
/// 8s is wasteful vs the 2s dup; it is not another classifier.
const TIP_STUCK_AFTER: Duration = Duration::from_secs(8);

/// `BLVM_IBD_HASH_FETCH=1` — latched at first read (tests re-read).
pub(crate) fn enabled() -> bool {
    latch_env!(bool, {
        match std::env::var("BLVM_IBD_HASH_FETCH")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("1") | Some("true") | Some("yes") | Some("on") => true,
            _ => false,
        }
    })
}

pub(crate) fn fetch_ahead() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_HASH_FETCH_AHEAD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_FETCH_AHEAD)
            .clamp(64, 16_384)
    })
}

fn inflight_cap() -> usize {
    latch_env!(usize, {
        std::env::var("BLVM_IBD_HASH_FETCH_INFLIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_INFLIGHT_PER_PEER)
            .clamp(1, 128)
    })
}

/// Heights `[val+1, val+lane)` are the priority lane. Deep window stays on
/// everyone else. Not S-8: still one primary per height; tip cap stays 2.
fn lane_width() -> u64 {
    latch_env!(u64, {
        std::env::var("BLVM_IBD_HF_LANE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_LANE)
            .clamp(8, 32)
    })
}

#[derive(Clone, Debug)]
struct Inflight {
    peer: String,
    deadline: Instant,
    duplicate: bool,
    /// True when this height was taken from `store_ready` (body already on disk).
    from_store: bool,
    /// Tip primary: 2s deadline already counted; keep holder, wait for a dup.
    expire_noted: bool,
}

#[derive(Clone, Debug)]
struct PeerRate {
    ema_bps: f64,
    last: Instant,
}

/// Missing-set scheduler. `BTreeSet` drained lowest-first so the frontier is served first.
#[derive(Debug)]
pub(crate) struct HashFetchSched {
    missing: BTreeSet<u64>,
    /// Header known and body on disk. Emitted onto `block_tx` like flag-off LOCAL_DISK.
    store_ready: BTreeSet<u64>,
    inflight: HashMap<u64, Inflight>,
    /// Extra GetDatas for a height (frontier speculative second + tip escalate).
    extras: HashMap<u64, Vec<Inflight>>,
    rates: HashMap<String, PeerRate>,
    last_val: u64,
    fetch_ahead: u64,
    bytes_window: u64,
    window_start: Instant,
    /// Bytes admitted since the last `IBD_HF_LOG` share snapshot (per peer).
    peer_window_bytes: HashMap<String, u64>,
    /// Peers whose tip primary/dup just expired — do not re-primary them.
    tip_avoid: HashMap<u64, String>,
    /// S-12: peers that disconnected. Holders in this set are ghosts.
    gone: HashSet<String>,
    /// S-14: when `val+1` last became neither takeable nor progressing.
    tip_blocked_since: Option<Instant>,
    /// S-14: this frontier was assigned at least once (blocks no-header insert).
    tip_ever_held: bool,
}

impl HashFetchSched {
    pub(crate) fn new(fetch_ahead: u64) -> Self {
        Self {
            missing: BTreeSet::new(),
            store_ready: BTreeSet::new(),
            inflight: HashMap::new(),
            extras: HashMap::new(),
            rates: HashMap::new(),
            last_val: 0,
            fetch_ahead,
            bytes_window: 0,
            window_start: Instant::now(),
            peer_window_bytes: HashMap::new(),
            tip_avoid: HashMap::new(),
            gone: HashSet::new(),
            tip_blocked_since: None,
            tip_ever_held: false,
        }
    }

    pub(crate) fn missing_len(&self) -> usize {
        self.missing.len()
    }

    pub(crate) fn inflight_len(&self) -> usize {
        self.inflight.len() + self.extras.values().map(|v| v.len()).sum::<usize>()
    }

    pub(crate) fn inflight_peer_count(&self) -> usize {
        let mut peers: BTreeSet<&str> = BTreeSet::new();
        for inf in self.inflight.values() {
            peers.insert(inf.peer.as_str());
        }
        for xs in self.extras.values() {
            for inf in xs {
                peers.insert(inf.peer.as_str());
            }
        }
        peers.len()
    }

    fn extra_count(&self, h: u64) -> usize {
        self.extras.get(&h).map(|v| v.len()).unwrap_or(0)
    }

    fn tip_n(&self, h: u64) -> usize {
        usize::from(self.inflight.contains_key(&h)) + self.extra_count(h)
    }

    fn holds(&self, h: u64, peer: &str) -> bool {
        self.inflight.get(&h).is_some_and(|i| i.peer == peer)
            || self
                .extras
                .get(&h)
                .is_some_and(|v| v.iter().any(|i| i.peer == peer))
    }

    fn push_extra(&mut self, h: u64, peer: &str, now: Instant, val: u64) {
        if h == val.saturating_add(1) {
            self.tip_ever_held = true;
        }
        let deadline = now + self.deadline_for(h, val);
        self.extras.entry(h).or_default().push(Inflight {
            peer: peer.to_string(),
            deadline,
            duplicate: true,
            from_store: false,
            expire_noted: false,
        });
    }

    /// Drop validated heights; insert `[val+1, val+fetch_ahead]` whose header is known
    /// and whose body is absent.
    pub(crate) fn refill(&mut self, val: u64, store: &BlockStore) {
        let lo = val.saturating_add(1);
        let hi = val.saturating_add(self.fetch_ahead);
        self.missing.retain(|&h| h >= lo && h <= hi);
        self.store_ready.retain(|&h| h >= lo && h <= hi);
        self.inflight.retain(|&h, _| h >= lo && h <= hi);
        self.extras.retain(|&h, _| h >= lo && h <= hi);
        for h in lo..=hi {
            if self.missing.contains(&h)
                || self.store_ready.contains(&h)
                || self.inflight.contains_key(&h)
            {
                continue;
            }
            // S-13: tip extra_only must still enter missing so a new primary
            // can be taken. Ahead extras stay skipped (already racing).
            if self.extras.contains_key(&h) && h != lo {
                continue;
            }
            let Ok(Some(hash)) = store.get_hash_by_height(h) else {
                continue;
            };
            match store.has_block_body(&hash) {
                Ok(false) => {
                    self.missing.insert(h);
                }
                Ok(true) => {
                    self.store_ready.insert(h);
                }
                _ => {}
            }
        }
        if val != self.last_val {
            self.tip_blocked_since = None;
            self.tip_ever_held = false;
        }
        self.last_val = val;
    }

    /// Tests / local refill without a store: insert a height as missing.
    #[cfg(test)]
    pub(crate) fn insert_missing(&mut self, h: u64) {
        self.missing.insert(h);
        self.last_val = h.saturating_sub(1).min(self.last_val);
    }

    fn tip_holder_peers(&self) -> Vec<String> {
        let tip = self.last_val.saturating_add(1);
        let mut v = Vec::new();
        if let Some(inf) = self.inflight.get(&tip) {
            v.push(inf.peer.clone());
        }
        if let Some(xs) = self.extras.get(&tip) {
            for inf in xs {
                v.push(inf.peer.clone());
            }
        }
        v
    }

    fn peer_is_live(&self, peer: &str) -> bool {
        !self.gone.contains(peer)
    }

    fn tip_has_live_holder(&self) -> bool {
        self.tip_holder_peers()
            .iter()
            .any(|p| self.peer_is_live(p))
    }

    fn tip_takeable(&self) -> bool {
        let tip = self.last_val.saturating_add(1);
        self.missing.contains(&tip) || self.store_ready.contains(&tip)
    }

    /// S-14: one rule. Dead/absent → missing now. Held and not takeable for
    /// `TIP_STUCK_AFTER` → missing; live holders keep racing. Cap-full → drop
    /// the primary map slot (GetData is not cancelled) so a new primary fits.
    fn ensure_frontier_takeable(&mut self, now: Instant) {
        let tip = self.last_val.saturating_add(1);
        if (self.inflight.contains_key(&tip) || self.extras.contains_key(&tip))
            && !self.tip_has_live_holder()
        {
            self.revive_tip("holders_gone");
        }
        if self.tip_takeable() {
            self.tip_blocked_since = None;
            if let Some(inf) = self.inflight.get_mut(&tip) {
                if now >= inf.deadline && !inf.expire_noted {
                    inf.expire_noted = true;
                    N_TIP_EXPIRE.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        let since = *self.tip_blocked_since.get_or_insert(now);
        let age = now.saturating_duration_since(since);
        if age < TIP_STUCK_AFTER {
            if let Some(inf) = self.inflight.get_mut(&tip) {
                if now >= inf.deadline && !inf.expire_noted {
                    inf.expire_noted = true;
                    N_TIP_EXPIRE.fetch_add(1, Ordering::Relaxed);
                }
            }
            return;
        }
        let held_now = self.inflight.contains_key(&tip) || self.extras.contains_key(&tip);
        if !held_now && !self.tip_ever_held {
            return;
        }
        tracing::warn!(
            "[IBD_HF_TIP_STUCK] h={} age_ms={} holders={} extras={} inflight={} {}",
            tip,
            age.as_millis(),
            self.tip_holder_peers().join(","),
            self.extra_count(tip),
            u8::from(self.inflight.contains_key(&tip)),
            self.tip_state_suffix(self.last_val)
        );
        if !self.store_ready.contains(&tip) {
            self.missing.insert(tip);
        }
        if self.tip_n(tip) >= TIP_PEER_CAP {
            if let Some(p) = self.inflight.remove(&tip) {
                self.tip_avoid.insert(tip, p.peer);
            }
        }
        N_TIP_REVIVE.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            "[IBD_HF_TIP_PROMOTE] h={} reason=stuck_age extras={} holders={} hf_tip={}",
            tip,
            self.extra_count(tip),
            self.tip_holder_peers().join(","),
            if self.missing.contains(&tip) && self.extra_count(tip) > 0 {
                "missing+extra"
            } else if self.missing.contains(&tip) {
                "missing"
            } else {
                "store_ready"
            }
        );
        self.tip_blocked_since = None;
    }

    /// S-12.1: return a dead-held (or age-capped) frontier to `missing`.
    fn revive_tip(&mut self, reason: &str) {
        let tip = self.last_val.saturating_add(1);
        let holders = self.tip_holder_peers();
        let from_store = self.inflight.get(&tip).is_some_and(|i| i.from_store);
        self.inflight.remove(&tip);
        self.extras.remove(&tip);
        if from_store {
            self.store_ready.insert(tip);
        } else if !self.store_ready.contains(&tip) {
            self.missing.insert(tip);
        }
        N_TIP_REVIVE.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            "[IBD_HF_TIP_REVIVE] h={} reason={} holders={} hf_tip=missing",
            tip,
            reason,
            holders.join(",")
        );
    }

    /// Drop assignments held by a disconnected peer. If that empties the
    /// frontier, revive it. Reconnect (`take_work`) clears `gone`.
    pub(crate) fn peer_gone(&mut self, peer: &str) {
        self.gone.insert(peer.to_string());
        let tip = self.last_val.saturating_add(1);
        let held_tip = self.holds(tip, peer);
        self.inflight.retain(|_, inf| inf.peer != peer);
        for xs in self.extras.values_mut() {
            xs.retain(|inf| inf.peer != peer);
        }
        self.extras.retain(|_, xs| !xs.is_empty());
        if held_tip && !self.tip_has_live_holder() {
            // Maps already stripped; revive still inserts missing.
            self.revive_tip("holders_gone");
        } else if held_tip {
            self.ensure_frontier_takeable(Instant::now());
        }
    }

    /// S-12.2 / S-15.3: sleep only when the frontier cannot take a new
    /// primary. extra_only and dead/absent holders are that case.
    /// A *live* inflight past `TIP_DEADLINE` is not — the 2s dup already
    /// fired above, and sleeping every worker here expires ahead and
    /// reprints as 2× `took/h` (S-15.b). 8s stuck-age still recycles.
    fn frontier_unfillable(&self, val: u64) -> bool {
        let tip = val.saturating_add(1);
        if self.missing.contains(&tip) || self.store_ready.contains(&tip) {
            return false;
        }
        if !self.inflight.contains_key(&tip) && self.extras.contains_key(&tip) {
            return true;
        }
        if (self.inflight.contains_key(&tip) || self.extras.contains_key(&tip))
            && !self.tip_has_live_holder()
        {
            return true;
        }
        false
    }

    pub(crate) fn expire_deadlines(&mut self, now: Instant) {
        let tip = self.last_val.saturating_add(1);
        // Deep-ahead still expires. Tip: S-8.1 live holders keep racing;
        // S-12 dead/ghost holders return to missing.
        let mut expired_primary: Vec<(u64, bool, String)> = Vec::new();
        for (&h, inf) in &self.inflight {
            if now < inf.deadline || h == tip {
                continue;
            }
            expired_primary.push((h, inf.from_store, inf.peer.clone()));
        }
        let mut expired_extra_h: Vec<u64> = Vec::new();
        for (&h, xs) in &self.extras {
            if h == tip {
                continue;
            }
            if xs.iter().any(|inf| now >= inf.deadline) {
                expired_extra_h.push(h);
            }
        }
        for h in expired_extra_h {
            let from_store = self
                .extras
                .get(&h)
                .and_then(|v| v.first())
                .is_some_and(|i| i.from_store);
            self.extras.remove(&h);
            let primary_dead = self.inflight.get(&h).is_some_and(|p| now >= p.deadline);
            if primary_dead {
                if let Some(p) = self.inflight.remove(&h) {
                    self.tip_avoid.insert(h, p.peer);
                }
                if from_store {
                    self.store_ready.insert(h);
                } else {
                    self.missing.insert(h);
                }
            }
        }
        for (h, from_store, peer) in expired_primary {
            self.inflight.remove(&h);
            self.extras.remove(&h);
            self.tip_avoid.insert(h, peer);
            if from_store {
                self.store_ready.insert(h);
            } else {
                self.missing.insert(h);
            }
        }
        self.ensure_frontier_takeable(now);
    }

    fn peer_inflight(&self, peer: &str) -> usize {
        self.inflight.values().filter(|i| i.peer == peer).count()
            + self
                .extras
                .values()
                .flat_map(|v| v.iter())
                .filter(|i| i.peer == peer)
                .count()
    }

    fn ema(&self, peer: &str, now: Instant) -> f64 {
        let Some(r) = self.rates.get(peer) else {
            return 0.0;
        };
        if now.duration_since(r.last) >= IDLE_DECAY {
            r.ema_bps * 0.5
        } else {
            r.ema_bps
        }
    }

    fn best_ema(&self, now: Instant) -> f64 {
        self.rates
            .keys()
            .map(|p| self.ema(p, now))
            .fold(0.0_f64, f64::max)
    }

    /// Fastest peer by byte-rate EMA. None until `note_bytes` has a sample.
    fn best_peer(&self, now: Instant) -> Option<(String, f64)> {
        let best = self.best_ema(now);
        if best <= 0.0 {
            return None;
        }
        self.rates
            .keys()
            .map(|p| (p.clone(), self.ema(p, now)))
            .find(|(_, e)| *e >= best)
    }

    fn deadline_for(&self, h: u64, val: u64) -> Duration {
        if h == val.saturating_add(1) {
            return TIP_DEADLINE;
        }
        let d = h.saturating_sub(val.saturating_add(1));
        if d < FRONTIER {
            FRONTIER_DEADLINE
        } else if d < MID {
            MID_DEADLINE
        } else {
            DEEP_DEADLINE
        }
    }

    fn in_lane(&self, h: u64, val: u64) -> bool {
        h.saturating_sub(val.saturating_add(1)) < lane_width()
    }

    /// No samples → everyone may hold the lane (bootstrap). After rates exist,
    /// only peers within 70% of the best EMA.
    fn is_lane_peer(&self, ema: f64, best: f64) -> bool {
        best <= 0.0 || ema >= best * LANE_FAST_FRAC
    }

    /// Pull one height for `peer`. Lowest missing first. Frontier may duplicate.
    pub(crate) fn take_work(&mut self, peer: &str, val: u64, now: Instant) -> Option<u64> {
        if val != self.last_val {
            self.tip_blocked_since = None;
            self.tip_ever_held = false;
        }
        self.last_val = val;
        self.gone.remove(peer);
        self.expire_deadlines(now);
        if self.peer_inflight(peer) >= inflight_cap() {
            return None;
        }
        let ema = self.ema(peer, now);
        let best = self.best_ema(now);
        let tip = val.saturating_add(1);
        // S-7.2 / S-9.3: one different-peer extra at TIP_DEADLINE. Cap 2.
        if let Some(inf) = self.inflight.get(&tip) {
            if inf.peer != peer && !self.holds(tip, peer) && self.tip_n(tip) < TIP_PEER_CAP {
                if now >= inf.deadline {
                    let holder = inf.peer.clone();
                    self.push_extra(tip, peer, now, val);
                    N_TIP_DUP.fetch_add(1, Ordering::Relaxed);
                    N_GD_DUP.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        "[IBD_HF_TIP_DUP] h={} from={} to={}",
                        tip,
                        holder,
                        peer
                    );
                    return Some(tip);
                }
            }
        }
        // Speculative first extra on the tip only. S-8 sprayed val+2..val+8
        // (uncounted) and spent the mesh on blocks already in flight.
        if (ema >= best || best == 0.0)
            && self.inflight.get(&tip).is_some_and(|inf| inf.peer != peer)
            && self.extra_count(tip) == 0
        {
            self.push_extra(tip, peer, now, val);
            N_TIP_DUP.fetch_add(1, Ordering::Relaxed);
            N_GD_SPEC_TIP.fetch_add(1, Ordering::Relaxed);
            return Some(tip);
        }
        // S-12.2 / S-15.3: sleep only extra_only / dead / absent — not a
        // live tip past the 2s deadline (that is the healthy-path regressor).
        if self.frontier_unfillable(val) {
            return None;
        }
        // B-6 priority lane: next `lane_width` heights from fast peers only.
        // Same request count — slow peers skip the lane, they do not extra it.
        // S-8 was n=5 on one height + uncounted val+2..+8 extras. This is not that.
        let lane_peer = self.is_lane_peer(ema, best);
        let pick = {
            let mut chosen: Option<u64> = None;
            let mut deep: Option<u64> = None;
            for &h in &self.missing {
                if h == tip && self.tip_avoid.get(&h).is_some_and(|p| p == peer) {
                    continue;
                }
                if self.in_lane(h, val) {
                    if lane_peer && chosen.is_none() {
                        chosen = Some(h);
                    }
                    continue;
                }
                if deep.is_none() {
                    deep = Some(h);
                }
                if chosen.is_some() {
                    break;
                }
            }
            if lane_peer {
                chosen.or(deep)
            } else {
                deep
            }
        };
        let h = pick?;
        // S-14: missing means takeable, not "overwrite the live primary".
        if self.inflight.contains_key(&h) {
            if h == tip && !self.holds(tip, peer) && self.tip_n(tip) < TIP_PEER_CAP {
                self.push_extra(tip, peer, now, val);
                N_TIP_DUP.fetch_add(1, Ordering::Relaxed);
                N_GD_DUP.fetch_add(1, Ordering::Relaxed);
                return Some(tip);
            }
            return None;
        }
        self.missing.remove(&h);
        if h == tip {
            self.tip_avoid.remove(&h);
        }
        self.inflight.insert(
            h,
            Inflight {
                peer: peer.to_string(),
                deadline: now + self.deadline_for(h, val),
                duplicate: false,
                from_store: false,
                expire_noted: false,
            },
        );
        N_GD_PRI.fetch_add(1, Ordering::Relaxed);
        if self.in_lane(h, val) {
            N_GD_LANE.fetch_add(1, Ordering::Relaxed);
        }
        if h == tip {
            self.tip_ever_held = true;
            self.tip_blocked_since.get_or_insert(now);
        }
        Some(h)
    }

    /// Flag-off `download_chunk` pipelines up to `inflight_cap` GetDatas **inside**
    /// one `[lo, hi]` range. A span that does **not** start at `val+1` locked the
    /// fat TCP on warehouse (R-174: 190–200k **18**, `154.30` at 70–97% bytes,
    /// tip on a thin holder, `tip_hole` 36–69s). **Span only from the apply hole.**
    /// Tip extras and every ahead take stay one height. Do not jump a hole.
    pub(crate) fn take_work_span(
        &mut self,
        peer: &str,
        val: u64,
        now: Instant,
    ) -> Option<(u64, u64)> {
        let first = self.take_work(peer, val, now)?;
        let tip = val.saturating_add(1);
        let is_extra = self
            .extras
            .get(&first)
            .is_some_and(|v| v.iter().any(|i| i.peer == peer));
        if is_extra || first != tip {
            return Some((first, first));
        }
        let first_lane = self.in_lane(first, val);
        let mut last = first;
        loop {
            if self.peer_inflight(peer) >= inflight_cap() {
                break;
            }
            let nxt = last.saturating_add(1);
            if !self.missing.contains(&nxt) || self.inflight.contains_key(&nxt) {
                break;
            }
            if self.in_lane(nxt, val) != first_lane {
                break;
            }
            if nxt == val.saturating_add(1)
                && self.tip_avoid.get(&nxt).is_some_and(|p| p == peer)
            {
                break;
            }
            self.missing.remove(&nxt);
            self.inflight.insert(
                nxt,
                Inflight {
                    peer: peer.to_string(),
                    deadline: now + self.deadline_for(nxt, val),
                    duplicate: false,
                    from_store: false,
                    expire_noted: false,
                },
            );
            N_GD_PRI.fetch_add(1, Ordering::Relaxed);
            if self.in_lane(nxt, val) {
                N_GD_LANE.fetch_add(1, Ordering::Relaxed);
            }
            last = nxt;
        }
        Some((first, last))
    }

    /// R-178 scored this hybrid FAIL (10–50k **54**, fat **10**). Workers use
    /// `take_work_span` and skip `get_work`. Kept for the unit test only.
    pub(crate) fn take_hole_span(
        &mut self,
        peer: &str,
        val: u64,
        now: Instant,
    ) -> Option<(u64, u64)> {
        let (lo, hi) = self.take_work_span(peer, val, now)?;
        let tip = val.saturating_add(1);
        if lo == tip {
            return Some((lo, hi));
        }
        self.release_span(lo, hi);
        None
    }

    /// Consecutive on-disk heights in the ahead window, same channel as flag-off LOCAL_DISK.
    /// Lowest first. Burst capped by remaining per-peer inflight and 64.
    pub(crate) fn take_store_ready(&mut self, peer: &str, val: u64, now: Instant) -> Option<(u64, u64)> {
        self.expire_deadlines(now);
        let room = inflight_cap().saturating_sub(self.peer_inflight(peer));
        if room == 0 {
            return None;
        }
        let first = *self.store_ready.iter().next()?;
        let lo = val.saturating_add(1);
        let hi = val.saturating_add(self.fetch_ahead);
        if first < lo || first > hi {
            return None;
        }
        let max_n = room.min(64);
        let mut last = first;
        let mut n = 1usize;
        while n < max_n {
            let nxt = last.saturating_add(1);
            if nxt > hi || !self.store_ready.contains(&nxt) {
                break;
            }
            last = nxt;
            n += 1;
        }
        for h in first..=last {
            self.store_ready.remove(&h);
            self.inflight.insert(
                h,
                Inflight {
                    peer: peer.to_string(),
                    deadline: now + self.deadline_for(h, val),
                    duplicate: false,
                    from_store: true,
                    expire_noted: false,
                },
            );
        }
        Some((first, last))
    }

    /// Drop the height. Returns every holder peer so the download path can
    /// force-cancel losing GetDatas (first body wins).
    pub(crate) fn complete(&mut self, h: u64) -> Vec<String> {
        let mut holders = Vec::new();
        if let Some(inf) = self.inflight.remove(&h) {
            holders.push(inf.peer);
        }
        if let Some(xs) = self.extras.remove(&h) {
            holders.extend(xs.into_iter().map(|i| i.peer));
        }
        self.missing.remove(&h);
        self.store_ready.remove(&h);
        self.tip_avoid.remove(&h);
        match holders.len() {
            0 => {}
            1 => {
                N_H1.fetch_add(1, Ordering::Relaxed);
            }
            2 => {
                N_H2.fetch_add(1, Ordering::Relaxed);
            }
            3 => {
                N_H3.fetch_add(1, Ordering::Relaxed);
            }
            4 => {
                N_H4.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                N_H5P.fetch_add(1, Ordering::Relaxed);
            }
        }
        holders
    }

    pub(crate) fn complete_span(&mut self, lo: u64, hi: u64) {
        for h in lo..=hi {
            self.complete(h);
        }
    }

    /// Deadline / download fail: return to missing. Not LIMITED.
    /// No-op if `complete` already cleared the height — a cancelled extra
    /// must not resurrect the tip after the winner landed.
    ///
    /// Both maps must be cleared (S-11): `||` short-circuit left extras
    /// holding the tip after the primary worker failed, so `tip_n>=2`
    /// forever and no later peer could re-dup.
    pub(crate) fn release(&mut self, h: u64) {
        let from_store = self.inflight.get(&h).is_some_and(|i| i.from_store);
        let had_inf = self.inflight.remove(&h).is_some();
        let had_ex = self.extras.remove(&h).is_some();
        if !had_inf && !had_ex {
            return;
        }
        if from_store {
            self.store_ready.insert(h);
        } else {
            self.missing.insert(h);
        }
    }

    pub(crate) fn release_span(&mut self, lo: u64, hi: u64) {
        for h in lo..=hi {
            self.release(h);
        }
    }

    pub(crate) fn note_bytes(&mut self, peer: &str, nbytes: u64, now: Instant) {
        self.bytes_window = self.bytes_window.saturating_add(nbytes);
        *self.peer_window_bytes.entry(peer.to_string()).or_insert(0) += nbytes;
        let r = self.rates.entry(peer.to_string()).or_insert(PeerRate {
            ema_bps: 0.0,
            last: now,
        });
        let dt = now.duration_since(r.last).as_secs_f64().max(0.001);
        let inst = nbytes as f64 / dt;
        r.ema_bps = if r.ema_bps == 0.0 {
            inst
        } else {
            0.75 * r.ema_bps + 0.25 * inst
        };
        r.last = now;
    }

    /// Drain the 5s byte window into a log suffix: total + top-3 share.
    /// Settles whether `hf_peers=N` is shared load or one hero (S-6.4).
    pub(crate) fn take_byte_share_suffix(&mut self) -> String {
        let total: u64 = self.peer_window_bytes.values().copied().sum();
        if total == 0 {
            self.peer_window_bytes.clear();
            return "hf_bytes=0 hf_bshare=-".to_string();
        }
        let mut v: Vec<(String, u64)> = self.peer_window_bytes.drain().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = v
            .iter()
            .take(3)
            .map(|(p, b)| format!("{}:{:.0}%", p, 100.0 * (*b as f64) / (total as f64)))
            .collect();
        format!("hf_bytes={} hf_bshare={}", total, top.join(","))
    }

    /// Aggregate byte-rate can fill `fetch_ahead` bodies inside a deep deadline.
    pub(crate) fn aggregate_can_fill_ahead(&self, now: Instant) -> bool {
        if self.rates.is_empty() {
            // No samples yet — do not abort.
            return true;
        }
        let sum: f64 = self.rates.keys().map(|p| self.ema(p, now)).sum();
        let need = self.fetch_ahead as f64 * 50_000.0 / DEEP_DEADLINE.as_secs_f64();
        sum >= need
    }

    pub(crate) fn is_ahead_of_validation(&self, height: u64) -> bool {
        height > self.last_val
    }

    /// S-11: tip membership on the 5s HF line. Distinguishes inflight-leak
    /// from missing-but-filtered from absent.
    pub(crate) fn tip_state_suffix(&self, val: u64) -> String {
        let tip = val.saturating_add(1);
        let holder = self
            .inflight
            .get(&tip)
            .map(|i| i.peer.as_str())
            .unwrap_or("-");
        let n_ex = self.extra_count(tip);
        let miss = self.missing.contains(&tip);
        let avoid = self
            .tip_avoid
            .get(&tip)
            .map(|s| s.as_str())
            .unwrap_or("-");
        let kind = if self.inflight.contains_key(&tip) {
            "inflight"
        } else if miss && n_ex > 0 {
            "missing+extra"
        } else if miss {
            "missing"
        } else if n_ex > 0 {
            "extra_only"
        } else {
            "absent"
        };
        let live = u8::from(self.tip_has_live_holder());
        format!(
            "hf_tip={kind} holder={holder} extras={n_ex} miss={} avoid={avoid} live={live}",
            u8::from(miss)
        )
    }
}

fn global() -> &'static Mutex<HashFetchSched> {
    static G: OnceLock<Mutex<HashFetchSched>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(HashFetchSched::new(DEFAULT_FETCH_AHEAD)))
}

static LAST_VAL: AtomicU64 = AtomicU64::new(0);
/// R-184: one HF rate retitle. First pin (preferred none) is not spent.
static PICK_SPENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static N_TIP_EXPIRE: AtomicU64 = AtomicU64::new(0);
static N_TIP_DUP: AtomicU64 = AtomicU64::new(0);
static N_TIP_ESC: AtomicU64 = AtomicU64::new(0);
static N_TIP_REVIVE: AtomicU64 = AtomicU64::new(0);
static N_GD_PRI: AtomicU64 = AtomicU64::new(0);
static N_GD_DUP: AtomicU64 = AtomicU64::new(0);
static N_GD_ESC: AtomicU64 = AtomicU64::new(0);
static N_GD_SPEC_TIP: AtomicU64 = AtomicU64::new(0);
static N_GD_SPEC_AHEAD: AtomicU64 = AtomicU64::new(0);
static N_GD_LANE: AtomicU64 = AtomicU64::new(0);
static N_H1: AtomicU64 = AtomicU64::new(0);
static N_H2: AtomicU64 = AtomicU64::new(0);
static N_H3: AtomicU64 = AtomicU64::new(0);
static N_H4: AtomicU64 = AtomicU64::new(0);
static N_H5P: AtomicU64 = AtomicU64::new(0);

pub(crate) fn session_start() {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = global().lock() {
        *g = HashFetchSched::new(fetch_ahead());
    }
    LAST_VAL.store(0, Ordering::Relaxed);
    PICK_SPENT.store(false, Ordering::Relaxed);
    N_TIP_EXPIRE.store(0, Ordering::Relaxed);
    N_TIP_DUP.store(0, Ordering::Relaxed);
    N_TIP_ESC.store(0, Ordering::Relaxed);
    N_TIP_REVIVE.store(0, Ordering::Relaxed);
    N_GD_PRI.store(0, Ordering::Relaxed);
    N_GD_DUP.store(0, Ordering::Relaxed);
    N_GD_ESC.store(0, Ordering::Relaxed);
    N_GD_SPEC_TIP.store(0, Ordering::Relaxed);
    N_GD_SPEC_AHEAD.store(0, Ordering::Relaxed);
    N_GD_LANE.store(0, Ordering::Relaxed);
    N_H1.store(0, Ordering::Relaxed);
    N_H2.store(0, Ordering::Relaxed);
    N_H3.store(0, Ordering::Relaxed);
    N_H4.store(0, Ordering::Relaxed);
    N_H5P.store(0, Ordering::Relaxed);
}

pub(crate) fn gd_volume_suffix() -> String {
    format!(
        "hf_gd_pri={} hf_gd_dup={} hf_gd_esc={} hf_gd_spec_tip={} hf_gd_spec_ahead={} hf_gd_lane={} hf_gd_h=1:{}:2:{}:3:{}:4:{}:5p:{}",
        N_GD_PRI.load(Ordering::Relaxed),
        N_GD_DUP.load(Ordering::Relaxed),
        N_GD_ESC.load(Ordering::Relaxed),
        N_GD_SPEC_TIP.load(Ordering::Relaxed),
        N_GD_SPEC_AHEAD.load(Ordering::Relaxed),
        N_GD_LANE.load(Ordering::Relaxed),
        N_H1.load(Ordering::Relaxed),
        N_H2.load(Ordering::Relaxed),
        N_H3.load(Ordering::Relaxed),
        N_H4.load(Ordering::Relaxed),
        N_H5P.load(Ordering::Relaxed),
    )
}

pub(crate) fn tip_expire_count() -> u64 {
    N_TIP_EXPIRE.load(Ordering::Relaxed)
}

pub(crate) fn tip_dup_count() -> u64 {
    N_TIP_DUP.load(Ordering::Relaxed)
}

pub(crate) fn tip_esc_count() -> u64 {
    N_TIP_ESC.load(Ordering::Relaxed)
}

pub(crate) fn missing_len() -> usize {
    global().lock().map(|g| g.missing_len()).unwrap_or(0)
}

pub(crate) fn inflight_len() -> usize {
    global().lock().map(|g| g.inflight_len()).unwrap_or(0)
}

pub(crate) fn inflight_peer_count() -> usize {
    global().lock().map(|g| g.inflight_peer_count()).unwrap_or(0)
}

pub(crate) fn refill_from_store(store: &BlockStore, val: u64) {
    if !enabled() {
        return;
    }
    LAST_VAL.store(val, Ordering::Relaxed);
    if let Ok(mut g) = global().lock() {
        g.refill(val, store);
    }
}

pub(crate) fn take_work(peer: &str, val: u64) -> Option<u64> {
    let Ok(mut g) = global().lock() else {
        return None;
    };
    g.take_work(peer, val, Instant::now())
}

pub(crate) fn take_work_span(peer: &str, val: u64) -> Option<(u64, u64)> {
    let Ok(mut g) = global().lock() else {
        return None;
    };
    g.take_work_span(peer, val, Instant::now())
}

pub(crate) fn take_hole_span(peer: &str, val: u64) -> Option<(u64, u64)> {
    let Ok(mut g) = global().lock() else {
        return None;
    };
    g.take_hole_span(peer, val, Instant::now())
}

/// S-12: peer disconnected. HASH_FETCH peer_id is `ip:port`.
pub(crate) fn peer_gone(peer: &str) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = global().lock() {
        g.peer_gone(peer);
    }
}

pub(crate) fn take_store_ready(peer: &str, val: u64) -> Option<(u64, u64)> {
    let Ok(mut g) = global().lock() else {
        return None;
    };
    g.take_store_ready(peer, val, Instant::now())
}

pub(crate) fn complete(h: u64) -> Vec<String> {
    global()
        .lock()
        .map(|mut g| g.complete(h))
        .unwrap_or_default()
}

pub(crate) fn complete_span(lo: u64, hi: u64) {
    if let Ok(mut g) = global().lock() {
        g.complete_span(lo, hi);
    }
}

pub(crate) fn release(h: u64) {
    if let Ok(mut g) = global().lock() {
        g.release(h);
    }
}

pub(crate) fn release_span(lo: u64, hi: u64) {
    if let Ok(mut g) = global().lock() {
        g.release_span(lo, hi);
    }
}

pub(crate) fn best_peer() -> Option<(String, f64)> {
    if !enabled() {
        return None;
    }
    global()
        .lock()
        .ok()
        .and_then(|g| g.best_peer(Instant::now()))
}

pub(crate) fn peer_ema(peer: &str) -> f64 {
    if !enabled() {
        return 0.0;
    }
    global()
        .lock()
        .map(|g| g.ema(peer, Instant::now()))
        .unwrap_or(0.0)
}

pub(crate) fn pick_spent() -> bool {
    PICK_SPENT.load(Ordering::Relaxed)
}

pub(crate) fn mark_pick_spent() {
    PICK_SPENT.store(true, Ordering::Relaxed);
}

pub(crate) fn note_bytes(peer: &str, nbytes: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = global().lock() {
        g.note_bytes(peer, nbytes, Instant::now());
    }
}

pub(crate) fn byte_share_suffix() -> String {
    global()
        .lock()
        .map(|mut g| g.take_byte_share_suffix())
        .unwrap_or_else(|_| "hf_bytes=? hf_bshare=?".to_string())
}

pub(crate) fn tip_state_suffix() -> String {
    let val = LAST_VAL.load(Ordering::Relaxed);
    global()
        .lock()
        .map(|g| g.tip_state_suffix(val))
        .unwrap_or_else(|_| "hf_tip=?".to_string())
}

pub(crate) fn aggregate_can_fill_ahead() -> bool {
    let Ok(g) = global().lock() else {
        return true;
    };
    g.aggregate_can_fill_ahead(Instant::now())
}

pub(crate) fn is_ahead_of_validation(height: u64) -> bool {
    height > LAST_VAL.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drains_lowest_first() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(10);
        s.insert_missing(5);
        s.insert_missing(7);
        let now = Instant::now();
        assert_eq!(s.take_work("fast", 4, now), Some(5));
        assert_eq!(s.take_work("fast", 4, now), Some(7));
        assert_eq!(s.take_work("fast", 4, now), Some(10));
    }

    #[test]
    fn deadline_returns_to_missing_without_dropping_peer_rate() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(20);
        let now = Instant::now();
        s.note_bytes("p", 80_000, now);
        assert_eq!(s.take_work("p", 4, now), Some(20));
        // Mid (dist 15) is 16s. 15s must not expire; 17s must.
        s.expire_deadlines(now + Duration::from_secs(15));
        assert!(s.inflight.contains_key(&20));
        s.expire_deadlines(now + Duration::from_secs(17));
        assert!(s.missing.contains(&20));
        assert!(s.inflight.is_empty());
        assert!(s.ema("p", now) > 0.0);
    }

    #[test]
    fn tip_deadline_keeps_primary_and_dups_other_peer() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        // No rates yet — otherwise the slow-peer frontier skip would refuse dead.
        assert_eq!(s.take_work("dead", 4, now), Some(5));
        s.note_bytes("live", 400_000, now);
        // Inside 2s: no expire, no extra from expire path.
        s.expire_deadlines(now + Duration::from_secs(1));
        assert!(s.inflight.contains_key(&5));
        assert_eq!(s.extra_count(5), 0);
        // Past 2s: primary stays; other peer gets the first extra.
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("live", 4, later), Some(5));
        assert!(s.inflight.contains_key(&5));
        assert_eq!(
            s.extras.get(&5).and_then(|v| v.first()).map(|i| i.peer.as_str()),
            Some("live")
        );
        assert_eq!(s.inflight.get(&5).map(|i| i.peer.as_str()), Some("dead"));
        // Same holder must not be the extra.
        assert_eq!(s.take_work("dead", 4, later), None);
    }

    #[test]
    fn tip_one_dup_no_third_peer() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("p1", 4, now), Some(5));
        let t2 = now + Duration::from_secs(3);
        assert_eq!(s.take_work("p2", 4, t2), Some(5));
        assert_eq!(s.tip_n(5), 2);
        s.insert_missing(20);
        let t4 = now + Duration::from_secs(4);
        // S-15.3: live tip at cap past 2s → fill ahead, do not sleep.
        assert_eq!(s.take_work("p3", 4, t4), Some(20));
        assert_eq!(s.tip_n(5), 2);
        s.expire_deadlines(t4 + Duration::from_secs(30));
        // S-14: 8s stuck-age makes the tip takeable and recycles the primary slot.
        assert!(s.missing.contains(&5));
        assert_eq!(s.extra_count(5), 1);
        assert!(!s.inflight.contains_key(&5));
    }

    #[test]
    fn no_speculative_deep_ahead() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(7);
        let now = Instant::now();
        s.note_bytes("a", 200_000, now);
        s.note_bytes("b", 200_000, now);
        assert_eq!(s.take_work("a", 4, now), Some(5));
        assert_eq!(s.take_work("a", 4, now), Some(7));
        // b may spec-dup the tip, not height 7.
        assert_eq!(s.take_work("b", 4, now), Some(5));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(s.extra_count(7), 0);
    }

    #[test]
    fn tip_holders_stay_until_complete() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("dead", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("live", 4, later), Some(5));
        let both_dead = later + Duration::from_secs(3);
        s.expire_deadlines(both_dead);
        assert!(!s.missing.contains(&5));
        assert_eq!(s.tip_n(5), 2);
        assert_eq!(s.take_work("dead", 4, both_dead), None);
        assert_eq!(s.take_work("live", 4, both_dead), None);
        s.insert_missing(20);
        // S-15.3: holders still mapped (not gone) → live; fill ahead.
        assert_eq!(s.take_work("other", 4, both_dead), Some(20));
        assert_eq!(s.tip_n(5), 2);
    }

    #[test]
    fn idle_decays_ema() {
        let mut s = HashFetchSched::new(32);
        let now = Instant::now();
        s.note_bytes("p", 1_000_000, now);
        let hot = s.ema("p", now);
        let cold = s.ema("p", now + IDLE_DECAY + Duration::from_millis(1));
        assert!(cold < hot);
        assert!(cold > 0.0);
    }

    #[test]
    fn frontier_duplicate_to_second_peer() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        s.note_bytes("a", 200_000, now);
        s.note_bytes("b", 200_000, now);
        assert_eq!(s.take_work("a", 4, now), Some(5));
        assert_eq!(s.take_work("b", 4, now), Some(5));
        assert_eq!(s.extra_count(5), 1);
    }

    #[test]
    fn complete_drops_dup_and_missing() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        s.take_work("a", 4, now);
        s.take_work("b", 4, now);
        let holders = s.complete(5);
        assert!(!s.missing.contains(&5));
        assert!(s.inflight.is_empty());
        assert!(s.extras.is_empty());
        assert_eq!(holders.len(), 2);
        s.release(5);
        assert!(!s.missing.contains(&5));
    }

    #[test]
    fn no_samples_does_not_fail_mesh_fill() {
        let s = HashFetchSched::new(2048);
        assert!(s.aggregate_can_fill_ahead(Instant::now()));
    }

    #[test]
    fn store_ready_emits_consecutive_lowest_first() {
        let mut s = HashFetchSched::new(32);
        s.store_ready.insert(10);
        s.store_ready.insert(11);
        s.store_ready.insert(12);
        s.store_ready.insert(20);
        let now = Instant::now();
        assert_eq!(s.take_store_ready("local-disk", 9, now), Some((10, 12)));
        assert_eq!(s.take_store_ready("local-disk", 9, now), Some((20, 20)));
        assert_eq!(s.take_store_ready("local-disk", 9, now), None);
        s.complete_span(10, 12);
        s.release_span(20, 20);
        assert!(s.store_ready.contains(&20));
        assert!(!s.missing.contains(&20));
    }

    #[test]
    fn byte_share_suffix_names_top_peer() {
        let mut s = HashFetchSched::new(32);
        let now = Instant::now();
        s.note_bytes("10.0.0.1", 900_000, now);
        s.note_bytes("10.0.0.2", 100_000, now);
        let line = s.take_byte_share_suffix();
        assert!(line.contains("hf_bytes=1000000"), "{line}");
        assert!(line.contains("10.0.0.1:90%"), "{line}");
        assert!(line.contains("10.0.0.2:10%"), "{line}");
        assert_eq!(s.take_byte_share_suffix(), "hf_bytes=0 hf_bshare=-");
    }

    #[test]
    fn slow_peer_skips_frontier() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(40);
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        s.note_bytes("slow", 10_000, now);
        assert_eq!(s.take_work("slow", 4, now), Some(40));
        assert!(s.missing.contains(&5));
    }

    /// S-8.1 / S-12: a *live* holder past deadline keeps racing. S-15.3:
    /// peers after the one dup fill ahead instead of sleeping.
    #[test]
    fn s11_expired_tip_live_holder_stays_inflight() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(20);
        let now = Instant::now();
        assert_eq!(s.take_work("live", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        s.expire_deadlines(later);
        assert!(s.inflight.contains_key(&5));
        assert!(!s.missing.contains(&5));
        assert!(s.tip_avoid.get(&5).is_none());
        assert_eq!(s.take_work("other", 4, later), Some(5));
        assert_eq!(s.take_work("third", 4, later), Some(20));
        assert_eq!(s.tip_n(5), 2);
        let suffix = s.tip_state_suffix(4);
        assert!(suffix.contains("hf_tip=inflight"), "{suffix}");
        assert!(suffix.contains("holder=live"), "{suffix}");
        assert!(suffix.contains("live=1"), "{suffix}");
    }

    /// S-12.1: expired tip with dead holders drains to missing (S-11 inverted).
    #[test]
    fn s12_dead_holders_return_tip_to_missing() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(20);
        let now = Instant::now();
        assert_eq!(s.take_work("ghost", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        s.peer_gone("ghost");
        assert!(s.missing.contains(&5), "revive on disconnect");
        assert!(!s.inflight.contains_key(&5));
        let suffix = s.tip_state_suffix(4);
        assert!(suffix.contains("hf_tip=missing"), "{suffix}");
        assert_eq!(s.take_work("fresh", 4, later), Some(5));
        assert_eq!(s.inflight.get(&5).map(|i| i.peer.as_str()), Some("fresh"));
    }

    /// S-14: held and not takeable for 8s → missing. Live holder keeps racing.
    #[test]
    fn s12_age_cap_revives_ghost() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("silent", 4, now), Some(5));
        s.expire_deadlines(now + Duration::from_secs(3));
        assert!(s.inflight.contains_key(&5));
        assert!(!s.missing.contains(&5));
        s.expire_deadlines(now + Duration::from_secs(8));
        assert!(s.missing.contains(&5));
        assert!(s.inflight.contains_key(&5), "S-8.1 live holder stays");
    }

    /// S-11: release of the primary must drop extras (was `||` short-circuit).
    #[test]
    fn s11_release_primary_clears_extras() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("a", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("b", 4, later), Some(5));
        assert_eq!(s.extra_count(5), 1);
        s.release(5);
        assert!(!s.inflight.contains_key(&5));
        assert_eq!(s.extra_count(5), 0);
        assert!(s.missing.contains(&5));
        let suffix = s.tip_state_suffix(4);
        assert!(suffix.contains("hf_tip=missing"), "{suffix}");
        // A later peer can primary again; a third can dup.
        assert_eq!(s.take_work("c", 4, later), Some(5));
        assert_eq!(s.take_work("d", 4, later + Duration::from_secs(3)), Some(5));
        assert_eq!(s.tip_n(5), 2);
    }

    /// S-14: extra_only is the same 8s rule (not a separate predicate).
    #[test]
    fn s13_extra_only_promotes_tip_to_missing() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(20);
        let now = Instant::now();
        assert_eq!(s.take_work("primary", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("extra", 4, later), Some(5));
        s.peer_gone("primary");
        assert_eq!(s.extra_count(5), 1, "extra stays racing");
        assert!(!s.inflight.contains_key(&5));
        s.expire_deadlines(now + Duration::from_secs(8));
        assert!(s.missing.contains(&5), "stuck-age puts extra_only in missing");
        let suffix = s.tip_state_suffix(4);
        assert!(suffix.contains("hf_tip=missing+extra"), "{suffix}");
        assert_eq!(
            s.take_work("fresh", 4, now + Duration::from_secs(8)),
            Some(5)
        );
        assert_eq!(s.inflight.get(&5).map(|i| i.peer.as_str()), Some("fresh"));
        assert_eq!(s.extra_count(5), 1);
    }

    /// S-14: expire_deadlines uses the same stuck-age rule (no extra_only special case).
    #[test]
    fn s13_expire_promotes_extra_only() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("primary", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("extra", 4, later), Some(5));
        s.inflight.remove(&5);
        s.expire_deadlines(later);
        assert!(!s.missing.contains(&5), "under 8s, extra_only waits");
        s.expire_deadlines(now + Duration::from_secs(8));
        assert!(s.missing.contains(&5));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(
            s.take_work("fresh", 4, now + Duration::from_secs(8)),
            Some(5)
        );
    }

    /// S-14: inflight-only sit (48200 class) becomes takeable at 8s without cancelling.
    #[test]
    fn s14_stuck_age_returns_tip_to_missing() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(20);
        let now = Instant::now();
        assert_eq!(s.take_work("holder", 4, now), Some(5));
        s.expire_deadlines(now + Duration::from_secs(8));
        assert!(s.missing.contains(&5));
        assert!(s.inflight.contains_key(&5));
        assert_eq!(s.take_work("fresh", 4, now + Duration::from_secs(8)), Some(5));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(s.inflight.get(&5).map(|i| i.peer.as_str()), Some("holder"));
    }

    /// S-14: at cap, stuck-age recycles the primary slot so a new peer can primary.
    #[test]
    fn s14_stuck_at_cap_recycles_primary_slot() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        let now = Instant::now();
        assert_eq!(s.take_work("a", 4, now), Some(5));
        assert_eq!(s.take_work("b", 4, now + Duration::from_secs(3)), Some(5));
        assert_eq!(s.tip_n(5), 2);
        s.expire_deadlines(now + Duration::from_secs(8));
        assert!(s.missing.contains(&5));
        assert!(!s.inflight.contains_key(&5));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(s.take_work("c", 4, now + Duration::from_secs(8)), Some(5));
        assert_eq!(s.inflight.get(&5).map(|i| i.peer.as_str()), Some("c"));
        assert_eq!(s.extra_count(5), 1);
    }

    /// S-15.3: extra_only / dead still sleep. Live past 2s does not.
    #[test]
    fn s15_none_only_when_unfillable() {
        let mut s = HashFetchSched::new(32);
        s.insert_missing(5);
        s.insert_missing(20);
        let now = Instant::now();
        assert_eq!(s.take_work("primary", 4, now), Some(5));
        let later = now + Duration::from_secs(3);
        assert_eq!(s.take_work("extra", 4, later), Some(5));
        s.inflight.remove(&5);
        assert!(s.frontier_unfillable(4), "extra_only still None");
        s.peer_gone("extra");
        assert!(s.missing.contains(&5), "holders_gone revives");
        assert!(!s.frontier_unfillable(4));
    }

    /// B-6: slow peer must not primary the near-frontier lane (16).
    #[test]
    fn b6_slow_skips_lane_takes_deep() {
        let mut s = HashFetchSched::new(64);
        for h in 5..=20 {
            s.insert_missing(h);
        }
        s.insert_missing(40);
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        s.note_bytes("slow", 10_000, now);
        assert_eq!(s.take_work("slow", 4, now), Some(40));
        for h in 5..=20 {
            assert!(s.missing.contains(&h), "lane h={h} stayed missing");
            assert_eq!(s.extra_count(h), 0);
        }
    }

    /// B-6: fast peer fills the lane before deep-ahead.
    #[test]
    fn b6_fast_fills_lane_before_deep() {
        let mut s = HashFetchSched::new(64);
        s.insert_missing(5);
        s.insert_missing(6);
        s.insert_missing(40);
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        assert_eq!(s.take_work("fast", 4, now), Some(5));
        assert_eq!(s.take_work("fast", 4, now), Some(6));
        assert_eq!(s.take_work("fast", 4, now), Some(40));
        assert_eq!(s.extra_count(5), 0);
        assert_eq!(s.extra_count(6), 0);
        assert_eq!(s.extra_count(40), 0);
    }

    /// B-6: val+2..lane get one primary, never an extra (S-8 spray).
    #[test]
    fn b6_lane_one_primary_no_extra() {
        let mut s = HashFetchSched::new(64);
        s.insert_missing(5);
        s.insert_missing(6);
        s.insert_missing(7);
        let now = Instant::now();
        s.note_bytes("a", 400_000, now);
        s.note_bytes("b", 400_000, now);
        assert_eq!(s.take_work("a", 4, now), Some(5));
        // Existing tip spec-dup (cap 2). Not a lane extra on 6/7.
        assert_eq!(s.take_work("b", 4, now), Some(5));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(s.take_work("a", 4, now), Some(6));
        assert_eq!(s.extra_count(6), 0);
        assert_eq!(s.take_work("a", 4, now), Some(7));
        assert_eq!(s.extra_count(7), 0);
        assert_eq!(s.tip_n(6), 1);
        assert_eq!(s.tip_n(7), 1);
    }

    #[test]
    fn best_peer_is_fastest_ema() {
        let mut s = HashFetchSched::new(64);
        let now = Instant::now();
        s.note_bytes("slow", 10_000, now);
        s.note_bytes("fast", 400_000, now);
        let (p, ema) = s.best_peer(now).expect("rates");
        assert_eq!(p, "fast");
        assert!(ema > 0.0);
    }

    /// R-173: one take fills the per-peer pipe, not a single GetData.
    #[test]
    fn take_work_span_fills_lane_pipe() {
        let mut s = HashFetchSched::new(64);
        for h in 5..=40 {
            s.insert_missing(h);
        }
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        assert_eq!(s.take_work_span("fast", 4, now), Some((5, 20)));
        for h in 5..=20 {
            assert_eq!(s.inflight.get(&h).map(|i| i.peer.as_str()), Some("fast"));
            assert_eq!(s.extra_count(h), 0);
        }
        assert!(s.missing.contains(&21));
    }

    #[test]
    fn take_work_span_slow_is_single_deep() {
        let mut s = HashFetchSched::new(64);
        for h in 5..=40 {
            s.insert_missing(h);
        }
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        s.note_bytes("slow", 10_000, now);
        assert_eq!(s.take_work_span("slow", 4, now), Some((21, 21)));
        for h in 5..=20 {
            assert!(s.missing.contains(&h), "lane h={h} stayed missing");
        }
        assert!(s.missing.contains(&22));
    }

    #[test]
    fn take_work_span_ahead_stays_single() {
        let mut s = HashFetchSched::new(64);
        s.insert_missing(5);
        for h in 21..=40 {
            s.insert_missing(h);
        }
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        assert_eq!(s.take_work_span("fast", 4, now), Some((5, 5)));
        s.complete(5);
        assert_eq!(s.take_work_span("fast", 4, now), Some((21, 21)));
        assert!(s.missing.contains(&22));
    }

    #[test]
    fn take_work_span_tip_dup_stays_single() {
        let mut s = HashFetchSched::new(64);
        s.insert_missing(5);
        s.insert_missing(6);
        let now = Instant::now();
        s.note_bytes("a", 400_000, now);
        s.note_bytes("b", 400_000, now);
        assert_eq!(s.take_work_span("a", 4, now), Some((5, 6)));
        assert_eq!(s.take_work_span("b", 4, now), Some((5, 5)));
        assert_eq!(s.extra_count(5), 1);
        assert_eq!(s.extra_count(6), 0);
    }

    #[test]
    fn take_hole_span_keeps_tip_releases_ahead() {
        let mut s = HashFetchSched::new(64);
        for h in 5..=40 {
            s.insert_missing(h);
        }
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        s.note_bytes("slow", 10_000, now);
        assert_eq!(s.take_hole_span("fast", 4, now), Some((5, 20)));
        assert_eq!(s.take_hole_span("slow", 4, now), None);
        assert!(s.missing.contains(&21), "deep take released for get_work farms");
        s.complete_span(5, 20);
        assert_eq!(s.take_hole_span("fast", 20, now), Some((21, 36)));
    }

    /// B-6: medium peer (50% of best) is not a lane holder.
    #[test]
    fn b6_medium_peer_is_deep_only() {
        let mut s = HashFetchSched::new(64);
        s.insert_missing(5);
        s.insert_missing(40);
        let now = Instant::now();
        s.note_bytes("fast", 400_000, now);
        s.note_bytes("mid", 200_000, now); // 0.5 < 0.7
        assert_eq!(s.take_work("mid", 4, now), Some(40));
        assert!(s.missing.contains(&5));
        assert_eq!(s.extra_count(5), 0);
    }
}
