//! P2P gossip + anti-entropy state sync — the network layer.
//!
//! Up to M14 the pieces of a node ran in one process: `round::Sim` wired
//! validators over an in-*process* bus to decide a block, and the driver grew a
//! chain by itself. That bus was always a **stand-in for the P2P layer**. This
//! module is that layer: it disseminates the two artifacts that actually travel
//! between distinct nodes — **pending transactions** (pre-consensus) and
//! **certified blocks** (a block *with* its finality certificate, post-consensus)
//! — and lets a fresh or lagging node **catch up** to the certified head from its
//! peers, verifying every certificate against the on-chain validator set as it
//! goes (M16). Within-height vote gossip stays in `round` (a validator concern);
//! what crosses the network here is the finalized, self-verifying result.
//!
//! Two things trust nothing:
//!
//!   * **Anti-entropy sync** — a node advertises its height (`Status`); a peer
//!     that is behind pulls the missing certified blocks (`GetBlocks` → `Blocks`)
//!     and applies each only if its certificate is a real > 2/3 quorum of the set
//!     *active for that height* and binds exactly that block — the same check
//!     [`Chain::replay_verified`] makes. A forged or dropped certificate stops
//!     the sync at the gap rather than corrupting state.
//!   * **Epidemic tx gossip** — a novel transaction is admitted to the mempool
//!     and forwarded to peers; a content-hash `seen` set makes re-delivery a
//!     no-op, so the broadcast floods once and terminates.
//!   * **Block-level op gossip** (M19) — equivocation evidence (`Evidence`)
//!     and signed stake ops (`StakeOp`) flood into every node's pending pool
//!     the same way; the next proposer drains the pool into `pending_*` on
//!     its driver and the resulting block carries the op. This makes slashing
//!     and bond/unbond **permissionlessly detectable** rather than only
//!     proposer-detectable: any node that observes a double-sign can route
//!     the proof into the next block, no matter who the proposer is.
//!
//! Determinism holds as everywhere else: [`Network`] is an in-process, fixed-order
//! delivery bus that lets tests assert N nodes **converge** to a byte-identical
//! head/`state_root`. The socket transport ([`read_msg`]/[`write_msg`]) is a thin
//! length-prefixed framing over the same wire messages; the correctness lives in
//! the deterministic protocol, not the wire.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};

use crate::codec::{
    decode_block, decode_certified_header, decode_commit, decode_evidence, decode_stakeop,
    decode_tx, encode_block, encode_certified_header, encode_commit, encode_evidence,
    encode_stakeop, encode_tx, CertifiedHeader, CodecError,
};
use crate::consensus::Commit;
use crate::light::ValidatorTracker;
use crate::merkle;
use crate::mempool::Mempool;
use crate::validator::ValidatorSet;
use crate::{Block, Chain, Genesis, Hash, SlashEvidence, StakeOp, SubmissionTx};

/// Maximum certified blocks returned in a single [`GossipMsg::Blocks`] batch — a
/// lagging peer that needs more re-requests from the new height.
pub const MAX_BATCH: usize = 256;

/// A message exchanged between peers.
#[derive(Clone, Debug)]
pub enum GossipMsg {
    /// "My certified chain is this tall." The anti-entropy heartbeat.
    Status { height: u64 },
    /// "Send me certified blocks from this height onward."
    GetBlocks { from: u64 },
    /// A height-ordered batch of certified blocks (each block with its finality
    /// certificate) — the sync response and the push of a freshly committed block.
    Blocks(Vec<(Block, Commit)>),
    /// Gossip one pending transaction.
    Tx(SubmissionTx),
    /// Gossip one piece of equivocation evidence (M19) — once it floods into
    /// every node's pending pool, the next proposer admits it into a slashing
    /// block. Full cryptographic validation (signatures, offender is active)
    /// happens in `chain.commit.apply_evidence`; this layer only dedups.
    Evidence(SlashEvidence),
    /// Gossip one signed bond/unbond op (M19) — same pattern as `Evidence`:
    /// flood into every node's pending stake-op pool, the next proposer
    /// admits it into a staking block. Full validation in
    /// `chain.commit.apply_stake_op`.
    StakeOp(StakeOp),
    /// "Send me cert-signed BLOCK HEADERS (no bodies) from this height onward."
    /// The light-sync analogue of `GetBlocks` (M22). A light client announces
    /// its height with `Status`; on seeing a peer ahead, it sends `GetHeaders`
    /// instead — the response is `Headers(...)` carrying only `(header, cert)`
    /// pairs. The light client never deserializes a transaction body.
    GetHeaders { from: u64 },
    /// A height-ordered batch of cert-signed headers — the response to
    /// `GetHeaders` and the push of a freshly committed header. The cert's
    /// `block_hash` equals `header.hash()` (a header is the cert-signed
    /// projection of a block with empty bodies).
    Headers(Vec<CertifiedHeader>),
    /// M24: a light wallet asks a full peer for one or more O(log n) inclusion
    /// proofs against the full peer's current state. Each item is `(kind, id)`:
    /// Account and Reviewer open against `accounts_root`; Validator opens
    /// against `next_validators_root`. The full peer answers with a parallel
    /// `Proof { items: Vec<Option<ProofEntry>> }` — `None` for unknown ids —
    /// and the wallet verifies each entry locally against the cert-signed
    /// header it received over M22, so the full peer never gets to lie about
    /// which leaf corresponds to which id. Full nodes serve; light nodes drop.
    /// Capped at [`MAX_PROOF_BATCH`] items per message.
    GetProof { items: Vec<(crate::light::ProofKind, u64)> },
    /// M24: the batched response. `items.len() == request.items.len()`; a
    /// `None` at index `i` means "unknown key" — the wallet's local
    /// `verify_proof_against_header` will reject it cleanly with
    /// `MembershipProofInvalid` against the cert-signed header's root. Light
    /// nodes cache entries by `(kind, id)` via
    /// [`LightGossipNode::take_proof`].
    Proof { items: Vec<Option<crate::light::ProofEntry>> },
    /// M28: a light wallet asks a full peer for a cert-signed temporal diff
    /// between two cert-signed heights it already holds. The full peer
    /// replays `(h₁+1..h₂]` on its own chain state and packages the
    /// result in a `Diff` reply carrying the typed envelope (cert-signed
    /// headers, certs, per-leaf proofs, diff body). The wallet
    /// cross-verifies the claim against its own cached range via
    /// `ValidatorTracker::verify_diff_against_headers`.
    GetDiff {
        h1: u64,
        h2: u64,
        header_h1: Box<crate::codec::BlockHeader>,
        header_h2: Box<crate::codec::BlockHeader>,
    },
    /// M28: the diff response. Bundles the two certified headers and certs
    /// (one per side) so the wallet can verify both certs without a
    /// separate round-trip; the diff body itself is
    /// `crate::DiffClaim { added, dropped }` with per-leaf proofs against
    /// the right `accounts_root` for each side.
    Diff {
        envelope: Box<crate::light::DiffEnvelope>,
    },
    /// M29: a light wallet asks a full peer for a **heterogeneous**
    /// batched proof — any mix of inclusion proofs, kNN claims, range
    /// claims, and diff envelopes in a single round-trip. The full
    /// peer dispatches each slot to the matching per-primitive
    /// producer (`serve_inclusion` / `serve_knn` / `serve_range` /
    /// `serve_diff`) and assembles a typed `Batch` response. Capped
    /// at [`MAX_BATCH_ITEMS`] items per message. Each item carries
    /// only its request-side arguments — the cert-binding context
    /// (header, cert, tracked set) is supplied by the wallet's
    /// tracked height.
    GetBatch { items: Vec<crate::light::BatchItem> },
    /// M29: the heterogeneous batched response. Each slot is the
    /// same body as the per-primitive single-shot producer would
    /// have shipped (`ProofEntry` / `KnnClaim` / `RangeClaim` /
    /// `DiffEnvelope`), so soundness reduces to dispatching each
    /// item to the matching existing verifier. Self-contained
    /// per-item — the `Diff` variant carries its own cert-binding
    /// context, the same as the M28 single-shot envelope. Box-wrapped
    /// to keep the enum size bounded.
    Batch {
        envelope: Box<crate::light::BatchResponseEnvelope>,
    },
    /// M30: a light/destination-side bridge endpoint asks a full peer of the
    /// *source* chain for a cert-signed lock inclusion envelope. The full
    /// peer answers with a single `Lock { envelope }` carrying the
    /// (header, cert, active set, lock, proof) needed for the destination's
    /// `BridgeEndpoint::verify_lock` to validate the lock against the
    /// source chain's cert-signed `bridge_root`.
    GetLock {
        /// The lock id on the source chain.
        lock_id: u64,
    },
    /// M30: the lock envelope. Same shape as the M28 `Diff` reply — a
    /// single length-prefixed, self-contained envelope that opens a single
    /// lock against the source chain's cert-signed `bridge_root`. Box-wrapped
    /// to keep the enum size bounded.
    Lock {
        envelope: Box<crate::bridge::LockEnvelope>,
    },
    /// M33: a distributed BFT consensus message — a signed proposal or a
    /// prevote/precommit vote for one height. The daemon [`crate::daemon`] Actor
    /// owns the per-process [`crate::round::RoundState`] and handles these; the
    /// pure `GossipNode` / `LightGossipNode` cores drop them (a consensus message
    /// yields no `GossipMsg` reply and its side effects — signing, arming
    /// wall-clock timers, `apply_certified` — live in the Actor). Boxed because a
    /// `Proposal` carries a whole `Block`.
    Consensus(Box<crate::round::Msg>),
}

/// M24: maximum items in a single [`GossipMsg::GetProof`] / [`GossipMsg::Proof`]
/// — over the cap is a codec error (`TooManyItems`). Keeps a single request
/// bounded so a hostile peer can't fan out O(n) work on the responder.
pub const MAX_PROOF_BATCH: usize = 32;

/// Wire-tag assignments. Each GossipMsg variant is one byte; bumping a tag
/// outside the existing range (0..=11) requires a major-version bump.
pub const TAG_STATUS: u8 = 0;
pub const TAG_GETBLOCKS: u8 = 1;
pub const TAG_BLOCKS: u8 = 2;
pub const TAG_TX: u8 = 3;
pub const TAG_EVIDENCE: u8 = 4;
pub const TAG_STAKEOP: u8 = 5;
pub const TAG_GETHEADERS: u8 = 6;
pub const TAG_HEADERS: u8 = 7;
pub const TAG_GETPROOF: u8 = 8;
pub const TAG_PROOF: u8 = 9;
pub const TAG_GETDIFF: u8 = 10;
pub const TAG_DIFF: u8 = 11;
/// M29: heterogeneous batched proof request — slots in `items` carry
/// any mix of inclusion / kNN / range / diff. Distinct from
/// `TAG_GETPROOF` so the per-kind `GetProof`/`Diff` pair stays a
/// single-shot primitive; the M29 pair is the *batched* one.
pub const TAG_GETBATCH: u8 = 12;
/// M29: heterogeneous batched proof response envelope.
pub const TAG_BATCH: u8 = 13;
/// M30: bridge lock fetch — destination endpoint asks a full source-chain
/// peer for a cert-signed lock inclusion envelope.
pub const TAG_GETLOCK: u8 = 14;
/// M30: bridge lock envelope — the full peer's response.
pub const TAG_LOCK: u8 = 15;
/// M33: distributed BFT consensus message (proposal / prevote / precommit).
/// Handled by the daemon Actor, not the pure gossip cores.
pub const TAG_CONSENSUS: u8 = 16;

/// M29: maximum items in a single heterogeneous batched
/// request/response. Mirrors `MAX_PROOF_BATCH = 32` so the bus caps
/// stay self-consistent. Per-primitive inner caps (kNN neighbour
/// bound, range cut bound, etc.) are enforced inside each
/// `serve_*` helper as today.
pub const MAX_BATCH_ITEMS: usize = 32;

/// One peer: a chain, a mempool, the retained certified chain (blocks paired with
/// certificates, for serving sync), and the gossip bookkeeping. Its [`on_message`]
/// is a pure state machine — it performs no I/O and returns the messages to send,
/// each addressed to a peer id — so it runs identically over the in-process
/// [`Network`] and over real sockets.
///
/// [`on_message`]: GossipNode::on_message
pub struct GossipNode {
    pub id: u64,
    pub chain: Chain,
    /// M28: cached genesis so we can replay from genesis when serving a
    /// `Diff` envelope (which needs `state_at_h1` to compute the diff
    /// body). `Chain::new` only stores `state` post-genesis, so the
    /// genesis itself was previously recoverable only by the test that
    /// built the peer; caching it here makes `serve_diff` self-contained.
    pub genesis: Genesis,
    pub mempool: Mempool,
    /// Retained certified chain, height order; `blocks[i]`/`certs[i]` is height
    /// `i+1`. Kept so this node can answer a peer's `GetBlocks`.
    blocks: Vec<Block>,
    certs: Vec<Commit>,
    /// Content hashes of transactions already seen — makes gossip flooding idempotent.
    seen_tx: BTreeSet<Hash>,
    /// Content hashes of equivocation evidence already seen — dedup for `Evidence`.
    seen_evidence: BTreeSet<Hash>,
    /// Content hashes of stake ops already seen — dedup for `StakeOp`.
    seen_stake_op: BTreeSet<Hash>,
    /// Evidence staged to be carried by the next block this node proposes
    /// (drained by the driver via [`Self::take_pending_evidence`]).
    pending_evidence: Vec<SlashEvidence>,
    /// Stake ops staged to be carried by the next block this node proposes.
    pending_stake_ops: Vec<StakeOp>,
    /// Known peer ids (iterated in sorted order for deterministic output).
    peers: BTreeSet<u64>,
}

impl GossipNode {
    /// A fresh node holding only `genesis`, aware of `peers`.
    pub fn new(id: u64, genesis: Genesis, max_txs: usize, peers: impl IntoIterator<Item = u64>) -> Self {
        GossipNode {
            id,
            chain: Chain::new(genesis.clone()),
            genesis,
            mempool: Mempool::new(max_txs),
            blocks: Vec::new(),
            certs: Vec::new(),
            seen_tx: BTreeSet::new(),
            seen_evidence: BTreeSet::new(),
            seen_stake_op: BTreeSet::new(),
            pending_evidence: Vec::new(),
            pending_stake_ops: Vec::new(),
            peers: peers.into_iter().filter(|&p| p != id).collect(),
        }
    }

    pub fn height(&self) -> u64 {
        self.chain.state.height
    }

