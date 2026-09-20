//! BFT votes and commit certificates — the safety-critical, verifiable core of
//! consensus.
//!
//! What this module IS: the *finality* primitive. A [`Commit`] is a set of
//! ed25519-signed precommit votes for one block at one (height, round). Anyone
//! — including a light client that never saw the network — can [`Commit::verify`]
//! it against a [`ValidatorSet`] and learn that > 2/3 of voting power finalized
//! that block hash. Combined with the Merkle state root (see `merkle`), a light
//! client can then trust any account proved against that block.
//!
//! What this module is NOT (yet): the full round state machine that *drives*
//! validators to a commit under partial synchrony — proposal timeouts,
//! prevote/precommit locking, round changes. That FSM governs *liveness*; the
//! certificate here governs *safety*, and safety is what a verifier checks. The
//! `commit_block` helper simulates one honest round in-process so the mechanism
//! is runnable and testable end-to-end.
//!
//! Accountability: [`detect_equivocation`] extracts any validator that signed
//! precommits for two different block hashes at the same height/round — the
//! cryptographic evidence a real chain would slash for.

use std::collections::{BTreeMap, BTreeSet};

use crate::validator::ValidatorSet;
use crate::{codec, crypto, Block, Hash, Keypair, Sig};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteType {
    Prevote,
    Precommit,
}

impl VoteType {
    pub(crate) fn tag(self) -> u8 {
        match self {
            VoteType::Prevote => 0,
            VoteType::Precommit => 1,
        }
    }

    pub(crate) fn from_tag(tag: u8) -> Option<VoteType> {
        match tag {
            0 => Some(VoteType::Prevote),
            1 => Some(VoteType::Precommit),
            _ => None,
        }
    }
}

/// A validator's signed vote for `block_hash` at (`height`, `round`).
#[derive(Clone, Debug)]
pub struct Vote {
    pub validator: u64,
    pub height: u64,
    pub round: u32,
    pub block_hash: Hash,
    pub vote_type: VoteType,
    pub signature: Sig,
}

/// The exact bytes a validator signs for a vote (everything but the signature).
pub fn vote_signing_bytes(
    validator: u64,
    height: u64,
    round: u32,
    block_hash: &Hash,
    vote_type: VoteType,
) -> Vec<u8> {
    let mut e = codec::Enc(Vec::new());
    e.u64(validator);
    e.u64(height);
    e.u32(round);
    e.raw(block_hash);
    e.u32(vote_type.tag() as u32);
    e.0
}

impl Vote {
    /// Construct and sign a vote with `kp` (must be `validator`'s key).
    pub fn signed(
        validator: u64,
        height: u64,
        round: u32,
        block_hash: Hash,
        vote_type: VoteType,
        kp: &Keypair,
    ) -> Self {
        let msg = vote_signing_bytes(validator, height, round, &block_hash, vote_type);
        Vote {
            validator,
            height,
            round,
            block_hash,
            vote_type,
            signature: kp.sign(&msg),
        }
    }

    fn signing_bytes(&self) -> Vec<u8> {
        vote_signing_bytes(
            self.validator,
            self.height,
            self.round,
            &self.block_hash,
            self.vote_type,
        )
    }
}

/// A commit certificate: proof that > 2/3 of voting power precommitted
/// `block_hash` at (`height`, `round`). This is the portable, verifiable
/// finality artifact.
#[derive(Clone, Debug)]
pub struct Commit {
    pub height: u64,
    pub round: u32,
    pub block_hash: Hash,
    pub precommits: Vec<Vote>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConsensusError {
    NoVotes,
    WrongVoteType(u64),
    Mismatch(u64),        // vote's height/round/hash disagrees with the commit
    UnknownValidator(u64),
    BadSignature(u64),
    DuplicateValidator(u64),
    NotEnoughPower { got: u64, need: u64 },
}

impl std::fmt::Display for ConsensusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsensusError::NoVotes => write!(f, "commit has no precommits"),
            ConsensusError::WrongVoteType(v) => write!(f, "validator {v} vote is not a precommit"),
            ConsensusError::Mismatch(v) => write!(f, "validator {v} vote does not match the commit"),
            ConsensusError::UnknownValidator(v) => write!(f, "unknown validator {v}"),
            ConsensusError::BadSignature(v) => write!(f, "invalid signature from validator {v}"),
            ConsensusError::DuplicateValidator(v) => write!(f, "validator {v} voted twice"),
            ConsensusError::NotEnoughPower { got, need } => {
                write!(f, "insufficient voting power: {got} < quorum {need}")
            }
        }
    }
}

