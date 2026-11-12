//! Test-line QA: a module context misses a key it was not given.

use std::collections::HashMap;

use blvm_node::module::traits::ModuleContext;

#[test]
fn get_config_misses_an_absent_key() {
    let context = ModuleContext::new(
        "demo".to_string(),
        "/tmp/demo.sock".to_string(),
        "/tmp/demo".to_string(),
        HashMap::new(),
    );

    match context.get_config("missing") {
        None => {}
        Some(value) => panic!("absent key was present: {value}"),
    }
}
