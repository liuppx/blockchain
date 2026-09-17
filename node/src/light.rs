//! Light-client validator-set follow protocol (Milestone 20).
//!
//! A full verifier ([`Chain::replay_verified`](crate::Chain::replay_verified))
//! must execute *every* block — applying transactions, minting/slashing $COG,
//! tracking accounts and the cognitive graph — just to learn *who the active
//! validator set is* at a given height, because the set is folded into
//! `state_root` and is only derivable by running each block's transition
//! ([`Chain::apply_block`](crate::Chain)). A wallet that merely wants to know
//! "which keys can finalize height H, with what power" should not have to run
//! the whole state machine.
//!
//! [`ValidatorTracker`] follows the active validator set across heights
//! **without full replay**. For each height it:
//!   1. verifies that height's finality certificate against the *currently
//!      tracked* set (> 2/3 power, real ed25519 signatures), and
//!   2. replicates *only* the validator-set transition `apply_block` performs
//!      (`validator_updates`, plus the power changes implied by `stake_ops` and
//!      `slashing_evidence`) — nothing else.
//!
//! Soundness: everything it consumes — `validator_updates`, `stake_ops`,
//! `slashing_evidence` — lives inside `block_hash`, which the certificate
//! signs. So the derived set is provably the chain's set: a prover cannot steer
//! the tracker to a false set without breaking > 2/3 of the signatures. The
//! result is byte-identical to `replay_verified`'s validator set at every
//! height, but the tracker never touches txs, accounts, the graph or minting.
//!
//! Why the strict mirror is also *complete*: `apply_block`'s next-set
//! derivation reads two pieces of chain state — the `bonds` map and account
//! pubkeys. The tracker mirrors both exactly:
//!   * `bonds` is mutated *only* by bond/unbond ops and by slashing (which
//!     removes the offender's bond); genesis bonds are empty and unbonding
//!     *maturity* never touches `bonds` or the set. So `bonds` is fully
//!     determined by the stake-ops + evidence in the certified blocks.
//!   * account pubkeys are immutable after genesis (accounts are only ever
//!     created at genesis), so the tracker seeds its pubkey registry once from
//!     `Genesis.accounts` and can never miss a touched id. A slashing
//!     offender's derived update carries power 0 (removal), for which
//!     `apply_updates` ignores the pubkey entirely.
//!
//! Transport: the tracker is a pure consumer of `(Block, Commit)` pairs —
//! exactly the pairs a full node already gossips via
//! [`GossipMsg::Blocks`](crate::net::GossipMsg). A light client subscribes to
//! that same sync stream and drops the tx bodies it does not need.

use std::collections::{BTreeMap, BTreeSet};

use crate::consensus::{Commit, ConsensusError};
use crate::validator::{Validator, ValidatorSet, ValidatorUpdate};
use crate::{Block, BondKind, Genesis, Hash, PubKey};

/// Follows the active validator set across a certified chain without full
/// replay. Construct with [`Self::from_genesis`], then feed certified blocks in
/// height order via [`Self::follow`] / [`Self::follow_all`].
#[derive(Clone, Debug)]
pub struct ValidatorTracker {
    /// The active set — the set that certifies the *next* height to follow.
    /// Starts as the genesis set.
    set: ValidatorSet,
    /// Mirror of the chain's bonded stake per account. Starts empty (genesis
    /// bonds are empty); the derived validator power of a touched id is read
    /// straight out of this map, exactly as `apply_block` does.
    bonds: BTreeMap<u64, u64>,
    /// Account id -> pubkey, seeded once from `Genesis.accounts` (immutable
    /// after genesis). Used to fill in a derived update's pubkey, mirroring
    /// `apply_block`'s `self.accounts[id].pubkey` (default `[0;32]` if absent).
    pubkeys: BTreeMap<u64, PubKey>,
    /// Hash of the last followed block — the head the next block must chain to.
    /// Starts as the genesis-block hash.
    head: Hash,
    /// Height of the last followed block. Starts at 0 (genesis).
    height: u64,
}

