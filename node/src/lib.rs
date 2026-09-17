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
pub mod light;
pub mod mempool;
pub mod merkle;
pub mod net;
pub mod round;
pub mod store;
pub mod validator;

use std::collections::{BTreeMap, BTreeSet};

use zhixing_engine::{compute_delta_k, CognitiveGraph, DeltaKParams, GraphNode, Submission, DIM};

pub use consensus::{Vote, VoteType};
pub use crypto::{Keypair, PubKey, Sig};
pub use hash::{hex, sha256};
pub use light::{LightError, ValidatorTracker};
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

/// Number of heights a withdrawal stays locked in the unbonding queue after an
/// [`StakeOp`] unbond. During this window the funds have left the validator's
/// voting power but not yet returned to the account balance — the delay is what
/// keeps an exiting validator's stake reachable by slashing (a later milestone).
pub const UNBONDING_PERIOD: u64 = 3;

/// Bond adds to a validator's stake (and voting power); Unbond schedules a
/// delayed withdrawal of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BondKind {
    Bond,
    Unbond,
}

impl BondKind {
    pub fn tag(self) -> u8 {
        match self {
            BondKind::Bond => 0,
            BondKind::Unbond => 1,
        }
    }
    pub fn from_tag(t: u8) -> Option<BondKind> {
        match t {
            0 => Some(BondKind::Bond),
            1 => Some(BondKind::Unbond),
            _ => None,
        }
    }
}

/// A self-bond staking operation: account `account` bonds or unbonds `amount`
/// micro-$COG toward the validator whose id **is** `account`. Bonding moves the
/// funds from the account balance into the bonded pool and gives the validator
/// that much voting power (effective next height, like any validator-set change);
/// unbonding removes the power and parks the funds in the unbonding queue for
/// [`UNBONDING_PERIOD`] heights before they return to the balance. Ties consensus
/// weight to economic skin-in-the-game (whitepaper B.2.3 / §5): power is bonded
/// $COG, not an out-of-band constant.
#[derive(Clone, Debug)]
pub struct StakeOp {
    pub account: u64,
    pub kind: BondKind,
    pub amount: u64,
    /// ed25519 signature by `account`'s key over [`codec::stakeop_signing_bytes`].
    pub signature: Sig,
}

impl StakeOp {
    /// Sign this op's canonical fields with `kp` (the account's key).
    pub fn signed(mut self, kp: &Keypair) -> Self {
        self.signature = kp.sign(&codec::stakeop_signing_bytes(&self));
        self
    }

    /// Content-addressed hash over the full signed encoding.
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_stakeop(self))
    }
}

/// Cryptographic proof of validator equivocation: two conflicting precommit
/// votes from the same validator at the same `(height, round)` but for
/// *different* block hashes, each carrying a valid ed25519 signature by the
/// offender's pubkey. Together they show the validator double-signed (a BFT
/// safety violation). Submitters submit `SlashEvidence` in a block; the chain
/// applies it on receipt (moving bonded stake and any still-maturing unbonding
/// entry to the treasury, and removing the offender at the next height).
#[derive(Clone, Debug)]
pub struct SlashEvidence {
    pub vote_a: Vote,
    pub vote_b: Vote,
}

impl SlashEvidence {
    /// Structural sanity: same validator, height, round, both precommit, two
    /// distinct block hashes. Does *not* check signatures — the chain does that
    /// with the offender's pubkey when applying the evidence, so this remains a
    /// pure-data constructor usable in tests.
    pub fn is_well_formed(&self) -> bool {
        let a = &self.vote_a;
        let b = &self.vote_b;
        a.validator == b.validator
            && a.height == b.height
            && a.round == b.round
            && a.vote_type == VoteType::Precommit
            && b.vote_type == VoteType::Precommit
            && a.block_hash != b.block_hash
    }

