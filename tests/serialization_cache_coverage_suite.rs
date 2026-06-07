//! Block/tx serialization LRU caches and IBD bypass guard.

use blvm_node::storage::serialization_cache::{
    cache_serialized_header, cache_serialized_tx, clear_all_caches, get_cached_serialized_header,
    get_cached_serialized_tx, ibd_header_serialize_cache_bypassed,
    IbdHeaderSerializeCacheBypassGuard,
};
use serial_test::serial;

#[test]
#[serial]
fn header_and_tx_cache_round_trip() {
    clear_all_caches();
    let h = [0x11u8; 32];
    let t = [0x22u8; 32];
    cache_serialized_header(h, b"hdr".to_vec());
    cache_serialized_tx(t, b"tx".to_vec());
    assert_eq!(get_cached_serialized_header(&h).unwrap().as_ref(), b"hdr");
    assert_eq!(get_cached_serialized_tx(&t).unwrap().as_ref(), b"tx");
    clear_all_caches();
    assert!(get_cached_serialized_header(&h).is_none());
    assert!(get_cached_serialized_tx(&t).is_none());
}

#[test]
#[serial]
fn header_cache_bypass_guard_skips_put_and_get() {
    clear_all_caches();
    let h = [0x33u8; 32];
    {
        let _guard = IbdHeaderSerializeCacheBypassGuard::enter();
        assert!(ibd_header_serialize_cache_bypassed());
        cache_serialized_header(h, b"skip".to_vec());
        assert!(get_cached_serialized_header(&h).is_none());
    }
    assert!(!ibd_header_serialize_cache_bypassed());
    cache_serialized_header(h, b"after".to_vec());
    assert_eq!(get_cached_serialized_header(&h).unwrap().as_ref(), b"after");
    clear_all_caches();
}
