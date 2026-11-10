//! BIP330 Erlay short-ID helpers (no minisketch / `erlay` feature required).

use blvm_node::network::erlay::{
    ErlayPeerNegotiation, build_erlay_short_id_map, compute_erlay_short_id, erlay_siphash_keys,
    resolve_erlay_short_ids, salt_from_wire_bytes,
};
use std::collections::HashSet;

#[test]
fn salt_from_wire_bytes_reads_le_u64_prefix() {
    let mut salt = [0u8; 16];
    salt[0..8].copy_from_slice(&42u64.to_le_bytes());
    assert_eq!(salt_from_wire_bytes(&salt), 42);
}

#[test]
fn negotiation_stores_siphash_keys() {
    let mut state = ErlayPeerNegotiation::default();
    state.apply_sendtxrcncl(1, 7, 32, 64, 11).unwrap();
    assert_eq!(state.siphash_keys(), Some(erlay_siphash_keys(11, 7)));
}

#[test]
fn resolve_short_ids_against_candidate_union() {
    let (k0, k1) = erlay_siphash_keys(1, 2);
    let mut local = HashSet::new();
    local.insert([1u8; 32]);
    local.insert([2u8; 32]);
    let mut remote = HashSet::new();
    remote.insert([2u8; 32]);
    remote.insert([4u8; 32]);
    let mut all = local.clone();
    all.extend(remote);

    let sid4 = compute_erlay_short_id(&[4u8; 32], k0, k1);
    let resolved = resolve_erlay_short_ids(&all, &[sid4], k0, k1);
    assert_eq!(resolved, vec![[4u8; 32]]);

    let local_map = build_erlay_short_id_map(&local, k0, k1);
    assert!(!local_map.contains_key(&sid4));
}
