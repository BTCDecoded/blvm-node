//! Per-peer transaction requests. The cap is this peer's outstanding getdata, not the mempool.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use blvm_protocol::Hash;

/// Requested transaction hashes that have not been answered.
pub(crate) const MAX_PEER_IN_FLIGHT: usize = 100;

/// Announced hashes waiting for an in-flight slot.
pub(crate) const MAX_PEER_ANNOUNCEMENTS: usize = 5_000;

/// An in-flight request older than this leaves the set.
pub(crate) const REQUEST_LIFETIME: Duration = Duration::from_secs(60);

#[derive(Default)]
struct PeerTxRequests {
    in_flight: HashMap<Hash, Instant>,
    waiting: VecDeque<Hash>,
    waiting_set: HashSet<Hash>,
}

/// In-flight transaction requests and the announcements waiting behind them.
#[derive(Default)]
pub(crate) struct TxRequestTracker {
    peers: HashMap<SocketAddr, PeerTxRequests>,
}

impl TxRequestTracker {
    /// Hashes to request now. A hash already in flight is skipped. Hashes past the
    /// waiting deque are dropped.
    pub(crate) fn admit(&mut self, peer: SocketAddr, hashes: &[Hash], now: Instant) -> Vec<Hash> {
        let state = self.peers.entry(peer).or_default();
        let mut request = Vec::new();
        promote(state, now, &mut request);
        for hash in hashes {
            if state.in_flight.contains_key(hash) || state.waiting_set.contains(hash) {
                continue;
            }
            if state.in_flight.len() < MAX_PEER_IN_FLIGHT {
                state.in_flight.insert(*hash, now);
                request.push(*hash);
            } else if state.waiting.len() < MAX_PEER_ANNOUNCEMENTS {
                state.waiting.push_back(*hash);
                state.waiting_set.insert(*hash);
            }
        }
        if state.in_flight.is_empty() && state.waiting.is_empty() {
            self.peers.remove(&peer);
        }
        request
    }

    /// Drop a hash that arrived or was refused. Expired requests leave at the same time,
    /// and waiting hashes fill the open slots.
    pub(crate) fn complete(&mut self, peer: SocketAddr, hash: Hash, now: Instant) -> Vec<Hash> {
        let Some(state) = self.peers.get_mut(&peer) else {
            return Vec::new();
        };
        state.in_flight.remove(&hash);
        let mut request = Vec::new();
        promote(state, now, &mut request);
        if state.in_flight.is_empty() && state.waiting.is_empty() {
            self.peers.remove(&peer);
        }
        request
    }

    pub(crate) fn forget_peer(&mut self, peer: SocketAddr) {
        self.peers.remove(&peer);
    }

    pub(crate) fn in_flight_len(&self, peer: SocketAddr) -> usize {
        self.peers
            .get(&peer)
            .map(|state| state.in_flight.len())
            .unwrap_or(0)
    }

    pub(crate) fn waiting_len(&self, peer: SocketAddr) -> usize {
        self.peers
            .get(&peer)
            .map(|state| state.waiting.len())
            .unwrap_or(0)
    }

    pub(crate) fn contains(&self, peer: SocketAddr, hash: &Hash) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|state| state.in_flight.contains_key(hash))
    }
}

