//! Test-line QA: Landlock refuses a module data directory that is not there.

use std::path::Path;

use blvm_node::module::sandbox::landlock::ModuleLandlock;

#[test]
fn prepare_rejects_a_missing_data_directory() {
    let data = Path::new("/tmp/blvm-line-qa-landlock-missing-data");
    match ModuleLandlock::prepare(
        Path::new("/bin/true"),
        data,
        Path::new("/tmp/blvm-line-qa-landlock-missing-sock/node.sock"),
        Path::new("/tmp"),
    ) {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("No such file or directory"),
                "prepare returned {message}"
            );
        }
        Ok(_) => panic!("a missing data directory was prepared"),
    }
}
