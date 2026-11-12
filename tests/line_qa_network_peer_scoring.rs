//! Test-line QA: scoring an empty peer list selects nobody.

use std::net::SocketAddr;

use blvm_node::network::peer_scoring::PeerScorer;

#[test]
fn get_best_peer_rejects_an_empty_peer_list() {
    let scorer = PeerScorer::new();
    let available: &[SocketAddr] = &[];
    match scorer.get_best_peer(available) {
        None => {}
        Some(peer) => panic!("empty peer list selected {peer}"),
    }
}
