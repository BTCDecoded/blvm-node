//! Test-line QA: an empty address string is not a script pubkey.

use blvm_node::rpc::rawtx::address_string_to_script_pubkey;

#[test]
fn empty_address_is_rejected() {
    match address_string_to_script_pubkey("") {
        Err(err) => {
            let message = err.to_string();
            assert!(
                message.contains("Invalid address format")
                    && message.contains("not Bech32/Bech32m and not valid Base58Check"),
                "address_string_to_script_pubkey returned {message}"
            );
        }
        Ok(script) => panic!("empty address decoded to script {script:?}"),
    }
}
