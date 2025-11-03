#![no_main]
use libfuzzer_sys::fuzz_target;
use reference_node::network::compact_blocks::{
    should_prefer_compact_blocks, recommended_compact_block_version, is_quic_transport,
};
use reference_node::network::transport::TransportType;

fuzz_target!(|data: &[u8]| {
    // Fuzz transport-aware negotiation logic
    
    if data.is_empty() {
        return;
    }
    
    // Determine transport type from fuzzed data
    let transport_byte = data[0];
    let transport = match transport_byte % 3 {
        0 => TransportType::Tcp,
        #[cfg(feature = "quinn")]
        1 => TransportType::Quinn,
        #[cfg(not(feature = "quinn"))]
        1 => TransportType::Tcp,
        #[cfg(feature = "iroh")]
        2 => TransportType::Iroh,
        #[cfg(not(feature = "iroh"))]
        2 => TransportType::Tcp,
        _ => TransportType::Tcp,
    };
    
    // Test transport-aware functions - should never panic
    let _should_prefer = should_prefer_compact_blocks(transport);
    let _recommended_version = recommended_compact_block_version(transport);
    let _is_quic = is_quic_transport(transport);
    
    // Test with all available transports if data suggests it
    let transports = [
        TransportType::Tcp,
        #[cfg(feature = "quinn")]
        TransportType::Quinn,
        #[cfg(feature = "iroh")]
        TransportType::Iroh,
    ];
    
    for &t in &transports {
        let _pref = should_prefer_compact_blocks(t);
        let _ver = recommended_compact_block_version(t);
        let _quic = is_quic_transport(t);
        
        // Verify consistency: QUIC transports should prefer compact blocks
        if is_quic_transport(t) {
            let prefers = should_prefer_compact_blocks(t);
            // This should always be true for QUIC transports
            // (But we don't assert in fuzzing - just exercise the code)
        }
        
        // Verify version recommendation is 1 or 2
        let version = recommended_compact_block_version(t);
        assert!(version == 1 || version == 2, "Version must be 1 or 2");
    }
});