/// Why a light client rejected a `(block, cert)` pair. Mirrors
/// [`ReplayError`](crate::ReplayError), plus a fork/splice guard and a
/// defensive stake-op consistency check.
#[derive(Debug)]
pub enum LightError {
    /// The block did not extend the tracked height by exactly one.
    BadHeight { expected: u64, got: u64 },
    /// The block's `prev_hash` does not chain to the tracked head — it belongs
    /// to a different chain (a splice/fork attempt).
    ForkDetected { height: u64 },
    /// The certificate's height/block_hash does not match the block it rides
    /// with.
    CertificateMismatch { height: u64 },
    /// [`Self::follow_all`] got a different number of blocks and certificates.
    CountMismatch { blocks: usize, certs: usize },
    /// The certificate failed to verify against the tracked set (not a > 2/3
    /// quorum, a bad signature, an unknown validator, …).
    Consensus(ConsensusError),
    /// Applying this block's updates would empty the set (consensus would
    /// become impossible) — the chain would have rejected it too.
    EmptyValidatorSet,
    /// An unbond exceeds the tracked bond for the account — impossible for a
    /// genuinely certified block; surfaced defensively instead of underflowing.
    InconsistentStakeOp { account: u64, height: u64 },
}

impl std::fmt::Display for LightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LightError::BadHeight { expected, got } => {
                write!(f, "light: bad height (expected {expected}, got {got})")
            }
            LightError::ForkDetected { height } => {
                write!(f, "light: block at height {height} does not chain to tracked head")
            }
            LightError::CertificateMismatch { height } => {
                write!(f, "light: certificate does not match block at height {height}")
            }
            LightError::CountMismatch { blocks, certs } => {
                write!(f, "light: {blocks} blocks but {certs} certificates")
            }
            LightError::Consensus(e) => write!(f, "light: certificate rejected: {e}"),
            LightError::EmptyValidatorSet => {
                write!(f, "light: validator-set update would empty the set")
            }
            LightError::InconsistentStakeOp { account, height } => {
                write!(f, "light: unbond exceeds tracked bond for account {account} at height {height}")
            }
        }
    }
}

impl std::error::Error for LightError {}

impl ValidatorTracker {
    /// Bootstrap from the trusted genesis: the initial validator set, the
    /// immutable account-pubkey registry, an empty bond map and the
    /// genesis-block head. This is the light client's single root of trust.
    pub fn from_genesis(g: &Genesis) -> Self {
        let set = ValidatorSet::new(
            g.validators
                .iter()
                .map(|&(id, pubkey, power)| Validator { id, pubkey, power })
                .collect(),
        );
        let pubkeys = g
            .accounts
            .iter()
            .map(|&(id, _endow, pubkey)| (id, pubkey))
            .collect();
        // genesis-block hash: height 0, zero prev, no txs / updates / ops —
        // exactly the block `ChainState::genesis` hashes for its head.
        let head = Block {
            height: 0,
            prev_hash: [0u8; 32],
            timestamp_days: g.timestamp_days,
            txs: Vec::new(),
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
        }
        .hash();
        ValidatorTracker {
            set,
            bonds: BTreeMap::new(),
            pubkeys,
            head,
            height: 0,
        }
    }

    /// The validator set active for the *next* height (the set that certifies
    /// it). After following height H this is the set that certifies H+1.
    pub fn validators(&self) -> &ValidatorSet {
        &self.set
    }

    /// Height of the last followed block (0 = only genesis so far).
    pub fn height(&self) -> u64 {
        self.height
    }

    /// Hash of the last followed block (genesis hash before any block).
    pub fn head(&self) -> Hash {
        self.head
    }