    /// Content hash for gossip dedup. Canonical encoding includes both votes'
    /// signatures, so two semantically identical pieces of evidence hash to
    /// the same 32 bytes — a stable, collision-safe identity under
    /// signing-key uniqueness.
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_evidence(self))
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
    /// Bond/unbond staking operations carried by this block. Applied after the
    /// submissions; the validator-power changes they imply take effect from the
    /// *next* height (the same discipline as `validator_updates`). Empty in the
    /// common case.
    pub stake_ops: Vec<StakeOp>,
    /// On-chain equivocation evidence — pairs of conflicting precommit votes
    /// from the same validator at the same (height, round). Applied after the
    /// staking ops; an offender's bonded stake (and any still-maturing unbonding
    /// entry) is moved to the treasury, and the offender is removed from the
    /// active validator set at the *next* height (same cross-height rule as
    /// `stake_ops`). Empty in the honest case; populated only by blocks
    /// submitted in response to a caught double-sign. Evidence itself is part
    /// of the block hash, but its *effects* — reduced bonds, grown treasury —
    /// are what fold into `state_root`, so honest chains see no root change.
    pub slashing_evidence: Vec<SlashEvidence>,
}

impl Block {
    /// Content-addressed block hash over the canonical codec encoding (the same
    /// bytes the block is persisted as, see [`codec`]).
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_block(self))
    }
}

// --- State -------------------------------------------------------------------

