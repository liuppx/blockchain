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
use crate::merkle;
use crate::validator::{Validator, ValidatorSet, ValidatorUpdate};
use crate::{Block, BondKind, Genesis, Hash, PubKey};

/// M24: which O(log n) inclusion proof the wallet is asking for (or
/// receiving) over the `GetProof` / `Proof` gossip pair. All kinds
/// ultimately verify against a cert-signed `BlockHeader` —
/// [`ProofKind::Account`], [`ProofKind::Reviewer`], and
/// [`ProofKind::GraphNode`] against `header.accounts_root`,
/// [`ProofKind::Validator`] against `header.next_validators_root`.
/// `GraphNode` (M25) joins the same tree as Account/Reviewer — graph
/// leaves occupy the third slice of [`crate::ChainState::merkle_leaves`]
/// (insertion order) so one root slot certifies all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProofKind {
    Account = 0,
    Reviewer = 1,
    Validator = 2,
    GraphNode = 3,
}

/// M24: typed leaf + proof for a single [`ProofKind`]. The wallet
/// recomputes the canonical leaf via [`Self::leaf`] and runs
/// `merkle::verify(root, merkle::leaf_hash(&leaf), &self.proof)`. The
/// leaf the prover hands back is **not trusted** — see
/// `verify_proof_against_header`.
#[derive(Clone, Debug, PartialEq)]
pub enum ProofEntry {
    Account {
        id: u64,
        account: crate::Account,
        proof: merkle::Proof,
    },
    Reviewer {
        id: u64,
        reputation: f32,
        proof: merkle::Proof,
    },
    Validator {
        id: u64,
        validator: Validator,
        proof: merkle::Proof,
    },
    /// M25: a single cognitive-graph node (kernel of concept) plus its
    /// `accounts_root` Merkle proof. `node_id` is the monotonic id
    /// assigned at insertion by `engine::CognitiveGraph::add`; it's the
    /// stable external identity for this node across the chain.
    GraphNode {
        node_id: u64,
        graph_node: crate::engine::GraphNode,
        proof: merkle::Proof,
    },
}

impl ProofEntry {
    pub fn kind(&self) -> ProofKind {
        match self {
            ProofEntry::Account { .. } => ProofKind::Account,
            ProofEntry::Reviewer { .. } => ProofKind::Reviewer,
            ProofEntry::Validator { .. } => ProofKind::Validator,
            ProofEntry::GraphNode { .. } => ProofKind::GraphNode,
        }
    }

    pub fn id(&self) -> u64 {
        match self {
            ProofEntry::Account { id, .. }
            | ProofEntry::Reviewer { id, .. }
            | ProofEntry::Validator { id, .. } => *id,
            ProofEntry::GraphNode { node_id, .. } => *node_id,
        }
    }

    pub fn proof(&self) -> &merkle::Proof {
        match self {
            ProofEntry::Account { proof, .. }
            | ProofEntry::Reviewer { proof, .. }
            | ProofEntry::Validator { proof, .. } => proof,
            ProofEntry::GraphNode { proof, .. } => proof,
        }
    }

    /// Local recomputation of the leaf bytes the verifier will hash.
    /// Mirrors the on-chain `merkle_leaves()` layout byte-for-byte.
    pub fn leaf(&self) -> Vec<u8> {
        match self {
            ProofEntry::Account { id, account, .. } => account.merkle_leaf(*id),
            ProofEntry::Reviewer { id, reputation, .. } => {
                crate::Reviewer { id: *id, reputation: *reputation }.merkle_leaf()
            }
            ProofEntry::Validator { validator, .. } => validator.merkle_leaf(),
            ProofEntry::GraphNode { graph_node, .. } => graph_node.merkle_leaf(),
        }
    }
}

