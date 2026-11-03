//! M215 (remote-signer S1): the `Signer` abstraction over the validator's consensus
//! signing key — the first slice of [`docs/design/remote-signer.md`](../../docs/design/remote-signer.md).
//!
//! The consensus path (`Vote::signed`, `Proposal::signed`, the `RoundState` methods) and
//! the peer-auth handshake sign through a `&dyn Signer` rather than a concrete
//! [`crate::Keypair`]. In-process signing ([`impl Signer for Keypair`]) is the default and
//! is **byte-identical** to the previous direct `Keypair::sign` — S1 introduces the seam
//! with no behavior change. A later slice adds an out-of-process `RemoteSigner` (Unix
//! socket, with an anti-double-sign high-water-mark) behind the same trait, plus the
//! `(domain, height, round, step)` signing context and fallible/`fail-closed` semantics
//! that an external signer needs (deferred here to keep the sync consensus state machine
//! unchanged).

use crate::{Keypair, PubKey, Sig};

/// Produces the validator's ed25519 signatures. Implemented in-process by [`Keypair`];
/// a future `RemoteSigner` implements it over an IPC boundary so the key never lives in
/// the node process.
pub trait Signer: Send + Sync {
    /// The validator public key this signer signs for (checked against the genesis set at
    /// startup).
    fn public(&self) -> PubKey;
    /// Sign `msg` (already the domain-separated bytes a vote/proposal/handshake commits to).
    fn sign(&self, msg: &[u8]) -> Sig;
}

impl Signer for Keypair {
    fn public(&self) -> PubKey {
        Keypair::public(self)
    }
    fn sign(&self, msg: &[u8]) -> Sig {
        Keypair::sign(self, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Keypair;

    #[test]
    fn keypair_as_signer_is_byte_identical_to_direct_sign() {
        // S1 invariant: routing through `&dyn Signer` changes nothing vs the former direct
        // `Keypair::sign` — same signature bytes, same pubkey. Guards the refactor.
        let kp = Keypair::from_seed([7u8; 32]);
        let msg = b"vote/proposal/handshake signing bytes";
        let via_trait: &dyn Signer = &kp;
        assert_eq!(via_trait.sign(msg), Keypair::sign(&kp, msg));
        assert_eq!(via_trait.public(), kp.public());
    }
}