impl std::error::Error for ConsensusError {}

impl Commit {
    /// Verify the certificate against `vset`: every precommit must be a valid
    /// signature by a known validator, for exactly this height/round/hash, with
    /// no validator counted twice, and total power must reach the quorum.
    /// Returns the accumulated voting power on success.
    pub fn verify(&self, vset: &ValidatorSet) -> Result<u64, ConsensusError> {
        if self.precommits.is_empty() {
            return Err(ConsensusError::NoVotes);
        }
        let mut seen = BTreeSet::new();
        let mut power = 0u64;
        for v in &self.precommits {
            if v.vote_type != VoteType::Precommit {
                return Err(ConsensusError::WrongVoteType(v.validator));
            }
            if v.height != self.height || v.round != self.round || v.block_hash != self.block_hash {
                return Err(ConsensusError::Mismatch(v.validator));
            }
            if !seen.insert(v.validator) {
                return Err(ConsensusError::DuplicateValidator(v.validator));
            }
            let val = vset
                .get(v.validator)
                .ok_or(ConsensusError::UnknownValidator(v.validator))?;
            if !crypto::verify(&val.pubkey, &v.signing_bytes(), &v.signature) {
                return Err(ConsensusError::BadSignature(v.validator));
            }
            power += val.power;
        }
        let need = vset.quorum();
        if power < need {
            return Err(ConsensusError::NotEnoughPower { got: power, need });
        }
        Ok(power)
    }
}

/// Simulate one honest consensus round in-process: the given validators (by
/// keypair) all precommit `block`, and if their combined power reaches quorum a
/// verified [`Commit`] is returned. Models the happy path; the surrounding
/// round FSM (timeouts, locking) is future work.
///
/// `voters` are the validators that actually vote (e.g. omit a crashed one to
/// test fault tolerance). Each must appear in `keys`.
pub fn commit_block(
    vset: &ValidatorSet,
    keys: &BTreeMap<u64, Keypair>,
    block: &Block,
    round: u32,
    voters: &[u64],
) -> Option<Commit> {
    let block_hash = block.hash();
    let mut precommits = Vec::new();
    for &id in voters {
        let kp = keys.get(&id)?;
        precommits.push(Vote::signed(
            id,
            block.height,
            round,
            block_hash,
            VoteType::Precommit,
            kp,
        ));
    }
    let commit = Commit {
        height: block.height,
        round,
        block_hash,
        precommits,
    };
    commit.verify(vset).ok().map(|_| commit)
}

