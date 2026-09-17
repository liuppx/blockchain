//! Reference PoK consensus node for ZhixingGraph (deterministic state machine).
//!
//! This is the on-chain counterpart to the economic simulation (`sim/`) and the
//! ΔK engine (`engine/`): a *deterministic* state transition function that turns
//! a block of submissions into minted/slashed $COG, driven by the SAME B.2.3 ΔK
//! contract (`zhixing_engine::compute_delta_k`). Given identical genesis and
//! identical blocks, every node computes byte-identical state — the prerequisite
//! for consensus.
//!
//! What this layer IS: block/tx/account types, escrow-staked submissions, ΔK
//! finalization, mint/slash accounting, on-chain (outcome-based) reviewer
//! reputation, ed25519-authenticated transactions, a content-addressed block
//! hash chain, a state root, a Merkle-authenticated account state with
//! light-client inclusion proofs, an append-only block log with replay, a
//! deterministic mempool/block builder, a BFT finality core (validator
//! set, proposer selection, verifiable commit certificates), a BFT round
//! state machine that drives liveness under faults (timeouts, prevote/precommit
//! locking, round changes), and a chain driver that strings single-height
//! consensus into a growing, certificate-backed chain — see the sibling modules.
//!
//! What this layer is NOT (yet): real P2P networking — consensus is driven over
//! an in-process message bus (`round::Sim`, used by `driver`) standing in for
//! gossip. That is a later milestone; see README. Money is integer micro-$COG
//! (no floats), so accounting is exact.

pub mod codec;
pub mod consensus;
pub mod crypto;
pub mod driver;
pub mod hash;
pub mod mempool;
pub mod merkle;
pub mod net;
pub mod round;
pub mod store;
pub mod validator;

use std::collections::BTreeMap;

use zhixing_engine::{compute_delta_k, CognitiveGraph, DeltaKParams, GraphNode, Submission, DIM};

pub use crypto::{Keypair, PubKey, Sig};
pub use hash::{hex, sha256};
use validator::{Validator, ValidatorSet, ValidatorUpdate};

/// 1 $COG == 1_000_000 micro-$COG. All balances are integer micro-$COG.
pub const MICRO: u64 = 1_000_000;

pub type Hash = [u8; 32];
pub type Embedding = [f32; DIM];

// --- Transactions ------------------------------------------------------------

/// A reviewer's score for a submission, in [0, 1]. The reviewer's *reputation*
/// is not carried in the tx — it is read from chain state at apply time.
#[derive(Clone, Debug)]
pub struct Review {
    pub reviewer: u64,
    pub score: f32,
}

/// A knowledge submission: the unit of work that PoK mints against.
#[derive(Clone, Debug)]
pub struct SubmissionTx {
    pub author: u64,
    pub embedding: Embedding,
    pub domain: u32,
    /// Escrow staked with the submission, in micro-$COG. Returned on accept,
    /// slashed to treasury on reject.
    pub stake: u64,
    pub reviews: Vec<Review>,
    pub repl_success: u32,
    pub repl_total: u32,
    /// Author-claimed authoring time in days (used for freshness in ΔK).
    pub timestamp_days: f32,
    /// ed25519 signature by `author`'s key over [`codec::tx_signing_bytes`].
    pub signature: Sig,
}

impl SubmissionTx {
    /// Sign this tx's canonical fields with `kp`, filling in `signature`.
    /// The keypair's public key must be the one registered for `author`.
    pub fn signed(mut self, kp: &Keypair) -> Self {
        self.signature = kp.sign(&codec::tx_signing_bytes(&self));
        self
    }

    /// Content-addressed tx hash over the full signed encoding. The mempool
    /// orders by this, so every honest builder lays out identical blocks.
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_tx(self))
    }
}

/// A block: an ordered batch of submissions applied atomically.
#[derive(Clone, Debug)]
pub struct Block {
    pub height: u64,
    pub prev_hash: Hash,
    /// Wall-clock of the block in days; becomes `now_days` for ΔK freshness.
    pub timestamp_days: f32,
    pub txs: Vec<SubmissionTx>,
    /// On-chain validator-set changes carried by this block. Applied after the
    /// transactions and taking effect from the *next* height (this block is
    /// still certified by the set in force before it). Empty in the common case.
    pub validator_updates: Vec<ValidatorUpdate>,
}

impl Block {
    /// Content-addressed block hash over the canonical codec encoding (the same
    /// bytes the block is persisted as, see [`codec`]).
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_block(self))
    }
}

