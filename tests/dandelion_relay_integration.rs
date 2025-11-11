#![cfg(feature = "dandelion")]
use bllvm_node::network::relay::RelayManager;
use bllvm_protocol::Hash;

fn hash_from_u8(v: u8) -> Hash {
    [v; 32]
}

#[test]
fn stem_then_fluff_via_hop_limit() {
    let mut relay = RelayManager::new();
    let peers = vec!["p1".into(), "p2".into(), "p3".into()];

    // Enable Dandelion (it's disabled by default)
    relay.enable_dandelion = true;
    // Create dandelion instance if it doesn't exist
    #[cfg(feature = "dandelion")]
    if relay.dandelion.is_none() {
        use bllvm_node::network::dandelion::DandelionRelay;
        relay.dandelion = Some(DandelionRelay::new());
    }

    // Configure Dandelion for deterministic behavior
    relay.set_dandelion_fluff_probability(0.0);
    // Set hop limit to 1 (first hop starts stem, second fluffs)
    relay.set_dandelion_max_stem_hops(1);

    // Initialize path for current peer
    relay.initialize_dandelion_path("p1".into(), &peers);

    let tx = hash_from_u8(1);

    // First decision will start stem and return Some(next)
    let next = relay.relay_transaction_dandelion(tx, "p1".into(), &peers);
    assert!(next.is_some());
    // Second decision should fluff due to hop limit being reached
    let next2 = relay.relay_transaction_dandelion(tx, "p1".into(), &peers);
    assert!(next2.is_none());
}