    /// Follow one certified height: verify the certificate against the tracked
    /// set, then evolve the set exactly as the chain would. On success the
    /// tracker advances by one height and returns the voting power that
    /// certified the block. On any error the tracker is left unchanged.
    pub fn follow(&mut self, block: &Block, cert: &Commit) -> Result<u64, LightError> {
        // 1. the block must extend our head by exactly one height ...
        if block.height != self.height + 1 {
            return Err(LightError::BadHeight {
                expected: self.height + 1,
                got: block.height,
            });
        }
        // 2. ... and chain to the head we last followed (no splicing).
        if block.prev_hash != self.head {
            return Err(LightError::ForkDetected { height: block.height });
        }
        // 3. the certificate must certify exactly this block.
        let block_hash = block.hash();
        if cert.height != block.height || cert.block_hash != block_hash {
            return Err(LightError::CertificateMismatch { height: block.height });
        }
        // 4. the certificate must be a real > 2/3 quorum of the set active for
        //    this height — the set in force *before* this block applies.
        let power = cert.verify(&self.set).map_err(LightError::Consensus)?;

        // 5. replicate ONLY the validator-set transition (mirror apply_block):
        //    stake ops and slashing evidence change bonds; the derived update
        //    of every touched id reads the post-change bond as its new power.
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        for op in &block.stake_ops {
            match op.kind {
                BondKind::Bond => *self.bonds.entry(op.account).or_insert(0) += op.amount,
                BondKind::Unbond => {
                    let cur = self.bonds.get(&op.account).copied().unwrap_or(0);
                    if cur < op.amount {
                        return Err(LightError::InconsistentStakeOp {
                            account: op.account,
                            height: block.height,
                        });
                    }
                    if cur == op.amount {
                        self.bonds.remove(&op.account);
                    } else {
                        self.bonds.insert(op.account, cur - op.amount);
                    }
                }
            }
            touched.insert(op.account);
        }
        for ev in &block.slashing_evidence {
            // slashing removes the offender's entire bond; the derived update
            // (power 0) removes them from the set at the next height.
            self.bonds.remove(&ev.vote_a.validator);
            touched.insert(ev.vote_a.validator);
        }

        // explicit updates first, then one derived update per touched id —
        // the exact order and content of apply_block (lib.rs).
        let mut updates = block.validator_updates.clone();
        for id in touched {
            let power = self.bonds.get(&id).copied().unwrap_or(0);
            let pubkey = self.pubkeys.get(&id).copied().unwrap_or_default();
            updates.push(ValidatorUpdate { id, pubkey, power }); // power 0 == removal
        }
        if !updates.is_empty() {
            let next = self.set.apply_updates(&updates);
            if next.is_empty() {
                return Err(LightError::EmptyValidatorSet);
            }
            self.set = next;
        }

        self.head = block_hash;
        self.height = block.height;
        Ok(power)
    }

