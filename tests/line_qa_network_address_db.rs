//! Test-line QA: a zero address time is outside the relay horizon.

use blvm_node::network::address_db::address_within_relay_horizon;

#[test]
fn address_within_relay_horizon_rejects_a_zero_timestamp() {
    match address_within_relay_horizon(0) {
        false => {}
        true => panic!("a zero timestamp was inside the relay horizon"),
    }
}
