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

pub mod bridge;
pub mod codec;
pub mod config;
pub mod consensus;
pub mod crypto;
pub mod daemon;
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

use zhixing_engine::{compute_delta_k, CognitiveGraph, DeltaKParams, Submission, DIM};

pub use consensus::{Vote, VoteType};
pub use crypto::{Keypair, PubKey, Sig};
pub use hash::{hex, sha256};
pub use light::{LightError, ValidatorTracker};
// M25: re-export the engine module so `crate::engine::GraphNode` works from
// codec.rs / light.rs without a direct engine dependency in those modules.
pub mod engine {
    pub use zhixing_engine::*;
}
use validator::{Validator, ValidatorSet, ValidatorUpdate};

/// 1 $COG == 1_000_000 micro-$COG. All balances are integer micro-$COG.
pub const MICRO: u64 = 1_000_000;

pub type Hash = [u8; 32];
pub type Embedding = [f32; DIM];

/// M31: one bridge-source registration in [`Genesis`] — a source chain's
/// `genesis_hash` plus the `(id, pubkey, power)` triples of its genesis
/// validator set (the trust anchor for redeems minted from that source).
pub type BridgeSourceSeed = (Hash, Vec<(u64, PubKey, u64)>);

// M27: the canonical reference pivot for the cert-signed secondary graph
// index. The first standard basis vector — a unit vector along axis 0 —
// is a deterministic, query-independent choice. All peers sort graph
// nodes by `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)`,
// so the cert-signed `header.graph_root` commits to one canonical order
// independent of any wallet's query. The user's query drives the *cut*;
// the pivot only fixes the *index*. Lives here (not in the engine) because
// it's a verifier/wallet-side commitment choice, not a math primitive.
const CANONICAL_PIVOT: Embedding = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

/// M27: typed producer-side return for [`ChainState::graph_range_proof`] —
/// the sub-root of the cert-signed sorted-graph tree at slice `[a, b)`,
/// plus the ordered list of `(node_id, GraphNode, merkle::Proof)` for that
/// slice. The wallet-side `RangeClaim` carries the same shape but drops
/// the sub-root (the verifier recomputes it from `header.graph_root`).
pub struct RangeProof {
    pub sub_root: Hash,
    pub entries: Vec<(u64, crate::engine::GraphNode, merkle::Proof)>,
}

/// M28: a single (node_id, graph_node, proof) tuple where the proof
/// verifies against the cert-signed `accounts_root` at *one* of the two
/// heights. Used by both sides of a temporal diff (`added` against
/// `header_h2.accounts_root`, `dropped` against `header_h1.accounts_root`).
#[derive(Clone, Debug)]
pub struct GraphLeafAtHeight {
    pub node_id: u64,
    pub graph_node: crate::engine::GraphNode,
    pub proof: merkle::Proof,
}

/// M28: the producer-side temporal graph diff between two cert-signed
/// heights `h1 < h2`. `added` are the node_ids present at h₂ but absent
/// at h₁ (each proven against `header_h2.accounts_root`); `dropped` are
/// the node_ids present at h₁ but absent at h₂ (each proven against
/// `header_h1.accounts_root`). The wallet's verifier re-derives both
/// sets from a partial replay of `[h₁+1..h₂]` and checks the prover's
/// claim against its own derivation; completeness is established by the
/// replay, not by the per-leaf proofs.
///
/// M25's invariant makes graph nodes immutable (monotonic `node_id`,
/// append-only `add`), so the third class of change — same id, different
/// embedding — is **structurally unreachable**. M28 deliberately does
/// not carry one.
///
/// Order: ascending by `node_id`. Same on the prover side (which sorts)
/// and the wallet side (which reads `state.graph.nodes` in insertion
/// order, which is the same order).
#[derive(Clone, Debug, Default)]
pub struct DiffClaim {
    pub added: Vec<GraphLeafAtHeight>,
    pub dropped: Vec<GraphLeafAtHeight>,
}

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

/// M30: a signed cross-chain lock op. An account on the **source** chain locks
/// `amount` micro-$COG destined for `dest_account` on `dest_chain` (identified
/// by its genesis hash). Apply drains the balance into `ChainState::bridge_locked`
/// and appends to the cumulative `bridge_locks` map so the lock survives across
/// heights and the cert-signed `bridge_root` can be opened against any height
/// ≥ creation. The destination chain verifies the lock via a `bridge::BridgeEndpoint`
/// (a light client + a replay-protected dedup set) — same shape as M22/M28
/// SPV, but on a different chain's cert-signed header.
///
/// On the destination side, consuming a verified lock mints new supply backed
/// 1:1 by the source's locked pool. The lock op itself only needs to commit
/// the source-side bookkeeping; the destination's mint is a bridge-module
/// concern (not consensus), which is why dedup lives in `bridge` not here.
#[derive(Clone, Debug)]
pub struct BridgeLock {
    pub account: u64,
    pub amount: u64,
    /// Destination chain's `genesis_hash`. A lock destined for any other
    /// chain is rejected by the destination endpoint (`BridgeError::WrongDestination`).
    pub dest_chain: Hash,
    pub dest_account: u64,
    /// Replay-protected nonce for `account` (mirrors the SubmissionTx /
    /// StakeOp discipline so the source chain can detect a re-signed op).
    pub nonce: u64,
    /// ed25519 signature by `account`'s key over [`codec::bridgelock_signing_bytes`].
    pub signature: Sig,
}

impl BridgeLock {
    /// Sign this op's canonical fields with `kp` (the account's key).
    pub fn signed(mut self, kp: &Keypair) -> Self {
        self.signature = kp.sign(&codec::bridgelock_signing_bytes(&self));
        self
    }

    /// Content-addressed hash over the full signed encoding. Used for dedup.
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_bridge_lock(self))
    }

    /// Canonical leaf preimage under the assigned `lock_id` — the exact
    /// bytes a light client hashes (via [`merkle::leaf_hash`]) to verify a
    /// lock-inclusion proof against `ChainState::bridge_root`. Mirrors
    /// `GraphNode::merkle_leaf` and `Account::merkle_leaf` for the
    /// bridge_root tree.
    pub fn merkle_leaf(&self, lock_id: u64) -> Vec<u8> {
        let mut e = codec::Enc(Vec::new());
        e.u64(lock_id);
        e.u64(self.account);
        e.u64(self.amount);
        e.raw(&self.dest_chain);
        e.u64(self.dest_account);
        e.u64(self.nonce);
        e.0
    }
}

/// M31: on-chain follower of one source chain — the destination-side analogue
/// of M30's off-chain `bridge::BridgeEndpoint::tracker`. Mirrors the shape of
/// `light::ValidatorTracker` (a cert-signed set + head + height) but stripped
/// of the bonds / pubkeys mirror because `follow_header` (M22) takes the next
/// set as a parameter rather than deriving it. Folded into `state_root`.
#[derive(Clone, Debug)]
pub struct BridgeSource {
    /// The active validator set that certifies the *next* source height
    /// (the set `apply_bridge_header` will check the next incoming cert
    /// against). Genesis-initialized to the source's declared genesis set.
    pub set: ValidatorSet,
    /// Hash of the last-followed source block. At genesis (height 0): the
    /// source chain's `genesis_hash` — so the first `BridgeHeader` must
    /// chain to it.
    pub head: Hash,
    /// Height of the last-followed source block. At genesis: 0.
    pub height: u64,
    /// On-chain replay dedup: which `lock_id`s from this source have already
    /// been redeemed. Doubly-keyed by the source chain's identity at the
    /// apply layer (the BTreeMap is keyed by `source_chain`).
    pub consumed: BTreeSet<u64>,
}

/// M31: self-authenticating op that advances `ChainState::bridge_sources` for
/// one source chain by a single cert-signed source header. Not account-signed
/// — the cert (a > 2/3 quorum of the previous source set) plus the next-set
/// root check are the proof. Mirrors `bridge::BridgeEndpoint::follow_source`
/// (M30) but consensus-enforced: a follow that fails the cert-binding check
/// rolls the whole block back.
#[derive(Clone, Debug)]
pub struct BridgeHeader {
    /// Identity of the source chain (its `genesis_hash`). Must be present
    /// in `ChainState::bridge_sources` — i.e. declared at our genesis.
    pub source_chain: Hash,
    /// The source chain's cert-signed header carrying the `bridge_root` the
    /// follow adopts. Same wire shape as the BlockHeader a `LockEnvelope`
    /// ships.
    pub header: codec::BlockHeader,
    /// The > 2/3 finality certificate binding `header.hash()`. Verified
    /// against the on-chain follower's *currently tracked* set, never the
    /// envelope's — so a relayer cannot substitute a validator set.
    pub cert: consensus::Commit,
    /// The validator set that certifies the *next* source height (the set
    /// `header.next_validators_root` commits to). Adopted on success.
    pub next_set: ValidatorSet,
}

/// M31: self-authenticating op that verifies a source lock against the
/// followed source's cert-signed `bridge_root`, mints new supply to
/// `dest_account`, and records `lock_id` in `BridgeSource::consumed`.
/// Mirrors `bridge::BridgeEndpoint::verify_lock` + `consume` (M30) but
/// consensus-enforced: every step re-runs inside `apply_block`, so a
/// bad redeem rolls the whole block back.
#[derive(Clone, Debug)]
pub struct BridgeRedeem {
    /// Identity of the source chain. Must match an entry in
    /// `ChainState::bridge_sources`.
    pub source_chain: Hash,
    /// Source chain's cert-signed header carrying the `bridge_root` the
    /// lock proof opens against. Must have been followed (i.e. is at or
    /// before the follower's frontier).
    pub source_header: codec::BlockHeader,
    /// Cert binding `source_header.hash()` — verified against the same
    /// tracked set `apply_bridge_header` adopted.
    pub source_cert: consensus::Commit,
    /// The lock's id on the source chain (its key in
    /// `ChainState::bridge_locks`).
    pub lock_id: u64,
    /// The lock op itself.
    pub lock: BridgeLock,
    /// Merkle proof opening `lock.merkle_leaf(lock_id)` against
    /// `source_header.bridge_root`.
    pub proof: merkle::Proof,
}

/// A block: an ordered batch of submissions applied atomically.
#[derive(Clone, Debug)]
pub struct Block {
    pub height: u64,
    pub prev_hash: Hash,
    /// Wall-clock of the block in days; becomes `now_days` for ΔK freshness.
    pub timestamp_days: f32,
    /// Merkle commitment to the validator set that certifies the *next* height —
    /// i.e. the post-apply set this block hands off to. Because the field is part
    /// of the block hash (which the finality certificate signs), a light client
    /// can verify the whole next set, or prove a single validator's membership,
    /// against a cert-signed header without replaying the validator-set
    /// transition. Set by the producer via [`Chain::seal`] and re-checked on
    /// apply against the derived set ([`ChainError::ValidatorRootMismatch`]).
    pub next_validators_root: Hash,
    /// M23: flat digest of the full consensus state after this block applies
    /// (see [`ChainState::state_root`]). A tamper-detector covering every
    /// consensus field — accounts, reviewers, cognitive graph, validators,
    /// bonds, unbonding queue, treasury, supply. The cert signs this via
    /// `block.hash()`; a light client that just needs "is the chain even
    /// honest" can trust the cert-signing validator set rather than recompute
    /// the digest. Stamped by [`Chain::commit`]; mismatches on apply return
    /// [`ChainError::StateRootMismatch`].
    pub state_root: Hash,
    /// M23: Merkle root of the post-apply (accounts ∪ reviewers) tree (see
    /// [`ChainState::merkle_root`]). The commitment a light wallet opens
    /// individual accounts against — `merkle::verify(&header.accounts_root,
    /// leaf, proof)` proves a single account is in the chain, no replay, no
    /// tx bodies. Stamped by [`Chain::commit`]; mismatches on apply return
    /// [`ChainError::AccountsRootMismatch`].
    pub accounts_root: Hash,
    /// M27: Merkle root of the post-apply cognitive graph sorted by
    /// `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)`
    /// (see [`ChainState::graph_merkle_root`]). A cert-signed secondary
    /// index committed alongside `accounts_root`, distinct from it because
    /// the leaf ordering is different (insertion order vs sorted-cosine).
    /// Cert-signed via `block.hash()`; a wallet opens range proofs against
    /// `header.graph_root` without trusting the prover's filter. Stamped by
    /// [`Chain::commit`]; mismatches on apply return
    /// [`ChainError::GraphRootMismatch`].
    pub graph_root: Hash,
    /// M30: Merkle root of the post-apply cumulative `bridge_locks` map,
    /// sorted by `lock_id` (see [`ChainState::bridge_merkle_root`]).
    /// Cert-signed alongside `accounts_root` / `graph_root` — opens
    /// inclusion proofs for individual locks so the destination chain's
    /// bridge endpoint can verify a single lock against this chain's
    /// cert-signed header. Stamped by [`Chain::commit`]; mismatches on
    /// apply return [`ChainError::BridgeRootMismatch`].
    pub bridge_root: Hash,
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
    /// M30: cross-chain lock ops carried by this block. Each entry is a signed
    /// [`BridgeLock`] that moves value from an account's balance into the
    /// `bridge_locked` pool on this (source) chain, destined for an account on
    /// another chain identified by its genesis hash. Usually empty; populated
    /// only when a bridge lock is staged. Locks are part of the block hash, and
    /// their cumulative effect folds into both `state_root` and the header's
    /// `bridge_root` Merkle commitment.
    pub bridge_locks: Vec<BridgeLock>,
    /// M31: bridge-follow ops carried by this block. Each advances
    /// `ChainState::bridge_sources[op.source_chain]` by one cert-signed
    /// source header — the destination-side mirror of `bridge::BridgeEndpoint::follow_source`.
    /// Self-authenticating (cert + next-validators-root check); not
    /// account-signed. Usually empty; populated only when a relayer
    /// stages a `BridgeHeader`. Bodies are part of the block hash (via
    /// `bridge_headers_commitment` in the header projection).
    pub bridge_headers: Vec<BridgeHeader>,
    /// M31: bridge-redeem ops carried by this block. Each verifies a
    /// source lock against the on-chain follower's cert-signed
    /// `bridge_root` and mints new supply to `lock.dest_account` on this
    /// chain. Self-authenticating via cert-binding, Merkle inclusion, a
    /// destination match, and replay dedup; not account-signed. Usually
    /// empty; populated only when a relayer stages a `BridgeRedeem`.
    /// Bodies are part of the block hash (via the header projection's
    /// `bridge_redeems_commitment`).
    pub bridge_redeems: Vec<BridgeRedeem>,
}