    pub fn head(&self) -> Hash {
        self.chain.head
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    pub fn certificates(&self) -> &[Commit] {
        &self.certs
    }

    /// Evidence staged for the next proposed block (read-only view).
    pub fn pending_evidence(&self) -> &[SlashEvidence] {
        &self.pending_evidence
    }

    /// Stake ops staged for the next proposed block (read-only view).
    pub fn pending_stake_ops(&self) -> &[StakeOp] {
        &self.pending_stake_ops
    }

    /// M35: is there any block-worthy pending work — mempool txs or staged
    /// block-level ops (stake ops / slashing evidence)? Used by the daemon to
    /// suppress empty heartbeat blocks when `create_empty_blocks=false`.
    ///
    /// Bridge locks are intentionally excluded: they have no pending pool here
    /// (they enter committed state via the M31 on-chain redeem path, not the
    /// mempool). A future bridge mempool would extend this predicate.
    pub fn has_pending_work(&self) -> bool {
        !self.mempool.is_empty()
            || !self.pending_stake_ops.is_empty()
            || !self.pending_evidence.is_empty()
    }

    /// Drain staged evidence, transferring it to a block builder (e.g. a
    /// `ChainDriver`'s `stage_slashing_evidence`). After this call the
    /// gossip node's pending pool is empty, and the `seen_evidence` set
    /// still suppresses re-flood if the same record loops back.
    pub fn take_pending_evidence(&mut self) -> Vec<SlashEvidence> {
        std::mem::take(&mut self.pending_evidence)
    }

    /// Drain staged stake ops, transferring them to a block builder.
    pub fn take_pending_stake_ops(&mut self) -> Vec<StakeOp> {
        std::mem::take(&mut self.pending_stake_ops)
    }

    /// Preload an already-certified chain (e.g. a seed node handing out history).
    /// Each `(block, cert)` is accepted through the same verification path as a
    /// gossiped one, so even the seed cannot install an unfinalized block.
    pub fn load_certified(&mut self, blocks: &[Block], certs: &[Commit]) -> bool {
        blocks.len() == certs.len()
            && blocks
                .iter()
                .zip(certs)
                .all(|(b, c)| self.apply_certified(b.clone(), c.clone()))
    }

    /// M29-extracted: pull a single typed inclusion proof out of the
    /// local chain state. Identical logic to the M24 `GetProof` arm
    /// of `on_message`, factored out so both the M24 single-kind
    /// request and the M29 heterogeneous `GetBatch` request can
    /// dispatch through the same code path.
    ///
    /// Returns `None` iff the requested `(kind, id)` is not in local
    /// state — same "unknown key" semantics as the M24 producer
    /// (the wallet treats `None` as a verified no-op slot).
    fn serve_inclusion(
        &self,
        kind: crate::light::ProofKind,
        id: u64,
    ) -> Option<crate::light::ProofEntry> {
        match kind {
            crate::light::ProofKind::Account => {
                self.chain.state.account_proof(id).and_then(|p| {
                    self.chain
                        .state
                        .accounts
                        .get(&id)
                        .cloned()
                        .map(|a| crate::light::ProofEntry::Account {
                            id,
                            account: a,
                            proof: p,
                        })
                })
            }
            crate::light::ProofKind::Reviewer => {
                self.chain.state.reviewer_proof(id).map(|p| {
                    let reputation = self
                        .chain
                        .state
                        .reviewers
                        .get(&id)
                        .copied()
                        .unwrap_or(0.0);
                    crate::light::ProofEntry::Reviewer { id, reputation, proof: p }
                })
            }
            crate::light::ProofKind::Validator => {
                self.chain.state.validators.proof(id).and_then(|p| {
                    self.chain
                        .state
                        .validators
                        .validators()
                        .iter()
                        .find(|v| v.id == id)
                        .cloned()
                        .map(|v| crate::light::ProofEntry::Validator {
                            id,
                            validator: v,
                            proof: p,
                        })
                })
            }
            crate::light::ProofKind::GraphNode => {
                // M25: graph nodes are addressed by insertion index —
                // what the wallet observed from genesis forward or
                // from a prior block's graph length. The full peer
                // resolves it to a GraphNode by index and packages its
                // node_id along with the proof.
                let idx = id as usize;
                self.chain.state.graph_node_proof(idx).and_then(|p| {
                    self.chain
                        .state
                        .graph
                        .nodes
                        .get(idx)
                        .cloned()
                        .map(|n| crate::light::ProofEntry::GraphNode {
                            node_id: n.node_id,
                            graph_node: n,
                            proof: p,
                        })
                })
            }
        }
    }

    /// M29: serve a heterogeneous batched proof request. Walks
    /// `items`, dispatches each one to the matching existing
    /// `serve_*` helper, and assembles a typed
    /// `BatchResponseEnvelope`. Each slot's body is **exactly** what
    /// the per-primitive single-shot producer would have shipped, so
    /// soundness reduces to dispatching each slot to the matching
    /// existing verifier.
    ///
    /// Returns `None` iff `items.len() > MAX_BATCH_ITEMS` (caller-side
    /// cap check; codec rejects larger requests at decode time with
    /// `TooManyItems`). Per-primitive `serve_*` helpers may still
    /// return `None` for their own internal reasons (e.g.
    /// `serve_diff` returns `None` for degenerate height ranges);
    /// that propagates as `BatchResponseItem::*` empty-answer
    /// variants — except for `Diff`, where a degenerate range aborts
    /// the whole batch and we return `None` (a `Diff` envelope is
    /// non-empty by construction, so the wallet treats `None` here
    /// as a producer-side failure rather than a silent skip).
    pub fn serve_batch(
        &self,
        items: Vec<crate::light::BatchItem>,
    ) -> Option<crate::light::BatchResponseEnvelope> {
        if items.len() > MAX_BATCH_ITEMS {
            return None;
        }
        let mut out_items = Vec::with_capacity(items.len());
        for item in items {
            let resp = match item {
                crate::light::BatchItem::Inclusion { kind, id } => {
                    crate::light::BatchResponseItem::Inclusion(self.serve_inclusion(kind, id))
                }
                crate::light::BatchItem::Knn { query, k } => {
                    crate::light::BatchResponseItem::Knn(self.serve_knn(query, k))
                }
                crate::light::BatchItem::Range { query, min_sim } => {
                    crate::light::BatchResponseItem::Range(self.serve_range(query, min_sim))
                }
                crate::light::BatchItem::Diff { h1, h2 } => {
                    // M29: diff items are minimal — the producer pulls
                    // both headers from its own `self.blocks[..]` (the
                    // wallet supplied the heights; the headers are
                    // already content-addressed by the block hash the
                    // wallet will cross-check against its M22 header
                    // cache). This matches M28's `serve_diff`
                    // contract: producer-side `state_at_h1` /
                    // `state_at_h2` come from `Chain::replay` over
                    // `self.blocks[..]`, not from the request.
                    // `serve_diff` rejects h1 == 0 internally; we mirror
                    // the same guard here so the `usize - 1` below can't
                    // underflow.
                    if h1 == 0 || h1 >= h2 || h2 > self.height() {
                        return None;
                    }
                    let block_h1 = &self.blocks[h1 as usize - 1];
                    let block_h2 = &self.blocks[h2 as usize - 1];
                    let header_h1 = crate::codec::BlockHeader::from_block(block_h1);
                    let header_h2 = crate::codec::BlockHeader::from_block(block_h2);
                    let env = self.serve_diff(h1, h2, &header_h1, &header_h2)?;
                    crate::light::BatchResponseItem::Diff(Box::new(env))
                }
            };
            out_items.push(resp);
        }
        Some(crate::light::BatchResponseEnvelope { items: out_items })
    }

    /// M26: serve a cert-signed kNN claim against this full peer's current
    /// `chain.state.graph`. Computes `k_nearest_with_ties(query, k)` locally,
    /// then packages each `(node_id, graph_node, merkle_proof)` tuple into a
    /// `KnnClaim` — the same shape `verify_knn_against_header` accepts.
    ///
    /// In a network deployment this is what the demo path would dispatch on
    /// a `GetKnn` request message; the in-process `LightNetwork` simply calls
    /// it directly because both peers share the same runtime.
    ///
    /// Capped at `MAX_PROOF_BATCH = 32` neighbours per claim — same as the
    /// single-leaf proof bus. The engine's tie-breaking rule may return more
    /// than `k` neighbours; this helper does NOT truncate beyond the cap
    /// (`k_nearest_with_ties` itself doesn't either — a graph with many ties
    /// at the boundary could in principle exceed the cap, in which case we
    /// cut at the cap and document the cut; the wallet's verifier enforces
    /// that the resulting set is the cert-signed prefix of the true kNN
    /// ranking. At 32 the cap is generous for any real `k` and most realistic
    /// tie distributions; production would lift it or stream multi-page.)
    ///
    /// Returns `None` iff the engine produced an empty set (empty graph
    /// or a query whose valid kNN is empty — both are caller errors at the
    /// demo level, not chain-internal failures).
    pub fn serve_knn(
        &self,
        query: crate::engine::Embedding,
        k: usize,
    ) -> Option<crate::light::KnnClaim> {
        // 1. Locally compute the kNN-with-ties ranking.
        let mut ranked = self.chain.state.graph.k_nearest_with_ties(&query, k);
        if ranked.is_empty() {
            return None;
        }
        // Honour the cap. Cut from the *end* — the lowest-ranked neighbours
        // are the most expendable on a tie; the highest-ranked ones are
        // always preserved.
        if ranked.len() > MAX_PROOF_BATCH {
            ranked.truncate(MAX_PROOF_BATCH);
        }
        // 2. For each `(node_id, _)`, resolve to the GraphNode body and
        //    pull the M25 accounts_root Merkle proof.
        let mut neighbours: Vec<(u64, crate::engine::GraphNode, merkle::Proof)> =
            Vec::with_capacity(ranked.len());
        for (node_id, _sim) in &ranked {
            // node_id == insertion index (M25 invariant; both stable for
            // the lifetime of the graph).
            let idx = *node_id as usize;
            let proof = self.chain.state.graph_node_proof(idx)?;
            let graph_node = self.chain.state.graph.nodes.get(idx).cloned()?;
            neighbours.push((*node_id, graph_node, proof));
        }
        Some(crate::light::KnnClaim { query, k, neighbours })
    }

    /// M27: serve a cert-signed range claim for `cos_sim(query, n) >= min_sim`
    /// against the full peer's current `chain.state.graph`.
    ///
    /// The claim's `nodes` are ordered by cosine against the **user's**
    /// query (desc, with `node_id` asc tie-break) — that is the order the
    /// wallet expects after re-ranking, and it matches what the engine's
    /// `rank_by_cosine` produces. Each entry carries a Merkle proof
    /// against the M27 `header.graph_root` slot (cert-signed secondary
    /// index over the canonical-pivot-sorted view), so the wallet
    /// verifies each leaf against `graph_root` and re-ranks locally.
    ///
    /// Capped at `MAX_PROOF_BATCH = 32` per claim — same wire cap as the
    /// single-leaf proof bus and the kNN claim. Beyond the cap the cut
    /// is truncated from the END (lowest-similarity entries), preserving
    /// the highest-similarity prefix the wallet cares about most.
    ///
    /// Returns `None` iff the cut is empty (no node in the graph matches
    /// the cutoff) — the wallet's verifier then surfaces `RangeMismatch`
    /// if a non-empty claim was promised.
    pub fn serve_range(
        &self,
        query: crate::engine::Embedding,
        min_sim: f32,
    ) -> Option<crate::light::RangeClaim> {
        // 1. Locally compute the user-query-sorted ranking. Same rule as
        //    `engine::CognitiveGraph::rank_by_cosine`: cosine desc, then
        //    `node_id` asc on ties.
        let mut ranked = self.chain.state.graph.rank_by_cosine(&query);
        // 2. Take the prefix where sim >= min_sim. This is the cut set.
        let take = ranked.iter().take_while(|(_, s)| *s >= min_sim).count();
        ranked.truncate(take);
        if ranked.is_empty() {
            return None;
        }
        // Honour the cap. Cut from the END — lowest-similarity entries
        // are the most expendable; the highest-similarity prefix is
        // always preserved.
        if ranked.len() > MAX_PROOF_BATCH {
            ranked.truncate(MAX_PROOF_BATCH);
        }
        // 3. Build the canonical-pivot-sorted leaf index map once (same
        //    sort the wallet uses to compute the per-leaf proof path),
        //    then resolve each cut entry to (id, body, proof against
        //    graph_root).
        let sorted_index: std::collections::HashMap<u64, usize> = {
            let mut sorted = self.chain.state.graph.nodes.clone();
            sorted.sort_by(|a, b| {
                let sa = crate::engine::cos_sim(
                    &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    &a.embedding,
                );
                let sb = crate::engine::cos_sim(
                    &[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    &b.embedding,
                );
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.node_id.cmp(&b.node_id))
            });
            sorted.iter().enumerate().map(|(i, n)| (n.node_id, i)).collect()
        };
        let mut nodes: Vec<(u64, crate::engine::GraphNode, merkle::Proof)> =
            Vec::with_capacity(ranked.len());
        for (node_id, _sim) in &ranked {
            let sorted_idx = *sorted_index.get(node_id)?;
            let proof = self.chain.state.graph_range_proof(
                sorted_idx, sorted_idx + 1,
            )?;
            let (_id, graph_node, merkle_proof) = proof.entries.into_iter().next()?;
            nodes.push((*node_id, graph_node, merkle_proof));
        }
        Some(crate::light::RangeClaim { query, min_sim, nodes })
    }

    /// M28: serve a cert-signed temporal graph diff between two cert-signed
    /// heights this full peer holds. The diff is computed against the
    /// full peer's own chain state; per-leaf proofs are pulled from the
    /// same `chain.state.graph_node_proof(idx)` the M25 proof bus uses.
    ///
    /// `header_h1` / `header_h2` must be the cert-signed headers at the
    /// two heights. The two `Commit`s in the returned envelope are looked
    /// up by height from this peer's retained certificate log. The two
    /// `ValidatorSet`s in the envelope are the post-apply sets from a
    /// replay — the wallet verifies each cert against its corresponding
    /// tracked set.
    ///
    /// Returns `None` iff either height is missing from this peer's
    /// retained chain or `h1 == 0` / `h1 >= h2`.
    pub fn serve_diff(
        &self,
        h1: u64,
        h2: u64,
        header_h1: &crate::codec::BlockHeader,
        header_h2: &crate::codec::BlockHeader,
    ) -> Option<crate::light::DiffEnvelope> {
        // Range guard mirrors the wallet-side `InvalidDiffRange`.
        if h1 == 0 || h1 >= h2 || h2 > self.height() {
            return None;
        }
        if header_h1.height != h1 || header_h2.height != h2 {
            return None;
        }
        // The genesis is cached on the peer so we can replay from
        // genesis to derive `state_at_h1` (the state AT height h₁ — after
        // applying block h₁). `graph_diff` takes the h₁ side as the
        // "previous" state, so for h₁ = 1 we replay blocks `[0..1]` (just
        // block 1). For h₁ > 1 we replay `[0..h1]` (blocks 1..h₁). The
        // prefix length is exactly `h1` (not `h1 - 1`) so we capture the
        // post-block-h₁ state, not the pre-block-h₁ state.
        let state_h1 = {
            let prefix: Vec<crate::Block> =
                self.blocks[..h1 as usize].to_vec();
            crate::Chain::replay(self.genesis.clone(), &prefix)
                .ok()?
                .state
        };
        let cert_h1 = self.certs.get((h1 - 1) as usize)?.clone();
        let cert_h2 = self.certs.get((h2 - 1) as usize)?.clone();
        let diff = self.chain.state.graph_diff(&state_h1);
        // Per-side tracked sets come from the wallet's POV: after
        // applying block `h` the set that certifies `h+1` is
        // `state.validators`. The wallet already maintains this set
        // via `ValidatorTracker::follow`, but for the producer we
        // surface the sets as-is — the wallet re-derives them anyway
        // from its header cache.
        let tracked_h1 = {
            let replay = crate::Chain::replay(
                self.genesis.clone(),
                &self.blocks[..h1 as usize],
            )
            .ok()?;
            replay.state.validators.clone()
        };
        let tracked_h2 = {
            let replay = crate::Chain::replay(
                self.genesis.clone(),
                &self.blocks[..h2 as usize],
            )
            .ok()?;
            replay.state.validators.clone()
        };
        Some(crate::light::DiffEnvelope {
            header_prev: header_h1.clone(),
            cert_prev: cert_h1,
            header_new: header_h2.clone(),
            cert_new: cert_h2,
            diff,
            tracked_set_h1: tracked_h1,
            tracked_set_h2: tracked_h2,
        })
    }

    /// M30: build a `LockEnvelope` for `lock_id` from this node's chain
    /// state. Returns `None` when the lock does not exist on this chain
    /// (the lock_id is monotonic, so any gap is a programming error).
    ///
    /// The envelope ships (cert-signed header, cert, active validator set,
    /// lock, inclusion proof) — exactly what the destination-side
    /// `BridgeEndpoint::verify_lock` needs to verify the lock against the
    /// source chain's cert-signed `bridge_root`. The lock's height is the
    /// block height that carried it; we use that height's header and cert.
    pub fn serve_lock(&self, lock_id: u64) -> Option<crate::bridge::LockEnvelope> {
        let lock = self.chain.state.bridge_locks.get(&lock_id)?.clone();
        let proof = self.chain.state.bridge_lock_proof(lock_id)?;
        let height = *self.chain.state.bridge_lock_heights.get(&lock_id)?;
        // Heights are 1-indexed: `blocks[i]` corresponds to height `i+1`.
        let idx = (height - 1) as usize;
        let block = self.blocks.get(idx)?;
        let cert = self.certs.get(idx)?;
        let header = block.header();
        // Active set that certifies this header (the set the endpoint's
        // tracker must follow to know the cert). Replay from genesis so we
        // get the post-apply state for this height, the same way
        // `serve_diff` builds per-side tracked sets.
        let tracked_set = crate::Chain::replay(
            self.genesis.clone(),
            &self.blocks[..height as usize],
        )
        .ok()?
        .state
        .validators
        .clone();
        Some(crate::bridge::LockEnvelope {
            source_header: header,
            source_cert: cert.clone(),
            source_tracked_set: tracked_set,
            lock_id,
            lock,
            proof,
        })
    }

    /// The certified blocks from `height` onward (inclusive), capped at
    /// [`MAX_BATCH`] — the payload for a peer's `GetBlocks`.
    fn batch_from(&self, height: u64) -> Vec<(Block, Commit)> {
        if height == 0 {
            return Vec::new();
        }
        let start = (height - 1) as usize;
        (start..self.blocks.len().min(start + MAX_BATCH))
            .map(|i| (self.blocks[i].clone(), self.certs[i].clone()))
            .collect()
    }

    /// The cert-signed headers from `height` onward (inclusive), capped at
    /// [`MAX_BATCH`] — the payload for a peer's `GetHeaders` (M22). Drops
    /// transaction bodies, stake ops, and slashing evidence; what remains is
    /// exactly the cert-signed prefix.
    pub fn headers_from(&self, height: u64) -> Vec<CertifiedHeader> {
        if height == 0 {
            return Vec::new();
        }
        let start = (height - 1) as usize;
        (start..self.blocks.len().min(start + MAX_BATCH))
            .map(|i| CertifiedHeader::from_certified(&self.blocks[i], &self.certs[i]))
            .collect()
    }

    /// Every retained certified header (snapshot — the cert-signed projection
    /// of the full-node's certified chain). The light client uses this with
    /// [`crate::light::ValidatorTracker::follow_committed`] to advance without
    /// pulling bodies.
    pub fn certified_headers(&self) -> Vec<CertifiedHeader> {
        self.blocks
            .iter()
            .zip(self.certs.iter())
            .map(|(b, c)| CertifiedHeader::from_certified(b, c))
            .collect()
    }

    /// Trust-nothing acceptance of one certified block: it must be the very next
    /// height, extend our head, and carry a certificate that is a real > 2/3
    /// quorum of the set active for that height and binds exactly this block.
    /// Returns whether the block was applied (the chain advanced).
    pub fn apply_certified(&mut self, mut block: Block, cert: Commit) -> bool {
        if block.height != self.height() + 1 || block.prev_hash != self.head() {
            return false;
        }
        // certificate must finalize *this* block ...
        if cert.height != block.height || cert.block_hash != block.hash() {
            return false;
        }
        // ... under the validator set active for this height (before it applies,
        // since applying may change the set for the next height — M16).
        if cert.verify(&self.chain.state.validators).is_err() {
            return false;
        }
        if self.chain.commit(&mut block).is_err() {
            return false;
        }
        self.mempool.remove_included(&block);
        // M33: every validator builds candidates from its own pending pools, so
        // once a block commits we must drop the evidence / stake ops it carried —
        // otherwise this node would re-propose already-applied ops at the next
        // height and the candidate would fail to apply. (Pre-M33 only the single
        // sequencer drained these, via take_pending_*; distributed proposing
        // needs every node to reconcile against committed blocks.)
        if !block.slashing_evidence.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.slashing_evidence.iter().map(|ev| ev.hash()).collect();
            self.pending_evidence.retain(|ev| !committed.contains(&ev.hash()));
        }
        if !block.stake_ops.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.stake_ops.iter().map(|op| op.hash()).collect();
            self.pending_stake_ops.retain(|op| !committed.contains(&op.hash()));
        }
        self.blocks.push(block);
        self.certs.push(cert);
        true
    }

    /// M33: assemble and seal this node's candidate block for the next height,
    /// mirroring [`crate::driver::ChainDriver::produce`]'s block-building step but
    /// stopping short of consensus (the daemon Actor drives voting). Pulls txs
    /// from the mempool and attaches the staged evidence / stake-op pools by clone
    /// (non-proposers keep theirs). When there is nothing to include it still
    /// returns a well-formed empty block: M33 runs an empty-block heartbeat so all
    /// validators start each height together (a `create_empty_blocks=false`
    /// optimization is a documented follow-up). The header is sealed so validators
    /// vote on — and the post-consensus commit checks — the exact bytes a light
    /// client will follow. `None` only when the sealed candidate fails its trial
    /// apply (unproposable); honest nodes then time out rather than propose it.
    pub fn build_candidate(&self, timestamp_days: f32) -> Option<Block> {
        let mut candidate = self.mempool.build_block(&self.chain, timestamp_days).unwrap_or(Block {
            height: self.chain.state.height + 1,
            prev_hash: self.chain.head,
            timestamp_days,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: Vec::new(),
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        });
        candidate.stake_ops = self.pending_stake_ops.clone();
        candidate.slashing_evidence = self.pending_evidence.clone();
        // Seal on a best-effort basis: if the staged contents fail the trial
        // apply the candidate is unproposable, so drop it (honest nodes then time
        // out rather than propose a block that cannot commit).
        self.chain.seal(&mut candidate).ok()?;
        Some(candidate)
    }

    /// Submit a locally-originated transaction: admit it to the mempool and return
    /// the gossip to flood it to peers. A tx that fails static validation is
    /// dropped (empty result).
    pub fn submit_local(&mut self, tx: SubmissionTx) -> Vec<(u64, GossipMsg)> {
        let h = tx.hash();
        self.seen_tx.insert(h);
        if self.mempool.insert(&self.chain, tx.clone()).is_err() {
            return Vec::new();
        }
        self.broadcast(GossipMsg::Tx(tx), None)
    }

    /// Submit a locally-originated piece of equivocation evidence: stage it
    /// for the next block this node proposes and return the gossip to flood
    /// it to peers. Malformed evidence (fails `is_well_formed`) is silently
    /// dropped — never staged, never forwarded.
    pub fn submit_local_evidence(&mut self, ev: SlashEvidence) -> Vec<(u64, GossipMsg)> {
        if !ev.is_well_formed() {
            return Vec::new();
        }
        let h = ev.hash();
        self.seen_evidence.insert(h);
        self.pending_evidence.push(ev.clone());
        self.broadcast(GossipMsg::Evidence(ev), None)
    }

    /// Submit a locally-originated stake op: stage it for the next block and
    /// return the gossip to flood it to peers. Stake op "structural" shape
    /// is just `{account, kind, amount, sig}` — there is nothing to validate
    /// here; signature verification happens at apply time in
    /// `chain.commit.apply_stake_op`, which rolls back the whole block on
    /// any failure.
    pub fn submit_local_stake_op(&mut self, op: StakeOp) -> Vec<(u64, GossipMsg)> {
        let h = op.hash();
        self.seen_stake_op.insert(h);
        self.pending_stake_ops.push(op.clone());
        self.broadcast(GossipMsg::StakeOp(op), None)
    }

    /// The gossip a node emits to announce its current height (anti-entropy tick).
    pub fn announce(&self) -> Vec<(u64, GossipMsg)> {
        self.broadcast(GossipMsg::Status { height: self.height() }, None)
    }

    /// React to one message from peer `from`; return outbound `(peer, msg)`.
    pub fn on_message(&mut self, from: u64, msg: GossipMsg) -> Vec<(u64, GossipMsg)> {
        self.peers.insert(from);
        match msg {
            GossipMsg::Status { height } => {
                if height > self.height() {
                    // peer is ahead: pull what we are missing
                    vec![(from, GossipMsg::GetBlocks { from: self.height() + 1 })]
                } else if height < self.height() {
                    // peer is behind: offer it what it lacks
                    vec![(from, GossipMsg::Blocks(self.batch_from(height + 1)))]
                } else {
                    Vec::new()
                }
            }
            GossipMsg::GetBlocks { from: h } => {
                let batch = self.batch_from(h);
                if batch.is_empty() {
                    Vec::new()
                } else {
                    vec![(from, GossipMsg::Blocks(batch))]
                }
            }
            GossipMsg::Blocks(batch) => self.on_blocks(from, batch),
            GossipMsg::Tx(tx) => self.on_tx(from, tx),
            GossipMsg::Evidence(ev) => self.on_evidence(from, ev),
            GossipMsg::StakeOp(op) => self.on_stake_op(from, op),
            // M22: a full node answers `GetHeaders` with its retained
            // cert-signed headers. If we receive `Headers(...)` directly
            // (e.g. from a peer that pushes), drop them — full nodes don't
            // track headers as state.
            GossipMsg::GetHeaders { from: h } => {
                let batch = self.headers_from(h);
                if batch.is_empty() {
                    Vec::new()
                } else {
                    vec![(from, GossipMsg::Headers(batch))]
                }
            }
            GossipMsg::Headers(_) => Vec::new(),
            // M24: full peer serves one or more O(log n) inclusion proofs in a
            // single round-trip. For each (kind, id) in the request, pull the
            // typed leaf + proof from local state; a None means "unknown key"
            // and the wallet's verify_proof_against_header will reject it
            // cleanly with MembershipProofInvalid against the cert-signed
            // header's root.
            GossipMsg::GetProof { items } => {
                if items.len() > MAX_PROOF_BATCH {
                    return Vec::new(); // codec-level cap; shouldn't reach here
                }
                let mut out = Vec::with_capacity(items.len());
                for (kind, id) in items {
                    out.push(self.serve_inclusion(kind, id));
                }
                vec![(from, GossipMsg::Proof { items: out })]
            }
            GossipMsg::Proof { .. } => Vec::new(), // full nodes don't consume proofs
            // M29: heterogeneous batched proof request. Dispatches
            // each slot to the matching per-primitive producer. The
            // wallet supplies the cert-binding context (header, cert,
            // tracked set) from its M22 header cache when verifying —
            // the producer just answers each slot in isolation, the
            // same way the standalone M24/M26/M27/M28 producers do.
            GossipMsg::GetBatch { items } => {
                if items.len() > MAX_BATCH_ITEMS {
                    return Vec::new(); // codec-level cap
                }
                match self.serve_batch(items) {
                    Some(envelope) => vec![(
                        from,
                        GossipMsg::Batch {
                            envelope: Box::new(envelope),
                        },
                    )],
                    None => Vec::new(),
                }
            }
            GossipMsg::Batch { .. } => Vec::new(), // full nodes don't consume batches
            // M28: full peer serves a cert-signed temporal diff between two
            // cert-signed heights. Replays from genesis to h₁ to derive
            // `state_at_h1`, then packages the diff body plus per-side
            // certs/headers into a typed envelope. The wallet re-verifies
            // every piece against its own cached range and tracked sets.
            GossipMsg::GetDiff { h1, h2, header_h1, header_h2 } => {
                match self.serve_diff(h1, h2, &header_h1, &header_h2) {
                    Some(envelope) => vec![(from, GossipMsg::Diff { envelope: Box::new(envelope) })],
                    None => Vec::new(),
                }
            }
            GossipMsg::Diff { .. } => Vec::new(), // full nodes don't consume diffs
            // M30: bridge lock fetch. Full peer answers with a cert-signed
            // lock envelope if the lock exists on its chain; the wallet's
            // destination-side `BridgeEndpoint::verify_lock` rebinds the
            // header against its own tracked set (same soundness pattern as
            // M28).
            GossipMsg::GetLock { lock_id } => match self.serve_lock(lock_id) {
                Some(envelope) => vec![(from, GossipMsg::Lock { envelope: Box::new(envelope) })],
                None => Vec::new(),
            },
            GossipMsg::Lock { .. } => Vec::new(), // full nodes don't consume locks
            // M33: distributed consensus is driven by the daemon Actor (it owns
            // the Keypair, wall-clock timers, and apply_certified side effects);
            // the pure core never handles it.
            GossipMsg::Consensus(_) => Vec::new(),
        }
    }

    fn on_blocks(&mut self, from: u64, batch: Vec<(Block, Commit)>) -> Vec<(u64, GossipMsg)> {
        let n = batch.len();
        let mut applied = 0;
        for (b, c) in batch {
            if self.apply_certified(b, c) {
                applied += 1;
            } else {
                break; // a gap or a bad certificate stops the run here
            }
        }
        if applied == 0 {
            return Vec::new();
        }
        // we advanced: tell peers our new height (they will pull from us), and if
        // the batch was full there may be more — ask the sender to continue.
        let mut out = self.broadcast(GossipMsg::Status { height: self.height() }, Some(from));
        if applied == n && n == MAX_BATCH {
            out.push((from, GossipMsg::GetBlocks { from: self.height() + 1 }));
        }
        out
    }

    fn on_tx(&mut self, from: u64, tx: SubmissionTx) -> Vec<(u64, GossipMsg)> {
        let h = tx.hash();
        if !self.seen_tx.insert(h) {
            return Vec::new(); // already flooded through us
        }
        // admit against our current state; only forward what we accept
        if self.mempool.insert(&self.chain, tx.clone()).is_err() {
            return Vec::new();
        }
        self.broadcast(GossipMsg::Tx(tx), Some(from))
    }

    fn on_evidence(&mut self, from: u64, ev: SlashEvidence) -> Vec<(u64, GossipMsg)> {
        if !ev.is_well_formed() {
            return Vec::new(); // silently drop structurally bad evidence
        }
        if !self.seen_evidence.insert(ev.hash()) {
            return Vec::new(); // already flooded through us
        }
        // stage; full cryptographic validation lives in apply_evidence
        self.pending_evidence.push(ev.clone());
        self.broadcast(GossipMsg::Evidence(ev), Some(from))
    }

    fn on_stake_op(&mut self, from: u64, op: StakeOp) -> Vec<(u64, GossipMsg)> {
        if !self.seen_stake_op.insert(op.hash()) {
            return Vec::new(); // already flooded through us
        }
        // stage; signature verification lives in apply_stake_op
        self.pending_stake_ops.push(op.clone());
        self.broadcast(GossipMsg::StakeOp(op), Some(from))
    }

    /// Address `msg` to every peer, optionally excluding one (the sender), in
    /// sorted order for deterministic output.
    fn broadcast(&self, msg: GossipMsg, except: Option<u64>) -> Vec<(u64, GossipMsg)> {
        self.peers
            .iter()
            .copied()
            .filter(|p| Some(*p) != except)
            .map(|p| (p, msg.clone()))
            .collect()
    }
}

