//! The BFT chain driver — grow a certified chain one height at a time.
//!
//! Everything below this module handles a *single* decision: the mempool builds
//! one block (`mempool`), and the round state machine finalizes one block at one
//! height (`round`). This module strings those into a running chain: for each
//! height it builds the next block from the pool, drives BFT consensus over it,
//! applies the finalized block to the [`Chain`] state, and keeps the block's
//! [`Commit`] certificate. The result is a *certified chain* — every committed
//! block is backed by a verifiable > 2/3 finality proof.
//!
//! It stays deterministic and offline: consensus runs over the in-process
//! [`Sim`] bus (the P2P gossip layer is a later milestone), so two drivers with
//! the same genesis, validators and transactions grow byte-identical chains.
//!
//! Faults are first-class: [`ChainDriver::produce`] takes a `silent` set of
//! offline validators. Below 1/3 power crashed the chain keeps making progress
//! (liveness); at or above 1/3 it *stalls* rather than finalize without a quorum
//! (safety) — the driver returns an error and leaves the chain untouched.

use std::collections::{BTreeMap, BTreeSet};

use crate::consensus::Commit;
use crate::mempool::Mempool;
use crate::round::Sim;
use crate::validator::ValidatorUpdate;
use crate::{Block, Chain, ChainError, Genesis, Hash, Keypair, SlashEvidence, StakeOp, SubmissionTx};

#[derive(Debug)]
pub enum DriverError {
    /// No quorum finalized the height (too much voting power offline).
    ConsensusStalled { height: u64 },
    /// The finalized certificate failed verification — should be impossible for
    /// an honest driver; treated as a hard fault.
    BadCertificate { height: u64 },
    /// The finalized block failed to apply — also impossible (it was built by
    /// trial-execution against this exact state), surfaced defensively.
    Apply(ChainError),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::ConsensusStalled { height } => {
                write!(f, "consensus stalled at height {height} (quorum not reached)")
            }
            DriverError::BadCertificate { height } => {
                write!(f, "finality certificate at height {height} failed verification")
            }
            DriverError::Apply(e) => write!(f, "finalized block failed to apply: {e}"),
        }
    }
}

impl std::error::Error for DriverError {}

/// Drives BFT consensus over a growing [`Chain`], one height per [`produce`].
///
/// [`produce`]: ChainDriver::produce
pub struct ChainDriver {
    pub chain: Chain,
    pub mempool: Mempool,
    /// Validator signing-key seeds. Keypairs are not clonable, so the driver
    /// holds seeds and rebuilds the keypair map for each height's `Sim`. This is
    /// a *superset* of the active validators (which live in chain state and
    /// change on-chain) so keys are on hand for validators admitted later.
    seeds: BTreeMap<u64, [u8; 32]>,
    /// Validator-set changes staged to ride along in the next produced block.
    pending_updates: Vec<ValidatorUpdate>,
    /// Bond/unbond ops staged to ride along in the next produced block.
    pending_stake_ops: Vec<StakeOp>,
    /// Equivocation evidence staged to ride along in the next produced block.
    pending_slashing_evidence: Vec<SlashEvidence>,
    /// Each committed block, in height order — retained so the chain can be
    /// persisted (block log) alongside its certificates.
    blocks: Vec<Block>,
    /// One finality certificate per committed height, in order.
    certs: Vec<Commit>,
}

impl ChainDriver {
    pub fn new(genesis: Genesis, seeds: BTreeMap<u64, [u8; 32]>, max_txs: usize) -> Self {
        ChainDriver {
            chain: Chain::new(genesis),
            mempool: Mempool::new(max_txs),
            seeds,
            pending_updates: Vec::new(),
            pending_stake_ops: Vec::new(),
            pending_slashing_evidence: Vec::new(),
            blocks: Vec::new(),
            certs: Vec::new(),
        }
    }

    /// Admit a transaction to the mempool (static validation against current state).
    pub fn submit(&mut self, tx: SubmissionTx) -> Result<Hash, ChainError> {
        self.mempool.insert(&self.chain, tx)
    }

    /// Stage an on-chain validator-set change to be carried by the next block
    /// [`Self::produce`] finalizes. The change is certified by the *current*
    /// validator set and takes effect from the following height.
    pub fn stage_validator_update(&mut self, update: ValidatorUpdate) {
        self.pending_updates.push(update);
    }