// --- State -------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Account {
    pub pubkey: PubKey,
    pub balance: u64,
    pub staked_total: u64,
    pub earned_total: u64,
    pub slashed_total: u64,
    pub submissions: u64,
    pub accepted: u64,
}

impl Account {
    /// Canonical leaf bytes for this account under `id` — the exact preimage a
    /// light client hashes (via [`merkle::leaf_hash`]) to check an inclusion
    /// proof against [`ChainState::merkle_root`]. Kept here so a verifier needs
    /// only the account it was told, not the whole state.
    pub fn merkle_leaf(&self, id: u64) -> Vec<u8> {
        let mut e = codec::Enc(Vec::new());
        e.u64(id);
        e.raw(&self.pubkey);
        e.u64(self.balance);
        e.u64(self.staked_total);
        e.u64(self.earned_total);
        e.u64(self.slashed_total);
        e.u64(self.submissions);
        e.u64(self.accepted);
        e.0
    }
}

/// The full replicated state. Cloneable so blocks can be applied on a trial copy
/// and rolled back atomically if any tx is invalid.
#[derive(Clone)]
pub struct ChainState {
    pub accounts: BTreeMap<u64, Account>,
    pub reviewers: BTreeMap<u64, f32>, // reviewer id -> reputation
    pub graph: CognitiveGraph,
    pub params: DeltaKParams,
    /// micro-$COG minted per unit ΔK (governance knob, B.2.3 / §5.1).
    pub base_emission_micro: u64,
    /// Fraction of stake slashed on reject, in basis points (10000 = 100%).
    pub slash_bps: u32,
    pub supply: u64,   // total $COG in existence (micro)
    pub treasury: u64, // slashed stake pool (redistributed, not burned)
    pub height: u64,
    pub now_days: f32,
    /// The active validator set — part of consensus state, evolved on-chain by
    /// each block's [`Block::validator_updates`]. Holds the set that certifies
    /// the *next* height (at genesis, the set that certifies height 1).
    pub validators: ValidatorSet,
}

/// Genesis configuration.
pub struct Genesis {
    pub accounts: Vec<(u64, u64, PubKey)>,  // (id, endowment micro-$COG, pubkey)
    pub reviewers: Vec<(u64, f32)>,         // (id, initial reputation)
    pub seed_nodes: Vec<(Embedding, u32)>,  // pre-existing graph nodes
    pub params: DeltaKParams,
    pub base_emission_micro: u64,
    pub slash_bps: u32,
    pub timestamp_days: f32,
    /// The initial validator set (id, pubkey, voting power). Consensus over
    /// height 1 uses exactly this set; later heights evolve it on-chain.
    pub validators: Vec<(u64, PubKey, u64)>,
}

#[derive(Clone, Debug)]
pub enum ChainError {
    BadHeight { expected: u64, got: u64 },
    BadPrevHash,
    UnknownAccount(u64),
    UnknownReviewer(u64),
    InsufficientBalance { account: u64, need: u64, have: u64 },
    BadScore { reviewer: u64, score: f32 },
    EmptyReviews(u64),
    BadSignature(u64),
    /// A block's validator updates would leave the set empty — consensus would
    /// become impossible, so the block is rejected.
    EmptyValidatorSet,
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainError::BadHeight { expected, got } => {
                write!(f, "bad height: expected {expected}, got {got}")
            }
            ChainError::BadPrevHash => write!(f, "prev_hash does not match head"),
            ChainError::UnknownAccount(a) => write!(f, "unknown account {a}"),
            ChainError::UnknownReviewer(r) => write!(f, "unknown reviewer {r}"),
            ChainError::InsufficientBalance { account, need, have } => write!(
                f,
                "account {account} cannot stake {need} (has {have})"
            ),
            ChainError::BadScore { reviewer, score } => {
                write!(f, "reviewer {reviewer} score {score} out of [0,1]")
            }
            ChainError::EmptyReviews(a) => write!(f, "submission by {a} has no reviews"),
            ChainError::BadSignature(a) => write!(f, "invalid signature for account {a}"),
            ChainError::EmptyValidatorSet => {
                write!(f, "validator updates would empty the validator set")
            }
        }
    }
}

impl std::error::Error for ChainError {}

