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
use crate::validator::ValidatorSet;
use crate::{Block, Chain, ChainError, Genesis, Hash, Keypair, SubmissionTx};

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
    vset: ValidatorSet,
    /// Validator signing-key seeds. Keypairs are not clonable, so the driver
    /// holds seeds and rebuilds the keypair map for each height's `Sim`.
    seeds: BTreeMap<u64, [u8; 32]>,
    /// One finality certificate per committed height, in order.
    certs: Vec<Commit>,
}

impl ChainDriver {
    pub fn new(
        genesis: Genesis,
        vset: ValidatorSet,
        seeds: BTreeMap<u64, [u8; 32]>,
        max_txs: usize,
    ) -> Self {
        ChainDriver {
            chain: Chain::new(genesis),
            mempool: Mempool::new(max_txs),
            vset,
            seeds,
            certs: Vec::new(),
        }
    }

    /// Admit a transaction to the mempool (static validation against current state).
    pub fn submit(&mut self, tx: SubmissionTx) -> Result<Hash, ChainError> {
        self.mempool.insert(&self.chain, tx)
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
        let candidate = match self.mempool.build_block(&self.chain, timestamp_days) {
            Some(b) => b,
            None => return Ok(None),
        };
        let height = candidate.height;

        // drive BFT consensus over the candidate on the in-process bus
        let mut sim = Sim::new(self.vset.clone(), self.keys(), height, candidate.clone(), silent);
        let decisions = sim.run();

        // every honest validator decides the same block; take any certificate
        let commit = match decisions.into_values().next() {
            Some(c) => c,
            None => return Err(DriverError::ConsensusStalled { height }),
        };

        // trust nothing we did not verify: the certificate must be a real >2/3
        // quorum, and it must certify exactly the block we are about to commit
        if commit.verify(&self.vset).is_err() || commit.block_hash != candidate.hash() {
            return Err(DriverError::BadCertificate { height });
        }

        self.apply(&candidate)?;
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
    use crate::validator::Validator;
    use crate::{Genesis, Review, DIM, MICRO};
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
        let (vset, seeds) = validators();
        let mut d = ChainDriver::new(genesis(), vset, seeds, 1);
        d.submit(tx(1, 1, 1)).unwrap();
        d.submit(tx(2, 2, 2)).unwrap();
        d.submit(tx(3, 3, 3)).unwrap();
        d
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
}
