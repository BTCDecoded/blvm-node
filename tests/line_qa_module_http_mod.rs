//! Test-line QA: an origin without a scheme is rejected.

use blvm_node::module::http::parse_origin;

#[test]
fn parse_origin_rejects_a_url_without_a_scheme() {
    match parse_origin("example.com/path") {
        Err(message) => {
            assert!(
                message.contains("url needs a scheme"),
                "parse_origin returned {message}"
            );
        }
        Ok(origin) => panic!("scheme-less url parsed: {}", origin.display()),
    }
}