/// M26: cert-signed claim about the k nearest neighbours of `query`
/// against the cognitive graph at a cert-signed height. The verifier
/// recomputes the kNN ranking itself from the per-neighbour Merkle
/// proofs — the prover's `node_id` order is **not trusted** beyond the
/// leaf-hash check.
///
/// `k` is the **requested** size. `neighbours` may be longer than `k`
/// when the engine's tie-breaking rule (`rank_by_cosine`) reports a
/// boundary tie — the wallet enforces the same rule, so a prover who
/// omits a tied neighbour will be rejected with `KnnRankingMismatch`.
#[derive(Clone, Debug)]
pub struct KnnClaim {
    pub query: crate::engine::Embedding,
    pub k: usize,
    /// Sorted (cosine-desc, node_id-asc). May have `len > k` on ties.
    /// Each tuple is `(node_id, graph_node_leaves_body, merkle_proof)`.
    pub neighbours: Vec<(u64, crate::engine::GraphNode, merkle::Proof)>,
}

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
    /// The set the tracker derived (or was handed, in [`ValidatorTracker::follow_committed`])
    /// does not match the block's `next_validators_root` commitment — a divergent
    /// transition or a forged/mismatched next set.
    ValidatorRootMismatch { height: u64 },
    /// A validator-membership Merkle proof did not verify against a certified
    /// block's `next_validators_root`.
    MembershipProofInvalid { height: u64 },
    /// M26: the kNN claim's neighbour set was empty (`k == 0` or the
    /// certified graph is empty for this query). Returned instead of
    /// returning `Ok(())` so the wallet can distinguish "no answer" from
    /// "verified answer".
    EmptyKnnQuery { height: u64 },
    /// M26: the prover's kNN ranking does not match the ranking the wallet
    /// re-derives from the verified leaves. Could be a malicious prover, a
    /// stale snapshot, or — most commonly — a tied node the prover omitted.
    KnnRankingMismatch { height: u64 },
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
            LightError::ValidatorRootMismatch { height } => {
                write!(f, "light: validator set at height {height} does not match next_validators_root commitment")
            }
            LightError::MembershipProofInvalid { height } => {
                write!(f, "light: validator membership proof invalid against block {height}")
            }
            LightError::EmptyKnnQuery { height } => {
                write!(f, "light: kNN query produced no neighbours against block {height}")
            }
            LightError::KnnRankingMismatch { height } => {
                write!(f, "light: kNN claim ranking disagrees with re-derived ranking at block {height}")
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
        // genesis-block hash: height 0, zero prev, no txs / updates / ops, and
        // the genesis set as its next-set commitment — exactly the block
        // `ChainState::genesis` hashes for its head. M23 also stamps
        // `state_root`/`accounts_root` against the genesis state so the
        // light client's hash matches the live chain's hash byte-for-byte.
        let head = Block {
            height: 0,
            prev_hash: [0u8; 32],
            timestamp_days: g.timestamp_days,
            next_validators_root: set.merkle_root(),
            state_root: crate::ChainState::state_root_for_genesis(g),
            accounts_root: crate::ChainState::merkle_root_for_genesis(g),
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

    /// Steps 1-4 shared by [`Self::follow`], [`Self::follow_committed`] and the
    /// M22 header-only [`Self::follow_header`]: the (header|block) must extend
    /// the tracked head by exactly one height, chain to it, be certified by
    /// *exactly* the supplied certificate, and that certificate must be a real
    /// \> 2/3 quorum of the currently tracked set. Read-only: on error the
    /// tracker is untouched. Returns `(header_hash, certifying_power)` so the
    /// header-only path uses `header.hash()` and the full path uses
    /// `block.hash()`.
    fn verify_cert_header(
        &self,
        height: u64,
        prev_hash: &Hash,
        header_hash: &Hash,
        cert: &Commit,
    ) -> Result<u64, LightError> {
        // 1. the (header|block) must extend our head by exactly one height ...
        if height != self.height + 1 {
            return Err(LightError::BadHeight {
                expected: self.height + 1,
                got: height,
            });
        }
        // 2. ... and chain to the head we last followed (no splicing).
        if *prev_hash != self.head {
            return Err(LightError::ForkDetected { height });
        }
        // 3. the certificate must certify exactly this (header|block).
        if cert.height != height || cert.block_hash != *header_hash {
            return Err(LightError::CertificateMismatch { height });
        }
        // 4. the certificate must be a real > 2/3 quorum of the set active for
        //    this height — the set in force *before* this block applies.
        let power = cert.verify(&self.set).map_err(LightError::Consensus)?;
        Ok(power)
    }

    /// The full-Block variant of `verify_cert_header` (M20/M21 path).
    fn verify_cert(&self, block: &Block, cert: &Commit) -> Result<(Hash, u64), LightError> {
        let block_hash = block.hash();
        let power = self.verify_cert_header(block.height, &block.prev_hash, &block_hash, cert)?;
        Ok((block_hash, power))
    }

    /// Follow one certified height: verify the certificate against the tracked
    /// set, then evolve the set exactly as the chain would. On success the
    /// tracker advances by one height and returns the voting power that
    /// certified the block. On any error the tracker is left unchanged.
    ///
    /// As of M21 the derived set is cross-checked against the block's
    /// `next_validators_root` commitment (which the certificate signs), so the
    /// transition is confirmed against consensus rather than merely trusted —
    /// and the check also covers pubkeys, not just ids and powers.
    pub fn follow(&mut self, block: &Block, cert: &Commit) -> Result<u64, LightError> {
        let (block_hash, power) = self.verify_cert(block, cert)?;

        // 5. replicate ONLY the validator-set transition (mirror apply_block) on
        //    LOCAL copies, so a failed cross-check leaves the tracker unmutated:
        //    stake ops and slashing evidence change bonds; the derived update of
        //    every touched id reads the post-change bond as its new power.
        let mut bonds = self.bonds.clone();
        let mut touched: BTreeSet<u64> = BTreeSet::new();
        for op in &block.stake_ops {
            match op.kind {
                BondKind::Bond => *bonds.entry(op.account).or_insert(0) += op.amount,
                BondKind::Unbond => {
                    let cur = bonds.get(&op.account).copied().unwrap_or(0);
                    if cur < op.amount {
                        return Err(LightError::InconsistentStakeOp {
                            account: op.account,
                            height: block.height,
                        });
                    }
                    if cur == op.amount {
                        bonds.remove(&op.account);
                    } else {
                        bonds.insert(op.account, cur - op.amount);
                    }
                }
            }
            touched.insert(op.account);
        }
        for ev in &block.slashing_evidence {
            // slashing removes the offender's entire bond; the derived update
            // (power 0) removes them from the set at the next height.
            bonds.remove(&ev.vote_a.validator);
            touched.insert(ev.vote_a.validator);
        }

        // explicit updates first, then one derived update per touched id —
        // the exact order and content of apply_block (lib.rs).
        let mut updates = block.validator_updates.clone();
        for id in touched {
            let power = bonds.get(&id).copied().unwrap_or(0);
            let pubkey = self.pubkeys.get(&id).copied().unwrap_or_default();
            updates.push(ValidatorUpdate { id, pubkey, power }); // power 0 == removal
        }
        let mut next = self.set.clone();
        if !updates.is_empty() {
            next = self.set.apply_updates(&updates);
            if next.is_empty() {
                return Err(LightError::EmptyValidatorSet);
            }
        }

        // 6. cross-check the derived set against the header commitment (part of
        //    block_hash, so certificate-signed). Only then commit to self.
        if next.merkle_root() != block.next_validators_root {
            return Err(LightError::ValidatorRootMismatch { height: block.height });
        }

        self.bonds = bonds;
        self.set = next;
        self.head = block_hash;
        self.height = block.height;
        Ok(power)
    }

    /// Follow one certified height **without replaying the transition**: verify
    /// the certificate against the tracked set, then adopt `next_set` after
    /// checking it against the block's `next_validators_root` commitment. This is
    /// the M21 SPV path — a wallet that is *handed* each next set (e.g. over a
    /// header-sync transport) advances by verifying a Merkle root, never touching
    /// bonds, stake ops, or evidence. On any error the tracker is unchanged.
    pub fn follow_committed(
        &mut self,
        block: &Block,
        cert: &Commit,
        next_set: &ValidatorSet,
    ) -> Result<u64, LightError> {
        let (block_hash, power) = self.verify_cert(block, cert)?;
        if next_set.merkle_root() != block.next_validators_root {
            return Err(LightError::ValidatorRootMismatch { height: block.height });
        }
        if next_set.is_empty() {
            return Err(LightError::EmptyValidatorSet);
        }
        self.set = next_set.clone();
        self.head = block_hash;
        self.height = block.height;
        Ok(power)
    }

    /// Header-only SPV path (M22). Equivalent to [`Self::follow_committed`]
    /// but operating on a [`crate::codec::BlockHeader`] instead of a full
    /// `Block`: a light client can advance the tracker against a cert-signed
    /// header without ever deserializing a transaction body. The cert's
    /// `block_hash` equals `header.hash()` (the cert was issued by a full node
    /// for the empty-bodied projection — see [`crate::codec::encode_header`]).
    pub fn follow_header(
        &mut self,
        header: &crate::codec::BlockHeader,
        cert: &Commit,
        next_set: &ValidatorSet,
    ) -> Result<u64, LightError> {
        let header_hash = header.hash();
        let power = self.verify_cert_header(header.height, &header.prev_hash, &header_hash, cert)?;
        if next_set.merkle_root() != header.next_validators_root {
            return Err(LightError::ValidatorRootMismatch { height: header.height });
        }
        if next_set.is_empty() {
            return Err(LightError::EmptyValidatorSet);
        }
        self.set = next_set.clone();
        self.head = header_hash;
        self.height = header.height;
        Ok(power)
    }

    /// M24: the **only** SPV proof verifier. Replaces M22's
    /// `verify_membership_against_header` (validator proof) and M23's
    /// `verify_account_membership_against_header` (account proof), the
    /// reviewer proof path that was never wired up, and (M25) the
    /// graph-node proof path.
    ///
    /// Cert-signs `header.hash()`, computes the leaf locally from `entry`,
    /// then runs `merkle::verify` against the right root slot for `entry`'s
    /// [`ProofKind`]:
    ///   * [`ProofKind::Account`]   → `header.accounts_root`
    ///   * [`ProofKind::Reviewer`]  → `header.accounts_root`
    ///   * [`ProofKind::GraphNode`] → `header.accounts_root` (M25 — graph
    ///     leaves share the accounts_root tree)
    ///   * [`ProofKind::Validator`] → `header.next_validators_root`
    ///
    /// The prover does **not** get to claim a different leaf than it shipped
    /// — the verifier recomputes `merkle::leaf_hash(&entry.leaf())` and
    /// checks it against the proof's path. Returns `MembershipProofInvalid`
    /// if the leaf-hash / root don't match.
    pub fn verify_proof_against_header(
        header: &crate::codec::BlockHeader,
        cert: &Commit,
        tracked_set: &ValidatorSet,
        entry: &ProofEntry,
    ) -> Result<(), LightError> {
        let hh = header.hash();
        if cert.height != header.height || cert.block_hash != hh {
            return Err(LightError::CertificateMismatch { height: header.height });
        }
        cert.verify(tracked_set).map_err(LightError::Consensus)?;
        let leaf = merkle::leaf_hash(&entry.leaf());
        let root = match entry {
            ProofEntry::Account { .. }
            | ProofEntry::Reviewer { .. }
            | ProofEntry::GraphNode { .. } => &header.accounts_root,
            ProofEntry::Validator { .. } => &header.next_validators_root,
        };
        if !merkle::verify(root, &leaf, entry.proof()) {
            return Err(LightError::MembershipProofInvalid { height: header.height });
        }
        Ok(())
    }

    /// M23 header-only full-state digest check. The cert signs
    /// `header.state_root` (a flat digest of *all* consensus state at this
    /// height — accounts/reviewers/graph/validators/bonds/bonded/unbonding/
    /// treasury/supply). The wallet doesn't need to recompute the digest —
    /// the cert-signing validator set already vouches for it by binding
    /// `block_hash = header.hash()`. This helper exists to make that contract
    /// explicit and to surface any header/cert binding error before downstream
    /// consumers trust the root. Note: it does NOT prove the value of
    /// `state_root` itself — that's a property of the cert-signing set, not the
    /// verifier. The verifier only checks "the cert binds this header".
    pub fn verify_state_root_against_header(
        header: &crate::codec::BlockHeader,
        cert: &Commit,
        tracked_set: &ValidatorSet,
    ) -> Result<(), LightError> {
        let hh = header.hash();
        if cert.height != header.height || cert.block_hash != hh {
            return Err(LightError::CertificateMismatch { height: header.height });
        }
        cert.verify(tracked_set).map_err(LightError::Consensus)?;
        Ok(())
    }

    // ----- M26: cert-signed kNN over the cognitive graph -----

    // See also `engine::CognitiveGraph::k_nearest_with_ties`. The wallet-side
    // ranking here MUST match the engine's ranking byte-for-byte given the
    // same `query` and the same set of (verified) neighbour leaves.

    /// M26: cert-signed kNN over the cognitive graph.
    ///
    /// `claim.neighbours` is the prover's answer. The verifier:
    ///   1. cert-signing contract — same as `verify_proof_against_header`:
    ///      `cert.height == header.height`, `cert.block_hash == header.hash()`,
    ///      and `cert.verify(tracked_set)` succeeds.
    ///   2. for each neighbour in `claim.neighbours`, recompute the leaf
    ///      hash from `graph_node.merkle_leaf()` and verify the supplied
    ///      Merkle proof against `header.accounts_root` — same routing as
    ///      `ProofEntry::GraphNode` (M25).
    ///   3. re-rank the verified leaves by cosine against `claim.query`,
    ///      tie-break by `node_id` ascending, cut by `k_nearest_with_ties(k)`.
    ///   4. require the prover's neighbour order to equal the verifier-derived
    ///      order, set-equal and in the same `node_id` order. (Length-equal
    ///      is implied because both sides apply the same cut.)
    ///
    /// The prover is **untrusted** — the wallet rebuilds the ranking from
    /// committed leaves plus its own `cos_sim`. Sim values are recomputed
    /// and never trusted from the wire.
    ///
    /// Returns:
    /// - `EmptyKnnQuery { height }` — `claim.k == 0` or no neighbours.
    /// - `CertificateMismatch { height }` — header/cert binding broken.
    /// - `Consensus(e)` — cert did not verify against the tracked set.
    /// - `MembershipProofInvalid { height }` — a neighbour's Merkle proof
    ///   did not verify against `header.accounts_root`.
    /// - `KnnRankingMismatch { height }` — the prover's neighbour list
    ///   (set or order) disagrees with what the wallet re-derives.
    pub fn verify_knn_against_header(
        header: &crate::codec::BlockHeader,
        cert: &Commit,
        tracked_set: &ValidatorSet,
        claim: &KnnClaim,
    ) -> Result<(), LightError> {
        // 1. cert-signing contract (same as verify_proof_against_header).
        let hh = header.hash();
        if cert.height != header.height || cert.block_hash != hh {
            return Err(LightError::CertificateMismatch { height: header.height });
        }
        cert.verify(tracked_set).map_err(LightError::Consensus)?;

        // Empty-query guard: `k == 0` is a degenerate "show me nothing"
        // request, not a real proof. Surface it explicitly so callers
        // can distinguish "no answer" from "verified answer".
        if claim.k == 0 || claim.neighbours.is_empty() {
            return Err(LightError::EmptyKnnQuery { height: header.height });
        }

        // 2. Merkle-verify every neighbour leaf against header.accounts_root.
        //    This is the M25 GraphNode routing — graph leaves share the tree
        //    with accounts and reviewers, so `accounts_root` is the right
        //    cert-signed commitment slot.
        let root = &header.accounts_root;
        for (_node_id, graph_node, proof) in &claim.neighbours {
            let leaf = merkle::leaf_hash(&graph_node.merkle_leaf());
            if !merkle::verify(root, &leaf, proof) {
                return Err(LightError::MembershipProofInvalid { height: header.height });
            }
        }

        // 3. Re-rank the verified leaves locally. The wallet does NOT
        //    trust any `sim` value the prover might have shipped — every
        //    similarity is recomputed via `engine::cos_sim` against the
        //    just-verified embedding bytes.
        let mut local: Vec<(u64, f32)> = claim
            .neighbours
            .iter()
            .map(|(node_id, graph_node, _)| {
                (*node_id, crate::engine::cos_sim(&claim.query, &graph_node.embedding))
            })
            .collect();
        // Same sort as `engine::CognitiveGraph::rank_by_cosine`:
        // cosine desc, then node_id asc.
        local.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        // Apply the same tie-keeping cut as the engine's k_nearest_with_ties.
        // The cut keeps every node with sim >= ranked[k-1].sim; on ties the
        // result may grow beyond `k`. Since `local` is already sorted,
        // `take_while` is enough.
        let expected: Vec<u64> = if local.len() <= claim.k {
            local.iter().map(|(id, _)| *id).collect()
        } else {
            let cutoff = local[claim.k - 1].1;
            local
                .iter()
                .take_while(|(_, s)| *s >= cutoff)
                .map(|(id, _)| *id)
                .collect()
        };
        let claimed: Vec<u64> = claim.neighbours.iter().map(|(id, _, _)| *id).collect();

        // 4. Order + set equality. Both are derived from the SAME verified
        //    leaves via the SAME sort + cut rule, so any divergence here is
        //    the prover's fault: tampered proof, omitted tied neighbour,
        //    reordered list, etc.
        if expected != claimed {
            return Err(LightError::KnnRankingMismatch { height: header.height });
        }
        Ok(())
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

    /// Per-height committed sets from an authoritative replay: `sets[i]` is the
    /// validator set that certifies height `i + 2` (what block `i + 1` commits to
    /// in its `next_validators_root`).
    fn committed_sets(g: Genesis, blocks: &[Block]) -> Vec<ValidatorSet> {
        let mut replay = Chain::new(g);
        let mut sets = Vec::new();
        for b in blocks {
            let mut b = b.clone();
            replay.commit(&mut b).expect("commit");
            sets.push(replay.state.validators.clone());
        }
        sets
    }

    #[test]
    fn follow_committed_matches_transition_follow() {
        // a chain exercising explicit updates AND staking; the transition-free
        // path must reach the same set as the full replay.
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 2 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        d.stage_stake_op(bond(2, 7 * MICRO));
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let sets = committed_sets(base_genesis(), d.blocks());
        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        for (i, (b, c)) in d.blocks().iter().zip(d.certificates()).enumerate() {
            lt.follow_committed(b, c, &sets[i]).expect("follow_committed");
        }
        assert_eq!(ids_powers(lt.validators()), ids_powers(&d.chain.state.validators));
        assert_eq!(lt.head(), d.head());
        assert_eq!(lt.height(), 2);
    }

    #[test]
    fn follow_committed_rejects_a_wrong_next_set() {
        let mut d = driver(base_genesis());
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 5 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        // hand it the genesis set as the "next" set, but block 1 added validator
        // 24 — its committed root does not match the genesis set's root.
        let wrong = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let err = lt
            .follow_committed(&d.blocks()[0], &d.certificates()[0], &wrong)
            .unwrap_err();
        assert!(matches!(err, LightError::ValidatorRootMismatch { .. }), "got {err}");
        assert_eq!(lt.height(), 0, "tracker unchanged on rejection");
    }

    #[test]
    fn follow_cross_checks_against_the_committed_root() {
        // the M21 strengthening: an honest chain's follow still succeeds, and the
        // tracked set's own root matches every block's commitment along the way.
        let mut d = driver(base_genesis());
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 3 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        d.stage_stake_op(bond(1, 6 * MICRO));
        d.produce(2.0, &no_silence()).unwrap().expect("block");

        let mut lt = ValidatorTracker::from_genesis(&base_genesis());
        for (b, c) in d.blocks().iter().zip(d.certificates()) {
            lt.follow(b, c).expect("follow");
            assert_eq!(lt.validators().merkle_root(), b.next_validators_root);
        }
    }

    #[test]
    fn verify_proof_against_header_accepts_validator_membership() {
        let mut d = driver(base_genesis());
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 5 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");

        let block = &d.blocks()[0];
        let cert = &d.certificates()[0];
        let next_set = d.chain.state.validators.clone();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();

        let v = next_set.get(24).unwrap().clone();
        let proof = next_set.proof(24).unwrap();
        let header = crate::codec::BlockHeader::from_block(block);
        let entry = ProofEntry::Validator { id: 24, validator: v.clone(), proof };
        ValidatorTracker::verify_proof_against_header(&header, cert, &tracked, &entry)
            .expect("genuine validator membership verifies");
    }

    #[test]
    fn verify_proof_against_header_rejects_a_tampered_validator_leaf() {
        let mut d = driver(base_genesis());
        d.stage_validator_update(ValidatorUpdate { id: 24, pubkey: kp(24).public(), power: 5 });
        d.produce(1.0, &no_silence()).unwrap().expect("block");

        let block = &d.blocks()[0];
        let cert = &d.certificates()[0];
        let next_set = d.chain.state.validators.clone();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();

        let v = next_set.get(24).unwrap().clone();
        let proof = next_set.proof(24).unwrap();
        let mut forged = v.clone();
        forged.power += 1;
        let header = crate::codec::BlockHeader::from_block(block);
        let entry = ProofEntry::Validator { id: 24, validator: forged, proof };
        let err = ValidatorTracker::verify_proof_against_header(&header, cert, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    // --- M23/M24: account + reviewer SPV via the unified verifier ------------

    /// Build a tiny certified chain of length 1 carrying an account-affecting
    /// tx (so the accounts tree actually mutates between genesis and h1) and
    /// return everything a wallet would need: the block, cert, post-apply
    /// accounts_root, the full account object, and its inclusion proof.
    fn one_block_with_proof() -> (Block, Commit, crate::Account, merkle::Proof) {
        let mut d = driver(base_genesis());
        // account 1 submits a real tx -> balance / submissions change.
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");

        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        let account = d.chain.state.accounts.get(&1).cloned().expect("account 1 exists");
        let proof = d.chain.state.account_proof(1).expect("proof exists");
        (block, cert, account, proof)
    }

    #[test]
    fn verify_proof_against_header_accepts_account_membership() {
        let (block, cert, account, proof) = one_block_with_proof();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::Account { id: 1, account, proof };
        ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .expect("header-only account membership verifies");
        // And `state_root_against_header` accepts the same cert-signed header.
        ValidatorTracker::verify_state_root_against_header(&header, &cert, &tracked)
            .expect("state_root accepts the cert-signed header");
    }

    #[test]
    fn verify_proof_against_header_rejects_an_inflated_balance() {
        let (block, cert, mut account, proof) = one_block_with_proof();
        // The verifier computes the leaf from the supplied `account`, so a
        // tampered balance makes the recomputed leaf mismatch the proof's path.
        account.balance += 1;
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::Account { id: 1, account, proof };
        let err = ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    #[test]
    fn verify_proof_against_header_rejects_tampered_accounts_root() {
        let (block, cert, account, proof) = one_block_with_proof();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        // Mutate accounts_root in the header (cert-signed field!) — the proof
        // now opens against a different root and must be rejected. The cert's
        // block_hash also no longer matches the mutated header's hash, so the
        // FIRST rejection is CertificateMismatch. Either failure is sound.
        let mut bad_header = crate::codec::BlockHeader::from_block(&block);
        bad_header.accounts_root = [0xAB; 32];
        let entry = ProofEntry::Account { id: 1, account, proof };
        let err = ValidatorTracker::verify_proof_against_header(&bad_header, &cert, &tracked, &entry)
            .unwrap_err();
        assert!(
            matches!(err, LightError::CertificateMismatch { .. } | LightError::MembershipProofInvalid { .. }),
            "got {err}"
        );
    }

    #[test]
    fn verify_proof_against_header_rejects_a_wrong_certificate() {
        let (block, cert, account, proof) = one_block_with_proof();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::Account { id: 1, account, proof };
        // A cert for a different height doesn't bind this header.
        let mut wrong_height = cert.clone();
        wrong_height.height = block.height + 1;
        let err = ValidatorTracker::verify_proof_against_header(&header, &wrong_height, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::CertificateMismatch { .. }), "got {err}");
        // And a cert from a totally different chain (wrong block_hash) is rejected.
        let mut wrong_hash = cert.clone();
        wrong_hash.block_hash = [0xCC; 32];
        let err = ValidatorTracker::verify_proof_against_header(&header, &wrong_hash, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::CertificateMismatch { .. }), "got {err}");
    }

    #[test]
    fn verify_proof_against_header_rejects_unknown_account_id_against_a_cert_signed_header() {
        // An account that doesn't exist on-chain still has a "leaf preimage" the
        // prover could hand us, but the Merkle tree at the cert-signed height
        // doesn't contain it — so the locally-computed leaf won't verify against
        // the supplied proof.
        let (block, cert, _real_account, _proof) = one_block_with_proof();
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let header = crate::codec::BlockHeader::from_block(&block);
        let fake_account = crate::Account::default();
        let empty_proof = merkle::Proof { steps: Vec::new() };
        let entry = ProofEntry::Account { id: 999, account: fake_account, proof: empty_proof };
        let err = ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    #[test]
    fn verify_proof_against_header_accepts_reviewer_membership() {
        // Build a chain where reviewer #10 exists, pull a proof for them, verify.
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap(); // reviewers 10..13 review it
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        let reputation = d.chain.state.reviewers.get(&10).copied().expect("reviewer 10");
        let proof = d.chain.state.reviewer_proof(10).expect("reviewer proof");
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::Reviewer { id: 10, reputation, proof };
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .expect("reviewer proof verifies against accounts_root");
    }

    #[test]
    fn verify_proof_against_header_rejects_tampered_reputation() {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        let proof = d.chain.state.reviewer_proof(10).expect("reviewer proof");
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::Reviewer { id: 10, reputation: 999.0, proof };
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let err = ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    /// M25: produce a 1-block certified chain that includes at least one
    /// accepted submission (so `state.graph` is non-empty), build a
    /// `ProofEntry::GraphNode`, and confirm
    /// `verify_proof_against_header` accepts it against the cert-signed
    /// header's `accounts_root`.
    #[test]
    fn verify_proof_against_header_accepts_a_graph_node_proof() {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        // The chain state has at least the demo-genesis seed node(s) plus any
        // nodes added by the accepted submission. After `produce` block 1 the
        // graph is non-empty, so node id 0 is safe to request.
        assert!(!d.chain.state.graph.nodes.is_empty(), "graph must be non-empty");
        let idx = 0;
        let node = d.chain.state.graph.nodes[idx].clone();
        let proof = d.chain.state.graph_node_proof(idx).expect("graph_node_proof(0)");
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::GraphNode { node_id: node.node_id, graph_node: node.clone(), proof };
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .expect("graph-node proof must verify");
    }

    /// M25: tamper the embedding field in a `ProofEntry::GraphNode` — the
    /// verifier recomputes the leaf locally and rejects the proof with
    /// `MembershipProofInvalid` (prover-supplied leaf is not trusted).
    #[test]
    fn verify_proof_against_header_rejects_a_tampered_graph_node_embedding() {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        let idx = 0;
        let mut node = d.chain.state.graph.nodes[idx].clone();
        let proof = d.chain.state.graph_node_proof(idx).expect("graph_node_proof(0)");
        // Tamper: bump the first embedding float.
        node.embedding[0] += 1.0;
        let header = crate::codec::BlockHeader::from_block(&block);
        let entry = ProofEntry::GraphNode { node_id: node.node_id, graph_node: node, proof };
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();
        let err = ValidatorTracker::verify_proof_against_header(&header, &cert, &tracked, &entry)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    // ----- M26: verify_knn_against_header -----

    /// Build a 1-block chain, then assemble a `KnnClaim` from the
    /// engine's `k_nearest_with_ties` over `chain.state.graph`. Returns the
    /// header, cert, and the claim so individual tests can mutate them.
    fn one_block_with_knn_claim(k: usize) -> (
        crate::codec::BlockHeader,
        Commit,
        ValidatorSet,
        KnnClaim,
    ) {
        let mut d = driver(base_genesis());
        d.submit(novel_tx(1, 1, 1, 1.0)).unwrap();
        d.produce(1.0, &no_silence()).unwrap().expect("block");
        let block = d.blocks()[0].clone();
        let cert = d.certificates()[0].clone();
        let header = crate::codec::BlockHeader::from_block(&block);
        let tracked = ValidatorTracker::from_genesis(&base_genesis()).validators().clone();

        // Pick a query embedding equal to the first graph node's embedding
        // — its cosine sim with that node is 1.0, so it's always the top
        // neighbour. This makes the expected ranking deterministic.
        let n = d.chain.state.graph.nodes.len();
        assert!(n >= 1, "test setup: graph must have at least one node");
        let query = d.chain.state.graph.nodes[0].embedding;
        let ranked = d.chain.state.graph.k_nearest_with_ties(&query, k);
        let mut neighbours: Vec<(u64, crate::engine::GraphNode, merkle::Proof)> =
            Vec::with_capacity(ranked.len());
        for (id, _sim) in &ranked {
            let idx = *id as usize;
            let proof = d.chain.state.graph_node_proof(idx).expect("proof");
            let node = d.chain.state.graph.nodes[idx].clone();
            neighbours.push((*id, node, proof));
        }
        let claim = KnnClaim { query, k, neighbours };
        (header, cert, tracked, claim)
    }

    #[test]
    fn verify_knn_against_header_accepts_a_cert_signed_neighborhood_claim() {
        let (header, cert, tracked, claim) = one_block_with_knn_claim(2);
        ValidatorTracker::verify_knn_against_header(&header, &cert, &tracked, &claim)
            .expect("wallet-side kNN claim verifies");
    }

    #[test]
    fn verify_knn_against_header_rejects_a_swapped_neighbour_order() {
        let (header, cert, tracked, mut claim) = one_block_with_knn_claim(2);
        // Two or more neighbours → swap them. The verifier re-derives the
        // ranking from verified leaves, so a swap across different sims
        // trips the order check.
        if claim.neighbours.len() < 2 {
            // Single-neighbour claim cannot test ordering — skip cleanly.
            return;
        }
        claim.neighbours.swap(0, 1);
        let err = ValidatorTracker::verify_knn_against_header(&header, &cert, &tracked, &claim)
            .unwrap_err();
        assert!(matches!(err, LightError::KnnRankingMismatch { .. }), "got {err}");
    }

    #[test]
    fn verify_knn_against_header_rejects_a_tampered_neighbour_embedding() {
        let (header, cert, tracked, mut claim) = one_block_with_knn_claim(2);
        // Tamper one neighbour's embedding — leaf hash mismatches, so the
        // Merkle proof against accounts_root fails. Same failure mode as
        // a tampered single-leaf ProofEntry::GraphNode.
        if claim.neighbours.is_empty() {
            return;
        }
        let mut g = claim.neighbours[0].1.clone();
        g.embedding[0] += 1.0;
        claim.neighbours[0].1 = g;
        let err = ValidatorTracker::verify_knn_against_header(&header, &cert, &tracked, &claim)
            .unwrap_err();
        assert!(matches!(err, LightError::MembershipProofInvalid { .. }), "got {err}");
    }

    #[test]
    fn verify_knn_against_header_rejects_a_tampered_accounts_root() {
        let (header, cert, tracked, claim) = one_block_with_knn_claim(2);
        let mut bad_header = header.clone();
        bad_header.accounts_root = [0xAB; 32];
        // accounts_root is part of header.hash(), so the cert no longer
        // matches the header — CertificateMismatch wins before any Merkle
        // check runs.
        let err = ValidatorTracker::verify_knn_against_header(&bad_header, &cert, &tracked, &claim)
            .unwrap_err();
        assert!(matches!(err, LightError::CertificateMismatch { .. }), "got {err}");
    }

    #[test]
    fn verify_knn_against_header_rejects_an_empty_claim() {
        // k > 0 but no neighbours: the engine produced an empty set
        // (or the prover lied). The wallet surfaces this as EmptyKnnQuery
        // so callers can distinguish "no answer" from "verified answer".
        let (header, cert, tracked, _) = one_block_with_knn_claim(2);
        let claim = KnnClaim { query: [0.0; 8], k: 1, neighbours: Vec::new() };
        let err = ValidatorTracker::verify_knn_against_header(&header, &cert, &tracked, &claim)
            .unwrap_err();
        assert!(matches!(err, LightError::EmptyKnnQuery { .. }), "got {err}");
    }
}