    /// Stage a signed bond/unbond op to be carried by the next block
    /// [`Self::produce`] finalizes. Like validator updates, the implied power
    /// change is certified by the *current* set and takes effect next height.
    pub fn stage_stake_op(&mut self, op: StakeOp) {
        self.pending_stake_ops.push(op);
    }

    /// Stage equivocation evidence to be carried by the next block. The offender
    /// is slashed and removed on apply — verified against the *current* set, with
    /// removal taking effect next height (same discipline as staking/updates).
    pub fn stage_slashing_evidence(&mut self, ev: SlashEvidence) {
        self.pending_slashing_evidence.push(ev);
    }

    pub fn height(&self) -> u64 {
        self.chain.state.height
    }

    pub fn head(&self) -> Hash {
        self.chain.head
    }

    /// The finality certificates of every committed height, in order.
    pub fn certificates(&self) -> &[Commit] {
        &self.certs
    }

    /// Every committed block, in height order — pair with [`Self::certificates`]
    /// (same order and length) to persist the certified chain.
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    fn keys(&self) -> BTreeMap<u64, Keypair> {
        self.seeds
            .iter()
            .map(|(&id, &s)| (id, Keypair::from_seed(s)))
            .collect()
    }

    /// Build the next block from the mempool and run one height of BFT consensus
    /// over it. `silent` names validators offline this height (fault injection).
    ///
    /// * `Ok(None)` — the mempool has nothing that would apply; no block produced.
    /// * `Ok(Some(commit))` — the height was finalized; the block is committed to
    ///   the chain and its verified certificate is returned and retained.
    /// * `Err(..)` — consensus stalled (quorum offline) or a finalized artifact
    ///   failed verification; the chain is left untouched.
    pub fn produce(
        &mut self,
        timestamp_days: f32,
        silent: &BTreeSet<u64>,
    ) -> Result<Option<Commit>, DriverError> {
        // build the next block from the pool; if the pool yields nothing but a
        // validator change, a stake op, or slashing evidence is staged, produce
        // an empty-tx block.
        let mut candidate = match self.mempool.build_block(&self.chain, timestamp_days) {
            Some(b) => b,
            None if !self.pending_updates.is_empty()
                || !self.pending_stake_ops.is_empty()
                || !self.pending_slashing_evidence.is_empty() =>
            {
                Block {
                    height: self.chain.state.height + 1,
                    prev_hash: self.chain.head,
                    timestamp_days,
                    txs: Vec::new(),
                    validator_updates: Vec::new(),
                    stake_ops: Vec::new(),
                    slashing_evidence: Vec::new(),
                }
            }
            None => return Ok(None),
        };
        candidate.validator_updates = self.pending_updates.clone();
        candidate.stake_ops = self.pending_stake_ops.clone();
        candidate.slashing_evidence = self.pending_slashing_evidence.clone();
        let height = candidate.height;

        // consensus over this height uses the set ACTIVE for it — the on-chain
        // set in force before this block applies. Updates the block carries only
        // take effect next height, so the new set never votes on its own arrival.
        let active = self.chain.state.validators.clone();

        // drive BFT consensus over the candidate on the in-process bus
        let mut sim = Sim::new(active.clone(), self.keys(), height, candidate.clone(), silent);
        let decisions = sim.run();

        // every honest validator decides the same block; take any certificate
        let commit = match decisions.into_values().next() {
            Some(c) => c,
            None => return Err(DriverError::ConsensusStalled { height }),
        };

        // trust nothing we did not verify: the certificate must be a real >2/3
        // quorum of the active set, and certify exactly the block we will commit
        if commit.verify(&active).is_err() || commit.block_hash != candidate.hash() {
            return Err(DriverError::BadCertificate { height });
        }

        self.apply(&candidate)?;
        self.pending_updates.clear();
        self.pending_stake_ops.clear();
        self.pending_slashing_evidence.clear();
        self.blocks.push(candidate);
        self.certs.push(commit.clone());
        Ok(Some(commit))
    }

    fn apply(&mut self, block: &Block) -> Result<(), DriverError> {
        self.chain.commit(block).map_err(DriverError::Apply)?;
        self.mempool.remove_included(block);
        Ok(())
    }

