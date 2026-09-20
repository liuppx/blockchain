//! Deterministic mempool + block builder.
//!
//! A running node does not receive ready-made blocks — it collects pending
//! [`SubmissionTx`]s, then *builds* a block from them. For that block to be
//! consensus-safe, two nodes holding the same pending set and the same chain
//! state must produce a **byte-identical** block. This module guarantees that:
//!
//!   * transactions are keyed by their content-addressed hash, so iteration
//!     order is canonical and independent of arrival order or map internals;
//!   * the builder *executes* candidates against a trial clone of the state in
//!     that canonical order, including only the ones that would commit and
//!     skipping the rest — so the produced block is guaranteed to apply cleanly
//!     and every honest builder drops exactly the same transactions.
//!
//! This is still single-proposer (no leader election / BFT — that is a later
//! milestone); the point here is the *deterministic construction* of a block.

use std::collections::BTreeMap;

use crate::{Block, Chain, ChainError, Hash, SubmissionTx};

/// Pending transactions awaiting inclusion, keyed by content hash for a
/// canonical, builder-independent ordering.
pub struct Mempool {
    pending: BTreeMap<Hash, SubmissionTx>,
    /// Maximum transactions the builder will place in a single block.
    max_txs: usize,
}

impl Mempool {
    pub fn new(max_txs: usize) -> Self {
        Mempool {
            pending: BTreeMap::new(),
            max_txs,
        }
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Whether a transaction with this content hash is currently pending.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.pending.contains_key(hash)
    }

    /// Admit a transaction after static validation against `chain`'s current
    /// state (signature, known account/reviewers, well-formed reviews, stake
    /// covered). Returns the tx hash on success. Duplicates (same content hash)
    /// are idempotent. Admission does **not** guarantee inclusion: balances can
    /// change before the tx is built into a block, and the builder re-checks.
    pub fn insert(&mut self, chain: &Chain, tx: SubmissionTx) -> Result<Hash, ChainError> {
        chain.state.validate_tx(&tx)?;
        let h = tx.hash();
        self.pending.insert(h, tx);
        Ok(h)
    }

    /// Build the next block on top of `chain.head`, executing candidates in
    /// canonical (hash) order against a trial clone and including only those
    /// that apply cleanly, up to `max_txs`. The returned block is guaranteed to
    /// commit onto `chain`, and is identical for any builder with the same
    /// pending set and state. Returns `None` if no candidate would apply.
    pub fn build_block(&self, chain: &Chain, timestamp_days: f32) -> Option<Block> {
        let mut trial = chain.state.clone();
        trial.now_days = timestamp_days;
        let mut included = Vec::new();
        for tx in self.pending.values() {
            if included.len() >= self.max_txs {
                break;
            }
            // apply_tx never partially mutates on error (see validate_tx), so a
            // failed candidate leaves `trial` untouched and is simply skipped.
            if trial.apply_tx(tx).is_ok() {
                included.push(tx.clone());
            }
        }
        if included.is_empty() {
            return None;
        }
        Some(Block {
            height: chain.state.height + 1,
            prev_hash: chain.head,
            timestamp_days,
            // left unsealed: the driver appends ops then seals via `Chain::seal`.
            next_validators_root: [0u8; 32],
            // M23: state_root/accounts_root are stamped by `Chain::commit`
            // after the trial apply succeeds, not by the builder.
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            // M27: same auto-stamp contract — the builder leaves it zero
            // and `Chain::seal` (or `Chain::commit`'s fallback) fills it in.
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: included,
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        })
    }

    /// Drop every transaction carried by `block` from the pool (call after the
    /// block commits). Transactions the builder skipped remain pending.
    pub fn remove_included(&mut self, block: &Block) {
        for tx in &block.txs {
            self.pending.remove(&tx.hash());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Genesis, Keypair, Review, DIM, MICRO};
    use zhixing_engine::DeltaKParams;

    fn unit(d: usize) -> [f32; DIM] {
        let mut e = [0.0f32; DIM];
        e[d] = 1.0;
        e
    }

    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
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
            validators: vec![
                (21, kp(21).public(), 1),
                (22, kp(22).public(), 1),
                (23, kp(23).public(), 1),
            ],
            bridge_sources: vec![],
        }
    }

    fn reviews() -> Vec<Review> {
        vec![
            Review { reviewer: 10, score: 0.9 },
            Review { reviewer: 11, score: 0.85 },
            Review { reviewer: 12, score: 0.9 },
        ]
    }

    fn tx(author: u64, dim: usize, domain: u32, stake: u64) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(dim),
            domain,
            stake,
            reviews: reviews(),
            repl_success: 3,
            repl_total: 3,
            timestamp_days: 1.0,
            signature: [0u8; 64],
        }
        .signed(&kp(author))
    }

    #[test]
    fn built_block_commits_and_orders_canonically() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        // insert in an arbitrary order
        mp.insert(&chain, tx(2, 2, 2, 2 * MICRO)).unwrap();
        mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).unwrap();
        mp.insert(&chain, tx(3, 3, 3, 2 * MICRO)).unwrap();
        assert_eq!(mp.len(), 3);

        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        // canonical order == sorted by tx hash, regardless of insertion order
        let order: Vec<Hash> = blk.txs.iter().map(|t| t.hash()).collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);

        let mut c = chain;
        c.seal(&mut blk).unwrap();
        assert!(c.commit(&mut blk).is_ok()); // built block is guaranteed to apply
    }

    #[test]
    fn two_builders_produce_identical_blocks() {
        let chain = Chain::new(genesis());
        let build = |order: &[(u64, usize, u32)]| {
            let mut mp = Mempool::new(16);
            for &(a, d, dom) in order {
                mp.insert(&chain, tx(a, d, dom, 2 * MICRO)).unwrap();
            }
            mp.build_block(&chain, 1.0).unwrap()
        };
        let a = build(&[(1, 1, 1), (2, 2, 2), (3, 3, 3)]);
        let b = build(&[(3, 3, 3), (1, 1, 1), (2, 2, 2)]); // different arrival order
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn builder_skips_a_tx_that_would_not_apply() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).unwrap();
        // account 1 also submits a second tx staking its whole balance; only one
        // of the two can be covered once the first escrows its stake.
        mp.insert(&chain, tx(1, 4, 4, 30 * MICRO)).unwrap();

        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        let mut c = chain;
        c.seal(&mut blk).unwrap();
        // whatever the builder chose, the block commits cleanly (no stale tx)
        assert!(c.commit(&mut blk).is_ok());
        assert!(c.state.supply_conserved());
    }

    #[test]
    fn remove_included_clears_committed_txs() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).unwrap();
        mp.insert(&chain, tx(2, 2, 2, 2 * MICRO)).unwrap();
        let blk = mp.build_block(&chain, 1.0).unwrap();
        let n = blk.txs.len();
        mp.remove_included(&blk);
        assert_eq!(mp.len(), 2 - n);
    }

    #[test]
    fn empty_pool_builds_nothing() {
        let chain = Chain::new(genesis());
        let mp = Mempool::new(16);
        assert!(mp.build_block(&chain, 1.0).is_none());
    }

    #[test]
    fn rejects_forged_tx_at_admission() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        // account 1's tx signed by account 2's key -> rejected before pooling
        let forged = SubmissionTx {
            author: 1,
            ..tx(1, 1, 1, 2 * MICRO)
        }
        .signed(&kp(2));
        assert!(matches!(
            mp.insert(&chain, forged),
            Err(ChainError::BadSignature(1))
        ));
        assert!(mp.is_empty());
    }
}