/// Error from replaying a chain *with finality re-verification*
/// ([`Chain::replay_verified`]). Distinguishes a state-transition failure from a
/// certificate that does not finalize the block it accompanies.
#[derive(Debug)]
pub enum ReplayError {
    /// A block failed to apply (bad height/prev-hash/tx) — see [`ChainError`].
    Chain(ChainError),
    /// A block's certificate is not a valid > 2/3 quorum for the validator set.
    Consensus(consensus::ConsensusError),
    /// The certificate at this height does not bind the block it accompanies
    /// (wrong height or block hash) — a certificate for some *other* block.
    CertificateMismatch { height: u64 },
    /// The block log and certificate log have different lengths.
    CountMismatch { blocks: usize, certs: usize },
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::Chain(e) => write!(f, "replay: {e}"),
            ReplayError::Consensus(e) => write!(f, "finality: {e}"),
            ReplayError::CertificateMismatch { height } => {
                write!(f, "certificate at height {height} does not bind its block")
            }
            ReplayError::CountMismatch { blocks, certs } => {
                write!(f, "have {blocks} block(s) but {certs} certificate(s)")
            }
        }
    }
}

impl std::error::Error for ReplayError {}

// --- Receipts ----------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TxReceipt {
    pub author: u64,
    pub accepted: bool,
    pub delta_k: f32,
    pub minted: u64,
    pub slashed: u64,
}

#[derive(Clone, Debug)]
pub struct BlockReceipt {
    pub height: u64,
    pub hash: Hash,
    pub minted: u64,
    pub slashed: u64,
    pub accepted: usize,
    pub rejected: usize,
    pub txs: Vec<TxReceipt>,
}

impl ChainState {
    /// Build genesis state; returns the state and the genesis block hash (which
    /// becomes the head every honest node starts from).
    pub fn genesis(g: Genesis) -> (ChainState, Hash) {
        let mut accounts = BTreeMap::new();
        let mut supply = 0u64;
        for (id, endow, pubkey) in g.accounts {
            supply = supply.saturating_add(endow);
            accounts.insert(
                id,
                Account {
                    pubkey,
                    balance: endow,
                    ..Default::default()
                },
            );
        }
        let reviewers: BTreeMap<u64, f32> = g.reviewers.into_iter().collect();
        let mut graph = CognitiveGraph::new();
        for (emb, dom) in g.seed_nodes {
            graph.add(GraphNode {
                embedding: emb,
                domain: dom,
            });
        }
        let validators = ValidatorSet::new(
            g.validators
                .into_iter()
                .map(|(id, pubkey, power)| Validator { id, pubkey, power })
                .collect(),
        );
        let state = ChainState {
            accounts,
            reviewers,
            graph,
            params: g.params,
            base_emission_micro: g.base_emission_micro,
            slash_bps: g.slash_bps,
            supply,
            treasury: 0,
            height: 0,
            now_days: g.timestamp_days,
            validators,
        };
        // genesis "block" hash: height 0, zero prev, no txs, no validator updates
        let gh = Block {
            height: 0,
            prev_hash: [0u8; 32],
            timestamp_days: g.timestamp_days,
            txs: Vec::new(),
            validator_updates: Vec::new(),
        }
        .hash();
        (state, gh)
    }

    /// Apply a block, mutating self. On `Err` self may be partially mutated —
    /// callers wanting atomicity should apply to a clone (see [`Chain::commit`]).
    pub fn apply_block(&mut self, block: &Block) -> Result<BlockReceipt, ChainError> {
        if block.height != self.height + 1 {
            return Err(ChainError::BadHeight {
                expected: self.height + 1,
                got: block.height,
            });
        }
        self.now_days = block.timestamp_days;

        let mut receipts = Vec::with_capacity(block.txs.len());
        let mut minted_total = 0u64;
        let mut slashed_total = 0u64;
        let mut n_accept = 0usize;
        let mut n_reject = 0usize;

        for tx in &block.txs {
            let r = self.apply_tx(tx)?;
            minted_total += r.minted;
            slashed_total += r.slashed;
            if r.accepted {
                n_accept += 1;
            } else {
                n_reject += 1;
            }
            receipts.push(r);
        }

        // on-chain validator-set transition: the updates in this block take
        // effect from the NEXT height (this block was certified by the set in
        // force before it). Guard against emptying the set, which would make
        // future consensus impossible. Applied on a trial clone via
        // `Chain::commit`, so a rejection here rolls the whole block back.
        if !block.validator_updates.is_empty() {
            let next = self.validators.apply_updates(&block.validator_updates);
            if next.is_empty() {
                return Err(ChainError::EmptyValidatorSet);
            }
            self.validators = next;
        }

        self.height = block.height;
        Ok(BlockReceipt {
            height: block.height,
            hash: block.hash(),
            minted: minted_total,
            slashed: slashed_total,
            accepted: n_accept,
            rejected: n_reject,
            txs: receipts,
        })
    }