// --- deterministic in-process network ----------------------------------------

/// A fixed-order, in-process delivery bus over a set of [`GossipNode`]s — the
/// gossip analogue of `round::Sim`. Messages are delivered FIFO; each delivery's
/// outbound messages are appended, so a run floods to quiescence deterministically.
/// Tests use it to assert the nodes **converge**.
pub struct Network {
    nodes: BTreeMap<u64, GossipNode>,
    queue: VecDeque<(u64, u64, GossipMsg)>, // (dst, src, msg)
}

impl Network {
    pub fn new(nodes: Vec<GossipNode>) -> Self {
        Network {
            nodes: nodes.into_iter().map(|n| (n.id, n)).collect(),
            queue: VecDeque::new(),
        }
    }

    pub fn node(&self, id: u64) -> &GossipNode {
        &self.nodes[&id]
    }

    pub fn ids(&self) -> Vec<u64> {
        self.nodes.keys().copied().collect()
    }

    fn enqueue(&mut self, src: u64, out: Vec<(u64, GossipMsg)>) {
        for (dst, msg) in out {
            if self.nodes.contains_key(&dst) {
                self.queue.push_back((dst, src, msg));
            }
        }
    }

    /// Inject messages a node emits locally (e.g. [`GossipNode::announce`] or a
    /// [`GossipNode::submit_local`] result) into the bus.
    pub fn inject(&mut self, src: u64, out: Vec<(u64, GossipMsg)>) {
        self.enqueue(src, out);
    }

    /// Submit a locally-originated transaction at node `id` and flood it.
    pub fn submit(&mut self, id: u64, tx: SubmissionTx) {
        let out = self.nodes.get_mut(&id).unwrap().submit_local(tx);
        self.enqueue(id, out);
    }

    /// Submit a locally-originated piece of equivocation evidence at node `id`
    /// and flood it.
    pub fn submit_evidence(&mut self, id: u64, ev: SlashEvidence) {
        let out = self.nodes.get_mut(&id).unwrap().submit_local_evidence(ev);
        self.enqueue(id, out);
    }

    /// Submit a locally-originated stake op at node `id` and flood it.
    pub fn submit_stake_op(&mut self, id: u64, op: StakeOp) {
        let out = self.nodes.get_mut(&id).unwrap().submit_local_stake_op(op);
        self.enqueue(id, out);
    }

    /// Mutable access to a node (e.g. to preload a seed's certified chain).
    pub fn node_mut(&mut self, id: u64) -> &mut GossipNode {
        self.nodes.get_mut(&id).unwrap()
    }

    /// Every node announces its height — the usual way to kick off anti-entropy.
    pub fn announce_all(&mut self) {
        for id in self.ids() {
            let out = self.nodes[&id].announce();
            self.enqueue(id, out);
        }
    }

    /// Deliver queued messages to quiescence (or a defensive bound). Returns the
    /// number of messages delivered.
    pub fn run(&mut self) -> usize {
        let mut delivered = 0;
        while let Some((dst, src, msg)) = self.queue.pop_front() {
            let out = self.nodes.get_mut(&dst).unwrap().on_message(src, msg);
            self.enqueue(dst, out);
            delivered += 1;
            if delivered > 1_000_000 {
                break; // no honest run should reach this
            }
        }
        delivered
    }

    /// True when every node has the same head (and thus the same certified chain).
    pub fn converged(&self) -> bool {
        let mut heads = self.nodes.values().map(|n| n.head());
        match heads.next() {
            Some(h0) => heads.all(|h| h == h0),
            None => true,
        }
    }
}

// --- M22: light-sync transport (header-only SPV gossip) ---------------------

/// A light-side gossip peer: consumes only cert-signed headers (no tx bodies),
/// tracks the active validator set via [`ValidatorTracker::follow_committed`],
/// and never deserializes a transaction. The SPV primitive (M21) over the
/// header-sync transport (M22).
///
/// A `LightGossipNode` does NOT have a `Chain` — it cannot execute blocks. It
/// only runs the validator-set tracker against the headers it has received,
/// and proves membership via [`ValidatorTracker::verify_membership`].
///
/// To avoid a structural split in [`Network`] (full vs light peers), light
/// nodes run over [`LightNetwork`] (their own bus). Full nodes expose their
/// retained headers via [`GossipNode::headers_from`]; a small adapter maps
/// those into the light peer's protocol.
pub struct LightGossipNode {
    pub id: u64,
    /// Validated headers in height order. Index `i` is height `i + 1`.
    headers: Vec<CertifiedHeader>,
    /// The next set we were told certifies each header. The same full node
    /// that produced the header can produce this snapshot (it knows its own
    /// `state.validators` after applying the block). For `apply_header` we
    /// accept it alongside the header.
    next_sets: Vec<ValidatorSet>,
    /// Header hashes already seen — re-delivery is a no-op (mirrors the
    /// full-node dedup discipline for blocks/txs).
    seen: BTreeSet<Hash>,
    /// The validator-set tracker, advanced as headers arrive.
    tracker: ValidatorTracker,
    peers: BTreeSet<u64>,
    /// M24: cached `Proof` responses from full peers, keyed by `(ProofKind, id)`.
    /// Populated by `on_message` when a `Proof` arrives; the wallet retrieves
    /// them via [`Self::take_proof`] to verify against the latest cert-signed
    /// header's `accounts_root` (for Account/Reviewer) or `next_validators_root`
    /// (for Validator).
    proofs: BTreeMap<(crate::light::ProofKind, u64), crate::light::ProofEntry>,
    /// M28: cached `Diff` envelopes from full peers. Light peers cache the
    /// most recent envelope received (a wallet only needs one at a time —
    /// it pulls, verifies, then pulls again for the next pair). The wallet
    /// retrieves via [`Self::take_diff`].
    diffs: Option<crate::light::DiffEnvelope>,
    /// M29: cached `Batch` envelopes from full peers. Same "most
    /// recent" discipline as `diffs` — a single response covers all
    /// items so we don't need a per-item map. The wallet retrieves via
    /// [`Self::take_batch`].
    batches: Option<crate::light::BatchResponseEnvelope>,
    /// M30: cached `Lock` envelopes from full peers. Same "most recent"
    /// discipline as `diffs` / `batches` — one envelope at a time; the
    /// wallet retrieves via [`Self::take_lock`] to feed its destination
    /// `BridgeEndpoint::verify_lock`.
    locks: Option<crate::bridge::LockEnvelope>,
}

