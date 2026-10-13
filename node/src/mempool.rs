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
    /// M143: how many future nonces beyond an account's current one the pool will
    /// buffer. `0` ⇒ strict (only `tx.nonce == acct.nonce` admitted — byte-identical
    /// to the pre-M143 one-pending-tx-per-account behavior; the default). `N` ⇒ an
    /// account may hold txs with `acct.nonce <= tx.nonce <= acct.nonce + N`, which the
    /// builder then lays into one block in nonce order.
    nonce_lookahead: usize,
    /// M144: when `true`, admitting a tx at a `(author, nonce)` already held by a
    /// pending tx *replaces* it iff the newcomer bids a strictly higher fee (else the
    /// newcomer is rejected). `false` (default) keeps the pre-M144 behavior — same-slot
    /// competitors coexist and the builder picks the highest fee at build time.
    replace_by_fee: bool,
    /// M145: minimum fee increment a replace-by-fee bid must clear — a replacement is
    /// admitted only when `new.fee >= incumbent.fee + min_fee_bump`. Default `1`, which
    /// for integer fees is exactly "strictly higher" (byte-identical to M144). A larger
    /// value dampens replacement churn (anti-spam); `0` permits equal-fee replacement.
    /// Only consulted when `replace_by_fee` is on.
    min_fee_bump: u64,
    /// M157: operator-configured minimum fee a tx must pay to be admitted (anti-spam
    /// floor). `0` (default) ⇒ no floor — behavior-preserving. A local admission policy,
    /// not a consensus rule: a block carrying a below-floor tx still applies.
    min_fee: u64,
    /// M161: cumulative admissions rejected by the M157 min-fee floor since boot
    /// (observability; surfaced as the `zhixing_txs_min_fee_rejected_total` counter).
    /// `0` while `min_fee == 0` (the floor is off).
    rejected_min_fee: u64,
    /// M162: cumulative admissions rejected by the M54 global capacity bound
    /// (`zhixing_txs_full_rejected_total`; `0` while capacity is unbounded).
    rejected_full: u64,
    /// M162: cumulative admissions rejected as a stale (already-consumed) nonce — a
    /// replay (`zhixing_txs_bad_nonce_rejected_total`).
    rejected_bad_nonce: u64,
    /// M162: cumulative admissions rejected for a nonce past the M143 pipelining window
    /// (`zhixing_txs_nonce_too_far_rejected_total`; `0` under the strict default).
    rejected_nonce_too_far: u64,
    /// M162: cumulative admissions rejected because an M144 replace-by-fee bid did not
    /// clear the incumbent by `min_fee_bump` (`zhixing_txs_replace_fee_rejected_total`;
    /// `0` while `replace_by_fee` is off).
    rejected_replace_fee: u64,
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
            nonce_lookahead: 0,
            replace_by_fee: false,
            min_fee_bump: 1,
            min_fee: 0,
            rejected_min_fee: 0,
            rejected_full: 0,
            rejected_bad_nonce: 0,
            rejected_nonce_too_far: 0,
            rejected_replace_fee: 0,
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

    /// M161: cumulative admissions rejected by the M157 min-fee floor since boot
    /// (monotonic counter; `0` while the floor is off).
    pub fn rejected_min_fee(&self) -> u64 {
        self.rejected_min_fee
    }

    /// M162: cumulative admissions rejected by the M54 global capacity bound since boot.
    pub fn rejected_full(&self) -> u64 {
        self.rejected_full
    }

    /// M162: cumulative admissions rejected as a stale (replayed) nonce since boot.
    pub fn rejected_bad_nonce(&self) -> u64 {
        self.rejected_bad_nonce
    }

    /// M162: cumulative admissions rejected past the M143 nonce pipelining window.
    pub fn rejected_nonce_too_far(&self) -> u64 {
        self.rejected_nonce_too_far
    }

    /// M162: cumulative admissions rejected by an under-bid M144 replace-by-fee attempt.
    pub fn rejected_replace_fee(&self) -> u64 {
        self.rejected_replace_fee
    }

    /// M143: set the nonce pipelining window — how many future nonces beyond an
    /// account's current one may be buffered. `0` ⇒ strict one-pending-tx-per-account
    /// (the default, byte-identical to pre-M143). See the field doc.
    pub fn set_nonce_lookahead(&mut self, lookahead: usize) {
        self.nonce_lookahead = lookahead;
    }

    /// M143: the configured nonce pipelining window (`0` ⇒ strict / off).
    pub fn nonce_lookahead(&self) -> usize {
        self.nonce_lookahead
    }

    /// M144: enable/disable replace-by-fee admission. When on, a new tx at a
    /// `(author, nonce)` already held replaces the incumbent iff it bids a strictly
    /// higher fee. `false` (default) keeps the pre-M144 coexist-and-pick-at-build rule.
    pub fn set_replace_by_fee(&mut self, enabled: bool) {
        self.replace_by_fee = enabled;
    }

    /// M144: whether replace-by-fee admission is enabled.
    pub fn replace_by_fee(&self) -> bool {
        self.replace_by_fee
    }

    /// M145: set the minimum fee increment a replace-by-fee bid must clear (a
    /// replacement needs `new.fee >= incumbent.fee + min_fee_bump`). Default `1`
    /// (= strictly higher). Only consulted when replace-by-fee is on.
    pub fn set_min_fee_bump(&mut self, min_fee_bump: u64) {
        self.min_fee_bump = min_fee_bump;
    }

    /// M145: the configured replace-by-fee minimum increment (default `1`).
    pub fn min_fee_bump(&self) -> u64 {
        self.min_fee_bump
    }

    /// M157: set the minimum-fee admission floor (anti-spam). `0` ⇒ no floor (the
    /// behavior-preserving default).
    pub fn set_min_fee(&mut self, min_fee: u64) {
        self.min_fee = min_fee;
    }

    /// M157: the configured minimum-fee admission floor (`0` ⇒ off).
    pub fn min_fee(&self) -> u64 {
        self.min_fee
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

    /// M111: a hash-ordered snapshot of the pending pool as `(content hash, author)` pairs —
    /// a lightweight directory for the `GET /mempool` read (the full tx bodies are large; a
    /// client that wants one fetches it by hash). `pending` is a `BTreeMap` keyed by hash, so
    /// the order is deterministic and stable.
    pub fn pending_summaries(&self) -> Vec<(Hash, u64)> {
        self.pending.iter().map(|(h, tx)| (*h, tx.author)).collect()
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
        // M157: cheap anti-spam gate first — reject a below-floor fee before the
        // (costlier) signature/state validation. `min_fee == 0` ⇒ no floor.
        if self.min_fee > 0 && tx.fee < self.min_fee {
            self.rejected_min_fee += 1;
            return Err(ChainError::FeeBelowMinimum {
                got: tx.fee,
                min: self.min_fee,
            });
        }
        // M143: validate everything except the nonce, then apply the pipelining window
        // instead of the strict `tx.nonce == acct.nonce` gate. With `nonce_lookahead == 0`
        // this is exactly the strict rule (only the current nonce admits), so behavior is
        // byte-identical to pre-M143.
        chain.state.validate_tx_ignoring_nonce(&tx)?;
        let acct_nonce = chain
            .state
            .accounts
            .get(&tx.author)
            .expect("account existence proven by validate_tx_ignoring_nonce")
            .nonce;
        if tx.nonce < acct_nonce {
            // Stale (already-consumed) nonce — a replay.
            self.rejected_bad_nonce += 1;
            return Err(ChainError::BadNonce {
                account: tx.author,
                expected: acct_nonce,
                got: tx.nonce,
            });
        }
        let limit = acct_nonce.saturating_add(self.nonce_lookahead as u64);
        if tx.nonce > limit {
            self.rejected_nonce_too_far += 1;
            return Err(ChainError::NonceTooFarAhead {
                account: tx.author,
                got: tx.nonce,
                limit,
            });
        }
        let h = tx.hash();
        // M144: replace-by-fee. If an incumbent (different content) holds this
        // `(author, nonce)` slot, replace it only when the newcomer bids a strictly
        // higher fee; otherwise reject. A same-hash re-insert is not a replacement — it
        // falls through to the idempotent path below. Collect the incumbent as owned
        // values first so the immutable borrow ends before we mutate the pool.
        if self.replace_by_fee {
            let incumbent = self
                .pending
                .iter()
                .find(|(ih, t)| t.author == tx.author && t.nonce == tx.nonce && **ih != h)
                .map(|(ih, t)| (*ih, t.fee));
            if let Some((inc_hash, inc_fee)) = incumbent {
                // M145: require the bid to clear the incumbent's fee by at least
                // `min_fee_bump` (default 1 = strictly higher for integer fees).
                if tx.fee >= inc_fee.saturating_add(self.min_fee_bump) {
                    // Drop the incumbent first so the capacity/quota gates below see the
                    // freed slot — a replacement keeps the pool and quota size unchanged.
                    self.pending.remove(&inc_hash);
                    if let Some(count) = self.per_author.get_mut(&tx.author) {
                        *count -= 1;
                        if *count == 0 {
                            self.per_author.remove(&tx.author);
                        }
                    }
                } else {
                    self.rejected_replace_fee += 1;
                    return Err(ChainError::FeeTooLowToReplace {
                        author: tx.author,
                        nonce: tx.nonce,
                        existing_fee: inc_fee,
                        got_fee: tx.fee,
                    });
                }
            }
        }
        let is_new = !self.pending.contains_key(&h);
        if is_new {
            if self.pending.len() >= self.capacity {
                self.rejected_full += 1;
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

    /// Build the next block on top of `chain.head`, executing candidates against a
    /// trial clone and including only those that apply cleanly, up to `max_txs`. The
    /// returned block is guaranteed to commit onto `chain`, and is identical for any
    /// builder with the same pending set and state. Returns `None` if no candidate
    /// would apply.
    ///
    /// M143: candidates are taken in fee-priority order (fee desc, content-hash asc
    /// tie-break) but a tx is only *eligible* when its `nonce` equals the author's
    /// current trial nonce, so an account's txs are laid in strict nonce order while
    /// still competing on fee across accounts. Applying a tx advances its account's
    /// trial nonce, exposing its successor; a multi-pass loop picks those up until a
    /// full pass makes no progress. When no account has more than one admissible tx
    /// (`nonce_lookahead == 0`), every candidate is eligible on the first pass and this
    /// is byte-identical to the pre-M143 single fee-sorted pass.
    pub fn build_block(&self, chain: &Chain, timestamp_days: f32) -> Option<Block> {
        let mut trial = chain.state.clone();
        trial.now_days = timestamp_days;
        let mut candidates: Vec<(&Hash, &SubmissionTx)> = self.pending.iter().collect();
        candidates.sort_by(|(ha, ta), (hb, tb)| tb.fee.cmp(&ta.fee).then(ha.cmp(hb)));
        let mut used = vec![false; candidates.len()];
        let mut included = Vec::new();
        'outer: loop {
            let mut progressed = false;
            for i in 0..candidates.len() {
                if used[i] {
                    continue;
                }
                if included.len() >= self.max_txs {
                    break 'outer;
                }
                let (_, tx) = candidates[i];
                // Eligible only at the author's current trial nonce — preserves per-account
                // nonce order. A future-nonce tx is left untouched (not `used`) so a later
                // pass can pick it up once its predecessor applies.
                if trial.accounts.get(&tx.author).map(|a| a.nonce) != Some(tx.nonce) {
                    continue;
                }
                // apply_tx never partially mutates on error, so a failed candidate leaves
                // `trial` untouched. Mark it `used` either way: on success it is included; on
                // failure (e.g. insufficient balance) it cannot apply at this nonce, and a
                // cheaper same-nonce sibling later in this pass may still apply instead.
                if trial.apply_tx(tx).is_ok() {
                    included.push(tx.clone());
                    progressed = true;
                }
                used[i] = true;
            }
            if !progressed {
                break;
            }
        }
        if included.is_empty() {
            return None;
        }
        Some(Block {
            height: chain.state.height + 1,
            prev_hash: chain.head,
            timestamp_days,
            // M122: the builder has no validator id; the caller (daemon: own id;
            // driver: the round's elected proposer) sets this before `Chain::seal`.
            proposer: 0,
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

    /// M143: drop pending txs whose `nonce` is now below the author's account nonce —
    /// stale entries left after a commit advanced the account (replays, and same-nonce
    /// duplicates whose sibling won inclusion). Call after `remove_included` so a
    /// pipelining pool does not accumulate dead txs (and their per-account quota slots).
    /// A tx whose author no longer exists in state is also dropped.
    pub fn prune_stale(&mut self, chain: &Chain) {
        let state = &chain.state;
        let stale: Vec<Hash> = self
            .pending
            .iter()
            .filter(|(_, tx)| {
                state
                    .accounts
                    .get(&tx.author)
                    .map(|a| tx.nonce < a.nonce)
                    .unwrap_or(true)
            })
            .map(|(h, _)| *h)
            .collect();
        for h in stale {
            if let Some(tx) = self.pending.remove(&h) {
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
            Review {
                reviewer: 10,
                score: 0.9,
            },
            Review {
                reviewer: 11,
                score: 0.85,
            },
            Review {
                reviewer: 12,
                score: 0.9,
            },
        ]
    }

    fn tx(author: u64, dim: usize, domain: u32, stake: u64) -> SubmissionTx {
        tx_nonce(author, dim, domain, stake, 0)
    }

    /// M122: explicit-nonce variant for tests that submit a follow-up tx after a
    /// prior one has already advanced the author's account nonce.
    fn tx_nonce(author: u64, dim: usize, domain: u32, stake: u64, nonce: u64) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(dim),
            domain,
            stake,
            fee: 0,
            nonce,
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
        // M162: the capacity rejection is counted; the admission is not.
        assert_eq!(mp.rejected_full(), 1);
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
            Err(ChainError::AccountQuotaFull {
                author: 1,
                limit: 2
            })
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
            Err(ChainError::AccountQuotaFull {
                author: 1,
                limit: 1
            })
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

        // slot freed: author 1 can be admitted again (M122: account nonce is now 1)
        assert!(mp.insert(&c, tx_nonce(1, 2, 2, 2 * MICRO, 1)).is_ok());
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

    // --- M143: nonce pipelining ------------------------------------------------

    /// M143: explicit fee + nonce variant for pipelining tests.
    fn tx_fee(
        author: u64,
        dim: usize,
        domain: u32,
        stake: u64,
        fee: u64,
        nonce: u64,
    ) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(dim),
            domain,
            stake,
            fee,
            nonce,
            reviews: reviews(),
            repl_success: 3,
            repl_total: 3,
            timestamp_days: 1.0,
            signature: [0u8; 64],
        }
        .signed(&kp(author))
    }

    #[test]
    fn lookahead_zero_rejects_future_nonce() {
        // Default (lookahead 0) preserves the strict one-pending-tx-per-account rule:
        // nonce 0 admits, nonce 1 is rejected as too far ahead (window = [0, 0]).
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert_eq!(mp.nonce_lookahead(), 0);
        assert!(mp.insert(&chain, tx_nonce(1, 1, 1, 2 * MICRO, 0)).is_ok());
        assert!(matches!(
            mp.insert(&chain, tx_nonce(1, 2, 2, 2 * MICRO, 1)),
            Err(ChainError::NonceTooFarAhead {
                account: 1,
                got: 1,
                limit: 0
            })
        ));
    }

    #[test]
    fn pipeline_admits_consecutive_nonces_when_lookahead_set() {
        // With a window, one account may buffer several consecutive future nonces.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_nonce_lookahead(4);
        assert!(mp.insert(&chain, tx_nonce(1, 1, 1, 2 * MICRO, 0)).is_ok());
        assert!(mp.insert(&chain, tx_nonce(1, 2, 2, 2 * MICRO, 1)).is_ok());
        assert!(mp.insert(&chain, tx_nonce(1, 3, 3, 2 * MICRO, 2)).is_ok());
        assert_eq!(mp.len(), 3);
    }

    #[test]
    fn pipeline_builds_account_txs_in_nonce_order_in_one_block() {
        // The three buffered nonces land in ONE block, in strict nonce order, and the
        // block commits cleanly with supply conserved.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_nonce_lookahead(4);
        mp.insert(&chain, tx_nonce(1, 1, 1, 2 * MICRO, 0)).unwrap();
        mp.insert(&chain, tx_nonce(1, 2, 2, 2 * MICRO, 1)).unwrap();
        mp.insert(&chain, tx_nonce(1, 3, 3, 2 * MICRO, 2)).unwrap();

        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        let nonces: Vec<u64> = blk.txs.iter().map(|t| t.nonce).collect();
        assert_eq!(nonces, vec![0, 1, 2], "account txs in strict nonce order");

        let mut c = chain;
        c.seal(&mut blk).unwrap();
        assert!(c.commit(&mut blk).is_ok());
        assert!(c.state.supply_conserved());
        assert_eq!(c.state.accounts[&1].nonce, 3, "all three nonces consumed");
    }

    #[test]
    fn build_fee_priority_across_accounts_respects_per_account_nonce_order() {
        // Account 2 pays a high fee (nonce 0); account 1 pays low fees (nonce 0, 1).
        // Expected block order: 2's tx first (fee priority), then 1's nonce 0, then 1's
        // nonce 1 (an account's own txs never reorder by fee — nonce order wins).
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_nonce_lookahead(4);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 1, 0)).unwrap();
        mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 1, 1)).unwrap();
        mp.insert(&chain, tx_fee(2, 1, 1, 2 * MICRO, 100, 0))
            .unwrap();

        let blk = mp.build_block(&chain, 1.0).unwrap();
        let order: Vec<(u64, u64)> = blk.txs.iter().map(|t| (t.author, t.nonce)).collect();
        assert_eq!(order, vec![(2, 0), (1, 0), (1, 1)]);
    }

    #[test]
    fn pipeline_rejects_stale_and_too_far_ahead() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_nonce_lookahead(2);
        // too far ahead: window is [0, 2], nonce 3 is rejected.
        assert!(matches!(
            mp.insert(&chain, tx_nonce(1, 1, 1, 2 * MICRO, 3)),
            Err(ChainError::NonceTooFarAhead {
                account: 1,
                got: 3,
                limit: 2
            })
        ));
        // M162: the window rejection is counted.
        assert_eq!(mp.rejected_nonce_too_far(), 1);
        // commit nonce 0 so the account advances to nonce 1.
        mp.insert(&chain, tx_nonce(1, 1, 1, 2 * MICRO, 0)).unwrap();
        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        let mut c = chain.clone();
        c.seal(&mut blk).unwrap();
        c.commit(&mut blk).unwrap();
        mp.remove_included(&blk);
        assert_eq!(c.state.accounts[&1].nonce, 1);
        // stale: nonce 0 is now below the account nonce (1) — a replay.
        assert!(matches!(
            mp.insert(&c, tx_nonce(1, 5, 9, 2 * MICRO, 0)),
            Err(ChainError::BadNonce {
                account: 1,
                expected: 1,
                got: 0
            })
        ));
        // M162: the replay rejection is counted (independently of the window counter).
        assert_eq!(mp.rejected_bad_nonce(), 1);
        assert_eq!(mp.rejected_nonce_too_far(), 1);
    }

    #[test]
    fn prune_stale_drops_committed_and_stale_duplicates() {
        // Two same-nonce (0) duplicates from account 1 are both admitted (lookahead 0
        // still allows multiple txs AT the current nonce). The builder includes one; the
        // other is stale once the account advances. prune_stale drops it + frees quota.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 5, 0)).unwrap();
        mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 1, 0)).unwrap();
        assert_eq!(mp.len(), 2);

        let mut blk = mp.build_block(&chain, 1.0).unwrap();
        assert_eq!(blk.txs.len(), 1, "only one tx can take nonce 0");
        let mut c = chain;
        c.seal(&mut blk).unwrap();
        c.commit(&mut blk).unwrap();
        mp.remove_included(&blk);
        assert_eq!(mp.len(), 1, "the losing duplicate is still pending");

        mp.prune_stale(&c);
        assert_eq!(mp.len(), 0, "stale duplicate pruned");
        // quota freed: account 1 can be admitted again at the new nonce.
        assert!(mp.insert(&c, tx_nonce(1, 3, 3, 2 * MICRO, 1)).is_ok());
    }

    // --- M144: replace-by-fee --------------------------------------------------

    #[test]
    fn rbf_replaces_lower_fee_at_same_slot() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 1, 0)).unwrap();
        let hi = mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 5, 0)).unwrap();
        assert_eq!(mp.len(), 1, "same slot holds exactly one tx");
        assert!(mp.contains(&hi), "the higher-fee tx is the one retained");
    }

    #[test]
    fn rbf_rejects_not_higher_fee() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 5, 0)).unwrap();
        // equal fee → rejected.
        assert!(matches!(
            mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 5, 0)),
            Err(ChainError::FeeTooLowToReplace {
                author: 1,
                nonce: 0,
                existing_fee: 5,
                got_fee: 5
            })
        ));
        // lower fee → rejected.
        assert!(matches!(
            mp.insert(&chain, tx_fee(1, 3, 3, 2 * MICRO, 3, 0)),
            Err(ChainError::FeeTooLowToReplace {
                existing_fee: 5,
                got_fee: 3,
                ..
            })
        ));
        assert_eq!(mp.len(), 1, "incumbent (fee 5) stays");
        // M162: both under-bid replacements are counted.
        assert_eq!(mp.rejected_replace_fee(), 2);
    }

    #[test]
    fn rbf_idempotent_same_tx() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        let t = tx_fee(1, 1, 1, 2 * MICRO, 5, 0);
        let h = mp.insert(&chain, t.clone()).unwrap();
        assert_eq!(mp.insert(&chain, t).unwrap(), h, "re-insert is idempotent");
        assert_eq!(mp.len(), 1);
    }

    #[test]
    fn rbf_different_nonces_coexist() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.set_nonce_lookahead(4);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 1, 0)).unwrap();
        mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 1, 1)).unwrap();
        assert_eq!(
            mp.len(),
            2,
            "different (author,nonce) slots are not replaced"
        );
    }

    #[test]
    fn rbf_replacement_preserves_quota() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.set_per_account_limit(1);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 1, 0)).unwrap(); // fills the quota
                                                                      // replacement at the same slot does not exceed the quota.
        let hi = mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 5, 0)).unwrap();
        assert_eq!(mp.len(), 1);
        assert!(mp.contains(&hi));
    }

    #[test]
    fn rbf_off_by_default_keeps_same_slot_duplicates() {
        // Default (off): two same-(author,nonce) competitors coexist (pre-M144 behavior).
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert!(!mp.replace_by_fee());
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 1, 0)).unwrap();
        mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 5, 0)).unwrap();
        assert_eq!(mp.len(), 2);
    }

    // --- M145: replace-by-fee minimum bump -------------------------------------

    #[test]
    fn rbf_default_min_bump_is_one_strictly_higher() {
        // Default min_fee_bump == 1: for integer fees this is exactly "strictly higher".
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert_eq!(mp.min_fee_bump(), 1);
        mp.set_replace_by_fee(true);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 5, 0)).unwrap();
        // +1 clears the bump → replaces.
        let hi = mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 6, 0)).unwrap();
        assert_eq!(mp.len(), 1);
        assert!(mp.contains(&hi));
    }

    #[test]
    fn rbf_min_bump_requires_larger_increment() {
        // With a min bump of 10, a +5 bid is rejected; a +10 bid replaces.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.set_min_fee_bump(10);
        mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 100, 0))
            .unwrap();
        // +5 (105) does not clear 100 + 10 → rejected, incumbent stays.
        assert!(matches!(
            mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 105, 0)),
            Err(ChainError::FeeTooLowToReplace {
                existing_fee: 100,
                got_fee: 105,
                ..
            })
        ));
        assert_eq!(mp.len(), 1);
        // +10 (110) clears the bump → replaces.
        let hi = mp
            .insert(&chain, tx_fee(1, 3, 3, 2 * MICRO, 110, 0))
            .unwrap();
        assert_eq!(mp.len(), 1);
        assert!(mp.contains(&hi));
    }

    #[test]
    fn rbf_min_bump_zero_allows_equal_fee_replacement() {
        // min_fee_bump == 0 permits equal-fee replacement (new content wins the slot).
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        mp.set_replace_by_fee(true);
        mp.set_min_fee_bump(0);
        let lo = mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 5, 0)).unwrap();
        let eq = mp.insert(&chain, tx_fee(1, 2, 2, 2 * MICRO, 5, 0)).unwrap();
        assert_eq!(mp.len(), 1);
        assert!(
            mp.contains(&eq) && !mp.contains(&lo),
            "equal-fee newcomer took the slot"
        );
    }

    // --- M157: minimum-fee admission floor ----------------------------------------

    #[test]
    fn min_fee_floor_rejects_below_and_admits_at_or_above() {
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert_eq!(mp.min_fee(), 0);
        assert_eq!(mp.rejected_min_fee(), 0);
        mp.set_min_fee(10);
        // Below the floor → rejected.
        assert!(matches!(
            mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 9, 0)),
            Err(ChainError::FeeBelowMinimum { got: 9, min: 10 })
        ));
        assert!(mp.is_empty());
        // M161: the rejection is counted for observability.
        assert_eq!(mp.rejected_min_fee(), 1);
        // At the floor → admitted.
        assert!(mp.insert(&chain, tx_fee(2, 2, 2, 2 * MICRO, 10, 0)).is_ok());
        // Above the floor → admitted.
        assert!(mp.insert(&chain, tx_fee(3, 3, 3, 2 * MICRO, 50, 0)).is_ok());
        assert_eq!(mp.len(), 2);
        // Admissions do not bump the rejection counter.
        assert_eq!(mp.rejected_min_fee(), 1);
    }

    #[test]
    fn min_fee_floor_off_by_default_admits_zero_fee() {
        // Default floor is 0 (off): a zero-fee tx is admitted, so the pre-M157 path and
        // localnet (fee 0) are byte-identical.
        let chain = Chain::new(genesis());
        let mut mp = Mempool::new(16);
        assert!(mp.insert(&chain, tx_fee(1, 1, 1, 2 * MICRO, 0, 0)).is_ok());
        assert_eq!(mp.len(), 1);
    }
}