    /// Static validity checks that do NOT depend on ΔK or mutate state: reviews
    /// well-formed, reviewers/account known, signature authentic, stake covered.
    /// The mempool uses this for admission; `apply_tx` runs it first, so a tx
    /// that passes here never partially mutates state when applied.
    pub(crate) fn validate_tx(&self, tx: &SubmissionTx) -> Result<(), ChainError> {
        if tx.reviews.is_empty() {
            return Err(ChainError::EmptyReviews(tx.author));
        }
        for r in &tx.reviews {
            if !(0.0..=1.0).contains(&r.score) {
                return Err(ChainError::BadScore {
                    reviewer: r.reviewer,
                    score: r.score,
                });
            }
            if !self.reviewers.contains_key(&r.reviewer) {
                return Err(ChainError::UnknownReviewer(r.reviewer));
            }
        }
        let acct = self
            .accounts
            .get(&tx.author)
            .ok_or(ChainError::UnknownAccount(tx.author))?;
        // authenticate: the signature must be by the account's registered key.
        if !crypto::verify(&acct.pubkey, &codec::tx_signing_bytes(tx), &tx.signature) {
            return Err(ChainError::BadSignature(tx.author));
        }
        if acct.balance < tx.stake {
            return Err(ChainError::InsufficientBalance {
                account: tx.author,
                need: tx.stake,
                have: acct.balance,
            });
        }
        Ok(())
    }

    pub(crate) fn apply_tx(&mut self, tx: &SubmissionTx) -> Result<TxReceipt, ChainError> {
        // -- validate (never mutates; see validate_tx) -----------------------
        self.validate_tx(tx)?;
        // -- escrow stake ----------------------------------------------------
        {
            let acct = self.accounts.get_mut(&tx.author).unwrap();
            acct.balance -= tx.stake;
            acct.staked_total += tx.stake;
            acct.submissions += 1;
        }

        // -- ΔK via the shared B.2.3 contract --------------------------------
        let reviews_engine: Vec<(f32, f32)> = tx
            .reviews
            .iter()
            .map(|r| (*self.reviewers.get(&r.reviewer).unwrap(), r.score))
            .collect();
        let sub = Submission {
            embedding: tx.embedding,
            domain: tx.domain,
            timestamp_days: tx.timestamp_days,
        };
        let dk = compute_delta_k(
            &sub,
            &self.graph,
            &reviews_engine,
            (tx.repl_success, tx.repl_total),
            &self.params,
            self.now_days,
        );

        // -- finalize: mint or slash ----------------------------------------
        let (minted, slashed, accepted);
        if dk > 0.0 {
            let reward = ((self.base_emission_micro as f64) * (dk as f64)).round() as u64;
            {
                let acct = self.accounts.get_mut(&tx.author).unwrap();
                acct.balance += tx.stake + reward; // escrow returned + reward
                acct.earned_total += reward;
                acct.accepted += 1;
            }
            self.supply += reward;
            self.graph.add(GraphNode {
                embedding: tx.embedding,
                domain: tx.domain,
            });
            self.reward_reviewers(&tx.reviews, true);
            minted = reward;
            slashed = 0;
            accepted = true;
        } else {
            let slash = ((tx.stake as u128 * self.slash_bps as u128) / 10_000) as u64;
            {
                let acct = self.accounts.get_mut(&tx.author).unwrap();
                acct.balance += tx.stake - slash; // remainder returned
                acct.slashed_total += slash;
            }
            self.treasury += slash; // redistributed, not burned (supply-neutral)
            self.reward_reviewers(&tx.reviews, false);
            minted = 0;
            slashed = slash;
            accepted = false;
        }

        Ok(TxReceipt {
            author: tx.author,
            accepted,
            delta_k: dk,
            minted,
            slashed,
        })
    }

    /// Outcome-based reputation update: on-chain we cannot see "true quality",
    /// only the finalized decision. Reviewers who scored high on an accepted
    /// item gain; reviewers who scored high on a rejected item lose.
    fn reward_reviewers(&mut self, reviews: &[Review], accepted: bool) {
        for r in reviews {
            if let Some(rep) = self.reviewers.get_mut(&r.reviewer) {
                let delta = if accepted {
                    if r.score > 0.6 { 0.02 } else { -0.005 }
                } else if r.score > 0.6 {
                    -0.03
                } else {
                    0.01
                };
                *rep = (*rep + delta).max(0.05);
            }
        }
    }