/// Return the validators that precommitted *different* block hashes across two
/// commits at the same height/round — provable equivocation (a slashable BFT
/// fault). Empty if the two commits are consistent or at different heights.
pub fn detect_equivocation(a: &Commit, b: &Commit) -> Vec<u64> {
    if a.height != b.height || a.round != b.round || a.block_hash == b.block_hash {
        return Vec::new();
    }
    let a_by: BTreeMap<u64, Hash> = a.precommits.iter().map(|v| (v.validator, v.block_hash)).collect();
    let mut guilty = Vec::new();
    for v in &b.precommits {
        if let Some(&h) = a_by.get(&v.validator) {
            if h != v.block_hash {
                guilty.push(v.validator);
            }
        }
    }
    guilty.sort();
    guilty.dedup();
    guilty
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::Validator;

    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
    }

    fn keys(ids: &[u64]) -> BTreeMap<u64, Keypair> {
        ids.iter().map(|&id| (id, kp(id))).collect()
    }

    fn vset(ids: &[u64]) -> ValidatorSet {
        ValidatorSet::new(
            ids.iter()
                .map(|&id| Validator {
                    id,
                    pubkey: kp(id).public(),
                    power: 1,
                })
                .collect(),
        )
    }

    fn block(height: u64, tag: u8) -> Block {
        Block {
            height,
            prev_hash: [tag; 32],
            timestamp_days: height as f32,
            next_validators_root: [0u8; 32],
            // M23: state commitments stamped by Chain::commit.
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: Vec::new(),
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        }
    }

    #[test]
    fn quorum_of_precommits_commits() {
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let ks = keys(&ids);
        let b = block(1, 1);
        // 3 of 4 vote (one crashed) -> still commits (tolerates 1 fault)
        let commit = commit_block(&vs, &ks, &b, 0, &[1, 2, 3]).expect("commits");
        assert_eq!(commit.verify(&vs).unwrap(), 3);
    }

    #[test]
    fn below_quorum_does_not_commit() {
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let ks = keys(&ids);
        let b = block(1, 1);
        // only 2 of 4 -> below quorum (need 3)
        assert!(commit_block(&vs, &ks, &b, 0, &[1, 2]).is_none());
    }

    #[test]
    fn forged_precommit_is_rejected() {
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1, 1);
        let bh = b.hash();
        // validator 3's vote signed by validator 4's key
        let mut votes = vec![
            Vote::signed(1, 1, 0, bh, VoteType::Precommit, &kp(1)),
            Vote::signed(2, 1, 0, bh, VoteType::Precommit, &kp(2)),
            Vote::signed(3, 1, 0, bh, VoteType::Precommit, &kp(4)), // forged
        ];
        let commit = Commit { height: 1, round: 0, block_hash: bh, precommits: std::mem::take(&mut votes) };
        assert_eq!(commit.verify(&vs), Err(ConsensusError::BadSignature(3)));
    }

    #[test]
    fn double_counting_a_validator_is_rejected() {
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1, 1);
        let bh = b.hash();
        let commit = Commit {
            height: 1,
            round: 0,
            block_hash: bh,
            precommits: vec![
                Vote::signed(1, 1, 0, bh, VoteType::Precommit, &kp(1)),
                Vote::signed(1, 1, 0, bh, VoteType::Precommit, &kp(1)), // same validator twice
                Vote::signed(2, 1, 0, bh, VoteType::Precommit, &kp(2)),
            ],
        };
        assert_eq!(commit.verify(&vs), Err(ConsensusError::DuplicateValidator(1)));
    }

    #[test]
    fn a_prevote_is_not_a_valid_precommit() {
        let ids = [1, 2, 3];
        let vs = vset(&ids);
        let b = block(1, 1);
        let bh = b.hash();
        let commit = Commit {
            height: 1,
            round: 0,
            block_hash: bh,
            precommits: vec![Vote::signed(1, 1, 0, bh, VoteType::Prevote, &kp(1))],
        };
        assert_eq!(commit.verify(&vs), Err(ConsensusError::WrongVoteType(1)));
    }

    #[test]
    fn conflicting_commits_require_equivocation() {
        // With 4 validators and quorum 3, two commits for different blocks at the
        // same height can only BOTH verify if some validator signed both — which
        // detect_equivocation surfaces.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let ks = keys(&ids);
        let b1 = block(1, 1);
        let b2 = block(1, 2); // different hash, same height

        // an equivocating majority: {1,2,3} commit b1 and {1,2,4} commit b2
        // (validators 1 and 2 double-sign)
        let c1 = commit_block(&vs, &ks, &b1, 0, &[1, 2, 3]).unwrap();
        let c2 = commit_block(&vs, &ks, &b2, 0, &[1, 2, 4]).unwrap();
        assert!(c1.verify(&vs).is_ok() && c2.verify(&vs).is_ok());
        assert_eq!(detect_equivocation(&c1, &c2), vec![1, 2]); // caught
    }

    #[test]
    fn honest_validators_cannot_form_conflicting_commits() {
        // If no validator equivocates, the pigeonhole makes a second quorum
        // impossible: 4 validators, quorum 3, {1,2,3} used -> only {4} left.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let ks = keys(&ids);
        let b2 = block(1, 2);
        // the honest remainder ({4}) cannot reach quorum for a different block
        assert!(commit_block(&vs, &ks, &b2, 0, &[4]).is_none());
    }
}
