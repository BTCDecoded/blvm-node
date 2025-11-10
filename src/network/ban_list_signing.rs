//! Ban list cryptographic signing
//!
//! Provides functions to sign and verify ban lists for authenticity.

use crate::network::protocol::{BanListMessage, BanEntry};
use secp256k1::{Secp256k1, Message, ecdsa::Signature, SecretKey, PublicKey};

/// Sign a ban list with a private key
///
/// Returns the signature as bytes (64 bytes for secp256k1)
pub fn sign_ban_list(
    ban_list: &BanListMessage,
    private_key: &SecretKey,
) -> Result<Vec<u8>, secp256k1::Error> {
    let secp = Secp256k1::new();
    
    // Serialize ban list for signing
    let serialized = bincode::serialize(ban_list)
        .map_err(|_| secp256k1::Error::InvalidMessage)?;
    
    // Hash the serialized data
    use sha2::{Sha256, Digest};
    let hash = Sha256::digest(&serialized);
    
    // Create message from hash (convert GenericArray to slice)
    let message = Message::from_slice(hash.as_slice())
        .map_err(|_| secp256k1::Error::InvalidMessage)?;
    
    // Sign
    let signature = secp.sign_ecdsa(&message, private_key);
    
    // Serialize signature
    Ok(signature.serialize_compact().to_vec())
}

/// Verify a ban list signature
///
/// Returns true if signature is valid
pub fn verify_ban_list_signature(
    ban_list: &BanListMessage,
    signature: &[u8],
    public_key: &PublicKey,
) -> Result<bool, secp256k1::Error> {
    if signature.len() != 64 {
        return Ok(false);
    }
    
    let secp = Secp256k1::new();
    
    // Serialize ban list
    let serialized = bincode::serialize(ban_list)
        .map_err(|_| secp256k1::Error::InvalidMessage)?;
    
    // Hash the serialized data
    use sha2::{Sha256, Digest};
    let hash = Sha256::digest(&serialized);
    
    // Create message from hash (convert GenericArray to slice)
    let message = Message::from_slice(hash.as_slice())
        .map_err(|_| secp256k1::Error::InvalidMessage)?;
    
    // Parse signature
    let sig = Signature::from_compact(signature)
        .map_err(|_| secp256k1::Error::InvalidSignature)?;
    
    // Verify
    Ok(secp.verify_ecdsa(&message, &sig, public_key).is_ok())
}

/// Extended ban list message with signature
#[derive(Debug, Clone)]
pub struct SignedBanListMessage {
    /// The ban list message
    pub ban_list: BanListMessage,
    /// Signature over the ban list
    pub signature: Vec<u8>,
    /// Public key of the signer
    pub public_key: PublicKey,
}

impl SignedBanListMessage {
    /// Create a signed ban list message
    pub fn new(
        ban_list: BanListMessage,
        private_key: &SecretKey,
    ) -> Result<Self, secp256k1::Error> {
        let secp = Secp256k1::new();
        let public_key = PublicKey::from_secret_key(&secp, private_key);
        
        let signature = sign_ban_list(&ban_list, private_key)?;
        
        Ok(Self {
            ban_list,
            signature,
            public_key,
        })
    }
    
    /// Verify the signature
    pub fn verify(&self) -> Result<bool, secp256k1::Error> {
        verify_ban_list_signature(&self.ban_list, &self.signature, &self.public_key)
    }
}