    /// Deterministic state root: SHA-256 over a canonical digest of all state.
    pub fn state_root(&self) -> Hash {
        let mut e = codec::Enc(Vec::new());
        e.u64(self.height);
        e.u64(self.supply);
        e.u64(self.treasury);
        e.u64(self.accounts.len() as u64);
        for (id, a) in &self.accounts {
            e.u64(*id);
            e.raw(&a.pubkey);
            e.u64(a.balance);
            e.u64(a.staked_total);
            e.u64(a.earned_total);
            e.u64(a.slashed_total);
            e.u64(a.submissions);
            e.u64(a.accepted);
        }
        e.u64(self.reviewers.len() as u64);
        for (id, rep) in &self.reviewers {
            e.u64(*id);
            e.f32(*rep);
        }
        e.u64(self.graph.len() as u64);
        for n in &self.graph.nodes {
            e.emb(&n.embedding);
            e.u32(n.domain);
        }
        // validator set is consensus state: fold it into the root so a divergent
        // set (e.g. a missed on-chain update) yields a different state_root.
        let vs = self.validators.validators();
        e.u64(vs.len() as u64);
        for v in vs {
            e.u64(v.id);
            e.raw(&v.pubkey);
            e.u64(v.power);
        }
        sha256(&e.0)
    }

    /// Authenticated state root: a Merkle commitment to the same accounts
    /// field as `state_root`, but in the form of a binary tree whose leaves
    /// can be opened individually. A light client holds only this root and can
    /// verify any single `Account` (or reviewer entry) it knows by id.
    ///
    /// Uses the same canonical `codec::Enc` byte layout for each leaf so the
    /// Merkle root is content-addressed in lockstep with `state_root`: a change
    /// to any field flips both, but a change in the *encoding* would flip the
    /// Merkle root only and break the proof.
    pub fn merkle_root(&self) -> Hash {
        merkle::MerkleTree::from_leaf_hashes(self.merkle_leaves()).root()
    }

    /// Build an inclusion proof for `account_id` against [`Self::merkle_root`].
    /// Returns `None` if the id is unknown. Leaves are laid out with all
    /// accounts first, then reviewers, in `BTreeMap` order (deterministic).
    pub fn account_proof(&self, account_id: u64) -> Option<merkle::Proof> {
        let ids: Vec<u64> = self.accounts.keys().copied().collect();
        let index = ids.iter().position(|&k| k == account_id)?;
        merkle::MerkleTree::from_leaf_hashes(self.merkle_leaves()).proof(index)
    }

    /// Internal: collect each accounts/reviewers entry as a domain-separated
    /// leaf hash, in canonical BTreeMap order.
    fn merkle_leaves(&self) -> Vec<Hash> {
        let mut leaves = Vec::with_capacity(self.accounts.len() + self.reviewers.len());
        for (id, a) in &self.accounts {
            leaves.push(merkle::leaf_hash(&a.merkle_leaf(*id)));
        }
        for (id, rep) in &self.reviewers {
            let mut e = codec::Enc(Vec::new());
            e.u64(*id);
            e.f32(*rep);
            leaves.push(merkle::leaf_hash(&e.0));
        }
        leaves
    }

    /// Accounting invariant: every micro-$COG is either in an account balance or
    /// in the treasury (stake escrow is always resolved within a tx). Should
    /// hold after any sequence of blocks.
    pub fn supply_conserved(&self) -> bool {
        let held: u128 =
            self.accounts.values().map(|a| a.balance as u128).sum::<u128>() + self.treasury as u128;
        held == self.supply as u128
    }
}

// --- Chain: hash-linked sequence of blocks over the state --------------------

pub struct Chain {
    pub state: ChainState,
    pub head: Hash,
    pub genesis_hash: Hash,
    pub block_hashes: Vec<Hash>,
}

impl Chain {
    pub fn new(g: Genesis) -> Self {
        let (state, gh) = ChainState::genesis(g);
        Chain {
            state,
            head: gh,
            genesis_hash: gh,
            block_hashes: vec![gh],
        }
    }

    /// Validate and commit a block atomically: the block must extend `head`, and
    /// the whole block is applied on a trial clone so a single invalid tx rolls
    /// the entire block back (no partial state).
    pub fn commit(&mut self, block: &Block) -> Result<BlockReceipt, ChainError> {
        if block.prev_hash != self.head {
            return Err(ChainError::BadPrevHash);
        }
        let mut trial = self.state.clone();
        let receipt = trial.apply_block(block)?;
        self.state = trial;
        self.head = receipt.hash;
        self.block_hashes.push(receipt.hash);
        Ok(receipt)
    }

