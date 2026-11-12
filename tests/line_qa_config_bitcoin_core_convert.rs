//! Test-line QA: a missing bitcoin.conf does not parse.

use std::path::Path;

use blvm_node::config::bitcoin_core_convert::parse_bitcoin_conf;

#[test]
fn parse_bitcoin_conf_rejects_a_missing_file() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("bitcoin.conf");

    match parse_bitcoin_conf(Path::new(&path)) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("No such file or directory"),
                "parse_bitcoin_conf returned {message}"
            );
        }
        Ok(config) => panic!("missing file parsed: {config:?}"),
    }
}