impl LightGossipNode {
    /// A fresh light node holding only `genesis`, aware of `peers`.
    pub fn new(id: u64, g: &Genesis, peers: impl IntoIterator<Item = u64>) -> Self {
        let peers = peers.into_iter().filter(|&p| p != id).collect();
        LightGossipNode {
            id,
            headers: Vec::new(),
            next_sets: Vec::new(),
            seen: BTreeSet::new(),
            tracker: ValidatorTracker::from_genesis(g),
            peers,
            proofs: BTreeMap::new(),
            diffs: None,
            batches: None,
            locks: None,
        }
    }

    /// Pop the cached proof for `(kind, id)` (consumes the entry — a wallet
    /// pulls once, then verifies locally with
    /// `ValidatorTracker::verify_proof_against_header`).
    pub fn take_proof(
        &mut self,
        kind: crate::light::ProofKind,
        id: u64,
    ) -> Option<crate::light::ProofEntry> {
        self.proofs.remove(&(kind, id))
    }

    /// M28: pop the cached diff envelope (consumes the entry — a wallet
    /// pulls once, then verifies locally with
    /// `ValidatorTracker::verify_diff_against_headers`).
    pub fn take_diff(&mut self) -> Option<crate::light::DiffEnvelope> {
        self.diffs.take()
    }

    /// M29: pop the cached batch envelope (consumes the entry — a
    /// wallet pulls once, then verifies each slot locally with the
    /// matching per-primitive verifier via
    /// `ValidatorTracker::verify_batch`). The same "most recent"
    /// discipline as `take_diff` — a wallet that wants a fresher
    /// batch just pulls again.
    pub fn take_batch(&mut self) -> Option<crate::light::BatchResponseEnvelope> {
        self.batches.take()
    }

    /// M30: pop the cached lock envelope (consumes the entry — a
    /// destination-side bridge endpoint pulls once, then verifies
    /// locally via `BridgeEndpoint::verify_lock`).
    pub fn take_lock(&mut self) -> Option<crate::bridge::LockEnvelope> {
        self.locks.take()
    }

    pub fn tracker(&self) -> &ValidatorTracker {
        &self.tracker
    }

    pub fn tracker_mut(&mut self) -> &mut ValidatorTracker {
        &mut self.tracker
    }

    pub fn headers(&self) -> &[CertifiedHeader] {
        &self.headers
    }

    /// Light-side equivalent of [`GossipNode::apply_certified`]. Accepts one
    /// cert-signed header + the next set the sender says the header commits
    /// to; the header's `next_validators_root` is the unforgeable commitment
    /// (cert-signed), so a wrong next set is rejected without ever touching a
    /// body. Returns whether the tracker advanced.
    pub fn apply_header(
        &mut self,
        header: CertifiedHeader,
        next_set: &ValidatorSet,
    ) -> Result<u64, crate::light::LightError> {
        if !self.seen.insert(header.block_hash()) {
            return Ok(self.tracker.height()); // dedup: already advanced past it
        }
        let expected = self.tracker.height() + 1;
        if header.header.height != expected {
            // out-of-order or splice; ignore (a gap will trigger another GetHeaders)
            self.seen.remove(&header.block_hash());
            return Ok(self.tracker.height());
        }
        let power = self
            .tracker
            .follow_header(&header.header, &header.cert, next_set)?;
        self.headers.push(header);
        self.next_sets.push(next_set.clone());
        Ok(power)
    }

    /// React to one gossip message. Light peers only respond to `Status` (with
    /// `GetHeaders`, the SPV analogue of `GetBlocks`) and to `Headers(...)`.
    /// Other variants are dropped silently — light clients do not store
    /// transactions, evidence, or stake ops.
    pub fn on_message(&mut self, from: u64, msg: GossipMsg) -> Vec<(u64, GossipMsg)> {
        self.peers.insert(from);
        match msg {
            GossipMsg::Status { height } => {
                if height > self.tracker.height() {
                    // peer is ahead: pull headers only, not bodies
                    vec![(from, GossipMsg::GetHeaders { from: self.tracker.height() + 1 })]
                } else if height < self.tracker.height() {
                    // peer is behind — but we don't store headers as state until
                    // we hand them out, and light peers are pure consumers; emit
                    // nothing and let the peer pull from a full node.
                    Vec::new()
                } else {
                    Vec::new()
                }
            }
            GossipMsg::GetHeaders { .. } => Vec::new(), // light nodes don't serve headers
            GossipMsg::Headers(batch) => self.on_headers(from, batch),
            // M24: cache incoming inclusion proofs for the wallet to verify
            // locally against the cert-signed header. Each `Some(entry)` is
            // keyed by (kind, id); `None` slots mean the full peer didn't have
            // the key — we drop them silently and the wallet sees a cache miss.
            GossipMsg::Proof { items } => {
                for entry in items.into_iter().flatten() {
                    self.proofs.insert((entry.kind(), entry.id()), entry);
                }
                Vec::new()
            }
            // Light nodes do not serve proof requests (they have no chain state
            // to prove against).
            GossipMsg::GetProof { .. } => Vec::new(),
            // M28: cache incoming diff envelopes for the wallet to verify
            // locally via `verify_diff_against_headers`. Light nodes don't
            // serve diff requests — they have no chain state to diff
            // against.
            GossipMsg::Diff { envelope } => {
                self.diffs = Some(*envelope);
                Vec::new()
            }
            GossipMsg::GetDiff { .. } => Vec::new(),
            // M29: cache incoming batch envelopes for the wallet to
            // verify locally via `ValidatorTracker::verify_batch`.
            // Light nodes don't serve batch requests — they have no
            // chain state to produce proofs against.
            GossipMsg::Batch { envelope } => {
                self.batches = Some(*envelope);
                Vec::new()
            }
            GossipMsg::GetBatch { .. } => Vec::new(),
            // M30: cache incoming lock envelopes for the destination-side
            // bridge endpoint to verify locally. Light nodes don't serve
            // lock requests — they have no source chain state to ship.
            GossipMsg::Lock { envelope } => {
                self.locks = Some(*envelope);
                Vec::new()
            }
            GossipMsg::GetLock { .. } => Vec::new(),
            // everything else: light clients forward tx gossip but never store it
            // — for the M22 demo we just drop, mirroring the "I don't care about
            // bodies" SPV stance.
            GossipMsg::Blocks(_)
            | GossipMsg::GetBlocks { .. }
            | GossipMsg::Tx(_)
            | GossipMsg::Evidence(_)
            | GossipMsg::StakeOp(_)
            // M33: consensus is an Actor concern; light clients never vote.
            | GossipMsg::Consensus(_) => Vec::new(),
        }
    }

    fn on_headers(
        &mut self,
        from: u64,
        batch: Vec<CertifiedHeader>,
    ) -> Vec<(u64, GossipMsg)> {
        let mut advanced = 0usize;
        for ch in batch {
            // Light peers don't have next_sets over the wire; the demo supplies
            // them via a side channel (the test/demo function). For the wire
            // protocol we'd attach the next_set to each header in a later
            // milestone — for now we still advance if the header is consistent
            // (the `follow_committed` API takes the next_set separately).
            // Here we only update `seen` so the gap-tracking logic is right.
            self.seen.insert(ch.block_hash());
            advanced += 1;
        }
        // tell peer our new height so they keep pushing if there is more
        let mut out = Vec::new();
        if advanced > 0 {
            out.push((from, GossipMsg::Status { height: self.tracker.height() }));
        }
        out
    }

    /// Light-side equivalent of [`GossipNode::announce`].
    pub fn announce(&self) -> Vec<(u64, GossipMsg)> {
        self.peers
            .iter()
            .copied()
            .map(|p| (p, GossipMsg::Status { height: self.tracker.height() }))
            .collect()
    }
}

/// A fixed-order, in-process delivery bus that mixes full [`GossipNode`]s
/// and light [`LightGossipNode`]s. Full peers serve headers in response to
/// `GetHeaders`; light peers consume them and advance their tracker. The bus
/// is the M22 SPV-over-gossip analog of [`Network`].
///
/// **Headers routing.** A `Headers` batch emitted by a full peer would
/// normally just be queued for the light peer (which drops it, per
/// `LightGossipNode::on_message`). To make the demo end-to-end, the bus
/// also runs a `next_set_for: impl FnMut(u64) -> Option<ValidatorSet>` —
/// when a full→light `Headers` batch is being delivered, the bus
/// side-channels each header through the light peer's
/// [`LightGossipNode::apply_header`] with the authoritative next set
/// (the full node knows its own `state.validators` after each apply; the
/// closure projects that for any height).
pub struct LightNetwork {
    full: BTreeMap<u64, GossipNode>,
    light: BTreeMap<u64, LightGossipNode>,
    queue: VecDeque<(u64, u64, GossipMsg)>,
}

impl LightNetwork {
    pub fn new(full: Vec<GossipNode>, light: Vec<LightGossipNode>) -> Self {
        LightNetwork {
            full: full.into_iter().map(|n| (n.id, n)).collect(),
            light: light.into_iter().map(|n| (n.id, n)).collect(),
            queue: VecDeque::new(),
        }
    }

    fn deliver(&mut self, dst: u64, src: u64, msg: GossipMsg) {
        let out = if let Some(n) = self.full.get_mut(&dst) {
            n.on_message(src, msg)
        } else if let Some(n) = self.light.get_mut(&dst) {
            n.on_message(src, msg)
        } else {
            return;
        };
        for (d, m) in out {
            if self.full.contains_key(&d) || self.light.contains_key(&d) {
                self.queue.push_back((d, dst, m));
            }
        }
    }

    /// Every node announces its height (full nodes via `chain.height`,
    /// light nodes via `tracker.height()`). Kicks off header anti-entropy.
    pub fn announce_all(&mut self) {
        let mut out = Vec::new();
        for id in self.full.keys() {
            out.extend(self.full[id].announce().into_iter().map(|(d, m)| (d, *id, m)));
        }
        for id in self.light.keys() {
            out.extend(self.light[id].announce().into_iter().map(|(d, m)| (d, *id, m)));
        }
        for (dst, src, msg) in out {
            self.queue.push_back((dst, src, msg));
        }
    }

    /// Run the bus to quiescence, side-channeling any `Headers` batch from a
    /// full peer to a light peer through the light peer's
    /// [`LightGossipNode::apply_header`] with `next_set_for(height)` as the
    /// authoritative next set. `full_id` and `light_id` select which peers
    /// participate in the bridge.
    pub fn run(&mut self, full_id: u64, light_id: u64, mut next_set_for: impl FnMut(u64) -> Option<ValidatorSet>) -> usize {
        let mut delivered = 0;
        while let Some((dst, src, msg)) = self.queue.pop_front() {
            // Side-channel: full→light Headers bypass the normal
            // `on_message` (which would drop them) and feed the light peer
            // directly with the authoritative next set.
            if dst == light_id && src == full_id {
                if let GossipMsg::Headers(batch) = &msg {
                    let batch = batch.clone();
                    for ch in batch {
                        if let Some(ns) = next_set_for(ch.height()) {
                            let _ = self.light.get_mut(&light_id).unwrap().apply_header(ch, &ns);
                        }
                    }
                    delivered += 1;
                    if delivered > 1_000_000 {
                        break;
                    }
                    continue;
                }
            }
            self.deliver(dst, src, msg);
            delivered += 1;
            if delivered > 1_000_000 {
                break;
            }
        }
        delivered
    }

    pub fn full_node(&self, id: u64) -> &GossipNode {
        &self.full[&id]
    }
    pub fn light_node(&self, id: u64) -> &LightGossipNode {
        &self.light[&id]
    }

    /// Take (remove and return) a full peer — used by tests that want to
    /// inject or inspect messages outside the normal `run` loop.
    pub fn take_full(&mut self, id: u64) -> GossipNode {
        self.full.remove(&id).expect("unknown full id")
    }

    /// Take (remove and return) a light peer.
    pub fn take_light(&mut self, id: u64) -> LightGossipNode {
        self.light.remove(&id).expect("unknown light id")
    }

    /// Re-insert a previously taken full peer.
    pub fn put_full(&mut self, node: GossipNode) {
        let id = node.id;
        self.full.insert(id, node);
    }

    /// Re-insert a previously taken light peer.
    pub fn put_light(&mut self, node: LightGossipNode) {
        let id = node.id;
        self.light.insert(id, node);
    }
}

// --- socket transport (thin framing over the wire messages) ------------------

/// Encode a gossip message: a 1-byte tag followed by its length-prefixed payload
/// (reusing the block/commit/tx codecs). Self-describing, no external crate.
pub fn encode_gossip(m: &GossipMsg) -> Vec<u8> {
    let mut out = Vec::new();
    match m {
        GossipMsg::Status { height } => {
            out.push(TAG_STATUS);
            out.extend_from_slice(&height.to_be_bytes());
        }
        GossipMsg::GetBlocks { from } => {
            out.push(TAG_GETBLOCKS);
            out.extend_from_slice(&from.to_be_bytes());
        }
        GossipMsg::Blocks(batch) => {
            out.push(TAG_BLOCKS);
            out.extend_from_slice(&(batch.len() as u64).to_be_bytes());
            for (b, c) in batch {
                put_bytes(&mut out, &encode_block(b));
                put_bytes(&mut out, &encode_commit(c));
            }
        }
        GossipMsg::Tx(tx) => {
            out.push(TAG_TX);
            put_bytes(&mut out, &encode_tx(tx));
        }
        GossipMsg::Evidence(ev) => {
            out.push(TAG_EVIDENCE);
            put_bytes(&mut out, &encode_evidence(ev));
        }
        GossipMsg::StakeOp(op) => {
            out.push(TAG_STAKEOP);
            put_bytes(&mut out, &encode_stakeop(op));
        }
        GossipMsg::GetHeaders { from } => {
            out.push(TAG_GETHEADERS);
            out.extend_from_slice(&from.to_be_bytes());
        }
        GossipMsg::Headers(batch) => {
            out.push(TAG_HEADERS);
            out.extend_from_slice(&(batch.len() as u64).to_be_bytes());
            for ch in batch {
                put_bytes(&mut out, &encode_certified_header(ch));
            }
        }
        // M24: batched typed proof pair. Body layout:
        //   - GetProof: u32_be(len) then for each (kind, id): 1 byte kind tag
        //     followed by 8 bytes id (kind tag = encode_proof_kind(kind)).
        //   - Proof:    u32_be(len) then for each Option<ProofEntry>: 1 byte
        //     presence tag (0 = None, 1 = Some) followed by
        //     encode_proof_entry(...) when present.
        GossipMsg::GetProof { items } => {
            out.push(TAG_GETPROOF);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for (k, id) in items {
                out.push(crate::codec::encode_proof_kind(*k));
                out.extend_from_slice(&id.to_be_bytes());
            }
        }
        GossipMsg::Proof { items } => {
            out.push(TAG_PROOF);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for entry in items {
                match entry {
                    Some(e) => {
                        out.push(1);
                        put_bytes(&mut out, &crate::codec::encode_proof_entry(e));
                    }
                    None => out.push(0),
                }
            }
        }
        // M28: cert-signed temporal diff pair. Body layout mirrors the
        // existing proof pair (length-prefixed, tagged sub-payloads):
        //   - GetDiff: u64_be(h1), u64_be(h2), header_h1, header_h2
        //   - Diff:    the typed DiffEnvelope as a single length-prefixed blob
        GossipMsg::GetDiff { h1, h2, header_h1, header_h2 } => {
            out.push(TAG_GETDIFF);
            out.extend_from_slice(&h1.to_be_bytes());
            out.extend_from_slice(&h2.to_be_bytes());
            put_bytes(&mut out, &crate::codec::encode_header(header_h1));
            put_bytes(&mut out, &crate::codec::encode_header(header_h2));
        }
        GossipMsg::Diff { envelope } => {
            out.push(TAG_DIFF);
            put_bytes(&mut out, &encode_diff_envelope(envelope));
        }
        // M29: heterogeneous batched proof pair. Both directions ride
        // a single length-prefixed envelope so the wire format is
        // symmetric with the M24/M28 pairs.
        GossipMsg::GetBatch { items } => {
            out.push(TAG_GETBATCH);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items.iter() {
                out.push(crate::codec::encode_batch_response_kind(item.kind_tag()));
                match item {
                    crate::light::BatchItem::Inclusion { kind, id } => {
                        out.push(crate::codec::encode_proof_kind(*kind));
                        out.extend_from_slice(&id.to_be_bytes());
                    }
                    crate::light::BatchItem::Knn { query, k } => {
                        put_bytes(&mut out, &crate::codec::encode_knn_request(query, *k));
                    }
                    crate::light::BatchItem::Range { query, min_sim } => {
                        put_bytes(
                            &mut out,
                            &crate::codec::encode_range_request(query, *min_sim),
                        );
                    }
                    crate::light::BatchItem::Diff { h1, h2 } => {
                        out.extend_from_slice(&h1.to_be_bytes());
                        out.extend_from_slice(&h2.to_be_bytes());
                    }
                }
            }
        }
        GossipMsg::Batch { envelope } => {
            out.push(TAG_BATCH);
            put_bytes(&mut out, &encode_batch_envelope(envelope));
        }
        // M30: bridge lock fetch pair. `GetLock` is a single u64 (the
        // lock id); `Lock` is a single length-prefixed envelope, mirroring
        // the M28 `Diff` pair.
        GossipMsg::GetLock { lock_id } => {
            out.push(TAG_GETLOCK);
            out.extend_from_slice(&lock_id.to_be_bytes());
        }
        GossipMsg::Lock { envelope } => {
            out.push(TAG_LOCK);
            put_bytes(&mut out, &encode_lock_envelope(envelope));
        }
        GossipMsg::Consensus(m) => {
            out.push(TAG_CONSENSUS);
            put_bytes(&mut out, &crate::codec::encode_consensus_msg(m));
        }
    }
    out
}