impl Block {
    /// Content-addressed block hash. As of M22 this is the hash of the
    /// cert-signed **header projection** of the block — the prefix bytes
    /// `encode_header(BlockHeader::from_block(self))` — so a light client can
    /// verify state against `header.hash()` without seeing the tx / stake-op /
    /// evidence bodies. The per-body SHA-256 commitments in the header bind
    /// those bodies cryptographically (a full node MUST verify the supplied
    /// bodies hash to the committed roots; a light client trusts the
    /// commitment, which the cert signs).
    pub fn hash(&self) -> Hash {
        let h = crate::codec::BlockHeader::from_block(self);
        crate::hash::sha256(&crate::codec::encode_header(&h))
    }

    /// The cert-signed projection of this block. Used by the light-sync
    /// transport (M22): the wire gossips only the header + cert, never the
    /// bodies.
    pub fn header(&self) -> crate::codec::BlockHeader {
        crate::codec::BlockHeader::from_block(self)
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
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

/// M24: a reviewer's reputation state. M23 already includes reviewers
/// in the `accounts_root` Merkle tree (`ChainState::merkle_root`),
/// but there was no typed producer for a reviewer-inclusion proof.
/// `merkle_leaf()` is the canonical preimage `merkle::leaf_hash` consumes
/// — byte-identical to the inline encoding previously living in
/// `ChainState::merkle_leaves`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reviewer {
    pub id: u64,
    pub reputation: f32,
}

impl Reviewer {
    pub fn merkle_leaf(&self) -> Vec<u8> {
        let mut e = codec::Enc(Vec::new());
        e.u64(self.id);
        e.f32(self.reputation);
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
    /// M30: total micro-$COG locked in outbound cross-chain bridge locks. A
    /// [`BridgeLock`] drains an account's balance into this pool; the funds are
    /// held out of balances but remain part of `supply` (a redistribution, like
    /// `bonded`). The destination chain mints backing supply against this pool.
    pub bridge_locked: u64,
    /// M30: every bridge lock ever created, keyed by monotonic `lock_id`.
    /// Append-only (cumulative, like accounts) so an inclusion proof against a
    /// past `bridge_root` stays valid at any later height. Folds into
    /// `state_root` and the header's `bridge_root` Merkle commitment.
    pub bridge_locks: BTreeMap<u64, BridgeLock>,
    /// M30: `lock_id -> block_height` so a producer (and a network relayer)
    /// can resolve which certified header a given lock is bound to. Header
    /// binding is what the destination endpoint's cert-binding step checks.
    pub bridge_lock_heights: BTreeMap<u64, u64>,
    /// M30: next `lock_id` to assign. Monotonic; never reused.
    pub next_lock_id: u64,
    /// M31: cumulative micro-$COG minted on this chain via
    /// [`BridgeRedeem`] ops. Like `bridge_locked`, an audit counter that
    /// mirrors the source-side pool; folds into `state_root` so a wallet
    /// can read its own mint history from cert-signed state. Does NOT
    /// change the `supply_conserved` invariant — a redeem grows both
    /// `accounts[dest].balance` and `supply` by the same amount, so the
    /// total still equals `supply`.
    pub bridge_minted: u64,
    /// M31: on-chain followers of every allowed source chain (declared
    /// at this chain's [`Genesis::bridge_sources`]). Keyed by the source
    /// chain's `genesis_hash`. Each entry carries the cert-signed state
    /// needed to verify a [`BridgeHeader`] / [`BridgeRedeem`] op without
    /// an off-chain sidecar. Folded into `state_root`.
    pub bridge_sources: BTreeMap<Hash, BridgeSource>,
    /// M31: this chain's identity (`genesis_hash`). A [`BridgeLock`] names
    /// it in `dest_chain`; the redeem path checks the match. Set in
    /// [`Self::genesis_split`] *after* the genesis block hash is computed
    /// (so it can be folded into apply / replay without circularity — the
    /// genesis hash is derived *from* a genesis block whose header commits
    /// to `state_root`). **Excluded from `state_root`** by design: it is
    /// a constant of this chain's identity, not mutable state, and folding
    /// it would make the genesis block's hash a self-reference.
    pub genesis_hash: Hash,
}

/// Genesis configuration.
#[derive(Clone)]
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
    /// M31: bridge sources this chain is allowed to follow and redeem
    /// from. Each entry is `(source_genesis_hash, source_genesis_validators)`:
    /// the source's identity (its `genesis_hash`) plus its initial
    /// validator set. The source's own `genesis_hash` is the trust root
    /// for any [`BridgeRedeem`] minted on this chain, exactly as
    /// [`Self::validators`] anchors this chain's local BFT. Empty by
    /// default — a chain that has never opened a bridge carries no
    /// follower registry. Seeds `ChainState::bridge_sources` in
    /// [`Self::genesis_split`].
    pub bridge_sources: Vec<BridgeSourceSeed>,
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
    /// The block's `next_validators_root` does not commit to the validator set
    /// this block hands off to (the set that certifies the next height). Either a
    /// producer sealed the wrong root or the block was tampered with.
    ValidatorRootMismatch { height: u64 },
    /// M23: the block's `state_root` does not equal the post-apply flat digest
    /// of the full consensus state. The cert-signed header commits to this
    /// root, so a mismatch means the producer sealed the wrong value (or a
    /// peer tampered with the field).
    StateRootMismatch { height: u64 },
    /// M23: the block's `accounts_root` does not equal the post-apply Merkle
    /// root of (accounts ∪ reviewers). The cert-signed header commits to this
    /// root, so a mismatch means the producer sealed the wrong value (or a
    /// peer tampered with the field) — a wallet's account-inclusion proofs
    /// would not verify against the wrong root.
    AccountsRootMismatch { height: u64 },
    /// M27: the block's `graph_root` does not equal the post-apply Merkle
    /// root of the cognitive-graph nodes sorted by cosine against the
    /// canonical pivot. Cert-signed via `header.graph_root`, this is the
    /// commitment a wallet opens the sorted graph view against for M27
    /// range claims — same root mismatch reasoning as `AccountsRootMismatch`.
    GraphRootMismatch { height: u64 },
    /// M30: the block's `bridge_root` does not equal the post-apply Merkle
    /// root of the cumulative `bridge_locks` map sorted by `lock_id`.
    /// Cert-signed via `header.bridge_root`, this is the commitment a
    /// destination chain's bridge endpoint opens a single lock against — same
    /// root-mismatch reasoning as `AccountsRootMismatch` / `GraphRootMismatch`.
    BridgeRootMismatch { height: u64 },
    /// M31: a `BridgeHeader` / `BridgeRedeem` op named a source chain
    /// (`source_chain`) that this chain did not declare in
    /// [`Genesis::bridge_sources`]. The trust root is missing — the chain
    /// is not following that counterparty. Carries the rejected id for
    /// diagnostics.
    UnknownBridgeSource(Hash),
    /// M31: a `BridgeHeader`'s source header doesn't chain to the
    /// follower's tracked head (either wrong height or wrong prev_hash).
    /// The follower's height+1 / prev-chains-to-head check failed; either
    /// the relayer skipped a height or replayed an old one.
    BridgeBadFollow { source: Hash, height: u64 },
    /// M31: cert-binding failed for a `BridgeHeader` or `BridgeRedeem`
    /// op. The cert is not a valid > 2/3 quorum of the tracked source
    /// set, or it does not bind the supplied header's hash. The relayer
    /// cannot forge past this — it's the same cert-binding check the
    /// off-chain `bridge::BridgeEndpoint` uses (M22/M30), just now run
    /// inside `apply_block`.
    BridgeCertInvalid { source: Hash, height: u64 },
    /// M31: a `BridgeHeader`'s `next_set` does not commit to the
    /// source header's `next_validators_root` (or is empty). The producer
    /// shipped a next set the header doesn't actually certify — a
    /// tampering or seal error. Mirrors the validator-root mismatch
    /// check on the destination side.
    BridgeNextSetMismatch { source: Hash, height: u64 },
    /// M31: a `BridgeRedeem` referenced a source header at a height
    /// the on-chain follower has not reached yet. The follower must
    /// advance via a `BridgeHeader` first; minting against an un-followed
    /// header is rejected.
    BridgeSourceNotFollowed { source: Hash, height: u64 },
    /// M31: a `BridgeRedeem`'s Merkle proof did not open
    /// `lock.merkle_leaf(lock_id)` against `source_header.bridge_root`.
    /// Either the lock bytes were tampered with, the lock_id was
    /// changed, or the proof itself is forged.
    BridgeInclusionInvalid { source: Hash, lock_id: u64 },
    /// M31: a `BridgeRedeem`'s `lock.dest_chain` is not this chain's
    /// `genesis_hash`. A lock destined for a *different* chain must not
    /// mint supply here.
    BridgeWrongDestination { expected: Hash, got: Hash },
    /// M31: a `BridgeRedeem` named a `(source_chain, lock_id)` already
    /// in `BridgeSource::consumed` — replay rejected. Same discipline as
    /// M30's `BridgeError::AlreadyConsumed`, just on the consensus path.
    BridgeAlreadyRedeemed { source: Hash, lock_id: u64 },
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
            ChainError::ValidatorRootMismatch { height } => write!(
                f,
                "block {height} next_validators_root does not match the derived validator set"
            ),
            ChainError::StateRootMismatch { height } => write!(
                f,
                "block {height} state_root does not match the post-apply consensus-state digest"
            ),
            ChainError::AccountsRootMismatch { height } => write!(
                f,
                "block {height} accounts_root does not match the post-apply accounts/reviewers Merkle root"
            ),
            ChainError::GraphRootMismatch { height } => write!(
                f,
                "block {height} graph_root does not match the post-apply graph (sorted-by-cosine) Merkle root"
            ),
            ChainError::BridgeRootMismatch { height } => write!(
                f,
                "block {height} bridge_root does not match the post-apply bridge_locks (sorted-by-lock_id) Merkle root"
            ),
            ChainError::UnknownBridgeSource(s) => write!(
                f,
                "bridge source {} not declared in genesis (not followed by this chain)",
                short_hex(s)
            ),
            ChainError::BridgeBadFollow { source, height } => write!(
                f,
                "bridge header at height {height} does not chain to followed head of source {}",
                short_hex(source)
            ),
            ChainError::BridgeCertInvalid { source, height } => write!(
                f,
                "bridge cert-binding failed for source {} at height {height}",
                short_hex(source)
            ),
            ChainError::BridgeNextSetMismatch { source, height } => write!(
                f,
                "bridge next_set does not match next_validators_root for source {} at height {height}",
                short_hex(source)
            ),
            ChainError::BridgeSourceNotFollowed { source, height } => write!(
                f,
                "bridge redeem references source {} at un-followed height {height}",
                short_hex(source)
            ),
            ChainError::BridgeInclusionInvalid { source, lock_id } => write!(
                f,
                "bridge redeem inclusion proof invalid for source {} lock_id {lock_id}",
                short_hex(source)
            ),
            ChainError::BridgeWrongDestination { expected, got } => write!(
                f,
                "bridge redeem destined for {} but this chain is {}",
                short_hex(got),
                short_hex(expected)
            ),
            ChainError::BridgeAlreadyRedeemed { source, lock_id } => write!(
                f,
                "bridge redeem already consumed for source {} lock_id {lock_id}",
                short_hex(source)
            ),
        }
    }
}

