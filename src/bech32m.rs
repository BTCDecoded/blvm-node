//! BIP350/351: Bech32m Address Encoding
//!
//! Implements Bech32m encoding for Taproot (P2TR) addresses and Bech32 encoding
//! for SegWit (P2WPKH/P2WSH) addresses.
//!
//! BIP173: Bech32 encoding (SegWit addresses) - bc1...
//! BIP350: Bech32m encoding (Taproot addresses) - bc1p...
//! BIP351: Version 1 witness encoding for Taproot
//!
//! Specifications:
//! - https://github.com/bitcoin/bips/blob/master/bip-0173.mediawiki
//! - https://github.com/bitcoin/bips/blob/master/bip-0350.mediawiki
//! - https://github.com/bitcoin/bips/blob/master/bip-0351.mediawiki

use bech32::{FromBase32, ToBase32, Variant};

/// Bitcoin address encoding error
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    InvalidWitnessVersion,
    InvalidWitnessLength,
    InvalidEncoding,
    UnsupportedVariant,
    InvalidHRP,
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressError::InvalidWitnessVersion => write!(f, "Invalid witness version"),
            AddressError::InvalidWitnessLength => write!(f, "Invalid witness data length"),
            AddressError::InvalidEncoding => write!(f, "Invalid address encoding"),
            AddressError::UnsupportedVariant => write!(f, "Unsupported address variant"),
            AddressError::InvalidHRP => write!(f, "Invalid human-readable part"),
        }
    }
}

impl std::error::Error for AddressError {}

/// Network identifier for addresses
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
    Regtest,
}

impl Network {
    /// Get human-readable part (HRP) for Bech32 encoding
    pub fn hrp(&self) -> &'static str {
        match self {
            Network::Mainnet => "bc",
            Network::Testnet => "tb",
            Network::Regtest => "bcrt",
        }
    }
}

/// Encoded Bitcoin address
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinAddress {
    /// Network identifier
    pub network: Network,
    /// Witness version (0 for SegWit, 1 for Taproot)
    pub witness_version: u8,
    /// Witness program (20 bytes for P2WPKH, 32 bytes for P2WSH/P2TR)
    pub witness_program: Vec<u8>,
}

impl BitcoinAddress {
    /// Create a new Bech32/Bech32m address
    pub fn new(
        network: Network,
        witness_version: u8,
        witness_program: Vec<u8>,
    ) -> Result<Self, AddressError> {
        // Validate witness version
        if witness_version > 16 {
            return Err(AddressError::InvalidWitnessVersion);
        }

        // Validate witness program length
        match witness_version {
            0 => {
                // SegWit v0: P2WPKH (20 bytes) or P2WSH (32 bytes)
                if witness_program.len() != 20 && witness_program.len() != 32 {
                    return Err(AddressError::InvalidWitnessLength);
                }
            }
            1 => {
                // Taproot v1: P2TR (32 bytes)
                if witness_program.len() != 32 {
                    return Err(AddressError::InvalidWitnessLength);
                }
            }
            _ => {
                // Future witness versions: 2-16 bytes (as per BIP342)
                if witness_program.len() < 2 || witness_program.len() > 40 {
                    return Err(AddressError::InvalidWitnessLength);
                }
            }
        }

        Ok(BitcoinAddress {
            network,
            witness_version,
            witness_program,
        })
    }

    /// Encode address to Bech32 (for SegWit v0) or Bech32m (for Taproot v1+)
    ///
    /// BIP173: Witness version 0 uses Bech32
    /// BIP350: Witness version 1+ uses Bech32m
    pub fn encode(&self) -> Result<String, AddressError> {
        let hrp = self.network.hrp();

        // Convert witness program to base32 (u5)
        let program_base32 = witness_program_to_base32(&self.witness_program);

        // Combine witness version and program as u5 values
        // Witness version needs to be converted to u5 (it's 0-16, fits in u5)
        let mut data = vec![bech32::u5::try_from_u8(self.witness_version)
            .map_err(|_| AddressError::InvalidWitnessVersion)?];
        data.extend_from_slice(&program_base32);

        // Encode using appropriate variant
        let encoded = if self.witness_version == 0 {
            // BIP173: Bech32 for version 0
            bech32::encode(hrp, &data, Variant::Bech32)
                .map_err(|_| AddressError::InvalidEncoding)?
        } else {
            // BIP350: Bech32m for version 1+
            bech32::encode(hrp, &data, Variant::Bech32m)
                .map_err(|_| AddressError::InvalidEncoding)?
        };

        Ok(encoded)
    }

    /// Decode Bech32 or Bech32m address
    pub fn decode(encoded: &str) -> Result<Self, AddressError> {
        // Try Bech32m first (Taproot), then Bech32 (SegWit)
        let (hrp, data, variant) =
            bech32::decode(encoded).map_err(|_| AddressError::InvalidEncoding)?;

        // Determine network from HRP
        let network = match hrp.as_str() {
            "bc" => Network::Mainnet,
            "tb" => Network::Testnet,
            "bcrt" => Network::Regtest,
            _ => return Err(AddressError::InvalidHRP),
        };

        if data.is_empty() {
            return Err(AddressError::InvalidEncoding);
        }

        // First u5 value is witness version
        let witness_version_u5 = data[0];
        let witness_version = witness_version_u5.to_u8();
        if witness_version > 16 {
            return Err(AddressError::InvalidWitnessVersion);
        }

        // Remaining u5 values are witness program (base32 encoded)
        let program_base32 = &data[1..];
        let witness_program = base32_to_witness_program(program_base32)?;

        // Validate variant matches witness version
        match (witness_version, variant) {
            (0, Variant::Bech32) => {
                // Correct: SegWit v0 uses Bech32
            }
            (1..=16, Variant::Bech32m) => {
                // Correct: Taproot v1+ uses Bech32m
            }
            _ => {
                return Err(AddressError::UnsupportedVariant);
            }
        }

        Ok(BitcoinAddress {
            network,
            witness_version,
            witness_program,
        })
    }