    /// Produce heights (all validators honest) until the mempool no longer yields
    /// a block, up to `max_heights`. Returns the number of heights committed.
    pub fn produce_until_drained(
        &mut self,
        timestamp_days: f32,
        max_heights: usize,
    ) -> Result<usize, DriverError> {
        let mut n = 0;
        while n < max_heights {
            match self.produce(timestamp_days + n as f32, &BTreeSet::new())? {
                Some(_) => n += 1,
                None => break,
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::{Validator, ValidatorSet, ValidatorUpdate};
    use crate::{BondKind, Genesis, Review, SlashEvidence, Vote, VoteType, DIM, MICRO};
    use zhixing_engine::DeltaKParams;

    fn seed(id: u64) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&id.to_le_bytes());
        s
    }

    fn kp(id: u64) -> Keypair {
        Keypair::from_seed(seed(id))
    }

    fn unit(d: usize) -> [f32; DIM] {
        let mut e = [0.0f32; DIM];
        e[d % DIM] = 1.0;
        e
    }

    fn genesis() -> Genesis {
        Genesis {
            accounts: vec![
                (1, 30 * MICRO, kp(1).public()),
                (2, 30 * MICRO, kp(2).public()),
                (3, 30 * MICRO, kp(3).public()),
            ],
            reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
            seed_nodes: vec![(unit(0), 0)],
            params: DeltaKParams::default(),
            base_emission_micro: 8 * MICRO,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: [21u64, 22, 23, 24]
                .iter()
                .map(|&id| (id, kp(id).public(), 1))
                .collect(),
        }
    }

    fn validators() -> (ValidatorSet, BTreeMap<u64, [u8; 32]>) {
        let ids = [21u64, 22, 23, 24];
        let vset = ValidatorSet::new(
            ids.iter()
                .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
                .collect(),
        );
        let seeds = ids.iter().map(|&id| (id, seed(id))).collect();
        (vset, seeds)
    }

    fn tx(author: u64, dim: usize, domain: u32) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(dim),
            domain,
            stake: 2 * MICRO,
            reviews: vec![
                Review { reviewer: 10, score: 0.9 },
                Review { reviewer: 11, score: 0.85 },
                Review { reviewer: 12, score: 0.9 },
            ],
            repl_success: 3,
            repl_total: 3,
            timestamp_days: 1.0,
            signature: [0u8; 64],
        }
        .signed(&kp(author))
    }

    /// One driver, one block per height (max_txs = 1), fed three submissions.
    fn seeded_driver() -> ChainDriver {
        let (_vset, seeds) = validators();
        let mut d = ChainDriver::new(genesis(), seeds, 1);
        d.submit(tx(1, 1, 1)).unwrap();
        d.submit(tx(2, 2, 2)).unwrap();
        d.submit(tx(3, 3, 3)).unwrap();
        d
    }

    #[test]
    fn bonds_stake_and_activates_a_validator_through_the_certified_chain() {
        // seeds must include the bonding account so its validator can vote once active
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut d = ChainDriver::new(genesis(), seeds, 4);
        let bal0 = d.chain.state.accounts[&1].balance;

        // stage a bond by account 1 and finalize a (certified) block carrying it
        let op = StakeOp { account: 1, kind: BondKind::Bond, amount: 5 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1));
        d.stage_stake_op(op);
        d.produce(1.0, &BTreeSet::new()).unwrap().expect("a stake-only block is produced");
        assert_eq!(d.height(), 1);
        assert_eq!(d.chain.state.bonded, 5 * MICRO);
        assert_eq!(d.chain.state.accounts[&1].balance, bal0 - 5 * MICRO);
        // account 1 is now an active validator with power == its bond (certified by the old set)
        assert_eq!(d.chain.state.validators.get(1).map(|v| v.power), Some(5 * MICRO));

        // the grown set (now including #1) certifies the next height
        d.submit(tx(2, 2, 2)).unwrap();
        d.produce(2.0, &BTreeSet::new()).unwrap().expect("next height commits under the grown set");
        assert_eq!(d.height(), 2);
        assert_eq!(d.certificates().len(), 2);
        assert!(d.chain.state.supply_conserved());
    }

    #[test]
    fn slashes_an_equivocating_validator_through_the_certified_chain() {
        // seeds must include the bonding account so its validator can vote once active
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut d = ChainDriver::new(genesis(), seeds, 4);

        // account 1 self-bonds -> becomes an active validator effective height 2
        let op = StakeOp { account: 1, kind: BondKind::Bond, amount: 5 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1));
        d.stage_stake_op(op);
        d.produce(1.0, &BTreeSet::new()).unwrap().expect("a stake-only block is produced");
        assert_eq!(d.chain.state.validators.get(1).map(|v| v.power), Some(5 * MICRO));

        // it double-signs at height 2 — stage the cryptographic proof and finalize
        // a (certified) block carrying it; the offender is slashed and removed.
        let ev = SlashEvidence {
            vote_a: Vote::signed(1, 2, 0, [1u8; 32], VoteType::Precommit, &kp(1)),
            vote_b: Vote::signed(1, 2, 0, [2u8; 32], VoteType::Precommit, &kp(1)),
        };
        d.stage_slashing_evidence(ev);
        d.produce(2.0, &BTreeSet::new()).unwrap().expect("a slashing block is produced");
        assert_eq!(d.height(), 2);
        assert_eq!(d.certificates().len(), 2);
        assert_eq!(d.chain.state.treasury, 5 * MICRO, "bonded stake seized to treasury");
        assert_eq!(d.chain.state.bonded, 0);
        assert!(d.chain.state.validators.get(1).is_none(), "offender removed from the set");
        assert!(d.chain.state.supply_conserved());

        // the certified chain replays and re-verifies finality to the same state
        let chain = Chain::replay_verified(genesis(), d.blocks(), d.certificates()).unwrap();
        assert_eq!(chain.state.state_root(), d.chain.state.state_root());
    }

    #[test]
    fn grows_a_multi_height_certified_chain() {
        let mut d = seeded_driver();
        let n = d.produce_until_drained(1.0, 10).unwrap();
        assert_eq!(n, 3, "three single-tx blocks");
        assert_eq!(d.height(), 3);
        assert_eq!(d.certificates().len(), 3);
        assert!(d.chain.state.supply_conserved());
    }

    #[test]
    fn every_committed_height_has_a_valid_certificate() {
        let (vset, _) = validators();
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();
        // the block hash chain the driver committed: [genesis, h1, h2, h3]
        let hashes = d.chain.block_hashes.clone();
        for (i, commit) in d.certificates().iter().enumerate() {
            assert_eq!(commit.height, (i + 1) as u64);
            assert!(commit.verify(&vset).is_ok());
            // the certificate certifies exactly the block that was committed
            assert_eq!(commit.block_hash, hashes[i + 1]);
        }
    }

    #[test]
    fn certificate_binds_to_the_committed_block() {
        let (vset, _) = validators();
        let mut d = seeded_driver();
        let commit = d.produce(1.0, &BTreeSet::new()).unwrap().unwrap();
        assert_eq!(commit.height, 1);
        assert_eq!(commit.block_hash, d.head());
        assert!(commit.verify(&vset).is_ok());
    }

    #[test]
    fn progresses_with_one_crashed_validator() {
        // one of four validators offline (< 1/3 power) -> chain still grows
        let (vset, _) = validators();
        let mut d = seeded_driver();
        let mut silent = BTreeSet::new();
        silent.insert(24);
        let c = d.produce(1.0, &silent).unwrap().expect("committed");
        assert_eq!(d.height(), 1);
        assert!(c.verify(&vset).is_ok());
    }

    #[test]
    fn stalls_safely_when_quorum_is_impossible() {
        // two of four offline -> quorum 3 unreachable -> stall, chain untouched
        let mut d = seeded_driver();
        let mut silent = BTreeSet::new();
        silent.insert(23);
        silent.insert(24);
        let before = d.height();
        let r = d.produce(1.0, &silent);
        assert!(matches!(r, Err(DriverError::ConsensusStalled { height: 1 })));
        assert_eq!(d.height(), before, "no block committed on a stall");
    }

    #[test]
    fn two_drivers_grow_identical_chains() {
        let mut a = seeded_driver();
        let mut b = seeded_driver();
        a.produce_until_drained(1.0, 10).unwrap();
        b.produce_until_drained(1.0, 10).unwrap();
        assert_eq!(a.head(), b.head());
        assert_eq!(a.chain.state.state_root(), b.chain.state.state_root());
        // and the certificate chains match block-for-block
        let ha: Vec<Hash> = a.certificates().iter().map(|c| c.block_hash).collect();
        let hb: Vec<Hash> = b.certificates().iter().map(|c| c.block_hash).collect();
        assert_eq!(ha, hb);
    }

    #[test]
    fn retains_blocks_paired_with_certificates() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();
        assert_eq!(d.blocks().len(), d.certificates().len());
        // each retained block is exactly the one its certificate finalizes
        for (b, c) in d.blocks().iter().zip(d.certificates()) {
            assert_eq!(b.height, c.height);
            assert_eq!(b.hash(), c.block_hash);
        }
    }

    #[test]
    fn persisted_certified_chain_reverifies_finality() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();

        // replay the retained (blocks, certs) re-verifying every height's quorum
        let replayed =
            Chain::replay_verified(genesis(), d.blocks(), d.certificates()).unwrap();
        assert_eq!(replayed.head, d.head());
        assert_eq!(replayed.state.state_root(), d.chain.state.state_root());
    }

    #[test]
    fn replay_rejects_a_forged_certificate() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();

        // tamper: point the first certificate at a different block hash
        let mut certs = d.certificates().to_vec();
        certs[0].block_hash = [0xabu8; 32];
        let r = Chain::replay_verified(genesis(), d.blocks(), &certs);
        assert!(matches!(
            r,
            Err(crate::ReplayError::CertificateMismatch { height: 1 })
        ));
    }

    #[test]
    fn replay_rejects_a_dropped_certificate() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();

        // a certificate goes missing -> counts no longer line up
        let certs = &d.certificates()[..d.certificates().len() - 1];
        let r = Chain::replay_verified(genesis(), d.blocks(), certs);
        assert!(matches!(r, Err(crate::ReplayError::CountMismatch { .. })));
    }

    #[test]
    fn replay_rejects_a_certificate_below_quorum() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();

        // strip the first cert down to a single precommit -> not > 2/3 power
        let mut certs = d.certificates().to_vec();
        certs[0].precommits.truncate(1);
        let r = Chain::replay_verified(genesis(), d.blocks(), &certs);
        assert!(matches!(r, Err(crate::ReplayError::Consensus(_))));
    }

    #[test]
    fn grows_across_an_on_chain_validator_change() {
        // seeds are a superset (21..=25); the genesis set is only 21..=24
        let seeds: BTreeMap<u64, [u8; 32]> = (21u64..=25).map(|id| (id, seed(id))).collect();
        let mut d = ChainDriver::new(genesis(), seeds, 1);
        d.submit(tx(1, 1, 1)).unwrap();
        d.submit(tx(2, 2, 2)).unwrap();
        d.submit(tx(3, 3, 3)).unwrap();

        // height 1 under the genesis set of four
        d.produce(1.0, &BTreeSet::new()).unwrap().unwrap();
        assert_eq!(d.chain.state.validators.len(), 4);

        // admit validator #25; the change rides in the height-2 block but is
        // certified by the PRE-change set (the newcomer never votes on its arrival)
        let before2 = d.chain.state.validators.clone();
        d.stage_validator_update(ValidatorUpdate { id: 25, pubkey: kp(25).public(), power: 1 });
        let c2 = d.produce(2.0, &BTreeSet::new()).unwrap().unwrap();
        assert!(c2.verify(&before2).is_ok(), "certified by the old set");
        assert_eq!(before2.len(), 4);
        assert_eq!(d.chain.state.validators.len(), 5, "set grew for the next height");

        // height 3 is now certified by the NEW set of five
        let before3 = d.chain.state.validators.clone();
        let c3 = d.produce(3.0, &BTreeSet::new()).unwrap().unwrap();
        assert_eq!(before3.len(), 5);
        assert!(c3.verify(&before3).is_ok());

        // replay follows the handoff exactly: each height re-verified against the
        // set that was active for it, ending on the evolved five-validator set
        let replayed = Chain::replay_verified(genesis(), d.blocks(), d.certificates()).unwrap();
        assert_eq!(replayed.head, d.head());
        assert_eq!(replayed.state.validators.len(), 5);
        assert_eq!(replayed.state.state_root(), d.chain.state.state_root());
    }

    #[test]
    fn replay_under_a_different_genesis_validator_set_is_rejected() {
        let mut d = seeded_driver();
        d.produce_until_drained(1.0, 10).unwrap();

        // the validator set is genesis-anchored consensus state: replay against a
        // genesis naming different validators cannot re-verify the real quorum.
        let mut g = genesis();
        g.validators = vec![
            (90, kp(90).public(), 1),
            (91, kp(91).public(), 1),
            (92, kp(92).public(), 1),
        ];
        let r = Chain::replay_verified(g, d.blocks(), d.certificates());
        assert!(matches!(r, Err(crate::ReplayError::Consensus(_))));
    }
}
