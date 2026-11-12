//! Test-line QA: a module event is not a new-transaction payload.

use blvm_node::module::ipc::protocol::EventPayload;

#[test]
fn module_event_is_not_a_new_transaction() {
    let event = EventPayload::module_event("ping", Vec::new());
    match event.as_new_transaction() {
        None => {}
        Some(hash) => panic!("module event decoded as transaction {hash:?}"),
    }
}
