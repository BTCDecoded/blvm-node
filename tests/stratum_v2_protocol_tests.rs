//! Tests for Stratum V2 Protocol (TLV encoding/decoding)

#[cfg(feature = "stratum-v2")]
mod tests {
    use bllvm_node::network::stratum_v2::protocol::{TlvDecoder, TlvEncoder};

    #[test]
    fn test_tlv_encoder_new() {
        let encoder = TlvEncoder::new();
        // Should create successfully
        assert!(true);
    }

    #[test]
    fn test_tlv_encoder_encode() {
        let mut encoder = TlvEncoder::new();
        let tag = 0x0001u16;
        let payload = b"test payload";
        
        let result = encoder.encode(tag, payload);
        assert!(result.is_ok());
        
        let encoded = result.unwrap();
        // Should have length prefix (4 bytes) + tag (2 bytes) + payload length (4 bytes) + payload
        assert!(encoded.len() >= 10);
    }

    #[test]
    fn test_tlv_decoder_new() {
        let data = vec![0u8; 10];
        let decoder = TlvDecoder::new(data);
        // Should create successfully
        assert!(true);
    }

    #[test]
    fn test_tlv_encode_decode_roundtrip() {
        let tag = 0x0001u16;
        let payload = b"test payload";
        
        let mut encoder = TlvEncoder::new();
        let encoded = encoder.encode(tag, payload).unwrap();
        
        let mut decoder = TlvDecoder::new(encoded);
        let (decoded_tag, decoded_payload) = decoder.decode().unwrap();
        
        assert_eq!(tag, decoded_tag);
        assert_eq!(payload, decoded_payload.as_slice());
    }

    #[test]
    fn test_tlv_decode_raw() {
        let tag = 0x0002u16;
        let payload = b"raw payload";
        
        // Create raw TLV (tag + length + payload)
        let mut raw = Vec::new();
        raw.extend_from_slice(&tag.to_le_bytes());
        raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        raw.extend_from_slice(payload);
        
        let (decoded_tag, decoded_payload) = TlvDecoder::decode_raw(&raw).unwrap();
        
        assert_eq!(tag, decoded_tag);
        assert_eq!(payload, decoded_payload.as_slice());
    }

    #[test]
    fn test_tlv_decode_raw_insufficient_data() {
        let insufficient = vec![0u8; 5]; // Less than 6 bytes (tag + length)
        
        let result = TlvDecoder::decode_raw(&insufficient);
        assert!(result.is_err());
    }

    #[test]
    fn test_tlv_encode_decode_multiple() {
        let mut encoder = TlvEncoder::new();
        
        let tag1 = 0x0001u16;
        let payload1 = b"first";
        let encoded1 = encoder.encode(tag1, payload1).unwrap();
        
        let tag2 = 0x0002u16;
        let payload2 = b"second";
        let encoded2 = encoder.encode(tag2, payload2).unwrap();
        
        // Decode first
        let mut decoder1 = TlvDecoder::new(encoded1);
        let (decoded_tag1, decoded_payload1) = decoder1.decode().unwrap();
        assert_eq!(tag1, decoded_tag1);
        assert_eq!(payload1, decoded_payload1.as_slice());
        
        // Decode second
        let mut decoder2 = TlvDecoder::new(encoded2);
        let (decoded_tag2, decoded_payload2) = decoder2.decode().unwrap();
        assert_eq!(tag2, decoded_tag2);
        assert_eq!(payload2, decoded_payload2.as_slice());
    }
}