    /// Check if address is a Taproot address (P2TR)
    pub fn is_taproot(&self) -> bool {
        self.witness_version == 1 && self.witness_program.len() == 32
    }

    /// Check if address is a SegWit address (P2WPKH or P2WSH)
    pub fn is_segwit(&self) -> bool {
        self.witness_version == 0
    }

    /// Get address type as string
    pub fn address_type(&self) -> &'static str {
        match (self.witness_version, self.witness_program.len()) {
            (0, 20) => "P2WPKH",
            (0, 32) => "P2WSH",
            (1, 32) => "P2TR",
            _ => "Unknown",
        }
    }
}

/// Convert witness program bytes to base32 (u5)
fn witness_program_to_base32(program: &[u8]) -> Vec<bech32::u5> {
    // Convert bytes to base32 (returns Vec<u5> directly)
    program.to_base32()
}

/// Convert base32 (u5) to witness program bytes
fn base32_to_witness_program(data: &[bech32::u5]) -> Result<Vec<u8>, AddressError> {
    Vec::<u8>::from_base32(data).map_err(|_| AddressError::InvalidEncoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_segwit_p2wpkh() {
        // Example P2WPKH address
        let program = vec![0x75; 20]; // 20 bytes
        let addr = BitcoinAddress::new(Network::Mainnet, 0, program).unwrap();
        let encoded = addr.encode().unwrap();

        assert!(encoded.starts_with("bc1"));
        assert_eq!(addr.witness_version, 0);
        assert_eq!(addr.witness_program.len(), 20);
    }

    #[test]
    fn test_encode_segwit_p2wsh() {
        // Example P2WSH address
        let program = vec![0x75; 32]; // 32 bytes
        let addr = BitcoinAddress::new(Network::Mainnet, 0, program).unwrap();
        let encoded = addr.encode().unwrap();

        assert!(encoded.starts_with("bc1"));
        assert_eq!(addr.witness_version, 0);
        assert_eq!(addr.witness_program.len(), 32);
    }

    #[test]
    fn test_encode_taproot_p2tr() {
        // Example Taproot address (P2TR)
        let program = vec![0x75; 32]; // 32 bytes
        let addr = BitcoinAddress::new(Network::Mainnet, 1, program).unwrap();
        let encoded = addr.encode().unwrap();

        assert!(encoded.starts_with("bc1p"));
        assert!(addr.is_taproot());
        assert_eq!(addr.witness_version, 1);
        assert_eq!(addr.witness_program.len(), 32);
    }

    #[test]
    fn test_decode_segwit() {
        // This is a simplified test - actual addresses would need real test vectors
        // For now, we test the structure
        let program = vec![0x75; 20];
        let addr = BitcoinAddress::new(Network::Mainnet, 0, program.clone()).unwrap();
        let encoded = addr.encode().unwrap();

        let decoded = BitcoinAddress::decode(&encoded).unwrap();
        assert_eq!(decoded.witness_version, 0);
        assert_eq!(decoded.witness_program, program);
    }

    #[test]
    fn test_decode_taproot() {
        let program = vec![0x75; 32];
        let addr = BitcoinAddress::new(Network::Mainnet, 1, program.clone()).unwrap();
        let encoded = addr.encode().unwrap();

        let decoded = BitcoinAddress::decode(&encoded).unwrap();
        assert!(decoded.is_taproot());
        assert_eq!(decoded.witness_version, 1);
        assert_eq!(decoded.witness_program, program);
    }

    #[test]
    fn test_invalid_witness_version() {
        let program = vec![0x75; 20];
        let result = BitcoinAddress::new(Network::Mainnet, 17, program);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_witness_length_taproot() {
        // Taproot must be 32 bytes
        let program = vec![0x75; 20]; // Wrong length
        let result = BitcoinAddress::new(Network::Mainnet, 1, program);
        assert!(result.is_err());
    }

    #[test]
    fn test_network_hrp() {
        assert_eq!(Network::Mainnet.hrp(), "bc");
        assert_eq!(Network::Testnet.hrp(), "tb");
        assert_eq!(Network::Regtest.hrp(), "bcrt");
    }

    #[test]
    fn test_address_types() {
        let p2wpkh = BitcoinAddress::new(Network::Mainnet, 0, vec![0; 20]).unwrap();
        assert_eq!(p2wpkh.address_type(), "P2WPKH");
        assert!(p2wpkh.is_segwit());

        let p2wsh = BitcoinAddress::new(Network::Mainnet, 0, vec![0; 32]).unwrap();
        assert_eq!(p2wsh.address_type(), "P2WSH");
        assert!(p2wsh.is_segwit());

        let p2tr = BitcoinAddress::new(Network::Mainnet, 1, vec![0; 32]).unwrap();
        assert_eq!(p2tr.address_type(), "P2TR");
        assert!(p2tr.is_taproot());
    }
}