/// A withdrawal in flight: `amount` micro-$COG unbonded by `account`, returning
/// to its balance once the chain reaches `mature_height`. Until then the funds
/// are neither in a balance nor in the validator's power — they sit here, still
/// part of `supply` (and, in a later milestone, still slashable).
#[derive(Clone, Debug)]
pub struct UnbondingEntry {
    pub account: u64,
    pub amount: u64,
    pub mature_height: u64,
}

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
    /// Total micro-$COG bonded as validator stake (backs voting power). Equals
    /// the sum of [`Self::bonds`]. Held out of balances but part of `supply`.
    pub bonded: u64,
    /// Currently bonded micro-$COG per validator id (== that validator's voting
    /// power, applied to the set from the next height). The source of truth for
    /// stake-derived power; a validator drops out when its bond reaches zero.
    pub bonds: BTreeMap<u64, u64>,
    /// Withdrawals in the unbonding delay window, awaiting return to balances.
    pub unbonding: Vec<UnbondingEntry>,
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
    /// A bond/unbond op with a zero amount (never meaningful).
    ZeroStake(u64),
    /// An unbond of more than the account currently has bonded.
    InsufficientBond { account: u64, need: u64, have: u64 },
    /// Slashing evidence is malformed, against a non-validator, or carries an
    /// invalid signature. The block is rejected; the offending validator id is
    /// returned for diagnostics.
    BadEquivocationEvidence(u64),
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
            ChainError::ZeroStake(a) => write!(f, "account {a} bond/unbond amount is zero"),
            ChainError::InsufficientBond { account, need, have } => write!(
                f,
                "account {account} cannot unbond {need} (has {have} bonded)"
            ),
            ChainError::BadEquivocationEvidence(v) => write!(
                f,
                "equivocation evidence against validator {v} is malformed, stale, or not signable by that validator"
            ),
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
    /// micro-$COG newly bonded, newly unbonded, and returned from matured
    /// unbonding this block (staking flow, for reporting).
    pub bonded: u64,
    pub unbonded: u64,
    pub released: u64,
    /// micro-$COG moved into the treasury by `slashing_evidence` this block
    /// (sum of bond + any still-maturing unbonding entry, for the offender).
    /// Distinct from `slashed`, which tracks submission-bad-score burns.
    pub slashed_to_treasury: u64,
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
            bonded: 0,
            bonds: BTreeMap::new(),
            unbonding: Vec::new(),
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
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
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
        let new_height = block.height;

        // release any unbonding withdrawals that mature at or before this height,
        // returning the funds to the account balance (before this block's own
        // unbonds are scheduled, so a same-block bond/unbond never matures early).
        let mut released = 0u64;
        let mut still_unbonding = Vec::with_capacity(self.unbonding.len());
        for e in std::mem::take(&mut self.unbonding) {
            if e.mature_height <= new_height {
                if let Some(a) = self.accounts.get_mut(&e.account) {
                    a.balance += e.amount;
                } else {
                    // account gone (cannot happen for a self-bond) — keep the
                    // money in the system by routing it to the treasury.
                    self.treasury += e.amount;
                }
                released += e.amount;
            } else {
                still_unbonding.push(e);
            }
        }
        self.unbonding = still_unbonding;

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

        // staking operations: move funds between balance / bonded pool / unbonding
        // queue, tracking which validators' power changed so we can evolve the set.
        let mut bonded_total = 0u64;
        let mut unbonded_total = 0u64;
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        for op in &block.stake_ops {
            match op.kind {
                BondKind::Bond => bonded_total += op.amount,
                BondKind::Unbond => unbonded_total += op.amount,
            }
            self.apply_stake_op(op, new_height)?;
            touched.insert(op.account);
        }

        // on-chain equivocation evidence: pair of conflicting precommits from
        // the same validator at the same (height, round). Slash the offender's
        // bonded stake (and any still-maturing unbonding entry) into the
        // treasury, and queue the offender for power-zero removal at the next
        // height by inserting into `touched` — the same discipline as stake
        // ops (validation fully precedes mutation, so a bad evidence rolls
        // the whole block back).
        let mut slashed_to_treasury_total = 0u64;
        for ev in &block.slashing_evidence {
            let moved = self.apply_evidence(ev)?;
            slashed_to_treasury_total += moved;
            touched.insert(ev.vote_a.validator);
        }

        // on-chain validator-set transition: explicit updates PLUS the changes
        // implied by this block's staking ops (power == bonded stake). Both take
        // effect from the NEXT height — this block was certified by the set in
        // force before it, so a newly-bonded validator never votes on its own
        // arrival. Guard against emptying the set (future consensus impossible).
        // Applied on a trial clone via `Chain::commit`, so any rejection here
        // rolls the whole block back.
        let mut updates = block.validator_updates.clone();
        for id in touched {
            let power = self.bonds.get(&id).copied().unwrap_or(0);
            let pubkey = self.accounts.get(&id).map(|a| a.pubkey).unwrap_or_default();
            updates.push(ValidatorUpdate { id, pubkey, power }); // power 0 == removal
        }
        if !updates.is_empty() {
            let next = self.validators.apply_updates(&updates);
            if next.is_empty() {
                return Err(ChainError::EmptyValidatorSet);
            }
            self.validators = next;
        }

        self.height = new_height;
        Ok(BlockReceipt {
            height: block.height,
            hash: block.hash(),
            minted: minted_total,
            slashed: slashed_total,
            accepted: n_accept,
            rejected: n_reject,
            bonded: bonded_total,
            unbonded: unbonded_total,
            released,
            slashed_to_treasury: slashed_to_treasury_total,
            txs: receipts,
        })
    }

    /// Apply one bond/unbond op at `height` (the height of the block carrying it).
    /// Bond escrows funds from the account balance into the bonded pool; unbond
    /// removes them from the pool and schedules a delayed withdrawal. Never
    /// partially mutates on error (all checks precede any mutation), so a failing
    /// op rolls the whole block back cleanly.
    fn apply_stake_op(&mut self, op: &StakeOp, height: u64) -> Result<(), ChainError> {
        let acct = self
            .accounts
            .get(&op.account)
            .ok_or(ChainError::UnknownAccount(op.account))?;
        if !crypto::verify(&acct.pubkey, &codec::stakeop_signing_bytes(op), &op.signature) {
            return Err(ChainError::BadSignature(op.account));
        }
        if op.amount == 0 {
            return Err(ChainError::ZeroStake(op.account));
        }
        match op.kind {
            BondKind::Bond => {
                if acct.balance < op.amount {
                    return Err(ChainError::InsufficientBalance {
                        account: op.account,
                        need: op.amount,
                        have: acct.balance,
                    });
                }
                self.accounts.get_mut(&op.account).unwrap().balance -= op.amount;
                *self.bonds.entry(op.account).or_insert(0) += op.amount;
                self.bonded += op.amount;
            }
            BondKind::Unbond => {
                let cur = self.bonds.get(&op.account).copied().unwrap_or(0);
                if cur < op.amount {
                    return Err(ChainError::InsufficientBond {
                        account: op.account,
                        need: op.amount,
                        have: cur,
                    });
                }
                if cur == op.amount {
                    self.bonds.remove(&op.account);
                } else {
                    self.bonds.insert(op.account, cur - op.amount);
                }
                self.bonded -= op.amount;
                self.unbonding.push(UnbondingEntry {
                    account: op.account,
                    amount: op.amount,
                    mature_height: height + UNBONDING_PERIOD,
                });
            }
        }
        Ok(())
    }

    /// Apply one equivocation evidence: validate the pair of conflicting
    /// precommits against the offender's *active-validator* pubkey, then move
    /// the offender's bonded stake (and any still-maturing unbonding entry)
    /// into the treasury. Returns the amount routed to the treasury. The
    /// offender's removal from the validator set at the next height is done by
    /// the caller's derived-`ValidatorUpdate` step (power 0 == removal). All
    /// checks precede any mutation, so bad evidence rolls the whole block back.
    fn apply_evidence(&mut self, ev: &SlashEvidence) -> Result<u64, ChainError> {
        // 1. structural sanity — same validator, height, round, both precommit,
        //    two different block hashes.
        if !ev.is_well_formed() {
            return Err(ChainError::BadEquivocationEvidence(ev.vote_a.validator));
        }
        let id = ev.vote_a.validator;
        // 2. the offender must be an active validator (we need their pubkey to
        //    verify the signatures, and only active validators carry stake to
        //    slash). Evidence against anyone else (not in the set, or already
        //    removed) is rejected — same discipline as a malformed stake op.
        let val = self
            .validators
            .get(id)
            .ok_or(ChainError::BadEquivocationEvidence(id))?;
        let pubkey = val.pubkey;
        // 3. both vote signatures must verify against that pubkey — without
        //    this, anyone could forge a "double-sign" against an innocent id.
        let sig_a = consensus::vote_signing_bytes(
            ev.vote_a.validator,
            ev.vote_a.height,
            ev.vote_a.round,
            &ev.vote_a.block_hash,
            ev.vote_a.vote_type,
        );
        let sig_b = consensus::vote_signing_bytes(
            ev.vote_b.validator,
            ev.vote_b.height,
            ev.vote_b.round,
            &ev.vote_b.block_hash,
            ev.vote_b.vote_type,
        );
        if !crypto::verify(&pubkey, &sig_a, &ev.vote_a.signature)
            || !crypto::verify(&pubkey, &sig_b, &ev.vote_b.signature)
        {
            return Err(ChainError::BadEquivocationEvidence(id));
        }
        // 4. slash — bond pool first, then any still-maturing unbonding entry
        //    (still slashable until its `mature_height`; this is the whole
        //    reason M17 has an unbonding window). All moved to the treasury, so
        //    supply stays conserved.
        let mut moved = 0u64;
        if let Some(amt) = self.bonds.remove(&id) {
            self.bonded -= amt;
            moved += amt;
        }
        let mut still = Vec::with_capacity(self.unbonding.len());
        for e in std::mem::take(&mut self.unbonding) {
            if e.account == id {
                moved += e.amount;
            } else {
                still.push(e);
            }
        }
        self.unbonding = still;
        self.treasury += moved;
        Ok(moved)
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
        // staking state: the bonded pool, per-validator bonds, and the unbonding
        // queue are all consensus state and must move the root.
        e.u64(self.bonded);
        e.u64(self.bonds.len() as u64);
        for (id, amt) in &self.bonds {
            e.u64(*id);
            e.u64(*amt);
        }
        e.u64(self.unbonding.len() as u64);
        for u in &self.unbonding {
            e.u64(u.account);
            e.u64(u.amount);
            e.u64(u.mature_height);
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

    /// Accounting invariant: every micro-$COG is in an account balance, in the
    /// treasury, in the bonded pool, or in the unbonding queue (stake escrow for a
    /// submission is always resolved within a tx). Should hold after any sequence
    /// of blocks.
    pub fn supply_conserved(&self) -> bool {
        let unbonding: u128 = self.unbonding.iter().map(|u| u.amount as u128).sum();
        let held: u128 = self.accounts.values().map(|a| a.balance as u128).sum::<u128>()
            + self.treasury as u128
            + self.bonded as u128
            + unbonding;
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
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
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

    // ---- staking-bound validator power + unbonding (M17) ----

    /// Build a block carrying a single signed bond/unbond op (no txs).
    fn stake_block(chain: &Chain, height: u64, account: u64, kind: BondKind, amount: u64) -> Block {
        let op = StakeOp { account, kind, amount, signature: [0u8; 64] }.signed(&kp(account));
        let mut b = block(chain, height, vec![]);
        b.stake_ops = vec![op];
        b
    }

    #[test]
    fn bonding_makes_an_account_a_validator_next_height() {
        let mut chain = Chain::new(base_genesis());
        let bal_before = chain.state.accounts[&1].balance;
        let b = stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO);
        let r = chain.commit(&b).unwrap();
        assert_eq!(r.bonded, 5 * MICRO);
        // funds left the balance for the bonded pool (still part of supply)
        assert_eq!(chain.state.accounts[&1].balance, bal_before - 5 * MICRO);
        assert_eq!(chain.state.bonded, 5 * MICRO);
        assert_eq!(chain.state.bonds.get(&1), Some(&(5 * MICRO)));
        // account 1 is now a validator whose power == its bonded stake
        assert_eq!(chain.state.validators.get(1).map(|v| v.power), Some(5 * MICRO));
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn unbond_schedules_a_delayed_withdrawal_that_matures() {
        let mut chain = Chain::new(base_genesis());
        chain.commit(&stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO)).unwrap();
        let bal_after_bond = chain.state.accounts[&1].balance;

        // unbond at height 2: power drops immediately (next height), funds locked
        let r = chain.commit(&stake_block(&chain, 2, 1, BondKind::Unbond, 5 * MICRO)).unwrap();
        assert_eq!(r.unbonded, 5 * MICRO);
        assert_eq!(chain.state.bonded, 0);
        assert!(chain.state.validators.get(1).is_none(), "validator removed at power 0");
        assert_eq!(chain.state.accounts[&1].balance, bal_after_bond, "funds still locked");
        assert_eq!(chain.state.unbonding.len(), 1);
        assert_eq!(chain.state.unbonding[0].mature_height, 2 + UNBONDING_PERIOD);
        assert!(chain.state.supply_conserved());

        // advance empty blocks until the withdrawal matures
        while chain.state.height < 2 + UNBONDING_PERIOD {
            let h = chain.state.height + 1;
            let b = block(&chain, h, vec![]);
            chain.commit(&b).unwrap();
        }
        assert!(chain.state.unbonding.is_empty(), "matured out of the queue");
        assert_eq!(chain.state.accounts[&1].balance, bal_after_bond + 5 * MICRO, "funds returned");
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn bond_beyond_balance_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let b = stake_block(&chain, 1, 1, BondKind::Bond, 1000 * MICRO);
        assert!(matches!(chain.commit(&b), Err(ChainError::InsufficientBalance { .. })));
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
        assert!(chain.state.bonds.is_empty());
        assert_eq!(chain.state.bonded, 0);
    }

    #[test]
    fn unbond_beyond_bond_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let b = stake_block(&chain, 1, 1, BondKind::Unbond, MICRO);
        assert!(matches!(chain.commit(&b), Err(ChainError::InsufficientBond { .. })));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn forged_stakeop_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        // account 1's bond signed by account 2's key
        let op = StakeOp { account: 1, kind: BondKind::Bond, amount: MICRO, signature: [0u8; 64] }
            .signed(&kp(2));
        let mut b = block(&chain, 1, vec![]);
        b.stake_ops = vec![op];
        assert!(matches!(chain.commit(&b), Err(ChainError::BadSignature(1))));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn zero_amount_stakeop_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let b = stake_block(&chain, 1, 1, BondKind::Bond, 0);
        assert!(matches!(chain.commit(&b), Err(ChainError::ZeroStake(1))));
    }

    #[test]
    fn state_root_covers_bonded_stake() {
        let mut plain = Chain::new(base_genesis());
        let mut bonded = Chain::new(base_genesis());
        plain.commit(&block(&plain, 1, vec![novel_tx(1, 1, 1, 1.0)])).unwrap();
        let mut b = block(&bonded, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.stake_ops = vec![StakeOp { account: 2, kind: BondKind::Bond, amount: 3 * MICRO, signature: [0u8; 64] }.signed(&kp(2))];
        bonded.commit(&b).unwrap();
        assert_ne!(plain.state.state_root(), bonded.state.state_root());
    }

    #[test]
    fn a_full_bond_unbond_cycle_conserves_supply() {
        let mut chain = Chain::new(base_genesis());
        let start = chain.state.supply;
        chain.commit(&stake_block(&chain, 1, 1, BondKind::Bond, 7 * MICRO)).unwrap();
        assert!(chain.state.supply_conserved());
        chain.commit(&stake_block(&chain, 2, 1, BondKind::Unbond, 4 * MICRO)).unwrap();
        assert!(chain.state.supply_conserved());
        // still bonded 3, unbonding 4, balance rest — supply unchanged throughout
        assert_eq!(chain.state.bonded, 3 * MICRO);
        assert_eq!(chain.state.supply, start);
        assert!(chain.state.supply_conserved());
    }

    // ---- on-chain equivocation evidence + slashing (M18) ----

    /// Two conflicting precommits from `offender` at (`height`, `round`), each
    /// correctly signed by that validator's own key — valid double-sign evidence.
    fn evidence(offender: u64, height: u64, round: u32) -> SlashEvidence {
        SlashEvidence {
            vote_a: Vote::signed(offender, height, round, [1u8; 32], VoteType::Precommit, &kp(offender)),
            vote_b: Vote::signed(offender, height, round, [2u8; 32], VoteType::Precommit, &kp(offender)),
        }
    }

    /// A block carrying slashing evidence (no txs, no stake ops).
    fn evidence_block(chain: &Chain, height: u64, ev: Vec<SlashEvidence>) -> Block {
        let mut b = block(chain, height, vec![]);
        b.slashing_evidence = ev;
        b
    }

    #[test]
    fn slashing_burns_bonded_stake_to_treasury_and_removes_validator() {
        let mut chain = Chain::new(base_genesis());
        let start = chain.state.supply;
        // account 1 self-bonds and becomes a validator effective height 2
        chain.commit(&stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO)).unwrap();
        assert_eq!(chain.state.validators.get(1).map(|v| v.power), Some(5 * MICRO));

        // at height 2 the validator is active — submit proof it double-signed
        let r = chain.commit(&evidence_block(&chain, 2, vec![evidence(1, 2, 0)])).unwrap();
        assert_eq!(r.slashed_to_treasury, 5 * MICRO);
        assert_eq!(chain.state.treasury, 5 * MICRO, "bonded stake seized to treasury");
        assert_eq!(chain.state.bonded, 0);
        assert!(!chain.state.bonds.contains_key(&1));
        assert!(chain.state.validators.get(1).is_none(), "offender removed from the set");
        assert_eq!(chain.state.supply, start, "slash is supply-neutral");
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn slashing_also_seizes_a_maturing_unbonding_entry() {
        let mut chain = Chain::new(base_genesis());
        // bond 6, partially unbond 2 (leaving power 4 so the validator stays active),
        // then slash: both the remaining bond and the still-maturing entry are seized.
        chain.commit(&stake_block(&chain, 1, 1, BondKind::Bond, 6 * MICRO)).unwrap();
        chain.commit(&stake_block(&chain, 2, 1, BondKind::Unbond, 2 * MICRO)).unwrap();
        assert_eq!(chain.state.bonded, 4 * MICRO);
        assert_eq!(chain.state.unbonding.len(), 1);
        assert_eq!(chain.state.validators.get(1).map(|v| v.power), Some(4 * MICRO));

        let r = chain.commit(&evidence_block(&chain, 3, vec![evidence(1, 3, 0)])).unwrap();
        assert_eq!(r.slashed_to_treasury, 6 * MICRO, "bond + unbonding both seized");
        assert_eq!(chain.state.treasury, 6 * MICRO);
        assert_eq!(chain.state.bonded, 0);
        assert!(chain.state.unbonding.is_empty(), "maturing entry seized too");
        assert!(chain.state.validators.get(1).is_none());
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn slashing_a_genesis_validator_removes_it_without_moving_money() {
        let mut chain = Chain::new(base_genesis());
        // genesis validator 21 has power but no bonded stake — slashing removes it
        // and moves nothing (still supply-neutral).
        let r = chain.commit(&evidence_block(&chain, 1, vec![evidence(21, 1, 0)])).unwrap();
        assert_eq!(r.slashed_to_treasury, 0);
        assert_eq!(chain.state.treasury, 0);
        assert!(chain.state.validators.get(21).is_none());
        assert_eq!(chain.state.validators.len(), 2, "22 and 23 remain");
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn malformed_evidence_is_rejected_and_rolls_back() {
        let mut chain = Chain::new(base_genesis());
        // both votes name the same block hash -> not a conflict -> malformed
        let ev = SlashEvidence {
            vote_a: Vote::signed(21, 1, 0, [1u8; 32], VoteType::Precommit, &kp(21)),
            vote_b: Vote::signed(21, 1, 0, [1u8; 32], VoteType::Precommit, &kp(21)),
        };
        let b = evidence_block(&chain, 1, vec![ev]);
        assert!(matches!(chain.commit(&b), Err(ChainError::BadEquivocationEvidence(21))));
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
        assert_eq!(chain.state.validators.len(), 3);
    }

    #[test]
    fn evidence_against_a_non_validator_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        // account 1 never bonded -> not in the validator set -> cannot be slashed
        let b = evidence_block(&chain, 1, vec![evidence(1, 1, 0)]);
        assert!(matches!(chain.commit(&b), Err(ChainError::BadEquivocationEvidence(1))));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn forged_evidence_signature_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        // conflicting votes attributed to validator 21 but signed by account 1's key
        let ev = SlashEvidence {
            vote_a: Vote::signed(21, 1, 0, [1u8; 32], VoteType::Precommit, &kp(1)),
            vote_b: Vote::signed(21, 1, 0, [2u8; 32], VoteType::Precommit, &kp(1)),
        };
        let b = evidence_block(&chain, 1, vec![ev]);
        assert!(matches!(chain.commit(&b), Err(ChainError::BadEquivocationEvidence(21))));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn slashing_cannot_empty_the_validator_set() {
        let mut chain = Chain::new(base_genesis());
        // proof against every genesis validator in one block -> would empty the
        // set -> rejected, chain untouched.
        let b = evidence_block(
            &chain,
            1,
            vec![evidence(21, 1, 0), evidence(22, 1, 0), evidence(23, 1, 0)],
        );
        assert!(matches!(chain.commit(&b), Err(ChainError::EmptyValidatorSet)));
        assert_eq!(chain.state.height, 0);
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
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
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
