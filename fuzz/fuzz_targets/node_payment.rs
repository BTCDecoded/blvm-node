#![no_main]
//! BIP70-shaped bincode decode for payment types used by the node payment layer.
use blvm_protocol::payment::{Payment, PaymentACK, PaymentRequest};
use libfuzzer_sys::fuzz_target;

const CAP: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let buf = if data.len() > CAP {
        &data[..CAP]
    } else {
        data
    };

    let _ = bincode::deserialize::<PaymentRequest>(buf);
    let _ = bincode::deserialize::<Payment>(buf);
    let _ = bincode::deserialize::<PaymentACK>(buf);
});
