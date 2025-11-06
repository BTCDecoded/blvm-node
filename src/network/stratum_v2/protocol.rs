//! Stratum V2 Protocol Encoding/Decoding
//!
//! Implements Tag-Length-Value (TLV) encoding for Stratum V2 messages.
//! Each message consists of:
//! - Tag: u16 (message type)
//! - Length: u32 (payload size in bytes)
//! - Value: Vec<u8> (message payload)
//!
//! Messages are length-prefixed with a 4-byte length header before TLV data.

use crate::network::stratum_v2::error::{StratumV2Error, StratumV2Result};
use std::io::{Cursor, Read, Write};

/// TLV encoder for Stratum V2 messages
pub struct TlvEncoder {
    buffer: Vec<u8>,
}

impl TlvEncoder {
    /// Create a new TLV encoder
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Encode a TLV message
    ///
    /// Format: [4-byte length][2-byte tag][4-byte length][payload]
    pub fn encode(&mut self, tag: u16, payload: &[u8]) -> StratumV2Result<Vec<u8>> {
        let mut result = Vec::new();

        // Write 4-byte length prefix (TLV size: 2-byte tag + 4-byte length + payload)
        let tlv_size = 2 + 4 + payload.len() as u32;
        result
            .write_all(&tlv_size.to_le_bytes())
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to write length: {}", e)))?;

        // Write tag (2 bytes, little-endian)
        result
            .write_all(&tag.to_le_bytes())
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to write tag: {}", e)))?;

        // Write payload length (4 bytes, little-endian)
        let payload_len = payload.len() as u32;
        result.write_all(&payload_len.to_le_bytes()).map_err(|e| {
            StratumV2Error::TlvEncoding(format!("Failed to write payload length: {}", e))
        })?;

        // Write payload
        result
            .write_all(payload)
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to write payload: {}", e)))?;

        Ok(result)
    }

    /// Get encoded buffer
    pub fn into_vec(self) -> Vec<u8> {
        self.buffer
    }
}

impl Default for TlvEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// TLV decoder for Stratum V2 messages
pub struct TlvDecoder {
    cursor: Cursor<Vec<u8>>,
}

impl TlvDecoder {
    /// Create a new TLV decoder from bytes
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            cursor: Cursor::new(data),
        }
    }

    /// Decode a TLV message from length-prefixed format
    ///
    /// Format: [4-byte length][2-byte tag][4-byte length][payload]
    /// Returns: (tag, payload)
    pub fn decode(&mut self) -> StratumV2Result<(u16, Vec<u8>)> {
        // Read 4-byte length prefix
        let mut length_bytes = [0u8; 4];
        self.cursor.read_exact(&mut length_bytes).map_err(|e| {
            StratumV2Error::TlvEncoding(format!("Failed to read length prefix: {}", e))
        })?;
        let _total_length = u32::from_le_bytes(length_bytes);

        // Read tag (2 bytes, little-endian)
        let mut tag_bytes = [0u8; 2];
        self.cursor
            .read_exact(&mut tag_bytes)
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to read tag: {}", e)))?;
        let tag = u16::from_le_bytes(tag_bytes);

        // Read payload length (4 bytes, little-endian)
        let mut length_bytes = [0u8; 4];
        self.cursor.read_exact(&mut length_bytes).map_err(|e| {
            StratumV2Error::TlvEncoding(format!("Failed to read payload length: {}", e))
        })?;
        let payload_len = u32::from_le_bytes(length_bytes) as usize;

        // Read payload
        let mut payload = vec![0u8; payload_len];
        self.cursor
            .read_exact(&mut payload)
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to read payload: {}", e)))?;

        Ok((tag, payload))
    }

    /// Decode from raw bytes (without length prefix)
    ///
    /// Used when receiving from transport that already handles framing
    pub fn decode_raw(data: &[u8]) -> StratumV2Result<(u16, Vec<u8>)> {
        if data.len() < 6 {
            return Err(StratumV2Error::TlvEncoding(
                "Insufficient data for TLV header".to_string(),
            ));
        }

        let mut cursor = Cursor::new(data);

        // Read tag (2 bytes, little-endian)
        let mut tag_bytes = [0u8; 2];
        cursor
            .read_exact(&mut tag_bytes)
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to read tag: {}", e)))?;
        let tag = u16::from_le_bytes(tag_bytes);

        // Read payload length (4 bytes, little-endian)
        let mut length_bytes = [0u8; 4];
        cursor.read_exact(&mut length_bytes).map_err(|e| {
            StratumV2Error::TlvEncoding(format!("Failed to read payload length: {}", e))
        })?;
        let payload_len = u32::from_le_bytes(length_bytes) as usize;

        // Read payload
        let mut payload = vec![0u8; payload_len];
        cursor
            .read_exact(&mut payload)
            .map_err(|e| StratumV2Error::TlvEncoding(format!("Failed to read payload: {}", e)))?;

        Ok((tag, payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tlv_encode_decode() {
        let tag = 0x0001u16;
        let payload = b"test payload";

        let mut encoder = TlvEncoder::new();
        let encoded = encoder.encode(tag, payload).unwrap();

        // Decode from length-prefixed format
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
}