    /// Rebuild a chain by replaying `blocks` on top of `genesis` (e.g. from a
    /// [`store::BlockLog`]). Each block is validated exactly as if freshly
    /// committed, so a tampered log fails here rather than corrupting state.
    pub fn replay(genesis: Genesis, blocks: &[Block]) -> Result<Self, ChainError> {
        let mut chain = Chain::new(genesis);
        for b in blocks {
            chain.commit(b)?;
        }
        Ok(chain)
    }

    /// Replay `blocks` *and re-verify finality*: for each height the accompanying
    /// certificate in `certs` must be a valid > 2/3 quorum (`Commit::verify`)
    /// that binds exactly this block (matching height and hash), before the block
    /// is applied. Where [`Self::replay`] recovers deterministic *state*, this
    /// recovers *finality* — a restarted node (or a following light client)
    /// re-establishes that every block was finalized by a super-majority, not
    /// merely that it re-derives the same bytes. A dropped, swapped, or forged
    /// certificate is rejected here even though the block itself is well-formed.
    ///
    /// The validator set is **not** a caller-supplied constant: it is consensus
    /// state that lives in the chain and evolves on-chain. Each block's
    /// certificate is checked against the set *active for that height* — the set
    /// in force before the block is applied — and applying the block may itself
    /// change the set for the next height (see [`Block::validator_updates`]). So
    /// replay follows validator handoffs exactly as the live chain produced them.
    pub fn replay_verified(
        genesis: Genesis,
        blocks: &[Block],
        certs: &[consensus::Commit],
    ) -> Result<Self, ReplayError> {
        if blocks.len() != certs.len() {
            return Err(ReplayError::CountMismatch {
                blocks: blocks.len(),
                certs: certs.len(),
            });
        }
        let mut chain = Chain::new(genesis);
        for (b, c) in blocks.iter().zip(certs.iter()) {
            // the certificate must finalize *this* block, not some other one
            if c.height != b.height || c.block_hash != b.hash() {
                return Err(ReplayError::CertificateMismatch { height: b.height });
            }
            // ...and be a real super-majority under the set active for this
            // height (before committing, which may change it for the next one)
            c.verify(&chain.state.validators)
                .map_err(ReplayError::Consensus)?;
            chain.commit(b).map_err(ReplayError::Chain)?;
        }
        Ok(chain)
    }
}

