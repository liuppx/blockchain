//! M215/M216 (remote-signer S1+S2): the `Signer` abstraction over the validator's
//! consensus signing key — [`docs/design/remote-signer.md`](../../docs/design/remote-signer.md).
//!
//! The consensus path (`Vote::signed_with`, `Proposal::signed_with`, the `RoundState`
//! methods) and the peer-auth handshake sign through a `&dyn Signer` rather than a concrete
//! [`crate::Keypair`]. In-process signing ([`impl Signer for Keypair`]) is the default and
//! is **byte-identical** to the previous direct `Keypair::sign`, and is **infallible**.
//!
//! M216 (S2) made `sign` **fallible and domain-aware** so an out-of-process `RemoteSigner`
//! can (a) be unavailable and (b) enforce an **anti-double-sign high-water-mark** over
//! `(height, round, step)`. Consensus **fails closed** on `Err`: a node that cannot obtain a
//! signature simply does not vote/propose that step (a liveness cost, never a safety one).
//! The in-process `Keypair` signer never errors, so this is still zero behavior change for
//! the default deployment.

use crate::{Keypair, PubKey, Sig};

/// What a signature authorizes — lets an external signer apply an anti-double-sign
/// high-water-mark to *consensus* votes/proposals while leaving handshake signatures
/// (which legitimately repeat for fresh nonces) unconstrained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignDomain {
    /// A consensus vote or proposal at `(height, round, step)` — the tuple an external
    /// signer tracks monotonically to make double-signing physically impossible.
    Consensus {
        height: u64,
        round: u32,
        step: ConsensusStep,
    },
    /// A peer-auth handshake transcript — no high-water-mark (repeats with fresh nonces).
    Handshake,
}

/// The step of a consensus signature, ordered `Propose < Prevote < Precommit` so an
/// external signer can reject any request that would regress within a `(height, round)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConsensusStep {
    Propose,
    Prevote,
    Precommit,
}

/// Why a signature could not be produced. Consensus treats **any** `Err` as fail-closed
/// (no vote/proposal this step) — it never falls back to signing locally, or the key
/// isolation an external signer provides would be void.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerError {
    /// The signer is unreachable / timed out (e.g. an external signer process is down).
    Unavailable(String),
    /// The signer refused because the request would regress its anti-double-sign
    /// high-water-mark — the core protection an external signer exists to provide.
    DoubleSignRefused,
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::Unavailable(why) => write!(f, "signer unavailable: {why}"),
            SignerError::DoubleSignRefused => {
                write!(f, "signer refused (anti-double-sign high-water-mark)")
            }
        }
    }
}

impl std::error::Error for SignerError {}

/// Produces the validator's ed25519 signatures. Implemented in-process by [`Keypair`];
/// a future `RemoteSigner` implements it over an IPC boundary so the key never lives in
/// the node process.
pub trait Signer: Send + Sync {
    /// The validator public key this signer signs for (checked against the genesis set at
    /// startup).
    fn public(&self) -> PubKey;
    /// Sign `msg` (already the domain-separated bytes a vote/proposal/handshake commits to).
    /// `domain` lets an external signer enforce its high-water-mark; in-process signers
    /// ignore it. `Err` ⇒ caller fails closed.
    fn sign(&self, domain: SignDomain, msg: &[u8]) -> Result<Sig, SignerError>;
}

impl Signer for Keypair {
    fn public(&self) -> PubKey {
        Keypair::public(self)
    }
    /// In-process signing is **infallible** (no IPC, no high-water-mark) — byte-identical
    /// to the former direct `Keypair::sign`.
    fn sign(&self, _domain: SignDomain, msg: &[u8]) -> Result<Sig, SignerError> {
        Ok(Keypair::sign(self, msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Keypair;

    #[test]
    fn keypair_as_signer_is_byte_identical_and_infallible() {
        // The in-process signer changes nothing vs the former direct `Keypair::sign`:
        // same bytes, same pubkey, and it never errors regardless of domain.
        let kp = Keypair::from_seed([7u8; 32]);
        let msg = b"vote/proposal/handshake signing bytes";
        let via_trait: &dyn Signer = &kp;
        let dom = SignDomain::Consensus {
            height: 9,
            round: 1,
            step: ConsensusStep::Precommit,
        };
        assert_eq!(via_trait.sign(dom, msg).unwrap(), Keypair::sign(&kp, msg));
        assert_eq!(
            via_trait.sign(SignDomain::Handshake, msg).unwrap(),
            Keypair::sign(&kp, msg)
        );
        assert_eq!(via_trait.public(), kp.public());
    }

    #[test]
    fn consensus_step_orders_propose_prevote_precommit() {
        // The high-water-mark relies on this ordering.
        assert!(ConsensusStep::Propose < ConsensusStep::Prevote);
        assert!(ConsensusStep::Prevote < ConsensusStep::Precommit);
    }
}
