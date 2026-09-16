//! Cryptographic identity for the node: ed25519 signatures over transactions.
//!
//! Thin wrappers around the audited `ed25519-dalek` crate. We deliberately do
//! NOT implement the signature scheme ourselves. Keys here are raw 32-byte
//! public keys and 64-byte signatures so the rest of the node (codec, state,
//! hashing) stays plain bytes.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

pub type PubKey = [u8; 32];
pub type Sig = [u8; 64];

/// A signing keypair. `from_seed` is deterministic (no RNG), which keeps demos
/// and tests reproducible; production keys come from a CSPRNG / HSM.
pub struct Keypair(SigningKey);

impl Keypair {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Keypair(SigningKey::from_bytes(&seed))
    }

    pub fn public(&self) -> PubKey {
        self.0.verifying_key().to_bytes()
    }

    pub fn sign(&self, msg: &[u8]) -> Sig {
        self.0.sign(msg).to_bytes()
    }
}

/// Verify `sig` over `msg` against `pk`. Returns false on any malformed input
/// (bad point encoding, wrong signature) rather than erroring.
pub fn verify(pk: &PubKey, msg: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pk) else {
        return false;
    };
    let signature = Signature::from_bytes(sig);
    vk.verify(msg, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let kp = Keypair::from_seed([7u8; 32]);
        let pk = kp.public();
        let msg = b"knowledge submission";
        let sig = kp.sign(msg);
        assert!(verify(&pk, msg, &sig));
    }

    #[test]
    fn tampered_message_fails() {
        let kp = Keypair::from_seed([7u8; 32]);
        let pk = kp.public();
        let sig = kp.sign(b"original");
        assert!(!verify(&pk, b"tampered", &sig));
    }

    #[test]
    fn wrong_key_fails() {
        let kp = Keypair::from_seed([1u8; 32]);
        let other = Keypair::from_seed([2u8; 32]).public();
        let msg = b"x";
        let sig = kp.sign(msg);
        assert!(!verify(&other, msg, &sig));
    }
}