fn promote(state: &mut PeerTxRequests, now: Instant, request: &mut Vec<Hash>) {
    state
        .in_flight
        .retain(|_, requested_at| now.saturating_duration_since(*requested_at) <= REQUEST_LIFETIME);
    while state.in_flight.len() < MAX_PEER_IN_FLIGHT {
        let Some(hash) = state.waiting.pop_front() else {
            break;
        };
        state.waiting_set.remove(&hash);
        if state.in_flight.contains_key(&hash) {
            continue;
        }
        state.in_flight.insert(hash, now);
        request.push(hash);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(octet: u8) -> SocketAddr {
        SocketAddr::from(([octet, 0, 0, 1], 8333))
    }

    fn hash_at(n: u16) -> Hash {
        let mut hash = [0u8; 32];
        hash[0] = (n >> 8) as u8;
        hash[1] = n as u8;
        hash
    }

    fn hashes(n: u16) -> Vec<Hash> {
        (0..n).map(hash_at).collect()
    }

    #[test]
    fn one_hundred_fly_and_the_rest_wait_until_five_thousand() {
        let mut tracker = TxRequestTracker::default();
        let now = Instant::now();
        let announced = hashes(MAX_PEER_IN_FLIGHT as u16 + MAX_PEER_ANNOUNCEMENTS as u16 + 1);
        let requested = tracker.admit(peer(1), &announced, now);
        assert_eq!(requested.len(), MAX_PEER_IN_FLIGHT);
        assert_eq!(requested, hashes(MAX_PEER_IN_FLIGHT as u16));
        assert_eq!(tracker.waiting_len(peer(1)), MAX_PEER_ANNOUNCEMENTS);
        assert!(!tracker.contains(
            peer(1),
            &hash_at(MAX_PEER_IN_FLIGHT as u16 + MAX_PEER_ANNOUNCEMENTS as u16)
        ));
        assert_eq!(tracker.waiting_len(peer(1)), MAX_PEER_ANNOUNCEMENTS);
        let again = tracker.admit(peer(1), &requested, now);
        assert!(again.is_empty());
        assert_eq!(tracker.in_flight_len(peer(1)), MAX_PEER_IN_FLIGHT);
    }

    #[test]
    fn a_queued_hash_is_not_stored_twice() {
        let mut tracker = TxRequestTracker::default();
        let now = Instant::now();
        let first = hashes(MAX_PEER_IN_FLIGHT as u16 + 1);
        tracker.admit(peer(1), &first, now);
        tracker.admit(peer(1), &[hash_at(MAX_PEER_IN_FLIGHT as u16)], now);
        assert_eq!(tracker.waiting_len(peer(1)), 1);
    }

    #[test]
    fn an_expired_request_frees_the_next_queued_hash() {
        let mut tracker = TxRequestTracker::default();
        let start = Instant::now();
        let announced = hashes(MAX_PEER_IN_FLIGHT as u16 + 1);
        tracker.admit(peer(1), &announced, start);
        let still = tracker.admit(peer(1), &[], start + REQUEST_LIFETIME);
        assert!(still.is_empty());
        assert!(tracker.contains(peer(1), &hash_at(0)));
        let next = tracker.admit(
            peer(1),
            &[],
            start + REQUEST_LIFETIME + Duration::from_nanos(1),
        );
        assert_eq!(next, vec![hash_at(MAX_PEER_IN_FLIGHT as u16)]);
        assert!(!tracker.contains(peer(1), &hash_at(0)));
        assert!(tracker.contains(peer(1), &hash_at(MAX_PEER_IN_FLIGHT as u16)));
        assert_eq!(tracker.waiting_len(peer(1)), 0);
        assert_eq!(tracker.in_flight_len(peer(1)), 1);
    }

    #[test]
    fn a_completed_hash_leaves_and_the_caps_are_per_peer() {
        let mut tracker = TxRequestTracker::default();
        let now = Instant::now();
        let announced = hashes(MAX_PEER_IN_FLIGHT as u16 + 1);
        tracker.admit(peer(1), &announced, now);
        tracker.admit(peer(2), &hashes(MAX_PEER_IN_FLIGHT as u16), now);
        let followed = tracker.complete(peer(1), hash_at(3), now);
        assert_eq!(followed, vec![hash_at(MAX_PEER_IN_FLIGHT as u16)]);
        assert!(!tracker.contains(peer(1), &hash_at(3)));
        assert_eq!(tracker.in_flight_len(peer(2)), MAX_PEER_IN_FLIGHT);
        assert_eq!(tracker.waiting_len(peer(2)), 0);
        tracker.forget_peer(peer(1));
        assert_eq!(tracker.in_flight_len(peer(1)), 0);
        assert_eq!(tracker.in_flight_len(peer(2)), MAX_PEER_IN_FLIGHT);
    }
}