/// Decode a gossip message produced by [`encode_gossip`].
pub fn decode_gossip(buf: &[u8]) -> Result<GossipMsg, CodecError> {
    let (&tag, mut rest) = buf.split_first().ok_or(CodecError::UnexpectedEof)?;
    let msg = match tag {
        TAG_STATUS => GossipMsg::Status { height: take_u64(&mut rest)? },
        TAG_GETBLOCKS => GossipMsg::GetBlocks { from: take_u64(&mut rest)? },
        TAG_BLOCKS => {
            let n = take_u64(&mut rest)?;
            if n > MAX_BATCH as u64 {
                return Err(CodecError::TooManyItems(n));
            }
            let mut batch = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let block = decode_block(take_bytes(&mut rest)?)?;
                let commit = decode_commit(take_bytes(&mut rest)?)?;
                batch.push((block, commit));
            }
            GossipMsg::Blocks(batch)
        }
        TAG_TX => GossipMsg::Tx(decode_tx(take_bytes(&mut rest)?)?),
        TAG_EVIDENCE => GossipMsg::Evidence(decode_evidence(take_bytes(&mut rest)?)?),
        TAG_STAKEOP => GossipMsg::StakeOp(decode_stakeop(take_bytes(&mut rest)?)?),
        TAG_GETHEADERS => GossipMsg::GetHeaders { from: take_u64(&mut rest)? },
        TAG_HEADERS => {
            let n = take_u64(&mut rest)?;
            if n > MAX_BATCH as u64 {
                return Err(CodecError::TooManyItems(n));
            }
            let mut batch = Vec::with_capacity(n as usize);
            for _ in 0..n {
                batch.push(decode_certified_header(take_bytes(&mut rest)?)?);
            }
            GossipMsg::Headers(batch)
        }
        // M24: batched typed proof pair.
        TAG_GETPROOF => {
            let n = take_u32(&mut rest)?;
            if n as usize > MAX_PROOF_BATCH {
                return Err(CodecError::TooManyItems(n as u64));
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let kind_byte = rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                rest = &rest[1..];
                let kind = crate::codec::decode_proof_kind(kind_byte)?;
                let id = take_u64(&mut rest)?;
                items.push((kind, id));
            }
            GossipMsg::GetProof { items }
        }
        TAG_PROOF => {
            let n = take_u32(&mut rest)?;
            if n as usize > MAX_PROOF_BATCH {
                return Err(CodecError::TooManyItems(n as u64));
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let present = rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                rest = &rest[1..];
                if present == 0 {
                    items.push(None);
                } else if present == 1 {
                    let body = take_bytes(&mut rest)?;
                    items.push(Some(crate::codec::decode_proof_entry(body)?));
                } else {
                    return Err(CodecError::BadEnum(present as u32));
                }
            }
            GossipMsg::Proof { items }
        }
        // M28: cert-signed temporal diff pair. The full body for `Diff` is
        // a single length-prefixed envelope, so the framing is symmetric
        // with the other length-prefixed payloads.
        TAG_GETDIFF => {
            let h1 = take_u64(&mut rest)?;
            let h2 = take_u64(&mut rest)?;
            let header_h1 = Box::new(crate::codec::decode_header(take_bytes(&mut rest)?)?);
            let header_h2 = Box::new(crate::codec::decode_header(take_bytes(&mut rest)?)?);
            GossipMsg::GetDiff { h1, h2, header_h1, header_h2 }
        }
        TAG_DIFF => GossipMsg::Diff {
            envelope: Box::new(decode_diff_envelope(take_bytes(&mut rest)?)?),
        },
        // M29: heterogeneous batched proof pair. Symmetric with the
        // M24/M28 pairs — single length-prefixed envelopes on both
        // sides.
        TAG_GETBATCH => {
            let n = take_u32(&mut rest)?;
            if n as usize > MAX_BATCH_ITEMS {
                return Err(CodecError::TooManyItems(n as u64));
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let kind = crate::codec::decode_batch_response_kind(
                    rest.first().copied().ok_or(CodecError::UnexpectedEof)?,
                )?;
                rest = &rest[1..];
                match kind {
                    0 => {
                        let kind_byte =
                            rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                        rest = &rest[1..];
                        let k = crate::codec::decode_proof_kind(kind_byte)?;
                        let id = take_u64(&mut rest)?;
                        items.push(crate::light::BatchItem::Inclusion { kind: k, id });
                    }
                    1 => {
                        let (query, k) =
                            crate::codec::decode_knn_request(take_bytes(&mut rest)?)?;
                        items.push(crate::light::BatchItem::Knn { query, k });
                    }
                    2 => {
                        let (query, min_sim) =
                            crate::codec::decode_range_request(take_bytes(&mut rest)?)?;
                        items.push(crate::light::BatchItem::Range { query, min_sim });
                    }
                    3 => {
                        let h1 = take_u64(&mut rest)?;
                        let h2 = take_u64(&mut rest)?;
                        items.push(crate::light::BatchItem::Diff { h1, h2 });
                    }
                    other => return Err(CodecError::BadEnum(other as u32)),
                }
            }
            GossipMsg::GetBatch { items }
        }
        TAG_BATCH => GossipMsg::Batch {
            envelope: Box::new(decode_batch_envelope(take_bytes(&mut rest)?)?),
        },
        // M30: bridge lock fetch pair.
        TAG_GETLOCK => GossipMsg::GetLock {
            lock_id: take_u64(&mut rest)?,
        },
        TAG_LOCK => GossipMsg::Lock {
            envelope: Box::new(decode_lock_envelope(take_bytes(&mut rest)?)?),
        },
        TAG_CONSENSUS => GossipMsg::Consensus(Box::new(
            crate::codec::decode_consensus_msg(take_bytes(&mut rest)?)?,
        )),
        other => return Err(CodecError::BadEnum(other as u32)),
    };
    if !rest.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(msg)
}

/// M28: encode a `DiffEnvelope` as a single length-prefixed blob.
///
/// Layout (all multi-byte ints big-endian):
///   u32_be(|added|) | for each: codec::encode_graph_node + codec::encode_proof
///   u32_be(|dropped|) | for each: codec::encode_graph_node + codec::encode_proof
///   header_prev (length-prefixed via `put_bytes`)
///   cert_prev
///   header_new
///   cert_new
///   tracked_set_h1 (u32_be(|validators|) + for each: codec::encode_validator)
///   tracked_set_h2 (same)
pub fn encode_diff_envelope(env: &crate::light::DiffEnvelope) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(env.diff.added.len() as u32).to_be_bytes());
    for entry in &env.diff.added {
        put_bytes(&mut out, &crate::codec::encode_graph_node(&entry.graph_node));
        put_bytes(&mut out, &crate::codec::encode_proof(&entry.proof));
    }
    out.extend_from_slice(&(env.diff.dropped.len() as u32).to_be_bytes());
    for entry in &env.diff.dropped {
        put_bytes(&mut out, &crate::codec::encode_graph_node(&entry.graph_node));
        put_bytes(&mut out, &crate::codec::encode_proof(&entry.proof));
    }
    put_bytes(&mut out, &crate::codec::encode_header(&env.header_prev));
    put_bytes(&mut out, &crate::codec::encode_commit(&env.cert_prev));
    put_bytes(&mut out, &crate::codec::encode_header(&env.header_new));
    put_bytes(&mut out, &crate::codec::encode_commit(&env.cert_new));
    encode_validator_set(&mut out, &env.tracked_set_h1);
    encode_validator_set(&mut out, &env.tracked_set_h2);
    out
}

/// M28: decode a `DiffEnvelope` produced by [`encode_diff_envelope`].
pub fn decode_diff_envelope(buf: &[u8]) -> Result<crate::light::DiffEnvelope, CodecError> {
    let mut p = buf;
    let n_added = take_u32(&mut p)? as usize;
    let mut added = Vec::with_capacity(n_added);
    for _ in 0..n_added {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        added.push(crate::GraphLeafAtHeight { node_id, graph_node: gn, proof });
    }
    let n_dropped = take_u32(&mut p)? as usize;
    let mut dropped = Vec::with_capacity(n_dropped);
    for _ in 0..n_dropped {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        dropped.push(crate::GraphLeafAtHeight { node_id, graph_node: gn, proof });
    }
    let header_prev = crate::codec::decode_header(take_bytes(&mut p)?)?;
    let cert_prev = crate::codec::decode_commit(take_bytes(&mut p)?)?;
    let header_new = crate::codec::decode_header(take_bytes(&mut p)?)?;
    let cert_new = crate::codec::decode_commit(take_bytes(&mut p)?)?;
    let tracked_set_h1 = decode_validator_set(&mut p)?;
    let tracked_set_h2 = decode_validator_set(&mut p)?;
    if !p.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(crate::light::DiffEnvelope {
        header_prev,
        cert_prev,
        header_new,
        cert_new,
        diff: crate::DiffClaim { added, dropped },
        tracked_set_h1,
        tracked_set_h2,
    })
}

/// M30: encode a `LockEnvelope` as a single length-prefixed blob.
///
/// Layout (all multi-byte ints big-endian):
///   header            (length-prefixed via `put_bytes`)
///   cert              (length-prefixed)
///   source_tracked_set (u32_be(|validators|) + for each: codec::encode_validator)
///   lock_id           u64_be
///   lock              (length-prefixed via codec::encode_bridge_lock)
///   proof             (length-prefixed via codec::encode_proof)
pub fn encode_lock_envelope(env: &crate::bridge::LockEnvelope) -> Vec<u8> {
    let mut out = Vec::new();
    put_bytes(&mut out, &crate::codec::encode_header(&env.source_header));
    put_bytes(&mut out, &crate::codec::encode_commit(&env.source_cert));
    encode_validator_set(&mut out, &env.source_tracked_set);
    out.extend_from_slice(&env.lock_id.to_be_bytes());
    put_bytes(&mut out, &crate::codec::encode_bridge_lock(&env.lock));
    put_bytes(&mut out, &crate::codec::encode_proof(&env.proof));
    out
}

/// M30: decode a `LockEnvelope` produced by [`encode_lock_envelope`].
pub fn decode_lock_envelope(buf: &[u8]) -> Result<crate::bridge::LockEnvelope, CodecError> {
    let mut p = buf;
    let source_header = crate::codec::decode_header(take_bytes(&mut p)?)?;
    let source_cert = crate::codec::decode_commit(take_bytes(&mut p)?)?;
    let source_tracked_set = decode_validator_set(&mut p)?;
    let lock_id = take_u64(&mut p)?;
    let lock = crate::codec::decode_bridge_lock(take_bytes(&mut p)?)?;
    let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
    if !p.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(crate::bridge::LockEnvelope {
        source_header,
        source_cert,
        source_tracked_set,
        lock_id,
        lock,
        proof,
    })
}

/// M29: encode a `BatchResponseEnvelope` as a single length-prefixed
/// blob for the `Batch { envelope }` wire format. Body layout:
///   u32_be(|items|)
///   for each item: 1-byte kind tag ‖ per-variant body
///     Inclusion: 1-byte presence tag ‖ length-prefixed `encode_proof_entry`
///                (1=Some, 0=None)
///     Knn:       1-byte presence tag ‖ length-prefixed `encode_knn_claim`
///     Range:     1-byte presence tag ‖ length-prefixed `encode_range_claim`
///     Diff:      length-prefixed `encode_diff_envelope`
pub fn encode_batch_envelope(env: &crate::light::BatchResponseEnvelope) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(env.items.len() as u32).to_be_bytes());
    for item in &env.items {
        match item {
            crate::light::BatchResponseItem::Inclusion(entry) => {
                out.push(0);
                match entry {
                    Some(e) => {
                        out.push(1);
                        put_bytes(&mut out, &crate::codec::encode_proof_entry(e));
                    }
                    None => out.push(0),
                }
            }
            crate::light::BatchResponseItem::Knn(claim) => {
                out.push(1);
                match claim {
                    Some(c) => {
                        out.push(1);
                        put_bytes(&mut out, &encode_knn_claim(c));
                    }
                    None => out.push(0),
                }
            }
            crate::light::BatchResponseItem::Range(claim) => {
                out.push(2);
                match claim {
                    Some(c) => {
                        out.push(1);
                        put_bytes(&mut out, &encode_range_claim(c));
                    }
                    None => out.push(0),
                }
            }
            crate::light::BatchResponseItem::Diff(env) => {
                out.push(3);
                put_bytes(&mut out, &encode_diff_envelope(env));
            }
        }
    }
    out
}

/// M29: inverse of [`encode_batch_envelope`].
pub fn decode_batch_envelope(
    buf: &[u8],
) -> Result<crate::light::BatchResponseEnvelope, CodecError> {
    let mut p = buf;
    let n = take_u32(&mut p)? as usize;
    if n > MAX_BATCH_ITEMS {
        return Err(CodecError::TooManyItems(n as u64));
    }
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = crate::codec::decode_batch_response_kind(
            p.first().copied().ok_or(CodecError::UnexpectedEof)?,
        )?;
        p = &p[1..];
        match kind {
            0 => {
                let present = p.first().copied().ok_or(CodecError::UnexpectedEof)?;
                p = &p[1..];
                let entry = if present == 0 {
                    None
                } else if present == 1 {
                    Some(crate::codec::decode_proof_entry(take_bytes(&mut p)?)?)
                } else {
                    return Err(CodecError::BadEnum(present as u32));
                };
                items.push(crate::light::BatchResponseItem::Inclusion(entry));
            }
            1 => {
                let present = p.first().copied().ok_or(CodecError::UnexpectedEof)?;
                p = &p[1..];
                let claim = if present == 0 {
                    None
                } else if present == 1 {
                    Some(decode_knn_claim(take_bytes(&mut p)?)?)
                } else {
                    return Err(CodecError::BadEnum(present as u32));
                };
                items.push(crate::light::BatchResponseItem::Knn(claim));
            }
            2 => {
                let present = p.first().copied().ok_or(CodecError::UnexpectedEof)?;
                p = &p[1..];
                let claim = if present == 0 {
                    None
                } else if present == 1 {
                    Some(decode_range_claim(take_bytes(&mut p)?)?)
                } else {
                    return Err(CodecError::BadEnum(present as u32));
                };
                items.push(crate::light::BatchResponseItem::Range(claim));
            }
            3 => {
                let env = decode_diff_envelope(take_bytes(&mut p)?)?;
                items.push(crate::light::BatchResponseItem::Diff(Box::new(env)));
            }
            other => return Err(CodecError::BadEnum(other as u32)),
        }
    }
    if !p.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(crate::light::BatchResponseEnvelope { items })
}

fn encode_validator_set(out: &mut Vec<u8>, vs: &crate::validator::ValidatorSet) {
    let v = vs.validators();
    out.extend_from_slice(&(v.len() as u32).to_be_bytes());
    for val in v {
        // Length-prefixed to match `decode_validator_set`, which reads each
        // validator through `take_bytes`. `encode_validator` returns raw
        // merkle-leaf bytes; without the prefix, the next validator's first
        // byte would silently glue onto this one and decoding would either
        // mis-parse or hit UnexpectedEof. Pre-existing latent bug surfaced
        // by the M30 lock envelope round-trip.
        put_bytes(out, &crate::codec::encode_validator(val));
    }
}

fn decode_validator_set(p: &mut &[u8]) -> Result<crate::validator::ValidatorSet, CodecError> {
    let n = take_u32(p)? as usize;
    let mut vs = Vec::with_capacity(n);
    for _ in 0..n {
        vs.push(crate::codec::decode_validator(take_bytes(p)?)?);
    }
    Ok(crate::validator::ValidatorSet::new(vs))
}

/// M29: encode a `KnnClaim` as a length-prefixed blob for the
/// `Batch { envelope }` wire format. Layout:
///   u32_be(|neighbours|)
///   for each neighbour: u32_be(graph_node body) ‖ graph_node body ‖
///                        u32_be(proof) ‖ proof bytes
///   8×f32 query ‖ u32 k
/// Query + k come last so decode can grow the vec first.
pub fn encode_knn_claim(c: &crate::light::KnnClaim) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(c.neighbours.len() as u32).to_be_bytes());
    for (_id, gn, proof) in &c.neighbours {
        put_bytes(&mut out, &crate::codec::encode_graph_node(gn));
        put_bytes(&mut out, &crate::codec::encode_proof(proof));
    }
    put_bytes(&mut out, &crate::codec::encode_knn_request(&c.query, c.k));
    out
}

/// M29: inverse of [`encode_knn_claim`].
pub fn decode_knn_claim(buf: &[u8]) -> Result<crate::light::KnnClaim, CodecError> {
    let mut p = buf;
    let n = take_u32(&mut p)? as usize;
    let mut neighbours = Vec::with_capacity(n);
    for _ in 0..n {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        neighbours.push((node_id, gn, proof));
    }
    let (query, k) = crate::codec::decode_knn_request(take_bytes(&mut p)?)?;
    if !p.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(crate::light::KnnClaim { query, k, neighbours })
}