    /// Follow a whole certified chain in height order — the `(Block, Commit)`
    /// pairs a full node gossips. Blocks and certificates must be parallel and
    /// equal-length (index `i` is height `i + 1`).
    pub fn follow_all(&mut self, blocks: &[Block], certs: &[Commit]) -> Result<(), LightError> {
        if blocks.len() != certs.len() {
            return Err(LightError::CountMismatch {
                blocks: blocks.len(),
                certs: certs.len(),
            });
        }
        for (b, c) in blocks.iter().zip(certs.iter()) {
            self.follow(b, c)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::VoteType;
    use crate::driver::ChainDriver;
    use crate::{Chain, Review, SlashEvidence, StakeOp, SubmissionTx, Vote, MICRO};
    use zhixing_engine::{DeltaKParams, DIM};

    type Embedding = [f32; DIM];

    fn kp(id: u64) -> crate::Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        crate::Keypair::from_seed(seed)
    }

    fn seed(id: u64) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&id.to_le_bytes());
        s
    }

    fn unit(x: f32, d: usize) -> Embedding {
        let mut e = [0.0f32; DIM];
        e[d] = x;
        e
    }

    fn base_genesis() -> Genesis {
        Genesis {
            accounts: vec![
                (1, 30 * MICRO, kp(1).public()),
                (2, 30 * MICRO, kp(2).public()),
                (3, 30 * MICRO, kp(3).public()),
            ],
            reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
            seed_nodes: vec![(unit(1.0, 0), 0)],
            params: DeltaKParams::default(),
            base_emission_micro: 8 * MICRO,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: vec![
                (21, kp(21).public(), 1),
                (22, kp(22).public(), 1),
                (23, kp(23).public(), 1),
            ],
        }
    }

    /// All ids that might ever sign: genesis validators 21..=23 plus accounts
    /// 1..=3 (so a freshly-bonded account can sign consensus votes) plus 24
    /// (admitted mid-chain by some tests).
    fn driver(g: Genesis) -> ChainDriver {
        let seeds = [1u64, 2, 3, 21, 22, 23, 24]
            .iter()
            .map(|&id| (id, seed(id)))
            .collect();
        ChainDriver::new(g, seeds, 4)
    }

    fn novel_tx(author: u64, domain: u32, dim: usize, day: f32) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(1.0, dim),
            domain,
            stake: 2 * MICRO,
            reviews: vec![
                Review { reviewer: 10, score: 0.9 },
                Review { reviewer: 11, score: 0.85 },
                Review { reviewer: 12, score: 0.9 },
            ],
            repl_success: 3,
            repl_total: 3,
            timestamp_days: day,
            signature: [0u8; 64],
        }
        .signed(&kp(author))
    }

    fn bond(account: u64, amount: u64) -> StakeOp {
        StakeOp { account, kind: BondKind::Bond, amount, signature: [0u8; 64] }.signed(&kp(account))
    }

    fn unbond(account: u64, amount: u64) -> StakeOp {
        StakeOp { account, kind: BondKind::Unbond, amount, signature: [0u8; 64] }.signed(&kp(account))
    }

    fn no_silence() -> BTreeSet<u64> {
        BTreeSet::new()
    }

    /// The active ids+powers of a set, for comparison.
    fn ids_powers(vs: &ValidatorSet) -> Vec<(u64, u64)> {
        vs.validators().iter().map(|v| (v.id, v.power)).collect()
    }

    #[test]
    fn follows_a_plain_chain() {
        let mut d = driver(base_genesis());
        for h in 1..=3u64 {
            d.submit(novel_tx(1, 100 + h as u32, (h as usize) + 1, h as f32)).unwrap();
            d.produce(h as f32, &no_silence()).unwrap().expect("block");
        }
        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        lt.follow_all(d.blocks(), d.certificates()).expect("follow");
        assert_eq!(lt.height(), 3);
        assert_eq!(lt.head(), d.head());
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
        assert_eq!(ids_powers(lt.validators()), vec![(21, 1), (22, 1), (23, 1)]);
    }

    #[test]
    fn follows_explicit_validator_updates() {
        let mut d = driver(base_genesis());
        // h1: add validator 24. h2: remove validator 22.
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 5 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        d.stage_validator_update(ValidatorUpdate { id: 22, pubkey: kp(22).public(), power: 0 });
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        // follow height by height, checking the set tracks after each.
        lt.follow(&d.blocks()[0], &d.certificates()[0]).unwrap();
        assert_eq!(ids_powers(lt.validators()), vec![(21, 1), (22, 1), (23, 1), (24, 5)]);
        lt.follow(&d.blocks()[1], &d.certificates()[1]).unwrap();
        assert_eq!(ids_powers(lt.validators()), vec![(21, 1), (23, 1), (24, 5)]);
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
    }

    #[test]
    fn follows_staking_power_changes() {
        let mut d = driver(base_genesis());
        // h1: account 1 bonds 6 -> validator 1 power 6 next height.
        d.stage_stake_op(bond(1, 6 * MICRO));
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        // h2: account 1 unbonds 2 -> power 4 next height.
        d.stage_stake_op(unbond(1, 2 * MICRO));
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        lt.follow(&d.blocks()[0], &d.certificates()[0]).unwrap();
        assert_eq!(lt.validators().get(1).map(|v| v.power), Some(6 * MICRO));
        lt.follow(&d.blocks()[1], &d.certificates()[1]).unwrap();
        assert_eq!(lt.validators().get(1).map(|v| v.power), Some(4 * MICRO));
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
    }

    #[test]
    fn follows_slashing_removal() {
        let mut d = driver(base_genesis());
        // h1: account 1 bonds and becomes validator 1.
        d.stage_stake_op(bond(1, 6 * MICRO));
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        // h2: validator 1 double-signs; evidence slashes and removes it.
        let ev = SlashEvidence {
            vote_a: Vote::signed(1, 2, 0, [0xAA; 32], VoteType::Precommit, &kp(1)),
            vote_b: Vote::signed(1, 2, 0, [0xBB; 32], VoteType::Precommit, &kp(1)),
        };
        d.stage_slashing_evidence(ev);
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        lt.follow(&d.blocks()[0], &d.certificates()[0]).unwrap();
        assert!(lt.validators().get(1).is_some());
        lt.follow(&d.blocks()[1], &d.certificates()[1]).unwrap();
        assert!(lt.validators().get(1).is_none(), "offender removed by the light client");
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
    }

    #[test]
    fn ignores_transactions() {
        // a block carrying real (accepted) txs AND a validator update: the
        // tracker must reach the right set without executing the txs.
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.submit(novel_tx(2, 2, 2, 1.0)).unwrap();
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 3 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        assert!(!d.blocks()[0].txs.is_empty(), "block really carries txs");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        lt.follow_all(d.blocks(), d.certificates()).unwrap();
        assert_eq!(ids_powers(lt.validators()), vec![(21, 1), (22, 1), (23, 1), (24, 3)]);
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
    }

    #[test]
    fn rejects_a_forged_certificate() {
        let mut d = driver(base_genesis());
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 1 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");

        // tamper: drop precommits below quorum.
        let mut bad = d.certificates()[0].clone();
        bad.precommits.truncate(1);

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        let err = lt.follow(&d.blocks()[0], &bad).unwrap_err();
        assert!(matches!(err, LightError::Consensus(_)), "got {err}");
        assert_eq!(lt.height(), 0, "tracker unchanged on rejection");
    }

    #[test]
    fn rejects_a_spliced_block() {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        d.submit(novel_tx(2, 2, 2, 2.0)).unwrap();
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        // skip height 1 and feed height 2 directly -> its prev_hash is the h1
        // hash, not the genesis head, and its height is 2 not 1.
        let err = lt.follow(&d.blocks()[1], &d.certificates()[1]).unwrap_err();
        assert!(matches!(err, LightError::BadHeight { .. }), "got {err}");

        // a genuine splice at the right height: feed h2's block but claim h1 by
        // rebuilding a block whose height is 1 with a wrong prev_hash.
        let mut spliced = d.blocks()[0].clone();
        spliced.prev_hash = [0x99; 32];
        let err = lt.follow(&spliced, &d.certificates()[0]).unwrap_err();
        assert!(matches!(err, LightError::ForkDetected { .. }), "got {err}");
    }

    #[test]
    fn rejects_a_cert_for_the_wrong_block() {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        d.submit(novel_tx(2, 2, 2, 2.0)).unwrap();
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        // feed block 1 with block 2's certificate.
        let err = lt.follow(&d.blocks()[0], &d.certificates()[1]).unwrap_err();
        assert!(matches!(err, LightError::CertificateMismatch { .. }), "got {err}");
    }

    #[test]
    fn matches_replay_verified_final_set() {
        // one chain exercising ALL transition sources plus ordinary txs.
        let mut d = driver(base_genesis());
        // h1: add validator 24 (explicit update) + a real tx.
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 2 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        // h2: account 2 bonds 7 -> validator 2 (staking).
        d.stage_stake_op(bond(2, 7 * MICRO));
        d.produce(2.0, &no_silence()).unwrap().expect("block");
        // h3: account 2 double-signs -> slashed + removed (slashing).
        let ev = SlashEvidence {
            vote_a: Vote::signed(2, 3, 0, [0x01; 32], VoteType::Precommit, &kp(2)),
            vote_b: Vote::signed(2, 3, 0, [0x02; 32], VoteType::Precommit, &kp(2)),
        };
        d.stage_slashing_evidence(ev);
        d.produce(3.0, &no_silence()).unwrap().expect("block");

        // authoritative full replay ...
        let full = Chain::replay_verified(base_genesis(), d.blocks(), d.certificates())
            .expect("replay");
        // ... vs the light follow.
        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        lt.follow_all(d.blocks(), d.certificates()).expect("follow");

        assert_eq!(
            ids_powers(lt.validators()),
            ids_powers(&full.state.validators),
            "light-followed set must equal the authoritative replayed set"
        );
        assert_eq!(lt.head(), full.head);
        assert_eq!(lt.height(), full.state.height);
    }
}
