#![no_main]
//! Descriptor string parsing via the `miniscript` crate (aligned with the plan’s `node_miniscript` target).
use libfuzzer_sys::fuzz_target;
use miniscript::bitcoin::PublicKey;
use miniscript::Descriptor;
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let t = s.chars().take(512).collect::<String>();
        let _ = Descriptor::<PublicKey>::from_str(&t);
    }
});
