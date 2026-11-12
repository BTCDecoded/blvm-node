//! Test-line QA: an export-height override below the stored height is discarded.

use blvm_node::config::ibd::resolve_engine_export_height;

#[test]
fn resolve_engine_export_height_rejects_an_override_below_the_stored_height() {
    match resolve_engine_export_height(Some(720_000), Some(640_000)) {
        (Some(720_000), Some(640_000)) => {}
        other => panic!("below-stored override was not discarded: {other:?}"),
    }
}