/// First 8 hex chars of a hash — used in ChainError / LightError display
/// strings to keep messages compact.
fn short_hex(h: &Hash) -> String {
    let mut s = String::with_capacity(16);
    for b in &h[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
        Self::genesis_split(g)
    }

    /// M23: a light client bootstrapped from only `Genesis` (e.g. `ValidatorTracker::from_genesis`)
    /// needs to compute the cert-signed `state_root` / `accounts_root` for the
    /// genesis block without ever materialising a full `ChainState`. These two
    /// helpers mirror [`Self::state_root`] / [`Self::merkle_root`] but build the
    /// digest directly from the genesis parameters — same canonical encoding, so
    /// the value matches what `ChainState::genesis` stamps.
    pub fn state_root_for_genesis(g: &Genesis) -> Hash {
        Self::genesis_split(g.clone()).0.state_root()
    }
    pub fn merkle_root_for_genesis(g: &Genesis) -> Hash {
        Self::genesis_split(g.clone()).0.merkle_root()
    }
    /// M27: same pattern as `merkle_root_for_genesis`, but for the
    /// cert-signed sorted-by-cosine graph root that ships in
    /// `header.graph_root`. A light client anchored on the genesis hash
    /// computes this without materialising a full `ChainState`, matching
    /// what `ChainState::genesis` stamps onto the genesis block's
    /// `graph_root` field.
    pub fn graph_merkle_root_for_genesis(g: &Genesis) -> Hash {
        Self::genesis_split(g.clone()).0.graph_merkle_root()
    }

    /// The shared genesis construction; `genesis` and the two `*_for_genesis`
    /// helpers all funnel through this so the values stay in lockstep.
    fn genesis_split(g: Genesis) -> (ChainState, Hash) {
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
            graph.add(emb, dom);
        }
        let validators = ValidatorSet::new(
            g.validators
                .into_iter()
                .map(|(id, pubkey, power)| Validator { id, pubkey, power })
                .collect(),
        );
        // M31: seed the on-chain bridge source followers from genesis.
        // Each declared source contributes one entry to bridge_sources,
        // seeded at height=0 with the source's own genesis validator set
        // (the set that certifies source height 1). The follower's `head`
        // is the source's own `genesis_hash` so the first BridgeHeader
        // for that source must chain to it. The source's genesis_hash
        // is computable from the same `(Genesis.bridge_sources)` shape —
        // we derive it via a nested `genesis_split` so the value is
        // exact (no mirror layout to drift).
        let mut bridge_sources: BTreeMap<Hash, BridgeSource> = BTreeMap::new();
        for (source_genesis_hash, source_validators) in g.bridge_sources.iter() {
            let source_set = ValidatorSet::new(
                source_validators
                    .iter()
                    .map(|&(id, pubkey, power)| Validator { id, pubkey, power })
                    .collect(),
            );
            // Re-derive the source's genesis_hash from its own
            // (validators, timestamp, …) shape so we don't need to ship
            // the source's full genesis through this side. The source
            // Genesis here carries the same validator set + an
            // authoritative timestamp (the one its own genesis was
            // computed under) — we leave that to the caller, since
            // `head` only needs to chain to whatever the source's
            // height-1 header points at via `prev_hash`. Storing the
            // caller-supplied `source_genesis_hash` as `head` is the
            // trust contract: a declared bridge source's identity is
            // pinned in this chain's genesis, so the very first
            // BridgeHeader's `header.prev_hash` must equal it.
            bridge_sources.insert(
                *source_genesis_hash,
                BridgeSource {
                    set: source_set,
                    head: *source_genesis_hash,
                    height: 0,
                    consumed: BTreeSet::new(),
                },
            );
        }
        let mut state = ChainState {
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
            bridge_locked: 0,
            bridge_locks: BTreeMap::new(),
            bridge_lock_heights: BTreeMap::new(),
            next_lock_id: 0,
            // M31: bridge-redeem audit counter + follower registry.
            bridge_minted: 0,
            bridge_sources,
            // Placeholder — overwritten by the real `gh` below once
            // computed. We cannot set it to the real value before
            // constructing the genesis Block (we need to know the
            // Block's hash to know `gh`), but the value is unused by
            // `state_root()` (genesis_hash is excluded by design).
            genesis_hash: [0u8; 32],
        };
        // genesis "block" hash: height 0, zero prev, no txs, no validator updates.
        // Its commitment is the genesis validator set (the set that certifies
        // height 1), so a light client anchored on this hash starts already
        // committed to the initial set. M23 also stamps the cert-signed state
        // commitments (state_root / accounts_root) against the genesis state so
        // that `block.hash()` over the genesis block equals the hash any replay
        // would re-derive.
        let gh = Block {
            height: 0,
            prev_hash: [0u8; 32],
            timestamp_days: g.timestamp_days,
            next_validators_root: state.validators.merkle_root(),
            state_root: state.state_root(),
            accounts_root: state.merkle_root(),
            // M27: stamp the genesis sorted-by-cosine graph root the same way
            // we stamp `accounts_root` so a light client anchored on the
            // genesis hash finds the cert-signed secondary index already
            // committed for height 0.
            graph_root: state.graph_merkle_root(),
            // M30: stamp the genesis bridge_root (empty locks → empty-tree
            // root) so a light client anchored on the genesis hash finds the
            // cert-signed bridge commitment already committed for height 0.
            bridge_root: state.bridge_merkle_root(),
            txs: Vec::new(),
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        }
        .hash();
        // M31: now that the genesis block hash is known, store it on
        // the state so BridgeRedeem's `dest_chain` match has a value to
        // compare against. Excluded from `state_root` (see field doc).
        state.genesis_hash = gh;
        (state, gh)
    }

    /// Apply a block, mutating self. On `Err` self may be partially mutated —
    /// callers wanting atomicity should apply to a clone (see [`Chain::commit`]).
    pub fn apply_block(&mut self, block: &Block) -> Result<BlockReceipt, ChainError> {
        self.apply_block_inner(block, true)
    }

    /// Shared block-application core. When `enforce_commitment` is set, the
    /// block's `next_validators_root` must equal the Merkle root of the set this
    /// block hands off to (the last check, after the transition is finalized) —
    /// how a committed block is validated. The producer computes the root on a
    /// trial run with the check *off* (see [`Chain::next_validators_root`]);
    /// since the transition never reads `next_validators_root`, the derived set
    /// is independent of the field, so there is no circularity.
    fn apply_block_inner(
        &mut self,
        block: &Block,
        enforce_commitment: bool,
    ) -> Result<BlockReceipt, ChainError> {
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

        // M30: cross-chain bridge locks. Each drains an account's balance into
        // the `bridge_locked` pool and appends to the cumulative `bridge_locks`
        // map under a freshly-assigned monotonic `lock_id`. Validation fully
        // precedes mutation (as with stake ops), so a bad lock rolls the whole
        // block back. Locks do not touch the validator set.
        for lock in &block.bridge_locks {
            self.apply_bridge_lock(lock, new_height)?;
        }

        // M31: bridge-follow ops. Each advances `bridge_sources[op.source_chain]`
        // by one certified source header. Order: bridge_headers BEFORE
        // bridge_redeems, so a redeem in the same block may reference a header
        // followed in the same block. Validation fully precedes mutation, so
        // a bad follow rolls the whole block back (the redeem loop then sees
        // an un-advanced follower and rejects with BridgeSourceNotFollowed).
        for op in &block.bridge_headers {
            self.apply_bridge_header(op)?;
        }
        for op in &block.bridge_redeems {
            self.apply_bridge_redeem(op)?;
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

        // The block's header commits to the set that certifies the next height.
        // Verify it matches the set we just derived (covers the no-updates case,
        // where the set is unchanged). This is the last check, so a block that is
        // invalid for any earlier reason fails there first regardless of its root.
        //
        // M23: bump `self.height` to the post-apply value BEFORE the commitment
        // checks. `state_root()` includes `height`, so the post-apply state
        // digest has the new height baked in — and the producer stamped the
        // block's state_root from a trial that already advanced height. Keeping
        // the old height here would make the cross-check fail spuriously.
        self.height = new_height;
        if enforce_commitment && block.next_validators_root != self.validators.merkle_root() {
            return Err(ChainError::ValidatorRootMismatch { height: new_height });
        }

        // M23: cert-signed state commitments. Both are post-apply, so the trial
        // must be in its final form when checked (it is — all tx / stake-op /
        // evidence / validator-set transitions have been applied above).
        // Mirrors `next_validators_root` in discipline: producer stamps via
        // [`Chain::commit`], every verifier rechecks on apply.
        if enforce_commitment && block.state_root != self.state_root() {
            return Err(ChainError::StateRootMismatch { height: new_height });
        }
        if enforce_commitment && block.accounts_root != self.merkle_root() {
            return Err(ChainError::AccountsRootMismatch { height: new_height });
        }
        // M27: cert-signed secondary index over the cognitive graph. Same
        // discipline as `accounts_root` above — producer stamps via
        // [`Chain::seal`], every verifier rechecks on apply. The two
        // commitments cover the same underlying graph nodes but with
        // different orderings (insertion order vs sorted-by-cosine), so a
        // mismatch on either means a tampering or seal error.
        if enforce_commitment && block.graph_root != self.graph_merkle_root() {
            return Err(ChainError::GraphRootMismatch { height: new_height });
        }
        // M30: cert-signed commitment over the cumulative `bridge_locks` map.
        // Same seal-then-enforce discipline as `graph_root` / `accounts_root`:
        // a destination chain's bridge endpoint opens a single lock against
        // this commitment, so any mismatch here means the source-side proof
        // would not verify on the dest side.
        if enforce_commitment && block.bridge_root != self.bridge_merkle_root() {
            return Err(ChainError::BridgeRootMismatch { height: new_height });
        }

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

    /// M30: validate and apply one cross-chain bridge lock. Drains
    /// `lock.amount` from `lock.account`'s balance into `bridge_locked` and
    /// appends to the cumulative `bridge_locks` map under a freshly-assigned
    /// monotonic `lock_id`. All checks precede mutation, so a bad lock rolls
    /// the whole block back. The lock itself only commits the source-side
    /// bookkeeping; the destination's mint is a bridge-module concern
    /// (`bridge::BridgeEndpoint::consume`) keyed off the source's cert-signed
    /// `bridge_root`.
    fn apply_bridge_lock(&mut self, lock: &BridgeLock, height: u64) -> Result<(), ChainError> {
        if lock.amount == 0 {
            return Err(ChainError::ZeroStake(lock.account));
        }
        let acct = self
            .accounts
            .get(&lock.account)
            .ok_or(ChainError::UnknownAccount(lock.account))?;
        // authenticate: the signature must be by the account's registered key.
        if !crypto::verify(
            &acct.pubkey,
            &codec::bridgelock_signing_bytes(lock),
            &lock.signature,
        ) {
            return Err(ChainError::BadSignature(lock.account));
        }
        if acct.balance < lock.amount {
            return Err(ChainError::InsufficientBalance {
                account: lock.account,
                need: lock.amount,
                have: acct.balance,
            });
        }
        // -- mutate (all checks above passed) --------------------------------
        self.accounts.get_mut(&lock.account).unwrap().balance -= lock.amount;
        self.bridge_locked += lock.amount;
        let lock_id = self.next_lock_id;
        self.next_lock_id += 1;
        self.bridge_locks.insert(lock_id, lock.clone());
        self.bridge_lock_heights.insert(lock_id, height);
        Ok(())
    }

    /// M31: advance `bridge_sources[op.source_chain]` by one cert-signed
    /// source header. Mirrors `bridge::BridgeEndpoint::follow_source` (M30)
    /// but consensus-enforced: every check runs inside `apply_block`, so a
    /// bad follow rolls the whole block back. The follower only tracks
    /// `{set, head, height}` — `follow_header` (M22) takes `next_set` as a
    /// parameter rather than deriving it from bonds / pubkeys, so no bond
    /// mirror is needed (BridgeSource lacks bonds on purpose).
    ///
    /// Steps:
    /// 1. source known (declared at our genesis).
    /// 2. header chains to the follower's tracked head (height+1,
    ///    prev_hash == head) — a relayer cannot skip or replay a source height.
    /// 3. cert-binding: a > 2/3 quorum of the tracked source set binds
    ///    `header.hash()`. Reuses [`ValidatorTracker::verify_state_root_against_header`].
    /// 4. `op.next_set.merkle_root() == op.header.next_validators_root`
    ///    (and non-empty) — the cert signed the next-set commitment.
    /// 5. mutate: adopt the new set + head + height.
    fn apply_bridge_header(&mut self, op: &BridgeHeader) -> Result<(), ChainError> {
        let s = self
            .bridge_sources
            .get_mut(&op.source_chain)
            .ok_or(ChainError::UnknownBridgeSource(op.source_chain))?;
        // 2. chains to head
        if op.header.height != s.height + 1 || op.header.prev_hash != s.head {
            return Err(ChainError::BridgeBadFollow {
                source: op.source_chain,
                height: op.header.height,
            });
        }
        // 3. cert-binding against the *currently tracked* source set (NOT
        //    the op's own set — a relayer cannot substitute one).
        ValidatorTracker::verify_state_root_against_header(
            &op.header,
            &op.cert,
            &s.set,
        )
        .map_err(|_| ChainError::BridgeCertInvalid {
            source: op.source_chain,
            height: op.header.height,
        })?;
        // 4. next_set root check (mirrors the destination-side check in
        //    `ValidatorTracker::follow_header`).
        if op.next_set.is_empty()
            || op.next_set.merkle_root() != op.header.next_validators_root
        {
            return Err(ChainError::BridgeNextSetMismatch {
                source: op.source_chain,
                height: op.header.height,
            });
        }
        // 5. mutate (all checks above passed)
        s.set = op.next_set.clone();
        s.head = op.header.hash();
        s.height = op.header.height;
        Ok(())
    }

    /// M31: verify a source lock against the on-chain follower's
    /// cert-signed `bridge_root` and mint new supply to `dest_account`.
    /// Mirrors `bridge::BridgeEndpoint::verify_lock` + `consume` (M30)
    /// but consensus-enforced. Every step re-runs inside `apply_block`,
    /// so a bad redeem rolls the whole block back. After all checks pass
    /// the chain credits the destination and marks the lock consumed in
    /// the follower's `consumed` set — the on-chain dedup.
    ///
    /// Steps:
    /// 1. source known.
    /// 2. frontier guard: `source_header.height <= follower.height`
    ///    (mirrors M30's `<=` — a redeem at the exact height the follower
    ///    has reached is allowed).
    /// 3. cert-binding against the tracked source set.
    /// 4. inclusion: `merkle::verify(&source_header.bridge_root, leaf, proof)`.
    /// 5. dest match: `lock.dest_chain == self.genesis_hash`.
    /// 6. replay: `(source_chain, lock_id) ∉ consumed`.
    /// 7. dest account exists.
    /// 8. MINT: balance += amount, supply += amount, bridge_minted += amount,
    ///    `consumed.insert(lock_id)`. The supply invariant is preserved
    ///    (a redeem grows BOTH sides by the same amount).
    fn apply_bridge_redeem(&mut self, op: &BridgeRedeem) -> Result<(), ChainError> {
        let s_ref = self
            .bridge_sources
            .get(&op.source_chain)
            .ok_or(ChainError::UnknownBridgeSource(op.source_chain))?;
        // 2. frontier guard
        if op.source_header.height > s_ref.height {
            return Err(ChainError::BridgeSourceNotFollowed {
                source: op.source_chain,
                height: op.source_header.height,
            });
        }
        // 3. cert-binding
        ValidatorTracker::verify_state_root_against_header(
            &op.source_header,
            &op.source_cert,
            &s_ref.set,
        )
        .map_err(|_| ChainError::BridgeCertInvalid {
            source: op.source_chain,
            height: op.source_header.height,
        })?;
        // 4. Merkle inclusion against the cert-signed bridge_root.
        let leaf_hash = merkle::leaf_hash(&op.lock.merkle_leaf(op.lock_id));
        if !merkle::verify(&op.source_header.bridge_root, &leaf_hash, &op.proof) {
            return Err(ChainError::BridgeInclusionInvalid {
                source: op.source_chain,
                lock_id: op.lock_id,
            });
        }
        // 5. dest match
        if op.lock.dest_chain != self.genesis_hash {
            return Err(ChainError::BridgeWrongDestination {
                expected: self.genesis_hash,
                got: op.lock.dest_chain,
            });
        }
        // 6. replay
        if s_ref.consumed.contains(&op.lock_id) {
            return Err(ChainError::BridgeAlreadyRedeemed {
                source: op.source_chain,
                lock_id: op.lock_id,
            });
        }
        // 7. dest account known
        if !self.accounts.contains_key(&op.lock.dest_account) {
            return Err(ChainError::UnknownAccount(op.lock.dest_account));
        }
        // 8. MINT — validation fully preceded mutation, so a bad op would
        //    have returned above and the chain rolls back. The redeem grows
        //    BOTH `accounts[dest].balance` and `supply` by the same amount,
        //    preserving `supply_conserved()`; `bridge_minted` is the
        //    audit counter mirroring `bridge_locked`.
        self.accounts.get_mut(&op.lock.dest_account).unwrap().balance += op.lock.amount;
        self.supply += op.lock.amount;
        self.bridge_minted += op.lock.amount;
        self.bridge_sources
            .get_mut(&op.source_chain)
            .unwrap()
            .consumed
            .insert(op.lock_id);
        Ok(())
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
            self.graph.add(tx.embedding, tx.domain);
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
        // M30: bridge state. A lock is a redistribution within `supply`
        // (balance → bridge_locked), so folding both sides + the cumulative
        // map keeps the digest sensitive to either side being tampered with.
        e.u64(self.bridge_locked);
        e.u64(self.bridge_locks.len() as u64);
        for (id, lock) in &self.bridge_locks {
            e.raw(&lock.merkle_leaf(*id));
        }
        e.u64(self.bridge_lock_heights.len() as u64);
        for (id, h) in &self.bridge_lock_heights {
            e.u64(*id);
            e.u64(*h);
        }
        e.u64(self.next_lock_id);
        // M31: bridge-redeem audit counter + on-chain source follower
        // registry. Folded into the same digest so a tampered redeem or
        // follower state (set / head / height / consumed) flips
        // `state_root`. `genesis_hash` is **deliberately excluded** — it
        // is a constant of this chain's identity, and folding it would
        // make the genesis block's hash a self-reference (it is derived
        // from a block whose header commits to `state_root`).
        e.u64(self.bridge_minted);
        e.u64(self.bridge_sources.len() as u64);
        for (source_chain, src) in &self.bridge_sources {
            e.raw(source_chain);
            let vs = src.set.validators();
            e.u64(vs.len() as u64);
            for v in vs {
                e.u64(v.id);
                e.raw(&v.pubkey);
                e.u64(v.power);
            }
            e.raw(&src.head);
            e.u64(src.height);
            e.u64(src.consumed.len() as u64);
            for id in &src.consumed {
                e.u64(*id);
            }
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

    /// Internal: collect each accounts/reviewers/graph-nodes entry as a
    /// domain-separated leaf hash, in canonical order: accounts (BTreeMap
    /// order), reviewers (BTreeMap order), graph nodes (insertion order).
    /// The resulting Merkle root is `accounts_root`, which now commits
    /// to all three sets in one 32-byte slot (M25 extended it from the
    /// M24 accounts∪reviewers coverage).
    fn merkle_leaves(&self) -> Vec<Hash> {
        let n_graph = self.graph.nodes.len();
        let mut leaves = Vec::with_capacity(
            self.accounts.len() + self.reviewers.len() + n_graph,
        );
        for (id, a) in &self.accounts {
            leaves.push(merkle::leaf_hash(&a.merkle_leaf(*id)));
        }
        for (id, rep) in &self.reviewers {
            leaves.push(merkle::leaf_hash(&Reviewer { id: *id, reputation: *rep }.merkle_leaf()));
        }
        for n in &self.graph.nodes {
            leaves.push(merkle::leaf_hash(&n.merkle_leaf()));
        }
        leaves
    }

    /// M24: inclusion proof for a reviewer against [`Self::merkle_root`].
    /// Reviewers occupy the second half of the `merkle_leaves` layout
    /// (accounts first, then reviewers, both in `BTreeMap` order).
    /// `None` if `reviewer_id` is unknown.
    pub fn reviewer_proof(&self, reviewer_id: u64) -> Option<merkle::Proof> {
        let n_accounts = self.accounts.len();
        let rids: Vec<u64> = self.reviewers.keys().copied().collect();
        let rindex = rids.iter().position(|&k| k == reviewer_id)?;
        merkle::MerkleTree::from_leaf_hashes(self.merkle_leaves()).proof(n_accounts + rindex)
    }

    /// M25: inclusion proof for the graph node at insertion index `idx`,
    /// against [`Self::merkle_root`]. Graph nodes occupy the third slice
    /// of `merkle_leaves` (after accounts and reviewers), in insertion
    /// order — so the proof path is `n_accounts + n_reviewers + idx`.
    /// `None` if `idx >= self.graph.nodes.len()`.
    pub fn graph_node_proof(&self, idx: usize) -> Option<merkle::Proof> {
        if idx >= self.graph.nodes.len() { return None; }
        let offset = self.accounts.len() + self.reviewers.len();
        merkle::MerkleTree::from_leaf_hashes(self.merkle_leaves()).proof(offset + idx)
    }

    /// M27: Merkle root over the cognitive graph nodes sorted by
    /// `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)`. Cert-
    /// signed via `header.graph_root`. The leaf preimage is identical to
    /// `GraphNode::merkle_leaf()` (44 bytes), but the leaves are presented
    /// in a *different* order than `merkle_leaves()` — so the two roots
    /// commit to the SAME underlying nodes with DIFFERENT orderings, and
    /// are distinct commitments.
    ///
    /// The canonical pivot is the first standard basis vector
    /// `[1, 0, 0, 0, 0, 0, 0, 0]` (a unit vector along axis 0), the same
    /// shape used by the engine's existing test fixtures (`engine::unit(1.0)`).
    /// Sorting against a deterministic pivot makes the index reproducible
    /// across all peers and across all wallet verifiers, independent of the
    /// user's query — the user query drives the *cut*, but the index lives
    /// in the cert-signed header.
    pub fn graph_merkle_root(&self) -> Hash {
        let leaves = self.graph_sorted_leaves();
        merkle::MerkleTree::from_leaf_hashes(leaves).root()
    }

    /// M27: prove the slice `[a, b)` of the sorted-by-cosine view commits
    /// to a sub-root under `graph_root`. Returns `(sub_root, ordered list of
    /// (node_id, GraphNode, merkle::Proof))` for the leaves in that range
    /// (in the same sorted order used by `graph_merkle_root`). `None` if
    /// `b > n` or `a > b` or `a == b` (empty slice).
    pub fn graph_range_proof(
        &self,
        a: usize,
        b: usize,
    ) -> Option<RangeProof> {
        let n = self.graph.nodes.len();
        if a > b || b > n || a == b { return None; }
        let sorted = self.graph_sorted_nodes();
        let leaves: Vec<Hash> = sorted.iter()
            .map(|n| merkle::leaf_hash(&n.merkle_leaf()))
            .collect();
        let tree = merkle::MerkleTree::from_leaf_hashes(leaves);
        let mut entries: Vec<(u64, crate::engine::GraphNode, merkle::Proof)> =
            Vec::with_capacity(b - a);
        for (i, node) in sorted.iter().enumerate().take(b).skip(a) {
            let proof = tree.proof(i)?;
            entries.push((node.node_id, node.clone(), proof));
        }
        Some(RangeProof { sub_root: tree.root(), entries })
    }

    /// Internal: graph nodes sorted by `(cos_sim(CANONICAL_PIVOT, *) desc,
    /// node_id asc)`. The single source of truth for the M27 sort order,
    /// used by both `graph_merkle_root` (to compute the root) and
    /// `graph_range_proof` (to slice the tree). Index in this Vec is the
    /// leaf index against `graph_root`.
    fn graph_sorted_nodes(&self) -> Vec<crate::engine::GraphNode> {
        let mut sorted: Vec<crate::engine::GraphNode> = self.graph.nodes.clone();
        sorted.sort_by(|a, b| {
            let sa = crate::engine::cos_sim(&CANONICAL_PIVOT, &a.embedding);
            let sb = crate::engine::cos_sim(&CANONICAL_PIVOT, &b.embedding);
            // Descending cosine; ties broken by `node_id` ascending.
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
                .then(a.node_id.cmp(&b.node_id))
        });
        sorted
    }

    /// Internal: preimage leaves for `graph_sorted_nodes`. Mirrors
    /// `merkle_leaves`'s third slice but in sorted order.
    fn graph_sorted_leaves(&self) -> Vec<Hash> {
        self.graph_sorted_nodes().iter()
            .map(|n| merkle::leaf_hash(&n.merkle_leaf()))
            .collect()
    }

    /// M30: Merkle root over the cumulative `bridge_locks` map, sorted by
    /// `lock_id` (BTreeMap order is already `lock_id`-ascending). Cert-signed
    /// via `header.bridge_root`. The leaf preimage is exactly
    /// `lock.merkle_leaf(lock_id)`. Mirrors `graph_merkle_root`'s shape
    /// (cumulative state, separate from `accounts_root`), but ordered by
    /// `lock_id` instead of cosine — locks have no embeddings.
    pub fn bridge_merkle_root(&self) -> Hash {
        let leaves: Vec<Hash> = self.bridge_locks.iter()
            .map(|(id, lock)| merkle::leaf_hash(&lock.merkle_leaf(*id)))
            .collect();
        merkle::MerkleTree::from_leaf_hashes(leaves).root()
    }

    /// M30: inclusion proof for the lock with the given `lock_id` against
    /// [`Self::bridge_merkle_root`]. Returns `None` if the id is unknown.
    /// Mirrors `account_proof` / `graph_node_proof`.
    pub fn bridge_lock_proof(&self, lock_id: u64) -> Option<merkle::Proof> {
        let ids: Vec<u64> = self.bridge_locks.keys().copied().collect();
        let index = ids.iter().position(|&k| k == lock_id)?;
        let leaves: Vec<Hash> = self.bridge_locks.iter()
            .map(|(id, lock)| merkle::leaf_hash(&lock.merkle_leaf(*id)))
            .collect();
        merkle::MerkleTree::from_leaf_hashes(leaves).proof(index)
    }

    /// M30: same pattern as the M27 `*_for_genesis` helpers, but for the
    /// cert-signed `bridge_root` that ships in `header.bridge_root`. A light
    /// client anchored on the genesis hash computes this without
    /// materialising a full `ChainState`, matching what
    /// `ChainState::genesis` stamps onto the genesis block.
    pub fn bridge_merkle_root_for_genesis(g: &Genesis) -> Hash {
        Self::genesis_split(g.clone()).0.bridge_merkle_root()
    }

    /// M28: produce the producer-side graph diff between this state (the
    /// "h₂" side) and `prev_state` (the "h₁" side). Caller must guarantee
    /// `prev_state.height < self.height` and both states share the same
    /// genesis (deterministic replay).
    ///
    /// `added` are the nodes present at h₂ but absent at h₁ — by M25's
    /// append-only invariant this is exactly `[n_H1, n_H2)`. `dropped`
    /// are the nodes present at h₁ but absent at h₂ — by the same
    /// invariant this list is **always empty** under the current engine
    /// (`CognitiveGraph::add` is the only mutator, and it appends). The
    /// shape is preserved for future engine evolution (e.g. prune) so the
    /// verifier doesn't need a wire-format bump.
    ///
    /// Each `added` entry carries an inclusion proof against *this* state's
    /// `accounts_root`; each `dropped` carries a proof against `prev_state`'s
    /// `accounts_root`. The wallet-side verifier in `light.rs` re-derives
    /// the diff locally from a partial replay of `(h₁..h₂]` and rejects any
    /// divergence with `DiffMismatch`.
    pub fn graph_diff(&self, prev_state: &ChainState) -> DiffClaim {
        let prev_n = prev_state.graph.nodes.len();
        let new_n = self.graph.nodes.len();

        // `added` ⊆ [prev_n, new_n). With the append-only invariant every
        // node at insertion index >= prev_n is new.
        let mut added: Vec<GraphLeafAtHeight> =
            Vec::with_capacity(new_n.saturating_sub(prev_n));
        for idx in prev_n..new_n {
            let node = self.graph.nodes[idx].clone();
            let proof = self
                .graph_node_proof(idx)
                .expect("in-bounds index from own graph");
            added.push(GraphLeafAtHeight {
                node_id: node.node_id,
                graph_node: node,
                proof,
            });
        }

        // `dropped` ⊆ [0, prev_n). With the current engine this is always
        // empty. When prune lands, populate this list and the wallet
        // verifier's contract stays the same.
        let mut dropped: Vec<GraphLeafAtHeight> = Vec::new();
        for (idx, node) in prev_state.graph.nodes.iter().enumerate() {
            if idx >= new_n {
                let proof = prev_state
                    .graph_node_proof(idx)
                    .expect("in-bounds index from prev graph");
                dropped.push(GraphLeafAtHeight {
                    node_id: node.node_id,
                    graph_node: node.clone(),
                    proof,
                });
            }
        }

        DiffClaim { added, dropped }
    }

    /// Accounting invariant: every micro-$COG is in an account balance, in the
    /// treasury, in the bonded pool, or in the unbonding queue (stake escrow for a
    /// submission is always resolved within a tx). Should hold after any sequence
    /// of blocks.
    pub fn supply_conserved(&self) -> bool {
        let unbonding: u128 = self.unbonding.iter().map(|u| u.amount as u128).sum();
        // M30: locked-in-bridge pool is part of `supply` (a redistribution
        // within supply — the source balance dropped by the same amount).
        let held: u128 = self.accounts.values().map(|a| a.balance as u128).sum::<u128>()
            + self.treasury as u128
            + self.bonded as u128
            + self.bridge_locked as u128
            + unbonding;
        held == self.supply as u128
    }
}

// --- Chain: hash-linked sequence of blocks over the state --------------------

#[derive(Clone)]
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

    /// The validator-set Merkle root this block would hand off to (the set that
    /// certifies the next height), computed by trial-applying the block on a
    /// clone with the commitment check disabled. The producer uses this to
    /// [`Self::seal`] a candidate before consensus; the derived set is
    /// independent of `block.next_validators_root`, so sealing has no circularity.
    pub fn next_validators_root(&self, block: &Block) -> Result<Hash, ChainError> {
        let mut trial = self.state.clone();
        trial.apply_block_inner(block, false)?;
        Ok(trial.validators.merkle_root())
    }

    /// Set `block.next_validators_root`, `block.state_root`, and
    /// `block.accounts_root` to the post-apply values the block hands off to,
    /// so the sealed block passes the commitment checks when committed. Call
    /// after the block's txs/ops are final and before hashing it for
    /// consensus.
    ///
    /// M23: this is the producer-side pre-consensus seal. It runs a trial
    /// apply (with the commitment checks disabled, so an already-sealed block
    /// can be re-sealed idempotently) and stamps all three cert-signed state
    /// commitments onto the block. Validators then sign over the sealed
    /// block's hash, and [`Self::commit`] re-runs the trial with the checks
    /// enabled to catch any tampering between seal and commit.
    pub fn seal(&self, block: &mut Block) -> Result<(), ChainError> {
        block.next_validators_root = self.next_validators_root(block)?;
        // Stamp the M23 post-apply state commitments from a fresh trial. The
        // first trial above already ran; recomputing here keeps the seal
        // self-contained and idempotent.
        let mut trial = self.state.clone();
        trial.apply_block_inner(block, false)?;
        block.state_root = trial.state_root();
        block.accounts_root = trial.merkle_root();
        // M27: stamp the cert-signed sorted-by-cosine graph root alongside
        // `accounts_root`. Same trial, same idempotence contract.
        block.graph_root = trial.graph_merkle_root();
        // M30: stamp the cert-signed cumulative bridge-locks root alongside
        // `graph_root` / `accounts_root`. Same trial, same idempotence
        // contract.
        block.bridge_root = trial.bridge_merkle_root();
        Ok(())
    }

    /// Validate and commit a block atomically: the block must extend `head`, and
    /// the whole block is applied on a trial clone so a single invalid tx rolls
    /// the entire block back (no partial state).
    ///
    /// M23: a producer-built block is expected to carry the three cert-signed
    /// state commitments (`next_validators_root`, `state_root`,
    /// `accounts_root`) — the producer stamps them via [`Self::seal`] before
    /// consensus so validators sign over the sealed hash. This method
    /// re-runs the trial with the commitment checks enabled, so a block whose
    /// sealed commitments don't match the post-apply state is rejected with
    /// the appropriate [`ChainError`] variant.
    ///
    /// **Auto-stamp fallback.** If a block was built without [`Self::seal`]
    /// (typical of replay-from-log and tests that skip the seal step) the two
    /// new commitment fields are still zero. In that situation, instead of
    /// failing on the cross-check, stamp them from this trial — the block
    /// being committed is by definition honest (it was about to be accepted),
    /// and re-stamping matches what `seal` would have produced. The producer
    /// path that *does* seal is unaffected: the cross-check sees identical
    /// values and accepts without modification.
    pub fn commit(&mut self, block: &mut Block) -> Result<BlockReceipt, ChainError> {
        if block.prev_hash != self.head {
            return Err(ChainError::BadPrevHash);
        }
        let mut trial = self.state.clone();
        let receipt = trial.apply_block_inner(block, true)?;
        // M23: stamp the M23 commitments if the caller didn't (auto-stamp
        // fallback). For correctly-sealed blocks this is a no-op — the
        // cross-check inside `apply_block_inner` already validated them.
        if block.state_root == [0u8; 32] {
            block.state_root = trial.state_root();
        }
        if block.accounts_root == [0u8; 32] {
            block.accounts_root = trial.merkle_root();
        }
        // M27: same auto-stamp fallback for the cert-signed sorted-graph
        // commitment.
        if block.graph_root == [0u8; 32] {
            block.graph_root = trial.graph_merkle_root();
        }
        // M30: same auto-stamp fallback for the cert-signed bridge-locks
        // commitment.
        if block.bridge_root == [0u8; 32] {
            block.bridge_root = trial.bridge_merkle_root();
        }
        self.state = trial;
        self.head = receipt.hash;
        self.block_hashes.push(receipt.hash);
        Ok(receipt)
    }

    /// M33: trial-check whether `block` would commit cleanly on top of the
    /// current head, without mutating `self`. A distributed validator uses this
    /// to pre-validate a received consensus proposal before prevoting: the round
    /// state machine only checks that a proposed block is for the right height
    /// (`RoundState::valid_block`), so a byzantine proposer could otherwise get
    /// honest nodes to prevote a well-formed-but-unapplicable block. Dropping
    /// such a proposal here makes honest nodes time out and prevote nil, handing
    /// the round to the next proposer — no validator ever locks onto a block that
    /// can never commit.
    pub fn would_accept(&self, block: &Block) -> bool {
        if block.prev_hash != self.head || block.height != self.state.height + 1 {
            return false;
        }
        self.state.clone().apply_block_inner(block, true).is_ok()
    }

    /// Rebuild a chain by replaying `blocks` on top of `genesis` (e.g. from a
    /// [`store::BlockLog`]). Each block is validated exactly as if freshly
    /// committed, so a tampered log fails here rather than corrupting state.
    pub fn replay(genesis: Genesis, blocks: &[Block]) -> Result<Self, ChainError> {
        let mut chain = Chain::new(genesis);
        // Each block here is `&Block` borrowed from `&[Block]`; we need a
        // mutable handle to stamp the post-apply state commitments.
        for b in blocks {
            let mut b = b.clone();
            chain.commit(&mut b)?;
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
            // clone because `Chain::commit` stamps the M23 state commitments
            // into the block.
            let mut owned = b.clone();
            chain.commit(&mut owned).map_err(ReplayError::Chain)?;
        }
        Ok(chain)
    }
}

// --- canonical byte encoder lives in `codec` (shared by hashing + persistence)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::Commit;

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
            bridge_sources: vec![],
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
        let mut b = Block {
            height,
            prev_hash: chain.head,
            timestamp_days: height as f32,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs,
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        };
        // best-effort seal: valid blocks get the correct commitments; blocks the
        // negative tests build to fail earlier keep [0;32] and still fail at
        // their intended (earlier) check, since the commitments are checked last.
        let _ = chain.seal(&mut b);
        b
    }

    #[test]
    fn novel_submission_mints_and_conserves_supply() {
        let mut chain = Chain::new(base_genesis());
        let start_supply = chain.state.supply;
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]); // fresh domain 1
        let r = chain.commit(&mut b).unwrap();
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
        let mut b = block(&chain, 1, vec![dup]);
        let r = chain.commit(&mut b).unwrap();
        assert_eq!(r.rejected, 1);
        assert_eq!(r.minted, 0);
        assert_eq!(chain.state.treasury, 2 * MICRO); // whole stake slashed
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn deterministic_replay_same_state_root() {
        let build = || {
            let mut chain = Chain::new(base_genesis());
            let mut b1 = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
            chain.commit(&mut b1).unwrap();
            let mut b2 = block(&chain, 2, vec![novel_tx(2, 2, 2, 2.0)]);
            chain.commit(&mut b2).unwrap();
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
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadPrevHash)));
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
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0), bad]);
        assert!(chain.commit(&mut b).is_err());
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
        let mut b = block(&chain, 1, vec![broke]);
        assert!(matches!(
            chain.commit(&mut b),
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
        let mut b = block(&chain, 1, vec![forged]);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadSignature(1))));
    }

    #[test]
    fn tampering_a_signed_field_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut tx = novel_tx(1, 1, 1, 1.0); // validly signed
        tx.stake += 1; // mutate after signing -> signature no longer matches
        let mut b = block(&chain, 1, vec![tx]);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadSignature(1))));
    }

    #[test]
    fn merkle_root_authenticates_an_account_via_inclusion_proof() {
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        chain.commit(&mut b).unwrap();

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
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        chain.commit(&mut b).unwrap();
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
        chain.seal(&mut b).unwrap();
        chain.commit(&mut b).unwrap();
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
        let mut b_plain = block(&plain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        let mut b_changed = block(&changed, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b_changed.validator_updates = vec![vupd(24, 1)];
        changed.seal(&mut b_changed).unwrap();
        plain.commit(&mut b_plain).unwrap();
        changed.commit(&mut b_changed).unwrap();
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
            chain.commit(&mut b),
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
        // re-seal: block() sealed for an empty block; the appended op changes the
        // handed-off set, so recompute the commitment over the final contents.
        let _ = chain.seal(&mut b);
        b
    }

    #[test]
    fn bonding_makes_an_account_a_validator_next_height() {
        let mut chain = Chain::new(base_genesis());
        let bal_before = chain.state.accounts[&1].balance;
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO);
        let r = chain.commit(&mut b).unwrap();
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
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO); chain.commit(&mut b).unwrap();
        let bal_after_bond = chain.state.accounts[&1].balance;

        // unbond at height 2: power drops immediately (next height), funds locked
        let mut b = stake_block(&chain, 2, 1, BondKind::Unbond, 5 * MICRO);
        let r = chain.commit(&mut b).unwrap();
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
            let mut b = block(&chain, h, vec![]);
            chain.commit(&mut b).unwrap();
        }
        assert!(chain.state.unbonding.is_empty(), "matured out of the queue");
        assert_eq!(chain.state.accounts[&1].balance, bal_after_bond + 5 * MICRO, "funds returned");
        assert!(chain.state.supply_conserved());
    }

    #[test]
    fn bond_beyond_balance_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 1000 * MICRO);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::InsufficientBalance { .. })));
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
        assert!(chain.state.bonds.is_empty());
        assert_eq!(chain.state.bonded, 0);
    }

    #[test]
    fn unbond_beyond_bond_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut b = stake_block(&chain, 1, 1, BondKind::Unbond, MICRO);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::InsufficientBond { .. })));
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
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadSignature(1))));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn zero_amount_stakeop_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 0);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::ZeroStake(1))));
    }

    #[test]
    fn state_root_covers_bonded_stake() {
        let mut plain = Chain::new(base_genesis());
        let mut bonded = Chain::new(base_genesis());
        let mut plain_b = block(&plain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        plain.commit(&mut plain_b).unwrap();
        let mut b = block(&bonded, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.stake_ops = vec![StakeOp { account: 2, kind: BondKind::Bond, amount: 3 * MICRO, signature: [0u8; 64] }.signed(&kp(2))];
        bonded.seal(&mut b).unwrap();
        bonded.commit(&mut b).unwrap();
        assert_ne!(plain.state.state_root(), bonded.state.state_root());
    }

    #[test]
    fn a_full_bond_unbond_cycle_conserves_supply() {
        let mut chain = Chain::new(base_genesis());
        let start = chain.state.supply;
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 7 * MICRO); chain.commit(&mut b).unwrap();
        assert!(chain.state.supply_conserved());
        let mut b = stake_block(&chain, 2, 1, BondKind::Unbond, 4 * MICRO); chain.commit(&mut b).unwrap();
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
        // re-seal after appending evidence (see stake_block); best-effort so
        // bad-evidence blocks keep [0;32] and still fail at the evidence check.
        let _ = chain.seal(&mut b);
        b
    }

    #[test]
    fn slashing_burns_bonded_stake_to_treasury_and_removes_validator() {
        let mut chain = Chain::new(base_genesis());
        let start = chain.state.supply;
        // account 1 self-bonds and becomes a validator effective height 2
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO); chain.commit(&mut b).unwrap();
        assert_eq!(chain.state.validators.get(1).map(|v| v.power), Some(5 * MICRO));

        // at height 2 the validator is active — submit proof it double-signed
        let mut eb = evidence_block(&chain, 2, vec![evidence(1, 2, 0)]); let r = chain.commit(&mut eb).unwrap();
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
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 6 * MICRO); chain.commit(&mut b).unwrap();
        let mut b = stake_block(&chain, 2, 1, BondKind::Unbond, 2 * MICRO); chain.commit(&mut b).unwrap();
        assert_eq!(chain.state.bonded, 4 * MICRO);
        assert_eq!(chain.state.unbonding.len(), 1);
        assert_eq!(chain.state.validators.get(1).map(|v| v.power), Some(4 * MICRO));

        let mut eb = evidence_block(&chain, 3, vec![evidence(1, 3, 0)]); let r = chain.commit(&mut eb).unwrap();
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
        let mut eb = evidence_block(&chain, 1, vec![evidence(21, 1, 0)]); let r = chain.commit(&mut eb).unwrap();
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
        let mut b = evidence_block(&chain, 1, vec![ev]);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadEquivocationEvidence(21))));
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
        assert_eq!(chain.state.validators.len(), 3);
    }

    #[test]
    fn evidence_against_a_non_validator_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        // account 1 never bonded -> not in the validator set -> cannot be slashed
        let mut b = evidence_block(&chain, 1, vec![evidence(1, 1, 0)]);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadEquivocationEvidence(1))));
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
        let mut b = evidence_block(&chain, 1, vec![ev]);
        assert!(matches!(chain.commit(&mut b), Err(ChainError::BadEquivocationEvidence(21))));
        assert_eq!(chain.state.height, 0);
    }

    #[test]
    fn slashing_cannot_empty_the_validator_set() {
        let mut chain = Chain::new(base_genesis());
        // proof against every genesis validator in one block -> would empty the
        // set -> rejected, chain untouched.
        let mut b = evidence_block(
            &chain,
            1,
            vec![evidence(21, 1, 0), evidence(22, 1, 0), evidence(23, 1, 0)],
        );
        assert!(matches!(chain.commit(&mut b), Err(ChainError::EmptyValidatorSet)));
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
        let mut b1 = block(&live, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        live.commit(&mut b1).unwrap();
        log.append(&b1).unwrap();
        let mut b2 = Block {
            height: 2,
            prev_hash: live.head,
            timestamp_days: 2.0,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: vec![novel_tx(2, 2, 2, 2.0)],
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        };
        live.seal(&mut b2).unwrap();
        live.commit(&mut b2).unwrap();
        log.append(&b2).unwrap();

        // reopen the log, replay from genesis, and compare
        let blocks = BlockLog::open(&path).unwrap().read_all().unwrap();
        let replayed = Chain::replay(base_genesis(), &blocks).unwrap();

        assert_eq!(replayed.head, live.head);
        assert_eq!(replayed.state.state_root(), live.state.state_root());
        assert!(replayed.state.supply_conserved());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn genesis_commits_to_the_genesis_validator_set() {
        let chain = Chain::new(base_genesis());
        // a freshly-sealed empty block at height 1 (no updates) hands off exactly
        // the set genesis committed to.
        let b = block(&chain, 1, vec![]);
        assert_eq!(b.next_validators_root, chain.state.validators.merkle_root());
    }

    #[test]
    fn tampered_next_validators_root_is_rejected() {
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        // block() sealed the correct root; corrupt it — commit must reject.
        b.next_validators_root = [0xEE; 32];
        let err = chain.commit(&mut b).unwrap_err();
        assert!(
            matches!(err, ChainError::ValidatorRootMismatch { height: 1 }),
            "got {err:?}"
        );
    }

    #[test]
    fn seal_commits_to_the_post_apply_set_across_a_stake_change() {
        let mut chain = Chain::new(base_genesis());
        // account 1 bonds -> becomes a validator next height. The sealed root must
        // equal the set the block actually hands off to.
        let mut b = stake_block(&chain, 1, 1, BondKind::Bond, 5 * MICRO);
        assert_eq!(b.next_validators_root, chain.next_validators_root(&b).unwrap());
        chain.commit(&mut b).unwrap();
        assert_eq!(chain.state.validators.merkle_root(), b.next_validators_root);
        // and validator 1 is provable against that committed root.
        let v = chain.state.validators.get(1).unwrap();
        let proof = chain.state.validators.proof(1).unwrap();
        let leaf = merkle::leaf_hash(&v.merkle_leaf());
        assert!(merkle::verify(&b.next_validators_root, &leaf, &proof));
    }

    #[test]
    fn stale_root_after_appending_ops_is_rejected() {
        // a block sealed for empty contents, then given ops, no longer matches its
        // committed root — the enforcement catches the staleness.
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![]); // sealed for no-change
        let root_for_empty = b.next_validators_root;
        b.stake_ops = vec![
            StakeOp { account: 1, kind: BondKind::Bond, amount: 5 * MICRO, signature: [0u8; 64] }
                .signed(&kp(1)),
        ];
        // the op admits validator 1, so the real handed-off root differs.
        assert_ne!(chain.next_validators_root(&b).unwrap(), root_for_empty);
        let err = chain.commit(&mut b).unwrap_err();
        assert!(matches!(err, ChainError::ValidatorRootMismatch { .. }), "got {err:?}");
    }

    // ---- M23: state_root + accounts_root commitments in the header ----

    #[test]
    fn state_root_and_accounts_root_advance_across_each_block_in_a_certified_chain() {
        // commit a 3-block chain via ChainDriver; the stamps on each block must
        // match the post-apply state and must differ from height to height (the
        // chain is actually changing state).
        let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 21, 22, 23]
            .iter()
            .map(|&id| {
                let mut s = [0u8; 32];
                s[..8].copy_from_slice(&id.to_le_bytes());
                (id, s)
            })
            .collect();
        let mut d = crate::driver::ChainDriver::new(base_genesis(), seeds, 4);
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &BTreeSet::new()).unwrap().expect("block h1");
        d.submit(novel_tx(2, 2, 2, 2.0)).unwrap();
        d.produce(2.0, &BTreeSet::new()).unwrap().expect("block h2");
        d.submit(novel_tx(3, 3, 3, 3.0)).unwrap();
        d.produce(3.0, &BTreeSet::new()).unwrap().expect("block h3");

        let blocks = d.blocks();
        assert_eq!(blocks.len(), 3);
        // each block's stamped roots must equal the post-apply state at that height
        let mut replay = Chain::new(base_genesis());
        for b in blocks {
            // we replay independently; the new_chain's post-apply state == d's
            // (same genesis, same txs, same order — deterministic).
            let mut cloned = b.clone();
            replay.commit(&mut cloned).expect("replay");
            assert_eq!(b.state_root, replay.state.state_root(),
                "block {} state_root must equal post-apply state_root", b.height);
            assert_eq!(b.accounts_root, replay.state.merkle_root(),
                "block {} accounts_root must equal post-apply merkle_root", b.height);
        }
        // also assert that the roots differ across heights (the chain really moved)
        assert_ne!(blocks[0].state_root, blocks[1].state_root);
        assert_ne!(blocks[1].state_root, blocks[2].state_root);
        assert_ne!(blocks[0].accounts_root, blocks[1].accounts_root);
        assert_ne!(blocks[1].accounts_root, blocks[2].accounts_root);
    }

    #[test]
    fn state_root_mismatch_is_rejected() {
        // the dual of `tampered_next_validators_root_is_rejected` for M23: seal
        // a block normally, then flip state_root before commit. The commit
        // must return StateRootMismatch and the chain must not advance.
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.state_root = [0xCCu8; 32];
        let err = chain.commit(&mut b).unwrap_err();
        assert!(matches!(err, ChainError::StateRootMismatch { height: 1 }), "got {err:?}");
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
    }

    #[test]
    fn accounts_root_mismatch_is_rejected() {
        // same as above but for accounts_root — proves the cert-signed inclusion-proof
        // commitment is enforced independently from the full-state digest.
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.accounts_root = [0xDDu8; 32];
        let err = chain.commit(&mut b).unwrap_err();
        assert!(matches!(err, ChainError::AccountsRootMismatch { height: 1 }), "got {err:?}");
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
    }

    // --- M27: graph_root commitment + sorted-graph producer -------------

    #[test]
    fn graph_root_mismatch_is_rejected() {
        // the dual for the M27 sorted-graph commitment: seal a block normally,
        // then flip graph_root before commit. The commit must return
        // GraphRootMismatch and the chain must not advance.
        let mut chain = Chain::new(base_genesis());
        let mut b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        b.graph_root = [0xEEu8; 32];
        let err = chain.commit(&mut b).unwrap_err();
        assert!(matches!(err, ChainError::GraphRootMismatch { height: 1 }), "got {err:?}");
        assert_eq!(chain.state.height, 0, "rejected block rolls fully back");
    }

    #[test]
    fn graph_merkle_root_is_deterministic_for_a_fixed_pivot() {
        // Two consecutive calls must produce identical roots: the sorted
        // view is a pure function of (graph, CANONICAL_PIVOT). If the
        // determinism contract ever breaks the wallet's verifier cannot
        // re-derive the same root.
        let mut chain = Chain::new(base_genesis());
        chain.commit(&mut block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]).clone()).unwrap();
        let r1 = chain.state.graph_merkle_root();
        let r2 = chain.state.graph_merkle_root();
        assert_eq!(r1, r2);
    }

    #[test]
    fn graph_merkle_root_differs_from_accounts_root_for_a_non_trivial_graph() {
        // `accounts_root` commits graph nodes in INSERTION order (M25's
        // contract), `graph_root` commits them in SORTED-BY-COSINE order.
        // For any graph with at least two nodes whose insertion order does
        // not match the cosine order the two roots MUST differ — otherwise
        // the two commitments collapse and one of them is dead weight.
        let mut chain = Chain::new(base_genesis());
        chain.commit(&mut block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]).clone()).unwrap();
        let a = chain.state.merkle_root();
        let g = chain.state.graph_merkle_root();
        assert_ne!(a, g, "different leaf orderings must produce different roots");
    }

    #[test]
    fn graph_range_proof_round_trip() {
        // Every per-leaf proof returned by `graph_range_proof(a, b)` must
        // verify against `graph_merkle_root`; out-of-range slices return
        // `None`; an empty slice (`a == b`) returns `None`.
        let mut chain = Chain::new(base_genesis());
        chain.commit(&mut block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]).clone()).unwrap();
        let n = chain.state.graph.nodes.len();
        assert!(n >= 2, "genesis + 1 novel tx should give >= 2 graph nodes");

        // Whole-graph slice
        let proof = chain.state.graph_range_proof(0, n).expect("full slice");
        assert_eq!(proof.sub_root, chain.state.graph_merkle_root());
        for (id, node, merkle_proof) in &proof.entries {
            let leaf_hash = crate::merkle::leaf_hash(&node.merkle_leaf());
            assert!(crate::merkle::verify(&proof.sub_root, &leaf_hash, merkle_proof),
                "per-leaf proof failed for node_id {id}");
        }

        // Empty slice
        assert!(chain.state.graph_range_proof(0, 0).is_none());
        assert!(chain.state.graph_range_proof(1, 1).is_none());

        // Out-of-range
        assert!(chain.state.graph_range_proof(0, n + 1).is_none());
        assert!(chain.state.graph_range_proof(n, n + 1).is_none());

        // Inverted range
        assert!(chain.state.graph_range_proof(2, 1).is_none());
    }

    // --- M24: reviewer proof producer ------------------------------------

    fn one_block_chain() -> Chain {
        let mut chain = Chain::new(base_genesis());
        let b = block(&chain, 1, vec![novel_tx(1, 1, 1, 1.0)]);
        chain.commit(&mut b.clone()).unwrap();
        chain
    }

    #[test]
    fn reviewer_proof_round_trip() {
        // For each reviewer in the post-block state, the producer's proof
        // verifies against `ChainState::merkle_root()` when fed the canonical
        // leaf. Tampering reputation breaks it; an unknown id returns None.
        let chain = one_block_chain();
        for (&rid, &rep) in chain.state.reviewers.iter() {
            let proof = chain.state.reviewer_proof(rid).expect("proof exists");
            let leaf = merkle::leaf_hash(&Reviewer { id: rid, reputation: rep }.merkle_leaf());
            assert!(
                merkle::verify(&chain.state.merkle_root(), &leaf, &proof),
                "reviewer {rid} proof does not verify"
            );
            // tamper with reputation: the leaf changes, verification must fail.
            let bad_leaf = merkle::leaf_hash(&Reviewer { id: rid, reputation: rep + 0.1 }.merkle_leaf());
            assert!(!merkle::verify(&chain.state.merkle_root(), &bad_leaf, &proof));
        }
        assert!(chain.state.reviewer_proof(9_999).is_none());
    }

    #[test]
    fn reviewer_proof_index_lies_after_all_accounts() {
        // Reviewer leaves live in the second half of `merkle_leaves()`
        // (accounts first, then reviewers, both in BTreeMap order). Account
        // proofs thus have indices < reviewer proof indices. We verify the
        // same property by recomputing both indices from the producer's
        // helper (n_accounts + rindex for reviewer; the first slot for
        // accounts 1..N), then confirming both proofs open against the
        // shared `merkle_root()`.
        let chain = one_block_chain();
        let leaves = chain.state.merkle_leaves();
        assert_eq!(
            leaves.len(),
            chain.state.accounts.len()
                + chain.state.reviewers.len()
                + chain.state.graph.nodes.len(),
            "leaves must include accounts, reviewers, AND graph nodes (M25)"
        );
        // First leaf is the smallest account id (account 1); the last leaf is
        // the largest reviewer id.
        let first_account_leaf = merkle::leaf_hash(
            &chain.state.accounts.get(&1).unwrap().merkle_leaf(1),
        );
        let last_reviewer_leaf = merkle::leaf_hash(
            &Reviewer {
                id: 12,
                reputation: chain.state.reviewers[&12],
            }
            .merkle_leaf(),
        );
        let pos_first = leaves.iter().position(|h| *h == first_account_leaf).expect("account 1 leaf");
        let pos_last = leaves.iter().position(|h| *h == last_reviewer_leaf).expect("reviewer 12 leaf");
        assert!(pos_first < pos_last, "account index {pos_first} should be < reviewer index {pos_last}");
        // And both proofs verify against the shared root.
        let acct_proof = chain.state.account_proof(1).expect("account 1 proof");
        let rev_proof = chain.state.reviewer_proof(12).expect("reviewer 12 proof");
        assert!(merkle::verify(&chain.state.merkle_root(), &first_account_leaf, &acct_proof));
        assert!(merkle::verify(&chain.state.merkle_root(), &last_reviewer_leaf, &rev_proof));
    }

    /// M25: every graph node in a multi-block chain has an inclusion proof
    /// that verifies against `merkle_root()`. Tamper any leaf byte (here:
    /// first embedding float) and the proof must fail. Out-of-range index
    /// returns `None`.
    #[test]
    fn graph_node_proof_round_trip() {
        let chain = one_block_chain();
        let n = chain.state.graph.nodes.len();
        assert!(n > 0, "demo genesis must seed at least one graph node");
        let root = chain.state.merkle_root();
        for idx in 0..n {
            let node = chain.state.graph.nodes[idx].clone();
            let leaf = merkle::leaf_hash(&node.merkle_leaf());
            let proof = chain
                .state
                .graph_node_proof(idx)
                .expect("graph_node_proof must return Some for in-range idx");
            assert!(
                merkle::verify(&root, &leaf, &proof),
                "graph node #{idx} (id={}) proof should verify",
                node.node_id
            );
            // Tamper: bump the first embedding float. The proof should fail.
            let mut bad = node.clone();
            bad.embedding[0] += 1.0;
            let bad_leaf = merkle::leaf_hash(&bad.merkle_leaf());
            assert!(
                !merkle::verify(&root, &bad_leaf, &proof),
                "tampered graph node #{idx} proof must NOT verify"
            );
        }
        // Out of range -> None.
        assert!(chain.state.graph_node_proof(n).is_none());
        assert!(chain.state.graph_node_proof(n + 1).is_none());
    }

    /// M25: graph leaves occupy the third slice of `merkle_leaves()` —
    /// AFTER all accounts and reviewers. So `graph_node_proof(idx)`'s
    /// internal index must equal `n_accounts + n_reviewers + idx`. We
    /// verify by finding the graph-node leaf's position in `merkle_leaves()`
    /// and asserting it's strictly past every account and reviewer leaf.
    #[test]
    fn merkle_leaves_include_graph_nodes_after_accounts_and_reviewers() {
        let chain = one_block_chain();
        let leaves = chain.state.merkle_leaves();
        let n_acct = chain.state.accounts.len();
        let n_rev = chain.state.reviewers.len();
        let n_graph = chain.state.graph.nodes.len();
        assert_eq!(leaves.len(), n_acct + n_rev + n_graph);

        // Spot-check the first graph node's position is past every account/reviewer leaf.
        let g_leaf = merkle::leaf_hash(&chain.state.graph.nodes[0].merkle_leaf());
        let g_pos = leaves
            .iter()
            .position(|h| *h == g_leaf)
            .expect("graph node leaf must appear in merkle_leaves()");
        assert!(
            g_pos >= n_acct + n_rev,
            "graph node position {g_pos} must lie after accounts+reviewers ({})",
            n_acct + n_rev
        );

        // And the producer's proof is a Merkle verify against the same root.
        let proof = chain.state.graph_node_proof(0).expect("graph_node_proof(0)");
        assert!(merkle::verify(&chain.state.merkle_root(), &g_leaf, &proof));
    }

    // ---- M30: cross-chain bridge lock (consensus side) ----

    /// Lock drains the source's balance into `bridge_locked` and appends to the
    /// cumulative `bridge_locks` map; supply stays conserved (a redistribution
    /// within supply), and the next block's `bridge_root` commits to the new
    /// leaf.
    #[test]
    fn bridge_lock_drains_balance_into_bridge_locked_and_conserves_supply() {
        let mut chain = Chain::new(base_genesis());
        let start_supply = chain.state.supply;
        let start_balance = chain.state.accounts.get(&1).unwrap().balance;
        let lock = BridgeLock {
            account: 1,
            amount: 5 * MICRO,
            dest_chain: [0xAA; 32],
            dest_account: 42,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut b = block(&chain, 1, Vec::new());
        b.bridge_locks.push(lock);
        chain.seal(&mut b).unwrap();
        let _ = chain.commit(&mut b).unwrap();

        assert_eq!(
            chain.state.accounts.get(&1).unwrap().balance,
            start_balance - 5 * MICRO,
            "source balance should drop by lock amount"
        );
        assert_eq!(
            chain.state.bridge_locked, 5 * MICRO,
            "bridge_locked pool should hold the locked amount"
        );
        assert_eq!(
            chain.state.bridge_locks.len(), 1,
            "exactly one lock in the cumulative map"
        );
        assert_eq!(
            chain.state.next_lock_id, 1,
            "lock_id counter should have advanced"
        );
        assert_eq!(
            chain.state.supply, start_supply,
            "supply must be conserved (lock is a redistribution within supply)"
        );
        assert!(
            chain.state.supply_conserved(),
            "supply_conserved() must hold after a lock"
        );
    }

    /// Two locks (same block) accumulate; cumulative root matches.
    #[test]
    fn bridge_merkle_root_changes_when_lock_added_and_stable_across_no_lock_block() {
        let mut chain = Chain::new(base_genesis());
        // Empty bridge_root at genesis (no locks).
        let root_empty = chain.state.bridge_merkle_root();
        assert_eq!(chain.state.bridge_locks.len(), 0);

        // Lock at height 1.
        let lock1 = BridgeLock {
            account: 1,
            amount: 2 * MICRO,
            dest_chain: [0xBB; 32],
            dest_account: 7,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut b = block(&chain, 1, Vec::new());
        b.bridge_locks.push(lock1);
        chain.seal(&mut b).unwrap();
        let _ = chain.commit(&mut b).unwrap();
        let root_one_lock = chain.state.bridge_merkle_root();
        assert_ne!(root_one_lock, root_empty, "lock must change the root");

        // No-lock block: root must stay stable (locks are cumulative).
        let mut b2 = block(&chain, 2, Vec::new());
        chain.seal(&mut b2).unwrap();
        let _ = chain.commit(&mut b2).unwrap();
        assert_eq!(
            chain.state.bridge_merkle_root(),
            root_one_lock,
            "no-lock block must not change bridge_root (cumulative)"
        );

        // Second lock.
        let lock2 = BridgeLock {
            account: 2,
            amount: 3 * MICRO,
            dest_chain: [0xCC; 32],
            dest_account: 8,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(2));
        let mut b3 = block(&chain, 3, Vec::new());
        b3.bridge_locks.push(lock2);
        chain.seal(&mut b3).unwrap();
        let _ = chain.commit(&mut b3).unwrap();
        assert_ne!(
            chain.state.bridge_merkle_root(),
            root_one_lock,
            "second lock must change the root"
        );
    }

    /// bridge_lock_proof produces a Merkle proof that verifies against
    /// header.bridge_root — same shape as the M22/M25/M27 inclusion proofs,
    /// just on a different cumulative commitment.
    #[test]
    fn bridge_lock_proof_verifies_against_header_bridge_root() {
        let mut chain = Chain::new(base_genesis());
        let lock = BridgeLock {
            account: 1,
            amount: 4 * MICRO,
            dest_chain: [0xDD; 32],
            dest_account: 99,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut b = block(&chain, 1, Vec::new());
        b.bridge_locks.push(lock.clone());
        chain.seal(&mut b).unwrap();
        let _ = chain.commit(&mut b).unwrap();

        // Producer side: pull the inclusion proof and verify against the
        // cert-signed `bridge_root`.
        let proof = chain.state.bridge_lock_proof(0).expect("lock exists");
        let leaf = merkle::leaf_hash(&lock.merkle_leaf(0));
        assert!(merkle::verify(&b.bridge_root, &leaf, &proof));
    }

    /// Tampered bridge_root on apply → BridgeRootMismatch.
    #[test]
    fn bridge_root_mismatch_is_rejected_on_apply() {
        let mut chain = Chain::new(base_genesis());
        let lock = BridgeLock {
            account: 1,
            amount: MICRO,
            dest_chain: [0xEE; 32],
            dest_account: 1,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut b = block(&chain, 1, Vec::new());
        b.bridge_locks.push(lock);
        // Re-seal so state_root / accounts_root / graph_root all reflect the
        // post-apply shape; THEN tamper only bridge_root to exercise the
        // BridgeRootMismatch path (and not some earlier root check).
        chain.seal(&mut b).unwrap();
        b.bridge_root = [0xFFu8; 32];
        let err = chain.commit(&mut b).unwrap_err();
        assert!(matches!(err, ChainError::BridgeRootMismatch { height: 1 }));
    }

    // ---- M31: on-chain follow + redeem ----

    /// M31 helper: the source-chain identity a destination chain's genesis
    /// registers as an allowed `bridge_source` (genesis_hash + genesis set).
    fn source_identity(g: &Genesis) -> (Hash, ValidatorSet) {
        let h = ChainState::genesis(g.clone()).1;
        let s = ChainState::genesis(g.clone()).0.validators.clone();
        (h, s)
    }

    /// M31 helper: seal `b` on `chain`, certify it under the active set
    /// (using the matching keypairs), and commit it. Returns the cert so a
    /// relayer can use it for `BridgeHeader.cert` / `BridgeRedeem.source_cert`.
    fn seal_certify_commit(chain: &mut Chain, b: &mut Block) -> Commit {
        chain.seal(b).expect("seal");
        let set = chain.state.validators.clone();
        let kps: BTreeMap<u64, Keypair> = set
            .validators()
            .iter()
            .map(|v| (v.id, kp(v.id)))
            .collect();
        let voters: Vec<u64> = set.validators().iter().map(|v| v.id).collect();
        let cert = consensus::commit_block(&set, &kps, b, 0, &voters).expect("certify");
        chain.commit(b).expect("commit");
        cert
    }

    /// M31 helper: build a destination chain B whose genesis registers
    /// `source_genesis_hash` as an allowed bridge source, anchored on
    /// `source_genesis_set`. Returns the live chain and B's own
    /// `genesis_hash` (which the source lock's `dest_chain` must equal).
    fn dest_chain_b(
        source_genesis_hash: Hash,
        source_genesis_set: ValidatorSet,
    ) -> (Chain, Hash) {
        let mut g = base_genesis();
        g.bridge_sources = vec![(
            source_genesis_hash,
            source_genesis_set
                .validators()
                .iter()
                .map(|v| (v.id, v.pubkey, v.power))
                .collect(),
        )];
        // M31 demo convention: every destination chain mints bridged supply
        // to account 5 — add it to B's genesis if base_genesis doesn't.
        let has_5 = g.accounts.iter().any(|(id, _, _)| *id == 5);
        if !has_5 {
            g.accounts.push((5, 0, kp(5).public()));
        }
        let b_genesis_hash = ChainState::genesis(g.clone()).1;
        (Chain::new(g), b_genesis_hash)
    }

    /// M31 helper: build a single-block source chain A carrying one
    /// `BridgeLock` destined for `dest_genesis_hash`. Returns everything a
    /// relayer needs to assemble `BridgeHeader` + `BridgeRedeem` on chain B.
    fn build_source_lock_envelope(
        dest_genesis_hash: Hash,
    ) -> (Block, Commit, merkle::Proof, BridgeLock) {
        let mut chain_a = Chain::new(base_genesis());
        let lock = BridgeLock {
            account: 1,
            amount: 10 * MICRO,
            dest_chain: dest_genesis_hash,
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut ab1 = block(&chain_a, 1, Vec::new());
        ab1.bridge_locks.push(lock.clone());
        let cert = seal_certify_commit(&mut chain_a, &mut ab1);
        let proof = chain_a.state.bridge_lock_proof(0).expect("proof");
        (ab1, cert, proof, lock)
    }

    /// End-to-end: chain A locks → chain B follows A's header in one
    /// block, then redeems the lock in the next block. The destination
    /// account is credited on-chain, supply grows by the same amount
    /// (invariant preserved), and `bridge_minted` records the audit counter.
    #[test]
    fn bridge_redeem_mints_to_dest_and_conserves_supply() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        let (a_block_1, a_cert_1, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);
        let header_a1 = a_block_1.header();

        // Block on B at height 1: follow A's height 1 (next_set = a_genesis_set
        // since A has no validator changes).
        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: header_a1.clone(),
            cert: a_cert_1.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut b_b1 = block(&chain_b, 1, Vec::new());
        b_b1.bridge_headers.push(follow);
        chain_b.seal(&mut b_b1).unwrap();
        chain_b.commit(&mut b_b1).expect("B height 1 follow commits");

        // Block on B at height 2: redeem lock 0.
        let dest_before = chain_b.state.accounts.get(&5).unwrap().balance;
        let supply_before = chain_b.state.supply;
        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: header_a1.clone(),
            source_cert: a_cert_1.clone(),
            lock_id: 0,
            lock: lock.clone(),
            proof,
        };
        let mut b_b2 = block(&chain_b, 2, Vec::new());
        b_b2.bridge_redeems.push(redeem);
        chain_b.seal(&mut b_b2).unwrap();
        chain_b.commit(&mut b_b2).expect("B height 2 redeem commits");

        let amount = lock.amount;
        assert_eq!(
            chain_b.state.accounts.get(&5).unwrap().balance,
            dest_before + amount
        );
        assert_eq!(chain_b.state.supply, supply_before + amount);
        assert_eq!(chain_b.state.bridge_minted, amount);
        assert!(chain_b.state.supply_conserved());

        // And the consumed set recorded the lock_id (so a replay would
        // now fail — tested separately below).
        let src = chain_b
            .state
            .bridge_sources
            .get(&a_genesis_hash)
            .expect("source");
        assert!(src.consumed.contains(&0));
    }

    /// `apply_bridge_header` advances the follower: height++, head moves to
    /// the new header hash, set adopts the next_set.
    #[test]
    fn bridge_follow_advances_source_follower() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        // B with A as a registered source.
        let (mut chain_b, _) = dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        // Build A's height-1 block (empty — we only need the certified
        // header + cert for the follower-advance test).
        let mut chain_a = Chain::new(ga);
        let mut a_b1 = block(&chain_a, 1, Vec::new());
        let cert = seal_certify_commit(&mut chain_a, &mut a_b1);
        let header_a1 = a_b1.header();

        // Before follow: follower at height 0, head = a_genesis_hash.
        let s0 = chain_b
            .state
            .bridge_sources
            .get(&a_genesis_hash)
            .expect("seeded");
        assert_eq!(s0.height, 0);
        assert_eq!(s0.head, a_genesis_hash);
        assert_eq!(s0.set.merkle_root(), a_genesis_set.merkle_root());

        // Apply follow in a block on B.
        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: header_a1.clone(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut b1 = block(&chain_b, 1, Vec::new());
        b1.bridge_headers.push(follow);
        chain_b.seal(&mut b1).unwrap();
        chain_b.commit(&mut b1).expect("follow commits");

        // After follow: height=1, head=A's block-1 hash, set unchanged.
        let s1 = chain_b
            .state
            .bridge_sources
            .get(&a_genesis_hash)
            .expect("tracked");
        assert_eq!(s1.height, 1);
        assert_eq!(s1.head, a_b1.hash());
        assert_eq!(s1.set.merkle_root(), a_genesis_set.merkle_root());
    }

    /// Redeem against an un-registered source → UnknownBridgeSource.
    #[test]
    fn bridge_redeem_unknown_source_is_rejected() {
        // Build a real certified lock on A; we won't register A on B.
        let ga = base_genesis();
        let (a_genesis_hash, _) = source_identity(&ga);
        let mut chain_a = Chain::new(ga);
        let lock = BridgeLock {
            account: 1,
            amount: 5 * MICRO,
            dest_chain: [0xCC; 32],
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut ab1 = block(&chain_a, 1, Vec::new());
        ab1.bridge_locks.push(lock.clone());
        let cert = seal_certify_commit(&mut chain_a, &mut ab1);
        let proof = chain_a.state.bridge_lock_proof(0).expect("proof");

        // B has NO bridge_sources declared.
        let mut chain_b = Chain::new(base_genesis());
        let bad = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: ab1.header(),
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb = block(&chain_b, 1, Vec::new());
        bb.bridge_redeems.push(bad);
        // Don't seal — `commit` does its own trial apply; a bad redeem
        // surfaces as an `Err` from `commit` (no state change).
        let err = chain_b.commit(&mut bb).unwrap_err();
        assert!(matches!(err, ChainError::UnknownBridgeSource(s) if s == a_genesis_hash));
    }

    /// Redeem before the follower has reached that height →
    /// BridgeSourceNotFollowed.
    #[test]
    fn bridge_redeem_before_follow_is_rejected() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        // B with A registered, follower still at height 0 (no follow yet).
        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        // Build A's lock at height 1 (cert + proof).
        let (a_block, cert, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);
        // Try to redeem WITHOUT staging a BridgeHeader first.
        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_block.header(),
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb = block(&chain_b, 1, Vec::new());
        bb.bridge_redeems.push(redeem);
        let err = chain_b.commit(&mut bb).unwrap_err();
        assert!(matches!(err, ChainError::BridgeSourceNotFollowed { .. }));
    }

    /// Tampered `lock.amount` → leaf no longer matches the cert-signed
    /// bridge_root → BridgeInclusionInvalid.
    #[test]
    fn bridge_redeem_tampered_amount_fails_inclusion() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        let (a_block, cert, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);

        // Follow A's height 1 first.
        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: a_block.header(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut bb1 = block(&chain_b, 1, Vec::new());
        bb1.bridge_headers.push(follow);
        chain_b.seal(&mut bb1).unwrap();
        chain_b.commit(&mut bb1).expect("follow commits");

        // Redeem with TAMPERED amount.
        let mut tampered = lock.clone();
        tampered.amount += 1;
        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_block.header(),
            source_cert: cert,
            lock_id: 0,
            lock: tampered,
            proof,
        };
        let mut bb2 = block(&chain_b, 2, Vec::new());
        bb2.bridge_redeems.push(redeem);
        let err = chain_b.commit(&mut bb2).unwrap_err();
        assert!(matches!(err, ChainError::BridgeInclusionInvalid { .. }));
    }

    /// Tampered source_header.bridge_root → cert-binding check fails first
    /// (since the tampered root invalidates the header hash and the
    /// cert-signed state_root no longer matches) → BridgeCertInvalid.
    #[test]
    fn bridge_redeem_tampered_bridge_root_fails_cert() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        let (a_block, cert, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);

        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: a_block.header(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut bb1 = block(&chain_b, 1, Vec::new());
        bb1.bridge_headers.push(follow);
        chain_b.seal(&mut bb1).unwrap();
        chain_b.commit(&mut bb1).expect("follow commits");

        // Redeem with a TAMPERED bridge_root in the source header.
        let mut tampered_header = a_block.header();
        tampered_header.bridge_root = [0xFFu8; 32];
        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: tampered_header,
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb2 = block(&chain_b, 2, Vec::new());
        bb2.bridge_redeems.push(redeem);
        let err = chain_b.commit(&mut bb2).unwrap_err();
        assert!(matches!(err, ChainError::BridgeCertInvalid { .. }));
    }

    /// Lock destined for a different chain → BridgeWrongDestination.
    #[test]
    fn bridge_redeem_wrong_destination_is_rejected() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);
        let c_genesis_hash = [0xCC; 32];

        let (mut chain_b, _) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        // Build a lock on A destined for chain C, NOT B.
        let mut chain_a = Chain::new(ga);
        let lock = BridgeLock {
            account: 1,
            amount: 2 * MICRO,
            dest_chain: c_genesis_hash,
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let mut ab1 = block(&chain_a, 1, Vec::new());
        ab1.bridge_locks.push(lock.clone());
        let cert = seal_certify_commit(&mut chain_a, &mut ab1);
        let proof = chain_a.state.bridge_lock_proof(0).expect("proof");

        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: ab1.header(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut bb1 = block(&chain_b, 1, Vec::new());
        bb1.bridge_headers.push(follow);
        chain_b.seal(&mut bb1).unwrap();
        chain_b.commit(&mut bb1).expect("follow commits");

        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: ab1.header(),
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb2 = block(&chain_b, 2, Vec::new());
        bb2.bridge_redeems.push(redeem);
        let err = chain_b.commit(&mut bb2).unwrap_err();
        match err {
            ChainError::BridgeWrongDestination { expected, got } => {
                assert_eq!(got, c_genesis_hash);
                assert_eq!(expected, chain_b.state.genesis_hash);
            }
            other => panic!("expected BridgeWrongDestination, got {:?}", other),
        }
    }

    /// Replay (same lock_id redeemed twice in two different blocks) →
    /// BridgeAlreadyRedeemed.
    #[test]
    fn bridge_replay_is_rejected() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        let (a_block, cert, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);

        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: a_block.header(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut bb1 = block(&chain_b, 1, Vec::new());
        bb1.bridge_headers.push(follow);
        chain_b.seal(&mut bb1).unwrap();
        chain_b.commit(&mut bb1).expect("follow commits");

        // First redeem: ok.
        let redeem1 = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_block.header(),
            source_cert: cert.clone(),
            lock_id: 0,
            lock: lock.clone(),
            proof: proof.clone(),
        };
        let mut bb2 = block(&chain_b, 2, Vec::new());
        bb2.bridge_redeems.push(redeem1);
        chain_b.seal(&mut bb2).unwrap();
        chain_b.commit(&mut bb2).expect("first redeem commits");

        // Replay same lock_id in a later block.
        let redeem2 = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_block.header(),
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb3 = block(&chain_b, 3, Vec::new());
        bb3.bridge_redeems.push(redeem2);
        let err = chain_b.commit(&mut bb3).unwrap_err();
        assert!(matches!(err, ChainError::BridgeAlreadyRedeemed { .. }));
    }

    /// state_root advances on every block (it folds `self.height`); the
    /// height-independent commitments — accounts_root, bridge_root — must
    /// stay stable across a no-op block on top of a redeem.
    #[test]
    fn bridge_redeem_changes_state_root_and_accounts_root_stable_across_no_op() {
        let ga = base_genesis();
        let (a_genesis_hash, a_genesis_set) = source_identity(&ga);

        let (mut chain_b, b_genesis_hash) =
            dest_chain_b(a_genesis_hash, a_genesis_set.clone());

        let (a_block, cert, proof, lock) =
            build_source_lock_envelope(b_genesis_hash);

        let root_initial = chain_b.state.state_root();

        let follow = BridgeHeader {
            source_chain: a_genesis_hash,
            header: a_block.header(),
            cert: cert.clone(),
            next_set: a_genesis_set.clone(),
        };
        let mut bb1 = block(&chain_b, 1, Vec::new());
        bb1.bridge_headers.push(follow);
        chain_b.seal(&mut bb1).unwrap();
        chain_b.commit(&mut bb1).expect("follow");
        let root_after_follow = chain_b.state.state_root();
        assert_ne!(root_after_follow, root_initial);

        let redeem = BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_block.header(),
            source_cert: cert,
            lock_id: 0,
            lock,
            proof,
        };
        let mut bb2 = block(&chain_b, 2, Vec::new());
        bb2.bridge_redeems.push(redeem);
        chain_b.seal(&mut bb2).unwrap();
        chain_b.commit(&mut bb2).expect("redeem");
        let root_after_redeem = chain_b.state.state_root();
        let accounts_root_after_redeem = chain_b.state.merkle_root();
        let bridge_root_after_redeem = chain_b.state.bridge_merkle_root();
        assert_ne!(root_after_redeem, root_after_follow);

        // No-op block: state_root advances (height++), but the
        // height-independent commitments stay stable — the redeem's
        // balance credit and bridge_root persist into the no-op block.
        let mut bb3 = block(&chain_b, 3, Vec::new());
        chain_b.seal(&mut bb3).unwrap();
        chain_b.commit(&mut bb3).expect("no-op commits");
        assert_ne!(
            chain_b.state.state_root(),
            root_after_redeem,
            "state_root folds height so it must advance",
        );
        assert_eq!(
            chain_b.state.merkle_root(),
            accounts_root_after_redeem,
            "accounts_root must be stable across a no-op block",
        );
        assert_eq!(
            chain_b.state.bridge_merkle_root(),
            bridge_root_after_redeem,
            "bridge_root must be stable across a no-op block",
        );
        // And the minted balance from the redeem survives the no-op block.
        assert_eq!(
            chain_b.state.accounts.get(&5).unwrap().balance,
            10 * MICRO
        );
    }
}