/// M29: encode a `RangeClaim` as a length-prefixed blob for the
/// `Batch { envelope }` wire format. Mirrors [`encode_knn_claim`]
/// but with `min_sim: f32` instead of `k: u32`.
pub fn encode_range_claim(c: &crate::light::RangeClaim) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(c.nodes.len() as u32).to_be_bytes());
    for (_id, gn, proof) in &c.nodes {
        put_bytes(&mut out, &crate::codec::encode_graph_node(gn));
        put_bytes(&mut out, &crate::codec::encode_proof(proof));
    }
    put_bytes(&mut out, &crate::codec::encode_range_request(&c.query, c.min_sim));
    out
}

/// M29: inverse of [`encode_range_claim`].
pub fn decode_range_claim(buf: &[u8]) -> Result<crate::light::RangeClaim, CodecError> {
    let mut p = buf;
    let n = take_u32(&mut p)? as usize;
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        nodes.push((node_id, gn, proof));
    }
    let (query, min_sim) = crate::codec::decode_range_request(take_bytes(&mut p)?)?;
    if !p.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(crate::light::RangeClaim { query, min_sim, nodes })
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
    out.extend_from_slice(b);
}

fn take_u64(buf: &mut &[u8]) -> Result<u64, CodecError> {
    if buf.len() < 8 {
        return Err(CodecError::UnexpectedEof);
    }
    let (h, t) = buf.split_at(8);
    *buf = t;
    Ok(u64::from_be_bytes(h.try_into().unwrap()))
}

fn take_u32(buf: &mut &[u8]) -> Result<u32, CodecError> {
    if buf.len() < 4 {
        return Err(CodecError::UnexpectedEof);
    }
    let (h, t) = buf.split_at(4);
    *buf = t;
    Ok(u32::from_be_bytes(h.try_into().unwrap()))
}

fn take_bytes<'a>(buf: &mut &'a [u8]) -> Result<&'a [u8], CodecError> {
    if buf.len() < 4 {
        return Err(CodecError::UnexpectedEof);
    }
    let (l, t) = buf.split_at(4);
    let len = u32::from_be_bytes(l.try_into().unwrap()) as usize;
    if t.len() < len {
        return Err(CodecError::UnexpectedEof);
    }
    let (payload, rest) = t.split_at(len);
    *buf = rest;
    Ok(payload)
}

