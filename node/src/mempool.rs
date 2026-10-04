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
    /// M54: maximum transactions held pending at once. `usize::MAX` ⇒ unbounded
    /// (the default; production opts into a real bound via [`set_capacity`]).
    capacity: usize,
    /// M57: author → current pending-tx count. The per-account quota index; an
    /// entry is removed when it hits 0 so the map stays bounded by distinct live
    /// authors, not by all authors ever seen.
    per_author: BTreeMap<u64, usize>,
    /// M57: max pending txs one author may hold at once. `usize::MAX` ⇒ unbounded
    /// (the default; production opts in via [`set_per_account_limit`]).
    per_account_limit: usize,
    /// M57: cumulative admissions rejected by the per-account quota (observability;
    /// surfaced as a metrics counter).
    rejected_quota: u64,
}

impl Mempool {
    pub fn new(max_txs: usize) -> Self {
        Mempool {
            pending: BTreeMap::new(),
            max_txs,
            capacity: usize::MAX,
            per_author: BTreeMap::new(),
            per_account_limit: usize::MAX,
            rejected_quota: 0,
        }
    }

    /// M54: bound the number of pending transactions. Admission past this is
    /// rejected with [`ChainError::MempoolFull`]. Lowering it below the current
    /// `len()` does not evict — it only blocks further growth.
    pub fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
    }

    /// M54: the configured pending-pool capacity bound (`usize::MAX` ⇒ unbounded).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// M57: bound how many pending transactions a single account (`author`) may
    /// hold at once. Admission past this is rejected with
    /// [`ChainError::AccountQuotaFull`]. `usize::MAX` ⇒ unbounded (the default).
    pub fn set_per_account_limit(&mut self, limit: usize) {
        self.per_account_limit = limit;
    }

    /// M57: the configured per-account pending-tx bound (`usize::MAX` ⇒ unbounded).
    pub fn per_account_limit(&self) -> usize {
        self.per_account_limit
    }

    /// M57: cumulative admissions rejected by the per-account quota since boot.
    pub fn rejected_quota(&self) -> u64 {
        self.rejected_quota
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
    ///
    /// M54: once the pool holds `capacity` transactions, admitting a *new* hash is
    /// rejected with [`ChainError::MempoolFull`]; re-inserting an already-pending
    /// hash stays idempotent (it does not grow the pool).
    ///
    /// M57: a *new* hash is also rejected with [`ChainError::AccountQuotaFull`] once
    /// the tx's `author` already holds `per_account_limit` pending txs — so one
    /// account cannot monopolize the pool. Checked after `validate_tx` (the author
    /// is signature-authenticated first) and after the global capacity gate.
    pub fn insert(&mut self, chain: &Chain, tx: SubmissionTx) -> Result<Hash, ChainError> {
        chain.state.validate_tx(&tx)?;
        let h = tx.hash();
        let is_new = !self.pending.contains_key(&h);
        if is_new {
            if self.pending.len() >= self.capacity {
                return Err(ChainError::MempoolFull {
                    capacity: self.capacity,
                });
            }
            let held = self.per_author.get(&tx.author).copied().unwrap_or(0);
            if held >= self.per_account_limit {
                self.rejected_quota += 1;
                return Err(ChainError::AccountQuotaFull {
                    author: tx.author,
                    limit: self.per_account_limit,
                });
            }
            *self.per_author.entry(tx.author).or_insert(0) += 1;
        }
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
    ///
    /// M57: each removed tx releases its author's quota slot; an author whose count
    /// reaches 0 is dropped from the index so it cannot grow unbounded.
    pub fn remove_included(&mut self, block: &Block) {
        for tx in &block.txs {
            if self.pending.remove(&tx.hash()).is_some() {
                if let Some(count) = self.per_author.get_mut(&tx.author) {
                    *count -= 1;
                    if *count == 0 {
                        self.per_author.remove(&tx.author);
                    }
                }
            }
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

    #[test]
    fn insert_rejects_when_at_capacity() {
        // M54: with a capacity of 1, the first distinct tx is admitted and the
        // second (a different hash) is rejected with MempoolFull.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_capacity(1);
        assert!(mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).is_ok());
        assert!(matches!(
            mp.insert(&chain, tx(2, 2, 2, 2 * MICRO)),
            Err(ChainError::MempoolFull { capacity: 1 })
        ));
        assert_eq!(mp.len(), 1);
    }

    #[test]
    fn at_capacity_still_allows_idempotent_reinsert() {
        // Re-inserting an already-pending hash at capacity is a no-op that still
        // succeeds — it does not grow the pool, so the bound is not violated.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_capacity(1);
        let t = tx(1, 1, 1, 2 * MICRO);
        let h = mp.insert(&chain, t.clone()).unwrap();
        assert_eq!(mp.insert(&chain, t).unwrap(), h);
        assert_eq!(mp.len(), 1);
    }

    #[test]
    fn insert_rejects_when_author_over_quota() {
        // M57: with a per-account limit of 2, author 1's first two distinct txs are
        // admitted; a third (a new hash from the same author) is rejected.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_per_account_limit(2);
        assert!(mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).is_ok());
        assert!(mp.insert(&chain, tx(1, 2, 2, 2 * MICRO)).is_ok());
        assert!(matches!(
            mp.insert(&chain, tx(1, 3, 3, 2 * MICRO)),
            Err(ChainError::AccountQuotaFull { author: 1, limit: 2 })
        ));
        assert_eq!(mp.len(), 2);
        assert_eq!(mp.rejected_quota(), 1);
    }

    #[test]
    fn quota_is_per_author_not_global() {
        // M57: the bound is per account — with a limit of 1, two *different* authors
        // each admit one tx (the pool holds both); the limit is not a global cap.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_per_account_limit(1);
        assert!(mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).is_ok());
        assert!(mp.insert(&chain, tx(2, 2, 2, 2 * MICRO)).is_ok());
        assert_eq!(mp.len(), 2);
    }

    #[test]
    fn idempotent_reinsert_does_not_consume_quota() {
        // M57: re-inserting an already-pending hash must not spend a second quota
        // slot; a *different* tx from the same author is what trips the limit.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_per_account_limit(1);
        let t = tx(1, 1, 1, 2 * MICRO);
        let h = mp.insert(&chain, t.clone()).unwrap();
        assert_eq!(mp.insert(&chain, t).unwrap(), h); // reinsert: still Ok, no double-count
        assert_eq!(mp.len(), 1);
        assert!(matches!(
            mp.insert(&chain, tx(1, 2, 2, 2 * MICRO)),
            Err(ChainError::AccountQuotaFull { author: 1, limit: 1 })
        ));
    }

    #[test]
    fn remove_included_frees_author_quota() {
        // M57: once an author's tx is committed and dropped from the pool, their
        // quota slot is released so a fresh tx from them is admitted again.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_per_account_limit(1);
        mp.insert(&chain, tx(1, 1, 1, 2 * MICRO)).unwrap();
        // at the limit: a second distinct tx from author 1 is rejected
        assert!(mp.insert(&chain, tx(1, 2, 2, 2 * MICRO)).is_err());

        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        let mut c = chain.clone();
        c.seal(&mut blk).unwrap();
        c.commit(&mut blk).unwrap();
        mp.remove_included(&blk);
        assert!(mp.is_empty());

        // slot freed: author 1 can be admitted again
        assert!(mp.insert(&c, tx(1, 2, 2, 2 * MICRO)).is_ok());
    }

    #[test]
    fn quota_off_by_default_admits_many() {
        // M57: the default limit is unbounded (off), so one author may hold many
        // pending txs — this guards the head-safe, behavior-preserving default.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert_eq!(mp.per_account_limit(), usize::MAX);
        for d in 0..5 {
            mp.insert(&chain, tx(1, d, d as u32, 2 * MICRO)).unwrap();
        }
        assert_eq!(mp.len(), 5);
        assert_eq!(mp.rejected_quota(), 0);
    }
}
