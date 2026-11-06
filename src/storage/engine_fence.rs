//! Dependency fence for the storage engines.

const ENGINES: &[&str] = blvm_storage_fence::ENGINE_CRATES;

/// Engine crate names declared in a Cargo dependency table.
pub fn engine_names_in_dependencies(toml: &str) -> Vec<&'static str> {
    let mut found = Vec::new();
    let mut in_deps = false;
    for raw in toml.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') && line.ends_with(']') {
            let header = line.trim_matches(&['[', ']'][..]);
            in_deps = header == "dependencies"
                || header == "dev-dependencies"
                || header == "build-dependencies"
                || header.starts_with("dependencies.")
                || header.starts_with("dev-dependencies.")
                || header.starts_with("build-dependencies.");
            if let Some(name) = header
                .strip_prefix("dependencies.")
                .or_else(|| header.strip_prefix("dev-dependencies."))
                .or_else(|| header.strip_prefix("build-dependencies."))
            {
                note(&mut found, name.trim().trim_matches('"'));
            }
            continue;
        }
        if !in_deps || line.is_empty() {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            note(&mut found, key.trim().trim_matches('"'));
        }
        if let Some(after) = line.split("package").nth(1) {
            if let Some(name) = after.split('"').nth(1) {
                note(&mut found, name);
            }
        }
    }
    found
}

fn note(found: &mut Vec<&'static str>, name: &str) {
    if let Some(engine) = ENGINES.iter().copied().find(|engine| *engine == name) {
        if !found.contains(&engine) {
            found.push(engine);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::engine_names_in_dependencies;

    #[test]
    fn the_node_does_not_name_an_engine_outside_the_fence() {
        let node = engine_names_in_dependencies(include_str!("../../Cargo.toml"));
        assert!(node.is_empty(), "blvm-node names storage engines {node:?}");
        let fence = engine_names_in_dependencies(include_str!(
            "../../crates/blvm-storage-fence/Cargo.toml"
        ));
        for engine in blvm_storage_fence::ENGINE_CRATES {
            assert!(fence.contains(engine), "fence crate does not own {engine}");
        }
    }

    #[test]
    fn a_node_manifest_that_names_an_engine_is_rejected() {
        let sample = "[dependencies]\nheed3 = { version = \"=0.22.1\" }\n[features]\nheed3 = []\n";
        assert_eq!(engine_names_in_dependencies(sample), vec!["heed3"]);
        let renamed = "[dependencies]\nstore = { package = \"rocksdb\", version = \"0.24\" }\n";
        assert_eq!(engine_names_in_dependencies(renamed), vec!["rocksdb"]);
    }
}