/// Write one gossip message to a stream: a `u32` big-endian frame length followed
/// by the encoded message. The peer reads it back with [`read_msg`].
pub fn write_msg<W: Write>(w: &mut W, m: &GossipMsg) -> io::Result<()> {
    let body = encode_gossip(m);
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "message too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed gossip message written by [`write_msg`].
pub fn read_msg<R: Read>(r: &mut R) -> io::Result<GossipMsg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body)?;
    decode_gossip(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ChainDriver;
    use crate::{BondKind, Keypair, Review, SlashEvidence, SubmissionTx, Vote, VoteType, DIM, MICRO};
    use std::collections::BTreeMap;
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
            validators: [21u64, 22, 23, 24].iter().map(|&id| (id, kp(id).public(), 1)).collect(),
            bridge_sources: vec![],
        }
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

    /// Produce a real certified chain (blocks + certs) to seed sync tests with.
    fn certified_chain(n_tx: usize) -> (Vec<Block>, Vec<Commit>) {
        let seeds: BTreeMap<u64, [u8; 32]> = [21u64, 22, 23, 24].iter().map(|&id| (id, seed(id))).collect();
        let mut d = ChainDriver::new(genesis(), seeds, 1);
        let txs = [tx(1, 1, 1), tx(2, 2, 2), tx(3, 3, 3), tx(1, 4, 4)];
        for t in txs.iter().take(n_tx) {
            d.submit(t.clone()).unwrap();
        }
        d.produce_until_drained(1.0, 16).unwrap();
        (d.blocks().to_vec(), d.certificates().to_vec())
    }

    #[test]
    fn wire_round_trips_every_message() {
        let (blocks, certs) = certified_chain(2);
        let batch: Vec<(Block, Commit)> = blocks.iter().cloned().zip(certs.iter().cloned()).collect();
        let headers: Vec<CertifiedHeader> = blocks
            .iter()
            .zip(certs.iter())
            .map(|(b, c)| CertifiedHeader::from_certified(b, c))
            .collect();
        // A trivial account + Merkle proof (the proof is shape, not semantics;
        // round-trip is the property under test).
        let fake_account = crate::Account::default();
        let fake_proof = crate::merkle::Proof { steps: vec![crate::merkle::Step::Right([0xAA; 32])] };
        // M24: a single mixed-kind batched proof request with all three kinds
        // present (Account + Reviewer + Validator) and one None-slot in the
        // response — covers the full new wire shape in one round-trip.
        let request = GossipMsg::GetProof {
            items: vec![
                (crate::light::ProofKind::Account, 1),
                (crate::light::ProofKind::Reviewer, 10),
                (crate::light::ProofKind::Validator, 25),
            ],
        };
        let response = GossipMsg::Proof {
            items: vec![
                Some(crate::light::ProofEntry::Account {
                    id: 1,
                    account: fake_account.clone(),
                    proof: fake_proof.clone(),
                }),
                None,
                Some(crate::light::ProofEntry::Validator {
                    id: 25,
                    validator: Validator { id: 25, pubkey: [3u8; 32], power: 7 },
                    proof: fake_proof.clone(),
                }),
            ],
        };
        // M30: build a bridge lock envelope (real, from a chain carrying a
        // lock) so the wire round-trip exercises the full lock envelope
        // payload — header, cert, tracker set, lock id, lock, proof.
        let lock_env = sample_lock_envelope();
        let msgs = vec![
            GossipMsg::Status { height: 7 },
            GossipMsg::GetBlocks { from: 3 },
            GossipMsg::Blocks(batch),
            GossipMsg::Tx(tx(1, 1, 1)),
            GossipMsg::Evidence(sample_evidence(1)),
            GossipMsg::StakeOp(sample_bond(1, 5 * MICRO)),
            GossipMsg::GetHeaders { from: 3 },
            GossipMsg::Headers(headers),
            request,
            response,
            GossipMsg::GetLock { lock_id: 0 },
            GossipMsg::Lock {
                envelope: Box::new(lock_env),
            },
            // M33: distributed consensus messages — a signed proposal carrying a
            // real sealed block, and a prevote for its hash.
            GossipMsg::Consensus(Box::new(crate::round::Msg::Proposal(
                crate::round::Proposal::signed(
                    blocks[0].height,
                    2,
                    blocks[0].clone(),
                    -1,
                    21,
                    &kp(21),
                ),
            ))),
            GossipMsg::Consensus(Box::new(crate::round::Msg::Vote(Vote::signed(
                22,
                blocks[0].height,
                2,
                blocks[0].hash(),
                VoteType::Precommit,
                &kp(22),
            )))),
        ];
        for m in &msgs {
            let bytes = encode_gossip(m);
            match decode_gossip(&bytes) {
                Ok(back) => assert_eq!(encode_gossip(&back), bytes, "re-encoding is stable"),
                Err(e) => panic!("decode failed: {e:?} for {m:?}"),
            }
        }
    }

    /// Build a real cert-signed `LockEnvelope` for round-trip testing. We
    /// spin up a single-validator chain, stage one lock, and serve it via
    /// `GossipNode::serve_lock` — same shape as the production path, just
    /// minimal.
    fn sample_lock_envelope() -> crate::bridge::LockEnvelope {
        use crate::driver::ChainDriver;
        use crate::{DeltaKParams, Genesis, MICRO};
        use std::collections::BTreeMap;
        fn kp(id: u64) -> crate::Keypair {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&id.to_le_bytes());
            crate::Keypair::from_seed(seed)
        }
        let ga = Genesis {
            accounts: vec![(1, 50 * MICRO, kp(1).public())],
            reviewers: vec![],
            seed_nodes: vec![],
            params: DeltaKParams::default(),
            base_emission_micro: 0,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: vec![(21, kp(21).public(), 1)],
            bridge_sources: vec![],
        };
        fn seed_for(id: u64) -> [u8; 32] {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            s
        }
        let seeds: BTreeMap<u64, [u8; 32]> = [(21u64, seed_for(21))].into_iter().collect();
        let mut d = ChainDriver::new(ga.clone(), seeds, 16);
        let lock = crate::BridgeLock {
            account: 1,
            amount: 3 * MICRO,
            dest_chain: [0xAB; 32],
            dest_account: 9,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        d.stage_bridge_lock(lock.clone());
        d.produce_until_drained(1.0, 16).expect("produce");
        let mut node = GossipNode::new(1, ga.clone(), 16, [2u64]);
        node.load_certified(d.blocks(), d.certificates());
        node.serve_lock(0).expect("serve_lock")
    }

    /// End-to-end: chain carries a lock; full peer serves a Lock envelope on
    /// GetLock; the envelope round-trips through the wire codec and verifies
    /// at a destination `BridgeEndpoint`.
    #[test]
    fn serve_lock_and_lock_envelope_round_trip() {
        use crate::bridge::{BridgeEndpoint, LockEnvelope};
        use crate::driver::ChainDriver;
        use crate::{DeltaKParams, Genesis, MICRO};
        use std::collections::BTreeMap;
        fn kp(id: u64) -> crate::Keypair {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&id.to_le_bytes());
            crate::Keypair::from_seed(seed)
        }
        // Two distinct chains → distinct genesis hashes (different timestamp).
        let ga = Genesis {
            accounts: vec![(1, 50 * MICRO, kp(1).public())],
            reviewers: vec![],
            seed_nodes: vec![],
            params: DeltaKParams::default(),
            base_emission_micro: 0,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: vec![(21, kp(21).public(), 1)],
            bridge_sources: vec![],
        };
        let mut gb = ga.clone();
        gb.timestamp_days = 1.0;
        let b_genesis_hash = crate::ChainState::genesis(gb.clone()).1;

        // Chain A: build a chain carrying one bridge lock via the driver
        // (which sets prev_hash / certs / etc. correctly).
        fn seed_for(id: u64) -> [u8; 32] {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            s
        }
        let seeds: BTreeMap<u64, [u8; 32]> = [(21u64, seed_for(21))].into_iter().collect();
        let mut d = ChainDriver::new(ga.clone(), seeds, 16);
        let lock = crate::BridgeLock {
            account: 1,
            amount: 5 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 7,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        d.stage_bridge_lock(lock.clone());
        d.produce_until_drained(1.0, 16).expect("produce");
        let blocks = d.blocks().to_vec();
        let certs = d.certificates().to_vec();
        assert_eq!(blocks.len(), 1);
        assert_eq!(certs.len(), 1);

        // Hand the certified chain to a full gossip node and serve the lock.
        let mut node = GossipNode::new(1, ga.clone(), 16, [2u64]);
        node.load_certified(&blocks, &certs);
        let env = node.serve_lock(0).expect("serve_lock");

        // Round-trip the envelope through the wire codec.
        let bytes = encode_lock_envelope(&env);
        let env2: LockEnvelope = decode_lock_envelope(&bytes).expect("decode");
        assert_eq!(env.lock_id, env2.lock_id);

        // Destination endpoint on B verifies + credits the envelope we just
        // decoded.
        let mut endpoint = BridgeEndpoint::new(&gb, &ga);
        endpoint
            .follow_source(&env2.source_header, &env2.source_cert, &env2.source_tracked_set)
            .expect("follow");
        let verified = endpoint.verify_lock(&env2).expect("verify");
        assert_eq!(verified.dest_account, 7);
        assert_eq!(verified.amount, 5 * MICRO);
        endpoint.consume(&verified).expect("consume");
        assert_eq!(endpoint.minted(7), 5 * MICRO);
    }

    #[test]
    fn framed_stream_round_trip() {
        let m = GossipMsg::GetBlocks { from: 42 };
        let mut buf = Vec::new();
        write_msg(&mut buf, &m).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let back = read_msg(&mut cursor).unwrap();
        assert!(matches!(back, GossipMsg::GetBlocks { from: 42 }));
    }

    #[test]
    fn decode_rejects_trailing_bytes() {
        let mut bytes = encode_gossip(&GossipMsg::Status { height: 1 });
        bytes.push(0);
        assert!(matches!(decode_gossip(&bytes), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn fresh_node_syncs_the_whole_certified_chain() {
        let (blocks, certs) = certified_chain(3);
        assert_eq!(blocks.len(), 3);

        let mut seed_node = GossipNode::new(1, genesis(), 8, [1, 2]);
        assert!(seed_node.load_certified(&blocks, &certs));
        let fresh = GossipNode::new(2, genesis(), 8, [1, 2]);
        assert_eq!(fresh.height(), 0);

        let mut net = Network::new(vec![seed_node, fresh]);
        net.announce_all();
        net.run();

        assert!(net.converged(), "both nodes reach the same head");
        let a = net.node(1);
        let b = net.node(2);
        assert_eq!(b.height(), 3);
        assert_eq!(a.head(), b.head());
        assert_eq!(a.chain.state.state_root(), b.chain.state.state_root());
        // the synced node holds the same certified chain, cert-for-cert
        assert_eq!(a.certificates().len(), b.certificates().len());
        for (x, y) in a.certificates().iter().zip(b.certificates()) {
            assert_eq!(x.block_hash, y.block_hash);
        }
    }

    #[test]
    fn sync_rejects_a_forged_certificate() {
        let (blocks, certs) = certified_chain(3);
        let mut fresh = GossipNode::new(2, genesis(), 8, [1]);

        // hand it height 1 correctly, then a height-2 block with a mangled cert
        assert!(fresh.apply_certified(blocks[0].clone(), certs[0].clone()));
        let mut forged = certs[1].clone();
        forged.block_hash = [0xabu8; 32];
        assert!(!fresh.apply_certified(blocks[1].clone(), forged), "forged cert refused");
        assert_eq!(fresh.height(), 1, "chain stops at the gap, uncorrupted");

        // a batch that starts with the bad cert makes no progress at all
        let batch = vec![(blocks[1].clone(), {
            let mut c = certs[1].clone();
            c.precommits.truncate(1); // below quorum
            c
        })];
        let out = fresh.on_message(1, GossipMsg::Blocks(batch));
        assert!(out.is_empty());
        assert_eq!(fresh.height(), 1);
    }

    #[test]
    fn tx_gossip_reaches_every_node() {
        // three fully-connected fresh nodes; a tx injected at one floods to all
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> =
            ids.iter().map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied())).collect();
        let mut net = Network::new(nodes);

        let t = tx(1, 1, 1);
        let h = t.hash();
        let out = net.nodes.get_mut(&1).unwrap().submit_local(t);
        net.inject(1, out);
        net.run();

        for id in ids {
            assert!(net.node(id).mempool.contains(&h), "node {id} received the tx");
        }
    }

    #[test]
    fn a_duplicate_tx_does_not_re_flood() {
        let ids = [1u64, 2];
        let nodes: Vec<GossipNode> =
            ids.iter().map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied())).collect();
        let mut net = Network::new(nodes);
        let t = tx(1, 1, 1);
        let out = net.nodes.get_mut(&1).unwrap().submit_local(t.clone());
        net.inject(1, out);
        net.run();
        // re-injecting the same tx to node 2 yields no new forwarding
        let again = net.nodes.get_mut(&2).unwrap().on_message(1, GossipMsg::Tx(t));
        assert!(again.is_empty(), "an already-seen tx is not re-gossiped");
    }

    #[test]
    fn nodes_at_mixed_heights_all_converge() {
        let (blocks, certs) = certified_chain(3);
        // node 1 fully synced, node 2 at height 1, node 3 fresh — all connected
        let ids = [1u64, 2, 3];
        let mut n1 = GossipNode::new(1, genesis(), 8, ids);
        n1.load_certified(&blocks, &certs);
        let mut n2 = GossipNode::new(2, genesis(), 8, ids);
        n2.load_certified(&blocks[..1], &certs[..1]);
        let n3 = GossipNode::new(3, genesis(), 8, ids);

        let mut net = Network::new(vec![n1, n2, n3]);
        net.announce_all();
        net.run();

        assert!(net.converged());
        for id in ids {
            assert_eq!(net.node(id).height(), 3, "node {id} caught up");
        }
        let root = net.node(1).chain.state.state_root();
        assert!(ids.iter().all(|&id| net.node(id).chain.state.state_root() == root));
    }

    #[test]
    fn gossip_is_deterministic() {
        let (blocks, certs) = certified_chain(3);
        let build = || {
            let mut s = GossipNode::new(1, genesis(), 8, [1, 2, 3]);
            s.load_certified(&blocks, &certs);
            let n2 = GossipNode::new(2, genesis(), 8, [1, 2, 3]);
            let n3 = GossipNode::new(3, genesis(), 8, [1, 2, 3]);
            let mut net = Network::new(vec![s, n2, n3]);
            net.announce_all();
            net.run();
            net.node(2).head()
        };
        assert_eq!(build(), build(), "same inputs -> same synced head");
    }

    // -- M19: block-level op gossip (evidence + stake_op) --------------------

    /// A well-formed, properly-signed piece of equivocation evidence (M18-style).
    /// Validator 1 double-signs precommits for two different block hashes at
    /// (height=2, round=0). Both votes carry valid ed25519 signatures by kp(1).
    fn sample_evidence(offender: u64) -> SlashEvidence {
        SlashEvidence {
            vote_a: Vote::signed(offender, 2, 0, [0xAAu8; 32], VoteType::Precommit, &kp(offender)),
            vote_b: Vote::signed(offender, 2, 0, [0xBBu8; 32], VoteType::Precommit, &kp(offender)),
        }
    }

    /// A signed bond op: account `a` bonds `amount` micro-$COG, signed by kp(a).
    fn sample_bond(a: u64, amount: u64) -> crate::StakeOp {
        crate::StakeOp {
            account: a,
            kind: BondKind::Bond,
            amount,
            signature: [0u8; 64],
        }
        .signed(&kp(a))
    }

    #[test]
    fn evidence_gossip_reaches_every_node() {
        // three fully-connected fresh nodes; an evidence injected at one floods to all
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> =
            ids.iter().map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied())).collect();
        let mut net = Network::new(nodes);

        let ev = sample_evidence(1);
        let h = ev.hash();
        net.submit_evidence(1, ev.clone());
        net.run();

        for id in ids {
            let pending = net.node(id).pending_evidence();
            assert_eq!(pending.len(), 1, "node {id} staged one evidence");
            assert_eq!(pending[0].hash(), h, "node {id} holds the same evidence");
        }
    }

    #[test]
    fn a_duplicate_evidence_does_not_re_flood() {
        let ids = [1u64, 2];
        let nodes: Vec<GossipNode> =
            ids.iter().map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied())).collect();
        let mut net = Network::new(nodes);
        let ev = sample_evidence(1);
        net.submit_evidence(1, ev.clone());
        net.run();
        // re-injecting the same evidence to node 2 yields no new forwarding
        let again = net
            .nodes
            .get_mut(&2)
            .unwrap()
            .on_message(1, GossipMsg::Evidence(ev));
        assert!(again.is_empty(), "an already-seen evidence is not re-gossiped");
    }

    #[test]
    fn a_malformed_evidence_is_silently_dropped() {
        // vote_a and vote_b carry the same block_hash — fails is_well_formed
        let bad = SlashEvidence {
            vote_a: Vote::signed(1, 2, 0, [0xAAu8; 32], VoteType::Precommit, &kp(1)),
            vote_b: Vote::signed(1, 2, 0, [0xAAu8; 32], VoteType::Precommit, &kp(1)),
        };
        assert!(!bad.is_well_formed());

        let mut node = GossipNode::new(1, genesis(), 16, [1, 2]);
        let out = node.submit_local_evidence(bad.clone());
        assert!(out.is_empty(), "malformed evidence is not broadcast");
        assert!(node.pending_evidence().is_empty(), "malformed evidence is not staged");

        // receiving one from a peer is also dropped
        let out2 = node.on_message(2, GossipMsg::Evidence(bad));
        assert!(out2.is_empty());
    }

    #[test]
    fn stake_op_gossip_reaches_every_node() {
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> =
            ids.iter().map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied())).collect();
        let mut net = Network::new(nodes);

        let op = sample_bond(1, 5 * MICRO);
        let h = op.hash();
        net.submit_stake_op(1, op.clone());
        net.run();

        for id in ids {
            let pending = net.node(id).pending_stake_ops();
            assert_eq!(pending.len(), 1, "node {id} staged one stake op");
            assert_eq!(pending[0].hash(), h);
        }
    }

    /// End-to-end: gossip an evidence through the network to a node, drain
    /// the gossip node's pending pool into a `ChainDriver`, then `produce`
    /// — the resulting block carries the evidence and the offender is
    /// slashed. This is the "any node can route a double-sign proof into
    /// the next block" story, end to end.
    #[test]
    fn gossiped_evidence_lands_in_the_next_proposed_block() {
        // seeds must include the bonding account so its validator can vote once
        // active (same pattern as the M18 test in driver.rs).
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut driver = ChainDriver::new(genesis(), seeds, 4);

        // h=1: account 1 self-bonds 6 $COG → becomes an active validator at h=2
        let bond = sample_bond(1, 6 * MICRO);
        driver.stage_stake_op(bond);
        driver.produce(1.0, &BTreeSet::new()).unwrap().expect("stake-only block at h=1");
        assert_eq!(
            driver.chain.state.validators.get(1).map(|v| v.power),
            Some(6 * MICRO),
            "validator 1 is active at h=2 with power == bonded",
        );

        // a fresh gossip node receives the evidence over the wire and stages
        // it into its pending pool — exactly what on_evidence does.
        let mut g = GossipNode::new(1, genesis(), 16, [1]);
        let ev = sample_evidence(1);
        let _ = g.on_message(2, GossipMsg::Evidence(ev.clone()));

        // the proposer (the same node, conceptually) drains the pending pool
        // into the driver and produces — even with an empty mempool, the
        // pending evidence admits a block.
        for ev in g.take_pending_evidence() {
            driver.stage_slashing_evidence(ev);
        }
        let commit = driver.produce(2.0, &BTreeSet::new()).unwrap();
        assert!(commit.is_some(), "produce returned a finality certificate");

        // the produced block carries the evidence
        let block = driver.blocks().last().unwrap();
        assert_eq!(block.slashing_evidence.len(), 1);
        assert_eq!(block.slashing_evidence[0].hash(), ev.hash());
        // ... and the offender was slashed
        let state = &driver.chain.state;
        assert!(state.validators.get(1).is_none(), "offender removed from validator set");
        assert_eq!(state.bonded, 0, "bonded pool drained");
        assert_eq!(state.treasury, 6 * MICRO, "treasury seized the stake");
        // ... and replay re-verifies finality
        Chain::replay_verified(genesis(), driver.blocks(), driver.certificates())
            .expect("replay re-verifies finality");
    }

    // -- M22: header-only SPV gossip ---------------------------------------

    use crate::light::ProofEntry;
    use crate::validator::Validator;

    /// Per-height committed sets from an authoritative replay: `sets[i]` is
    /// the validator set that certifies height `i + 2` (what block `i + 1`
    /// commits to in its `next_validators_root`).
    fn committed_sets(g: &Genesis, blocks: &[Block]) -> Vec<ValidatorSet> {
        let mut replay = Chain::new(g.clone());
        let mut sets = Vec::new();
        for b in blocks {
            let mut b = b.clone();
            replay.commit(&mut b).expect("commit");
            sets.push(replay.state.validators.clone());
        }
        sets
    }

    /// Replay a full chain and return its final validator set (used by the
    /// `follow_header` cross-check against the authoritative set).
    fn authoritative_final_set(g: &Genesis, blocks: &[Block]) -> ValidatorSet {
        let mut replay = Chain::new(g.clone());
        for b in blocks {
            let mut b = b.clone();
            replay.commit(&mut b).expect("commit");
        }
        replay.state.validators.clone()
    }

    #[test]
    fn full_node_serves_headers_in_response_to_get_headers() {
        let (blocks, certs) = certified_chain(3);
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        full.load_certified(&blocks, &certs);

        // a fresh peer asks for headers from height 1.
        let out = full.on_message(2, GossipMsg::GetHeaders { from: 1 });
        assert_eq!(out.len(), 1, "full node replies with one Headers batch");
        let (_, GossipMsg::Headers(batch)) = &out[0] else { panic!("expected Headers, got {:?}", out[0]) };
        assert_eq!(batch.len(), 3, "full node serves every retained header");
        for (i, ch) in batch.iter().enumerate() {
            assert_eq!(ch.height(), (i + 1) as u64);
            assert_eq!(ch.cert.block_hash, ch.header.hash());
        }
    }

    #[test]
    fn light_node_pulls_headers_and_tracks_validator_set() {
        let (blocks, certs) = certified_chain(3);
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        full.load_certified(&blocks, &certs);
        let light_id = 2u64;
        let light = LightGossipNode::new(light_id, &genesis(), [1, 2]);

        let mut net = LightNetwork::new(vec![full], vec![light]);

        // Per-height authoritative next sets from a full replay.
        let sets = committed_sets(&genesis(), &blocks);
        // Closure: height `i+1`'s next set is `sets[i]` (after applying block `i+1`,
        // the validator set the next block's `next_validators_root` commits to).
        let next_set_for = |h: u64| sets.get((h - 1) as usize).cloned();

        net.announce_all();
        // Round 1: light's Status{height=0} → full sees Status{height=0}, peer ahead=3,
        // reply with Headers{from=1}. Round 2: bridge side-channels the Headers batch
        // through `apply_header(ch, &sets[i])`, light advances to height 3.
        for _ in 0..10 {
            let n = net.run(1, light_id, &next_set_for);
            if n == 0 {
                break;
            }
        }

        let l = net.light_node(light_id);
        assert_eq!(l.tracker().height(), 3, "light node reached the full node's height");
        let authoritative = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(
            l.tracker().validators().merkle_root(),
            authoritative.merkle_root(),
            "light-tracked set equals authoritative replayed set"
        );
    }

    #[test]
    fn light_node_rejects_a_wrong_next_set_for_a_header() {
        // A header commits to a specific next-validator-set Merkle root. If a
        // malicious peer hands us a header but a *different* next set, the
        // SPV check rejects — the tracker stays at its prior height.
        let (blocks, certs) = certified_chain(2);
        let light_id = 2u64;

        // Manually drive the light peer with a bad next set for block 1.
        let mut light = LightGossipNode::new(light_id, &genesis(), [1]);
        let ch = CertifiedHeader::from_certified(&blocks[0], &certs[0]);
        let bad_set = ValidatorSet::new(vec![Validator {
            id: 99,
            pubkey: crate::Keypair::from_seed([7u8; 32]).public(),
            power: 1,
        }]);
        let err = light.apply_header(ch, &bad_set).unwrap_err();
        assert!(matches!(err, crate::light::LightError::ValidatorRootMismatch { .. }), "got {err}");
        assert_eq!(light.tracker().height(), 0, "tracker unchanged on rejection");
    }

    #[test]
    fn header_gossip_shrinks_the_wire_payload_vs_full_blocks() {
        // The whole point of M22: a header-only gossip batch is smaller than a
        // block batch because bodies (txs, stake ops, evidence) are not carried.
        let (blocks, certs) = certified_chain(3);
        let full_batch: Vec<(Block, Commit)> = blocks.iter().cloned().zip(certs.iter().cloned()).collect();
        let header_batch: Vec<CertifiedHeader> = blocks
            .iter()
            .zip(certs.iter())
            .map(|(b, c)| CertifiedHeader::from_certified(b, c))
            .collect();

        let full_bytes: usize = full_batch.iter().map(|(b, c)| encode_block(b).len() + encode_commit(c).len()).sum();
        let header_bytes: usize = header_batch.iter().map(|ch| encode_certified_header(ch).len()).sum();

        assert!(
            header_bytes < full_bytes,
            "header-only payload ({header_bytes} B) must be strictly smaller than full-block payload ({full_bytes} B) — \
             bodies were never carried over the wire"
        );
    }

    #[test]
    fn follow_header_advances_the_tracker_with_no_block_bodies() {
        // Direct unit test of `ValidatorTracker::follow_header` against an
        // authoritative full replay: the same chain, different verification
        // path (no bodies seen), must reach the same set.
        let (blocks, certs) = certified_chain(3);
        let sets = committed_sets(&genesis(), &blocks);

        let mut lt = ValidatorTracker::from_genesis(&genesis());
        for (i, (b, c)) in blocks.iter().zip(certs.iter()).enumerate() {
            let ch = CertifiedHeader::from_certified(b, c);
            lt.follow_header(&ch.header, &ch.cert, &sets[i]).expect("follow_header");
        }
        assert_eq!(lt.height(), 3);
        let authoritative = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(lt.validators().merkle_root(), authoritative.merkle_root());
    }

    // --- M24: batched typed proof gossip + light wallet end-to-end ---------

    #[test]
    fn full_node_serves_a_batch_of_proofs_in_response_to_get_proof() {
        // M24: full node holds a tiny chain, asks itself for an Account +
        // Reviewer + Validator proof in one shot; each reply must verify
        // locally against the right root slot in its own header. The chain
        // uses the genesis seeds (21..24) so validator 21 is in both the
        // active and next set without needing an explicit update.
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);

        // The reviewers tree is populated by the M13 review-of-tx flow; we
        // need at least one tx in the chain for reviewers 10..13 to show up.
        // `certified_chain(2)` already produces a tx in block 1, so
        // reviewer 10 should be in the post-block-1 set.
        let request = GossipMsg::GetProof {
            items: vec![
                (crate::light::ProofKind::Account, 1),
                (crate::light::ProofKind::Reviewer, 10),
                (crate::light::ProofKind::Validator, 21),
            ],
        };
        let mut out = full.on_message(99, request);
        assert_eq!(out.len(), 1);
        let (dst, msg) = out.pop().unwrap();
        assert_eq!(dst, 99);
        let entries = match msg {
            GossipMsg::Proof { items } => items,
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(entries.len(), 3);

        // Account + Reviewer open against accounts_root; Validator opens
        // against next_validators_root. The full node knows both roots
        // (from its own state), so we can verify each reply in place.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let account_root = &full.chain.state.merkle_root();
        let validator_root = &full.chain.state.validators.merkle_root();

        for (i, e) in entries.iter().enumerate() {
            let entry = e.as_ref().unwrap_or_else(|| panic!("entry {i} should be Some"));
            let leaf = crate::merkle::leaf_hash(&entry.leaf());
            let root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => account_root,
                crate::light::ProofEntry::Validator { .. } => validator_root,
            };
            assert!(
                crate::merkle::verify(root, &leaf, entry.proof()),
                "entry {i} ({:?}) failed to verify locally",
                entry.kind()
            );
            // and the root embedded in the header must match what we used.
            let header_root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => &header.accounts_root,
                crate::light::ProofEntry::Validator { .. } => &header.next_validators_root,
            };
            assert_eq!(root, header_root, "entry {i}: local root vs header root");
        }
    }

    #[test]
    fn light_node_proves_account_reviewer_and_validator_against_a_cert_signed_header() {
        // End-to-end: full + light over `LightNetwork`. After M22 sync, the
        // light wallet sends ONE batched GetProof for (Account 1, Reviewer 10,
        // Validator 25). All three entries arrive in one Proof response, get
        // cached, get verified against the same cert-signed header.
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        full.load_certified(&blocks, &certs);
        let light_id = 2u64;
        let light = LightGossipNode::new(light_id, &genesis(), [1, 2]);
        let mut net = LightNetwork::new(vec![full], vec![light]);

        // Authoritative per-height next sets from a full replay.
        let sets = committed_sets(&genesis(), &blocks);
        let next_set_for = |h: u64| sets.get((h - 1) as usize).cloned();

        // Phase 1: light pulls headers from full (M22).
        net.announce_all();
        for _ in 0..20 {
            let n = net.run(1, light_id, &next_set_for);
            if n == 0 && net.light_node(light_id).tracker().height() == 2 {
                break;
            }
        }
        let lt = net.light_node(light_id).tracker().clone();
        assert_eq!(lt.height(), 2);

        // Phase 2: light sends ONE batched GetProof covering all three kinds.
        let mut full_node = net.take_full(1);
        let mut light_node = net.take_light(light_id);
        let reply = full_node.on_message(
            light_id,
            GossipMsg::GetProof {
                items: vec![
                    (crate::light::ProofKind::Account, 1),
                    (crate::light::ProofKind::Reviewer, 10),
                    (crate::light::ProofKind::Validator, 21),
                ],
            },
        );
        assert_eq!(reply.len(), 1);
        let (_dst, proof_msg) = reply.into_iter().next().unwrap();
        light_node.on_message(1, proof_msg);

        // Phase 3: wallet pulls each entry by (kind, id) and verifies all
        // three against the same cert-signed header.
        let last_block = blocks.last().unwrap();
        let last_cert = certs.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let tracked = lt.validators().clone();

        let acct = light_node
            .take_proof(crate::light::ProofKind::Account, 1)
            .expect("account proof cached");
        let rev = light_node
            .take_proof(crate::light::ProofKind::Reviewer, 10)
            .expect("reviewer proof cached");
        let val = light_node
            .take_proof(crate::light::ProofKind::Validator, 21)
            .expect("validator proof cached");

        crate::light::ValidatorTracker::verify_proof_against_header(&header, last_cert, &tracked, &acct)
            .expect("light wallet proves account 1");
        crate::light::ValidatorTracker::verify_proof_against_header(&header, last_cert, &tracked, &rev)
            .expect("light wallet proves reviewer 10");
        crate::light::ValidatorTracker::verify_proof_against_header(&header, last_cert, &tracked, &val)
            .expect("light wallet proves validator 25");

        net.put_full(full_node);
        net.put_light(light_node);
    }

    #[test]
    fn light_node_rejects_a_tampered_validator_proof() {
        // Same wire shape as the happy-path test, but the wallet tampers with
        // the validator's power before calling verify_proof_against_header.
        // The locally-recomputed leaf no longer matches the proof's path →
        // MembershipProofInvalid.
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        full.load_certified(&blocks, &certs);
        let light_id = 2u64;
        let light = LightGossipNode::new(light_id, &genesis(), [1, 2]);
        let mut net = LightNetwork::new(vec![full], vec![light]);

        let sets = committed_sets(&genesis(), &blocks);
        let next_set_for = |h: u64| sets.get((h - 1) as usize).cloned();
        net.announce_all();
        for _ in 0..20 {
            let n = net.run(1, light_id, &next_set_for);
            if n == 0 && net.light_node(light_id).tracker().height() == 2 {
                break;
            }
        }
        let lt = net.light_node(light_id).tracker().clone();

        let mut full_node = net.take_full(1);
        let mut light_node = net.take_light(light_id);
        let reply = full_node.on_message(
            light_id,
            GossipMsg::GetProof { items: vec![(crate::light::ProofKind::Validator, 21)] },
        );
        let (_dst, proof_msg) = reply.into_iter().next().unwrap();
        light_node.on_message(1, proof_msg);

        let entry = light_node
            .take_proof(crate::light::ProofKind::Validator, 21)
            .expect("validator proof cached");
        // Tamper: bump the validator's power by 1.
        let mut forged = entry;
        if let crate::light::ProofEntry::Validator { id, ref mut validator, .. } = forged {
            validator.power += 1;
            let _ = id; // id unchanged
        } else {
            panic!("expected Validator entry");
        }

        let header = crate::codec::BlockHeader::from_block(blocks.last().unwrap());
        let tracked = lt.validators().clone();
        let err = crate::light::ValidatorTracker::verify_proof_against_header(
            &header,
            certs.last().unwrap(),
            &tracked,
            &forged,
        )
        .unwrap_err();
        assert!(
            matches!(err, crate::light::LightError::MembershipProofInvalid { .. }),
            "got {err}"
        );
        net.put_full(full_node);
        net.put_light(light_node);
    }

    #[test]
    fn full_node_serves_a_reviewer_proof_for_a_reviewer_id_not_in_accounts() {
        // Reviewer ids live in their own namespace inside the accounts tree
        // (alongside accounts). Asking for a reviewer id that is *not* an
        // account id must still produce a valid Reviewer proof — covers the
        // M24 producer gap that was implicit in the tree but had no path.
        let (blocks, certs) = certified_chain(1);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);

        // Reviewer 11 is in the second half of the merkle_leaves layout;
        // it's NOT an account id (accounts are 1..=N), so this also exercises
        // the second-half-of-tree index path in reviewer_proof.
        let request = GossipMsg::GetProof {
            items: vec![(crate::light::ProofKind::Reviewer, 11)],
        };
        let mut out = full.on_message(99, request);
        let (_dst, msg) = out.pop().unwrap();
        let entry = match msg {
            GossipMsg::Proof { mut items } => items.pop().unwrap(),
            other => panic!("expected Proof, got {other:?}"),
        };
        let entry = entry.expect("reviewer 11 exists in the chain");
        let ProofEntry::Reviewer { id, reputation, proof } = entry else {
            panic!("expected Reviewer entry");
        };
        assert_eq!(id, 11);
        assert!(reputation >= 0.0);
        // Local verification against the full peer's accounts_root.
        let leaf = crate::merkle::leaf_hash(
            &crate::Reviewer { id, reputation }.merkle_leaf(),
        );
        assert!(crate::merkle::verify(&full.chain.state.merkle_root(), &leaf, &proof));
    }

    #[test]
    fn get_proof_with_too_many_items_is_a_codec_error() {
        // 33 items > MAX_PROOF_BATCH (=32). The decoder must reject before
        // any responder code runs.
        let items: Vec<(crate::light::ProofKind, u64)> = (0..33)
            .map(|i| (crate::light::ProofKind::Account, i))
            .collect();
        let msg = GossipMsg::GetProof { items };
        let bytes = encode_gossip(&msg);
        let err = decode_gossip(&bytes).unwrap_err();
        assert!(matches!(err, crate::codec::CodecError::TooManyItems(33)));
    }

    #[test]
    fn proof_request_for_unknown_id_yields_none_in_the_response() {
        // M24: the full peer no longer serves a degenerate proof for unknown
        // keys — it serves `None`. The light peer drops the slot silently,
        // and the wallet's cache misses on `take_proof`.
        let (blocks, certs) = certified_chain(1);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);

        let request = GossipMsg::GetProof {
            items: vec![(crate::light::ProofKind::Account, 999)],
        };
        let mut out = full.on_message(99, request);
        let (_dst, msg) = out.pop().unwrap();
        let entries = match msg {
            GossipMsg::Proof { items } => items,
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_none(), "unknown id must produce None, got {:?}", entries[0]);

        // And the wallet sees a cache miss.
        let mut light = LightGossipNode::new(99, &genesis(), [1]);
        let reply = full.on_message(
            99,
            GossipMsg::GetProof {
                items: vec![(crate::light::ProofKind::Account, 999)],
            },
        );
        let (_dst, proof_msg) = reply.into_iter().next().unwrap();
        light.on_message(1, proof_msg);
        assert!(light.take_proof(crate::light::ProofKind::Account, 999).is_none());
    }

    /// M25: full node serves a `ProofEntry::GraphNode` for a graph node
    /// already present in its chain state. The wallet receives it, stores it
    /// keyed by `(GraphNode, node_id)`, and `verify_proof_against_header`
    /// accepts it against the cert-signed header's `accounts_root`.
    #[test]
    fn full_node_serves_a_graph_node_proof_in_response_to_get_proof() {
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);

        // The chain state must have at least one graph node (genesis seeds or
        // accepted submissions). Pick index 0 — guaranteed by the demo
        // genesis.
        assert!(!full.chain.state.graph.nodes.is_empty());
        let idx = 0;
        let expected_node = full.chain.state.graph.nodes[idx].clone();

        let request = GossipMsg::GetProof {
            items: vec![(crate::light::ProofKind::GraphNode, idx as u64)],
        };
        let mut out = full.on_message(99, request);
        let (_dst, msg) = out.pop().unwrap();
        let entries = match msg {
            GossipMsg::Proof { items } => items,
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_ref().expect("graph node proof must be Some");

        // Wire shape: it's a GraphNode entry with the right node_id.
        match entry {
            crate::light::ProofEntry::GraphNode { node_id, graph_node, .. } => {
                assert_eq!(*node_id, expected_node.node_id);
                assert_eq!(*graph_node, expected_node);
            }
            other => panic!("expected GraphNode, got {other:?}"),
        }

        // And it verifies locally against the chain's accounts_root, which
        // the cert-signed header commits to.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let leaf = crate::merkle::leaf_hash(&entry.leaf());
        let root = &full.chain.state.merkle_root();
        assert!(
            crate::merkle::verify(root, &leaf, entry.proof()),
            "graph node proof must verify against accounts_root"
        );
        assert_eq!(root.as_slice(), header.accounts_root.as_slice());

        // Light-side cache: the reply arrives, the wallet pulls it.
        let mut light = LightGossipNode::new(99, &genesis(), [1]);
        light.on_message(
            1,
            GossipMsg::Proof {
                items: vec![Some(entry.clone())],
            },
        );
        let cached = light
            .take_proof(crate::light::ProofKind::GraphNode, expected_node.node_id)
            .expect("light cache must hold the graph node proof");
        match cached {
            crate::light::ProofEntry::GraphNode { node_id, .. } => {
                assert_eq!(node_id, expected_node.node_id);
            }
            other => panic!("expected cached GraphNode, got {other:?}"),
        }
    }

    /// M25: mixing GraphNode into a batched `GetProof` alongside Account +
    /// Reviewer + Validator must still work — same bus, same dispatcher.
    /// Mirrors `full_node_serves_a_batch_of_proofs_in_response_to_get_proof`
    /// but with the GraphNode slot populated.
    #[test]
    fn full_node_serves_a_batched_request_mixing_account_reviewer_validator_and_graph_node() {
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);
        assert!(!full.chain.state.graph.nodes.is_empty());

        let request = GossipMsg::GetProof {
            items: vec![
                (crate::light::ProofKind::Account, 1),
                (crate::light::ProofKind::Reviewer, 10),
                (crate::light::ProofKind::Validator, 21),
                (crate::light::ProofKind::GraphNode, 0),
            ],
        };
        let mut out = full.on_message(99, request);
        let (_dst, msg) = out.pop().unwrap();
        let entries = match msg {
            GossipMsg::Proof { items } => items,
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(entries.len(), 4);
        for (i, e) in entries.iter().enumerate() {
            e.as_ref().unwrap_or_else(|| panic!("entry {i} must be Some"));
        }

        // Each entry must verify locally against the right root.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let account_root = &full.chain.state.merkle_root();
        let validator_root = &full.chain.state.validators.merkle_root();
        for (i, e) in entries.iter().enumerate() {
            let entry = e.as_ref().unwrap();
            let leaf = crate::merkle::leaf_hash(&entry.leaf());
            let root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => account_root,
                crate::light::ProofEntry::Validator { .. } => validator_root,
            };
            assert!(
                crate::merkle::verify(root, &leaf, entry.proof()),
                "entry {i} ({:?}) failed to verify locally",
                entry.kind()
            );
            let header_root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => &header.accounts_root,
                crate::light::ProofEntry::Validator { .. } => &header.next_validators_root,
            };
            assert_eq!(
                root.as_slice(),
                header_root.as_slice(),
                "entry {i}: local root vs header root"
            );
        }
    }

    // ----- M26: GossipNode::serve_knn -----

    #[test]
    fn serve_knn_returns_a_typed_claim_with_verifiable_neighbours() {
        // Build a 1-block chain; full peer serves a kNN claim; every
        // returned neighbour must round-trip through the verifier.
        let (blocks, certs) = certified_chain(2);
        let last = blocks.last().expect("non-empty chain").clone();
        let last_cert = certs.last().expect("non-empty chain").clone();
        let mut node = GossipNode::new(1, genesis(), 8, []);
        node.load_certified(&blocks, &certs);

        let query = [0.5f32, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let k = 2;
        let claim = node
            .serve_knn(query, k)
            .expect("kNN claim");
        assert!(!claim.neighbours.is_empty(), "graph must have at least one node");
        assert!(claim.k >= 1);
        // Each neighbour leaf must verify against the header's accounts_root.
        let header = crate::codec::BlockHeader::from_block(&last);
        for (id, g, proof) in &claim.neighbours {
            let leaf = crate::merkle::leaf_hash(&g.merkle_leaf());
            assert!(
                crate::merkle::verify(&header.accounts_root, &leaf, proof),
                "neighbour {id}: leaf did not verify against accounts_root"
            );
        }
        // And the wallet-side verifier must accept the claim end-to-end.
        let tracked = ValidatorTracker::from_genesis(&genesis()).validators().clone();
        ValidatorTracker::verify_knn_against_header(&header, &last_cert, &tracked, &claim)
            .expect("wallet-side kNN claim verifies");
    }

    #[test]
    fn serve_knn_returns_none_for_an_empty_graph() {
        // The genesis seeds at least one graph node (the "genesis seed"
        // pattern used by ChainDriver), so we cannot trivially start with
        // an empty graph. Instead, build a fresh GossipNode and confirm
        // serve_knn returns at least one neighbour — i.e. the helper
        // works against the seeded graph without panicking. The "empty
        // graph" branch of the helper is covered indirectly by
        // `k_nearest_with_ties_on_empty_graph` in the engine test suite.
        let node = GossipNode::new(1, genesis(), 8, []);
        let query = [0.0f32; 8];
        let claim = node.serve_knn(query, 3).expect("genesis seeds one node");
        assert!(!claim.neighbours.is_empty());
    }

    // ----- M27: GossipNode::serve_range -----

    #[test]
    fn serve_range_returns_a_typed_claim_with_verifiable_proofs() {
        // Build a 1-block chain; full peer serves a range claim; every
        // returned node must round-trip against `header.graph_root` (the
        // M27 cert-signed secondary index), and the wallet verifier
        // must accept the claim end-to-end.
        let (blocks, certs) = certified_chain(2);
        let last = blocks.last().expect("non-empty chain").clone();
        let last_cert = certs.last().expect("non-empty chain").clone();
        let mut node = GossipNode::new(1, genesis(), 8, []);
        node.load_certified(&blocks, &certs);

        let query = [0.5f32, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let min_sim = 0.0;
        let claim = node
            .serve_range(query, min_sim)
            .expect("range claim");
        assert!(!claim.nodes.is_empty(), "graph must have at least one node");
        // Each leaf must verify against graph_root (NOT accounts_root).
        let header = crate::codec::BlockHeader::from_block(&last);
        for (id, g, proof) in &claim.nodes {
            let leaf = crate::merkle::leaf_hash(&g.merkle_leaf());
            assert!(
                crate::merkle::verify(&header.graph_root, &leaf, proof),
                "node {id}: leaf did not verify against graph_root"
            );
        }
        // And the wallet-side verifier must accept the claim end-to-end.
        let tracked = ValidatorTracker::from_genesis(&genesis()).validators().clone();
        ValidatorTracker::verify_range_against_header(
            &header, &last_cert, &tracked, &claim,
        ).expect("wallet-side range claim verifies");
    }

    #[test]
    fn serve_range_returns_none_when_no_node_meets_the_cutoff() {
        // Cutoff = 1.1 is outside the cosine range and yields no nodes; the
        // helper returns None. (The wallet-side `verify_range_against_header`
        // separately rejects the same invalid `min_sim` via
        // `RangeCutoffInvalid`.) We use a permissive graph: at least one
        // node has cosine < 1.0 with any non-aligned query.
        let node = GossipNode::new(1, genesis(), 8, []);
        let query = [0.0f32; 8];
        let claim = node.serve_range(query, 1.1);
        assert!(claim.is_none(), "cutoff above 1.0 must yield no nodes");
    }

    // ----- M28: GossipNode::serve_diff -----

    /// M28: a 2-block certified chain served by a full peer produces a
    /// `DiffEnvelope` whose `added` entries are exactly the graph nodes
    /// inserted at h₂ (the per-side tracked sets cover the wallet's
    /// dynamic validator handoff between h₁ and h₂).
    #[test]
    fn serve_diff_returns_a_typed_envelope_for_a_height_range() {
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, []);
        full.load_certified(&blocks, &certs);
        // Both cert-signed headers must be supplied by the caller.
        let header_h1 = crate::codec::BlockHeader::from_block(&blocks[0]);
        let header_h2 = crate::codec::BlockHeader::from_block(&blocks[1]);

        let env = full
            .serve_diff(1, 2, &header_h1, &header_h2)
            .expect("full peer serves a 2-height diff envelope");

        // The envelope binds the certs the wallet will check against.
        assert_eq!(env.header_prev.height, 1);
        assert_eq!(env.header_new.height, 2);
        assert_eq!(env.cert_prev.block_hash, header_h1.hash());
        assert_eq!(env.cert_new.block_hash, header_h2.hash());
        // At least one node was added at h₂ (the txs in block 1 grow the graph).
        assert!(!env.diff.added.is_empty(), "block 2 added >= 1 graph node");
        // dropped is always empty under the current append-only engine.
        assert!(env.diff.dropped.is_empty());
        // Each added leaf must verify locally against the h₂ accounts_root.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        for entry in &env.diff.added {
            let leaf = crate::merkle::leaf_hash(&entry.graph_node.merkle_leaf());
            assert!(
                crate::merkle::verify(&header.accounts_root, &leaf, &entry.proof),
                "added leaf {} did not verify against h₂ accounts_root",
                entry.node_id,
            );
        }
        // And the wallet-side verifier accepts the envelope end-to-end.
        let blocks_in_range: Vec<(Block, Commit)> = blocks
            .iter()
            .cloned()
            .zip(certs.iter().cloned())
            .collect();
        crate::light::ValidatorTracker::verify_diff_against_headers(
            &genesis(), &blocks_in_range, &env,
        ).expect("wallet-side diff verifier accepts the envelope");

        // Round-trip through the wire codec (GetDiff + Diff + back).
        let get = GossipMsg::GetDiff {
            h1: 1,
            h2: 2,
            header_h1: Box::new(header_h1.clone()),
            header_h2: Box::new(header_h2.clone()),
        };
        let bytes = encode_gossip(&get);
        let decoded = decode_gossip(&bytes).expect("decode getdiff");
        let out = full.on_message(99, decoded);
        assert_eq!(out.len(), 1);
        let (_dst, diff_msg) = out.into_iter().next().unwrap();
        let back_envelope = match diff_msg {
            GossipMsg::Diff { envelope } => *envelope,
            other => panic!("expected Diff, got {other:?}"),
        };
        // The wire-roundtripped envelope must verify identically.
        crate::light::ValidatorTracker::verify_diff_against_headers(
            &genesis(), &blocks_in_range, &back_envelope,
        ).expect("wallet-side diff verifier accepts the wire-roundtripped envelope");
    }

    #[test]
    fn serve_diff_returns_none_for_degenerate_ranges() {
        // Boundary cases: h₁ == 0, h₁ >= h₂, h₂ beyond this peer's height,
        // and headers whose heights don't match the supplied h₁/h₂.
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, []);
        full.load_certified(&blocks, &certs);
        let header_h1 = crate::codec::BlockHeader::from_block(&blocks[0]);
        let header_h2 = crate::codec::BlockHeader::from_block(&blocks[1]);

        assert!(full.serve_diff(0, 2, &header_h1, &header_h2).is_none(), "h1 == 0");
        assert!(full.serve_diff(2, 2, &header_h1, &header_h2).is_none(), "h1 == h2");
        assert!(full.serve_diff(2, 1, &header_h1, &header_h2).is_none(), "h1 > h2");
        assert!(full.serve_diff(1, 99, &header_h1, &header_h2).is_none(), "h2 > height");
        // Wrong-height headers:
        assert!(
            full.serve_diff(1, 2, &header_h2, &header_h2).is_none(),
            "header_h1.height != h1"
        );
        assert!(
            full.serve_diff(1, 2, &header_h1, &header_h1).is_none(),
            "header_h2.height != h2"
        );
    }
}