// --- canonical byte encoder lives in `codec` (shared by hashing + persistence)

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(x: f32, d: usize) -> Embedding {
        let mut e = [0.0f32; DIM];
        e[d] = x;
        e
    }

    /// Deterministic test keypair for account `id`.
    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
    }

    fn base_genesis() -> Genesis {
        Genesis {
            accounts: vec![
                (1, 30 * MICRO, kp(1).public()),
                (2, 30 * MICRO, kp(2).public()),
                (3, 30 * MICRO, kp(3).public()),
            ],
            reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
            seed_nodes: vec![(unit(1.0, 0), 0)], // domain 0 already occupied
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

    fn good_reviews() -> Vec<Review> {
        vec![
            Review { reviewer: 10, score: 0.9 },
            Review { reviewer: 11, score: 0.85 },
            Review { reviewer: 12, score: 0.9 },
        ]
    }

    fn novel_tx(author: u64, domain: u32, dim: usize, day: f32) -> SubmissionTx {
        SubmissionTx {
            author,
            embedding: unit(1.0, dim),
            domain,
            stake: 2 * MICRO,
            reviews: good_reviews(),
            repl_success: 3,
            repl_total: 3,
            timestamp_days: day,
            signature: [0u8; 64],
        }
        .signed(&kp(author))
    }

    fn block(chain: &Chain, height: u64, txs: Vec<SubmissionTx>) -> Block {
        Block {
            height,
            prev_hash: chain.head,
            timestamp_days: height as f32,
            txs,
            validator_updates: Vec::new(),
        }
    }

    #[test]
    fn novel_submission_mints_and_conserves_supply() {
        let mut chain = Chain::new(base_genesis());
        let start_supply = chain.state.supply;
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]); // fresh domain 1
        let r = chain.commit(&b).unwrap();
        assert_eq!(r.accepted, 1);
        assert!(r.minted > 0);
        assert!(chain.state.supply > start_supply); // reward minted
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn near_duplicate_is_slashed_to_treasury() {
        let mut chain = Chain::new(base_genesis());
        // domain 0 already has unit(1.0,0); resubmit the same -> novelty 0 -> ΔK 0
        let dup = SubmissionTx {
            author: 1,
            embedding: unit(1.0, 0),
            domain: 0,
            stake: 2 * MICRO,
            reviews: good_reviews(),
            repl_success: 3,
            repl_total: 3,
            timestamp_days: 1.0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let b = block(&chain, 1, vec![dup]);
        let r = chain.commit(&b).unwrap();
        assert_eq!(r.rejected, 1);
        assert_eq!(r.minted, 0);
        assert_eq!(chain.state.treasury, 2 * MICRO); // whole stake slashed
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn deterministic_replay_same_state_root() {
        let build = || {
            let mut chain = Chain::new(base_genesis());
            let b1 = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
            chain.commit(&b1).unwrap();
            let b2 = block(&chain, 2, vec![novel_tx(2, 2, 2, 2.0)]);
            chain.commit(&b2).unwrap();
            chain
        };
        let a = build();
        let b = build();
        assert_eq!(a.head, b.head);
        assert_eq!(a.state.state_root(), b.state.state_root());
    }

    #[test]
    fn tampering_a_tx_changes_the_block_hash() {
        let chain = Chain::new(base_genesis());
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        let h1 = b.hash();
        let mut b2 = b.clone();
        b2.txs[0].stake += 1;
        assert_ne!(h1, b2.hash());
    }

    #[test]
    fn wrong_prev_hash_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.prev_hash = [9u8; 32];
        assert!(matches!(chain.commit(&b), Err(ChainError::BadPrevHash)));
    }

    #[test]
    fn invalid_tx_rolls_back_whole_block() {
        let mut chain = Chain::new(base_genesis());
        let root_before = chain.state.state_root();
        // second tx references unknown account -> whole block must roll back
        let bad = SubmissionTx {
            author: 999,
            ..novel_tx(1, 3, 3, 1.0)
        };
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0), bad]);
        assert!(chain.commit(&b).is_err());
        assert_eq!(chain.state.state_root(), root_before); // unchanged
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn cannot_stake_more_than_balance() {
        let mut chain = Chain::new(base_genesis());
        // re-sign after raising the stake, so it reaches the balance check
        let broke = SubmissionTx {
            stake: 1_000 * MICRO,
            ..novel_tx(1, 1, 1, 1.0)
        }
        .signed(&kp(1));
        let b = block(&chain, 1, vec![broke]);
        assert!(matches!(
            chain.commit(&b),
            Err(ChainError::InsufficientBalance { .. })
        ));
    }

    #[test]
    fn forged_signature_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        // account 1's submission signed by account 2's key
        let forged = SubmissionTx {
            author: 1,
            ..novel_tx(1, 1, 1, 1.0)
        }
        .signed(&kp(2));
        let b = block(&chain, 1, vec![forged]);
        assert!(matches!(chain.commit(&b), Err(ChainError::BadSignature(1))));
    }

    #[test]
    fn tampering_a_signed_field_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut tx = novel_tx(1, 1, 1, 1.0); // validly signed
        tx.stake += 1; // mutate after signing -> signature no longer matches
        let b = block(&chain, 1, vec![tx]);
        assert!(matches!(chain.commit(&b), Err(ChainError::BadSignature(1))));
    }

    #[test]
    fn merkle_root_authenticates_an_account_via_inclusion_proof() {
        let mut chain = Chain::new(base_genesis());
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        chain.commit(&b).unwrap();

        let root = chain.state.merkle_root();
        // a light client is told account 1's contents and given a proof
        let acct = chain.state.accounts.get(&1).unwrap().clone();
        let proof = chain.state.account_proof(1).unwrap();
        let leaf = merkle::leaf_hash(&acct.merkle_leaf(1));
        assert!(merkle::verify(&root, &leaf, &proof));
    }

    #[test]
    fn a_tampered_account_value_fails_the_proof() {
        let chain = Chain::new(base_genesis());
        let root = chain.state.merkle_root();
        let proof = chain.state.account_proof(2).unwrap();
        // claim a fatter balance than the state actually commits to
        let mut lying = chain.state.accounts.get(&2).unwrap().clone();
        lying.balance += 1_000 * MICRO;
        let leaf = merkle::leaf_hash(&lying.merkle_leaf(2));
        assert!(!merkle::verify(&root, &leaf, &proof));
    }

    #[test]
    fn proof_against_a_stale_root_fails_after_state_changes() {
        let mut chain = Chain::new(base_genesis());
        let old_root = chain.state.merkle_root();
        let acct1 = chain.state.accounts.get(&1).unwrap().clone();
        let old_proof = chain.state.account_proof(1).unwrap();
        assert!(merkle::verify(
            &old_root,
            &merkle::leaf_hash(&acct1.merkle_leaf(1)),
            &old_proof
        ));

        // account 1 mints; its leaf (and the root) move
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        chain.commit(&b).unwrap();
        let new_root = chain.state.merkle_root();
        assert_ne!(old_root, new_root);
        // the old (id,account,proof) no longer verifies against the new root
        assert!(!merkle::verify(
            &new_root,
            &merkle::leaf_hash(&acct1.merkle_leaf(1)),
            &old_proof
        ));
    }

    #[test]
    fn proof_for_unknown_account_is_none() {
        let chain = Chain::new(base_genesis());
        assert!(chain.state.account_proof(999).is_none());
    }

    fn vupd(id: u64, power: u64) -> ValidatorUpdate {
        ValidatorUpdate { id, pubkey: kp(id).public(), power }
    }

    #[test]
    fn genesis_seeds_the_validator_set_as_state() {
        let chain = Chain::new(base_genesis());
        let ids: Vec<u64> = chain
            .state
            .validators
            .validators()
            .iter()
            .map(|v| v.id)
            .collect();
        assert_eq!(ids, vec![21, 22, 23]);
        assert_eq!(chain.state.validators.total_power(), 3);
    }

    #[test]
    fn a_validator_update_takes_effect_next_height() {
        let mut chain = Chain::new(base_genesis());
        // a block that admits validator #24 (alongside a normal submission)
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.validator_updates = vec![vupd(24, 1)];
        chain.commit(&b).unwrap();
        let ids: Vec<u64> = chain
            .state
            .validators
            .validators()
            .iter()
            .map(|v| v.id)
            .collect();
        assert_eq!(ids, vec![21, 22, 23, 24], "set grew after the block committed");
        assert_eq!(chain.state.validators.total_power(), 4);
    }

    #[test]
    fn state_root_covers_the_validator_set() {
        // two chains identical except for an on-chain validator change must have
        // different state roots — the set is consensus state, not metadata.
        let mut plain = Chain::new(base_genesis());
        let mut changed = Chain::new(base_genesis());
        let b_plain = block(&plain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        let mut b_changed = block(&changed, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b_changed.validator_updates = vec![vupd(24, 1)];
        plain.commit(&b_plain).unwrap();
        changed.commit(&b_changed).unwrap();
        assert_ne!(
            plain.state.state_root(),
            changed.state.state_root(),
            "a validator handoff moves the state root"
        );
    }

    #[test]
    fn a_block_cannot_empty_the_validator_set() {
        let mut chain = Chain::new(base_genesis());
        // remove every genesis validator in one block -> rejected, chain untouched
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.validator_updates = vec![vupd(21, 0), vupd(22, 0), vupd(23, 0)];
        assert!(matches!(
            chain.commit(&b),
            Err(ChainError::EmptyValidatorSet)
        ));
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
        assert_eq!(chain.state.validators.len(), 3);
    }

    #[test]
    fn persisted_log_replays_to_identical_state() {
        use crate::store::BlockLog;

        // build an in-memory chain and persist each block to a temp log
        let mut path = std::env::temp_dir();
        path.push(format!(
            "zhixing-replay-{}-{:?}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let log = BlockLog::open(&path).unwrap();

        let mut live = Chain::new(base_genesis());
        let b1 = block(&live, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        live.commit(&b1).unwrap();
        log.append(&b1).unwrap();
        let b2 = Block {
            height: 2,
            prev_hash: live.head,
            timestamp_days: 2.0,
            txs: vec![novel_tx(2, 2, 2, 2.0)],
            validator_updates: Vec::new(),
        };
        live.commit(&b2).unwrap();
        log.append(&b2).unwrap();

        // reopen the log, replay from genesis, and compare
        let blocks = BlockLog::open(&path).unwrap().read_all().unwrap();
        let replayed = Chain::replay(base_genesis(), &blocks).unwrap();

        assert_eq!(replayed.head, live.head);
        assert_eq!(replayed.state.state_root(), live.state.state_root());
        assert!(replayed.state.supply_conserved());
        std::fs::remove_file(&path).ok();
    }
}
