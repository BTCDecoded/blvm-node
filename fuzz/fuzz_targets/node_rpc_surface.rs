#![no_main]
//! JSON + bincode parse surfaces with a 64 KiB cap (RPC/IPC style inputs).
use blvm_node::module::ipc::ModuleMessage;
use libfuzzer_sys::fuzz_target;

const CAP: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let buf = if data.len() > CAP {
        &data[..CAP]
    } else {
        data
    };

    let _ = serde_json::from_slice::<serde_json::Value>(buf);
    let _ = bincode::deserialize::<ModuleMessage>(buf);
});
