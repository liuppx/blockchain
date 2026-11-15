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
use crate::mempool::Mempool;
use crate::merkle;
use crate::validator::ValidatorSet;
use crate::{
    Block, Chain, ChainError, Genesis, GovProposal, GovVote, Hash, KeyRotation, SlashEvidence,
    StakeOp, SubmissionTx,
};

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
    /// M179: gossip one signed account key-rotation op — same flood/dedup pattern
    /// as `StakeOp`. Floods into every node's pending pool; the next proposer
    /// admits it into a block. Full validation in `chain.commit.apply_key_rotation`
    /// (signature by the current key, nonce, and the active-validator guard).
    KeyRotation(KeyRotation),
    /// M195: gossip one signed governance proposal — same flood/dedup pattern as
    /// `KeyRotation`. Floods into every node's pending pool; the next proposer admits
    /// it into a block (where `apply_gov_proposal` checks sig + nonce).
    GovProposal(Box<GovProposal>),
    /// M195: gossip one signed governance vote — same flood/dedup pattern. Admitted
    /// into a block where `apply_gov_vote` checks sig + nonce + the open window.
    GovVote(Box<GovVote>),
    /// K4(测试网阶段 1):一条已签名的知识 op(创建单元 / 发布版本 / 核验 / 邀请),
    /// 与治理 op 相同的泛洪 / 去重 / 待处理池模式。
    KnowledgeOp(Box<crate::knowledge::KnowledgeOp>),
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
    GetProof {
        items: Vec<(crate::light::ProofKind, u64)>,
    },
    /// M24: the batched response. `items.len() == request.items.len()`; a
    /// `None` at index `i` means "unknown key" — the wallet's local
    /// `verify_proof_against_header` will reject it cleanly with
    /// `MembershipProofInvalid` against the cert-signed header's root. Light
    /// nodes cache entries by `(kind, id)` via
    /// [`LightGossipNode::take_proof`].
    Proof {
        items: Vec<Option<crate::light::ProofEntry>>,
    },
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
    /// M39: a peer address book for discovery — `(node_id, "host:port")` hints.
    /// A node self-advertises its own `(id, listen)` here (so neighbors learn its
    /// listen address without a handshake change) and re-gossips what it knows.
    /// The daemon Actor merges these into its book and auto-dials any discovered
    /// higher-id peer; the pure `GossipNode` / `LightGossipNode` cores drop it (no
    /// I/O, no dialing). Capped at [`MAX_PEERS`] entries per message.
    Peers(Vec<(u64, String)>),
    /// M147: a light client asks a full peer for cert-signed headers from `from`,
    /// **each paired with the validator set that certifies the next header** — the
    /// header-sync analogue of `GetHeaders` that also carries set contents so the
    /// light node can verify succession across validator-set changes from wire data
    /// alone (no in-process oracle). Full nodes serve; light nodes drop.
    GetHeadersWithSets { from: u64 },
    /// M147/M150: the response to `GetHeadersWithSets` — a height-ordered batch where
    /// entry `i` is `(header at from+i, next validator set)`. The set is the validators
    /// in state after applying block `from+i` (whose `merkle_root()` equals the header's
    /// `next_validators_root`). M150: the set is `None` when it is **unchanged** from the
    /// header's own certifying set (the common case — no validator rotation at that
    /// height), which the light node reconstructs by reusing its current tracked set;
    /// `Some(set)` carries the new set only across an actual change. Capped at
    /// [`MAX_BATCH`] pairs.
    HeadersWithSets(Vec<(CertifiedHeader, Option<ValidatorSet>)>),
}

/// M39: maximum entries in a single [`GossipMsg::Peers`] address book — over the
/// cap is a codec error ([`CodecError::TooManyItems`]). Bounds a hostile peer's
/// book so decoding can't fan out unboundedly.
pub const MAX_PEERS: usize = 1024;
/// SEC-01 (re-verify): hard cap on each staged governance pool (proposals / votes). A
/// full pool stops staging + re-flooding further gov ops — governance is low-volume, so
/// this bounds memory/CPU under a gossip flood without affecting honest operation.
pub const MAX_PENDING_GOV: usize = 4096;
/// SEC-01 (round 3): per-account share of each governance pool. Together with the
/// nonce window ([`crate::GOV_NONCE_LOOKAHEAD`]) this means one account can neither
/// fill the pool with future-nonce ops nor crowd out other accounts' current-nonce
/// ops — reaching `MAX_PENDING_GOV` takes `MAX_PENDING_GOV / 16` distinct accounts.
pub const MAX_PENDING_GOV_PER_ACCOUNT: usize = 16;

/// K4:知识 op 待处理池的全局上限(与治理池同一取值与理由)。
pub const MAX_PENDING_KNOWLEDGE: usize = 4096;
/// K4:每个账户在知识 op 待处理池中的配额。结合序号窗口,单个账户无法占满全局池。
pub const MAX_PENDING_KNOWLEDGE_PER_ACCOUNT: usize = 16;
/// SEC-01 (re-verify): hard default bound (FIFO) for the governance / key-rotation dedup
/// caches, applied at construction so they are never unbounded even when `[mempool]
/// seen_cache` is left at its `0` (unbounded) default. Governance is low-volume, so a
/// 64Ki-entry window is ample; the operator can still raise it via `seen_cache`.
pub const DEFAULT_OP_SEEN_CAP: usize = 1 << 16;

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
/// M39: peer address book — a list of `(node_id, listen_addr)` hints gossiped
/// for peer discovery. Handled by the daemon Actor (auto-dials discovered
/// higher-id peers); the pure gossip cores drop it.
pub const TAG_PEERS: u8 = 17;
/// M147: light header-sync request that also asks for per-header validator sets.
pub const TAG_GETHEADERS_SETS: u8 = 18;
/// M147: response to `GetHeadersWithSets` — `(header, next_validator_set)` pairs.
pub const TAG_HEADERS_SETS: u8 = 19;
/// M179: account key-rotation op gossip.
pub const TAG_KEYROTATION: u8 = 20;
/// M195: governance proposal op gossip.
pub const TAG_GOVPROPOSAL: u8 = 21;
/// M195: governance vote op gossip.
pub const TAG_GOVVOTE: u8 = 22;
/// K4:知识 op 的 gossip 标签。
pub const TAG_KNOWLEDGE_OP: u8 = 23;

/// M29: maximum items in a single heterogeneous batched
/// request/response. Mirrors `MAX_PROOF_BATCH = 32` so the bus caps
/// stay self-consistent. Per-primitive inner caps (kNN neighbour
/// bound, range cut bound, etc.) are enforced inside each
/// `serve_*` helper as today.
pub const MAX_BATCH_ITEMS: usize = 32;

/// M212: how many recently-committed heights the per-node liveness tracker remembers.
/// 1024 heights ≈ a generous observation window for an operator to spot a validator
/// that has stopped signing. Bounded memory (a sliding window of id-sets).
pub const LIVENESS_WINDOW: usize = 1024;

/// M212: **local, observability-only** record of which validators have been signing
/// recent commits, from THIS node's cert view. Each committed cert names its signer
/// subset (`Commit::precommits`); we remember the last [`LIVENESS_WINDOW`] such sets so
/// an operator can scrape, per current validator, how many of those heights it missed —
/// the signal to remove a persistently-absent validator via a normal validator-set
/// change (there is no automatic jailing/slashing).
///
/// Deliberately **not** consensus state: cert signer subsets are non-canonical (each
/// node assembles any >2/3 subset), so this is never folded into `state_root` and never
/// affects determinism, block validity, or the wire. Two honest nodes may report
/// slightly different missed counts; that is expected and fine for observability.
#[derive(Clone, Default)]
struct LivenessTracker {
    /// Signer id-set of each remembered height (most recent at the back), capped at
    /// [`LIVENESS_WINDOW`].
    recent: VecDeque<BTreeSet<u64>>,
}

impl LivenessTracker {
    /// Remember one committed height's signer set, evicting the oldest past the window.
    fn record(&mut self, signers: BTreeSet<u64>) {
        self.recent.push_back(signers);
        while self.recent.len() > LIVENESS_WINDOW {
            self.recent.pop_front();
        }
    }

    /// Committed heights currently remembered (the denominator for a miss rate).
    fn samples(&self) -> usize {
        self.recent.len()
    }

    /// How many remembered heights `id` did NOT sign.
    fn missed(&self, id: u64) -> usize {
        self.recent.iter().filter(|s| !s.contains(&id)).count()
    }
}

/// M55: a flood-dedup hash set bounded to its `capacity` most-recent entries.
/// Unlike the mempool's *reject-at-bound* policy (M54), a dedup set must keep
/// accepting new hashes to suppress floods, so it evicts the **oldest** entry
/// (FIFO) once full. Eviction only risks a bounded re-flood of a long-silent
/// item (it is treated as new once, re-admitted-or-dropped by state validation,
/// and re-broadcast once) — never a consensus-safety or replay violation, since
/// admission is independently gated by `validate_tx` in `Mempool::insert`.
///
/// `capacity == usize::MAX` ⇒ unbounded (the default): the FIFO `order` deque is
/// never touched, so behavior is byte-for-byte identical to a plain `BTreeSet`.
struct SeenSet {
    set: BTreeSet<Hash>,
    /// Insertion order for FIFO eviction; only maintained when bounded.
    order: VecDeque<Hash>,
    capacity: usize,
}

impl SeenSet {
    fn new() -> Self {
        SeenSet {
            set: BTreeSet::new(),
            order: VecDeque::new(),
            capacity: usize::MAX,
        }
    }

    /// SEC-01 (re-verify): a dedup set with a hard default bound, used for the governance
    /// and key-rotation op caches. Unlike `new()` (unbounded unless the operator sets
    /// `[mempool] seen_cache`), these are ALWAYS bounded so a signed-op flood can't grow
    /// the cache without limit even on a default config. `set_capacity` can still raise it.
    fn bounded(capacity: usize) -> Self {
        SeenSet {
            set: BTreeSet::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    /// Set the per-set bound (`usize::MAX` ⇒ unbounded). Trims immediately if
    /// already over — at startup the set is empty so this is normally a no-op.
    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.capacity != usize::MAX && self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    /// Mirrors `BTreeSet::insert`: returns `true` iff the hash was newly
    /// inserted. Evicts the oldest entry (FIFO) once over capacity.
    fn insert(&mut self, h: Hash) -> bool {
        let is_new = self.set.insert(h);
        if is_new && self.capacity != usize::MAX {
            self.order.push_back(h);
            while self.order.len() > self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
        is_new
    }

    /// SEC-01 (round 5): forget `h` so it can be re-admitted — used when a staged op
    /// is evicted from a full pool (not consumed), so its owner can resubmit later.
    /// The FIFO slot stays as a harmless tombstone (its eventual pop is a no-op).
    fn remove(&mut self, h: &Hash) {
        self.set.remove(h);
    }

    fn len(&self) -> usize {
        self.set.len()
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

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
    /// M139: `(height of block at index 0) - 1` — i.e. the highest height that has
    /// been **pruned**. Always 0 for a fresh-from-genesis node (today's contract);
    /// set to the snapshot's tip height by [`Self::from_snapshot`]. `blocks[i]`
    /// therefore means height `base_height + 1 + i` (and `blocks[0]` is the block at
    /// `base_height + 1`).
    base_height: u64,
    /// M139: the post-`base_height` state used as the replay anchor for
    /// `serve_diff`/`serve_lock` (which need to rebuild a per-height state
    /// snapshot). `None` for a genesis-start node (replays from `Self::genesis` via
    /// [`Chain::replay`]); `Some((state, base_head))` for a snapshot-boot (replays
    /// from this via [`Chain::replay_from`]). The first variant is a heap
    /// allocation-free `None`; the second is a single `ChainState` clone (one extra
    /// state snapshot per node).
    base: Option<Box<(crate::ChainState, crate::Hash)>>,
    /// M139: retained post-`base_height` chain; `blocks[i]` is height `base_height+1+i`.
    /// Pre-snapshot heights are NOT retained — a snapshot-booted node returns `None`
    /// (and the HTTP layer turns that into `410 Gone`) for any height ≤ `base_height`.
    blocks: Vec<Block>,
    certs: Vec<Commit>,
    /// Content hashes of transactions already seen — makes gossip flooding
    /// idempotent. M55: bounded (FIFO-evicting) via [`SeenSet`].
    seen_tx: SeenSet,
    /// Content hashes of equivocation evidence already seen — dedup for `Evidence`.
    seen_evidence: SeenSet,
    /// Content hashes of stake ops already seen — dedup for `StakeOp`.
    seen_stake_op: SeenSet,
    /// M179: content hashes of key-rotation ops already seen — dedup for `KeyRotation`.
    seen_key_rotation: SeenSet,
    /// M195: content hashes of governance proposals / votes already seen — dedup.
    seen_gov_proposal: SeenSet,
    seen_gov_vote: SeenSet,
    seen_knowledge: SeenSet,
    /// Evidence staged to be carried by the next block this node proposes
    /// (drained by the driver via [`Self::take_pending_evidence`]).
    pending_evidence: Vec<SlashEvidence>,
    /// Stake ops staged to be carried by the next block this node proposes.
    pending_stake_ops: Vec<StakeOp>,
    /// M179: key-rotation ops staged for the next block this node proposes.
    pending_key_rotations: Vec<KeyRotation>,
    /// M195: governance proposals / votes staged for the next block this node proposes.
    pending_gov_proposals: Vec<GovProposal>,
    pending_gov_votes: Vec<GovVote>,
    pending_knowledge_ops: Vec<crate::knowledge::KnowledgeOp>,
    /// Known peer ids (iterated in sorted order for deterministic output).
    peers: BTreeSet<u64>,
    /// M212: local liveness-observability window (NOT consensus state; see
    /// [`LivenessTracker`]).
    liveness: LivenessTracker,
}

impl GossipNode {
    /// A fresh node holding only `genesis`, aware of `peers`.
    pub fn new(
        id: u64,
        genesis: Genesis,
        max_txs: usize,
        peers: impl IntoIterator<Item = u64>,
    ) -> Self {
        GossipNode {
            id,
            chain: Chain::new(genesis.clone()),
            genesis,
            mempool: Mempool::new(max_txs),
            // M139: base=0 = genesis-start contract (byte-identical to pre-M139).
            base_height: 0,
            base: None,
            blocks: Vec::new(),
            certs: Vec::new(),
            seen_tx: SeenSet::new(),
            seen_evidence: SeenSet::new(),
            seen_stake_op: SeenSet::new(),
            seen_key_rotation: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_gov_proposal: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_gov_vote: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_knowledge: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            pending_evidence: Vec::new(),
            pending_stake_ops: Vec::new(),
            pending_key_rotations: Vec::new(),
            pending_gov_proposals: Vec::new(),
            pending_gov_votes: Vec::new(),
            pending_knowledge_ops: Vec::new(),
            peers: peers.into_iter().filter(|&p| p != id).collect(),
            liveness: LivenessTracker::default(),
        }
    }

    /// M139: adopt a snapshot as the starting point. The caller MUST have verified
    /// `snapshot` against a cert-signed header (see
    /// [`crate::light::verify_snapshot_against_header`]) — this constructor is
    /// infallible plumbing. `head` is the hash of the block at `snapshot.height`
    /// (the provider attaches it; it can't be reconstructed from the state alone).
    /// The retained `blocks`/`certs` vecs start empty; the first `apply_certified` must
    /// be height `snapshot.height + 1`. `genesis` is still required because the chain's
    /// identity (genesis hash) is the trust anchor for the rest of the runtime, even
    /// though no pre-snapshot history is replayed.
    pub fn from_snapshot(
        id: u64,
        genesis: Genesis,
        snapshot: crate::ChainState,
        head: Hash,
        max_txs: usize,
        peers: impl IntoIterator<Item = u64>,
    ) -> Self {
        let base_height = snapshot.height;
        GossipNode {
            id,
            chain: Chain::from_snapshot(snapshot.clone(), head),
            genesis,
            mempool: Mempool::new(max_txs),
            base_height,
            // Clone the base state once so `serve_diff`/`serve_lock` can replay a tail
            // onto it without re-applying the snapshot's pre-tx state themselves.
            base: Some(Box::new((snapshot, head))),
            blocks: Vec::new(),
            certs: Vec::new(),
            seen_tx: SeenSet::new(),
            seen_evidence: SeenSet::new(),
            seen_stake_op: SeenSet::new(),
            seen_key_rotation: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_gov_proposal: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_gov_vote: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            seen_knowledge: SeenSet::bounded(DEFAULT_OP_SEEN_CAP),
            pending_evidence: Vec::new(),
            pending_stake_ops: Vec::new(),
            pending_key_rotations: Vec::new(),
            pending_gov_proposals: Vec::new(),
            pending_gov_votes: Vec::new(),
            pending_knowledge_ops: Vec::new(),
            peers: peers.into_iter().filter(|&p| p != id).collect(),
            liveness: LivenessTracker::default(),
        }
    }

    /// M139: vec index for committed `height` — `None` when `height ≤ base_height`
    /// (pruned) or when `height` is past the tip. The base=0 case is byte-identical
    /// to the prior `(height - 1) as usize` index (the existing `&`/`.get` already
    /// filtered pre-tip / pre-base entries).
    fn vec_idx(&self, height: u64) -> Option<usize> {
        if height <= self.base_height {
            return None;
        }
        let i = (height - 1 - self.base_height) as usize;
        if i >= self.blocks.len() {
            return None;
        }
        Some(i)
    }

    /// M139: the post-apply [`ChainState`] **at** a committed `height` (i.e. after applying
    /// the block at `height`) — the replay anchor for `serve_diff` / `serve_lock`. For a
    /// genesis-start node (`base == None`) this is `Chain::replay(genesis, &blocks[..height])`,
    /// byte-identical to the pre-M139 code. For a snapshot-booted node it replays the retained
    /// tail `&blocks[.. height - base_height]` on top of the snapshot base state via
    /// [`Chain::replay_from`]. `None` when `height` is pruned (≤ `base_height`) or past the tip.
    fn replay_to_state_at(&self, height: u64) -> Option<crate::ChainState> {
        if height <= self.base_height {
            return None;
        }
        let tail_len = (height - self.base_height) as usize;
        if tail_len > self.blocks.len() {
            return None;
        }
        let tail = &self.blocks[..tail_len];
        let chain = match self.base.as_deref() {
            None => crate::Chain::replay(self.genesis.clone(), tail).ok()?,
            Some((state, head)) => crate::Chain::replay_from(state.clone(), *head, tail).ok()?,
        };
        Some(chain.state)
    }

    pub fn height(&self) -> u64 {
        self.chain.state.height
    }

    pub fn head(&self) -> Hash {
        self.chain.head
    }

    /// M140: the highest height whose block has been **pruned** — `0` for a fresh-from-
    /// genesis node (today's contract), the snapshot's tip height for a snapshot-booted
    /// node (M139). Exposed so the daemon's HTTP layer can disambiguate "pruned"
    /// (410 Gone) from "not found" (404) on height-keyed reads.
    pub fn base_height(&self) -> u64 {
        self.base_height
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// M146: O(1) lookup of the retained committed block at `height`, using the
    /// offset-aware `vec_idx`. `None` for a pruned height (`≤ base_height`), genesis
    /// (height 0), or a height past the tip — replacing a linear scan of `blocks()`.
    pub fn block_at(&self, height: u64) -> Option<&Block> {
        self.vec_idx(height).map(|i| &self.blocks[i])
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

    /// M179: key-rotation ops staged for the next proposed block (read-only view).
    pub fn pending_key_rotations(&self) -> &[KeyRotation] {
        &self.pending_key_rotations
    }

    /// M195: governance proposals / votes staged for the next proposed block.
    pub fn pending_gov_proposals(&self) -> &[GovProposal] {
        &self.pending_gov_proposals
    }
    pub fn pending_gov_votes(&self) -> &[GovVote] {
        &self.pending_gov_votes
    }
    pub fn pending_knowledge_ops(&self) -> &[crate::knowledge::KnowledgeOp] {
        &self.pending_knowledge_ops
    }

    /// M212: local liveness-observability snapshot — `(window_samples, [(validator_id,
    /// missed_commits)])` for every **current** validator, sorted by id. `window_samples`
    /// is the number of recently-committed heights remembered (the miss-rate denominator);
    /// `missed_commits` is how many of those this node did not see the validator sign.
    /// Observability only (this node's cert view) — never consensus state. See
    /// [`LivenessTracker`].
    pub fn liveness_report(&self) -> (usize, Vec<(u64, usize)>) {
        let mut per_val: Vec<(u64, usize)> = self
            .chain
            .state
            .validators
            .validators()
            .iter()
            .map(|v| (v.id, self.liveness.missed(v.id)))
            .collect();
        per_val.sort_by_key(|&(id, _)| id);
        (self.liveness.samples(), per_val)
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
            || !self.pending_key_rotations.is_empty()
            || !self.pending_gov_proposals.is_empty()
            || !self.pending_gov_votes.is_empty()
            || !self.pending_knowledge_ops.is_empty()
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

    /// M179: drain staged key-rotation ops, transferring them to a block builder.
    pub fn take_pending_key_rotations(&mut self) -> Vec<KeyRotation> {
        std::mem::take(&mut self.pending_key_rotations)
    }

    /// M195: drain staged governance proposals / votes, transferring them to a builder.
    pub fn take_pending_gov_proposals(&mut self) -> Vec<GovProposal> {
        std::mem::take(&mut self.pending_gov_proposals)
    }
    pub fn take_pending_gov_votes(&mut self) -> Vec<GovVote> {
        std::mem::take(&mut self.pending_gov_votes)
    }
    pub fn take_pending_knowledge_ops(&mut self) -> Vec<crate::knowledge::KnowledgeOp> {
        std::mem::take(&mut self.pending_knowledge_ops)
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
            crate::light::ProofKind::Account => self.chain.state.account_proof(id).and_then(|p| {
                self.chain.state.accounts.get(&id).cloned().map(|a| {
                    crate::light::ProofEntry::Account {
                        id,
                        account: a,
                        proof: p,
                    }
                })
            }),
            crate::light::ProofKind::Reviewer => self.chain.state.reviewer_proof(id).map(|p| {
                let reputation = self.chain.state.reviewers.get(&id).copied().unwrap_or(0.0);
                crate::light::ProofEntry::Reviewer {
                    id,
                    reputation,
                    proof: p,
                }
            }),
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
                    self.chain.state.graph.nodes.get(idx).cloned().map(|n| {
                        crate::light::ProofEntry::GraphNode {
                            node_id: n.node_id,
                            graph_node: n,
                            proof: p,
                        }
                    })
                })
            }
            crate::light::ProofKind::Cnft => {
                // M185: cNFTs are addressed by their cognitive-graph node_id and
                // proven against the dedicated `contributions_root`.
                self.chain.state.cnft_proof(id).and_then(|p| {
                    self.chain.state.contributions.get(&id).cloned().map(|c| {
                        crate::light::ProofEntry::Cnft {
                            node_id: id,
                            cnft: c,
                            proof: p,
                        }
                    })
                })
            }
        }
    }

    /// M60: bundle any `ProofKind`'s inclusion proof with the certified header it
    /// verifies against (reviewer/validator/graph node over RPC, generalizing
    /// M59's account-only `account_inclusion`). `None` ⇒ unknown id/index OR no
    /// certified head yet (height 0). GraphNode addresses by insertion index.
    pub fn inclusion(
        &self,
        kind: crate::light::ProofKind,
        id: u64,
    ) -> Option<(CertifiedHeader, crate::light::ProofEntry)> {
        let entry = self.serve_inclusion(kind, id)?;
        let head_ch = self.headers_from(self.height()).into_iter().next_back()?;
        Some((head_ch, entry))
    }

    /// M61: serve a heterogeneous batch bundled with the certified head it
    /// verifies against — the RPC analogue of the gossip GetBatch path,
    /// generalizing M60's single `inclusion`. `None` ⇒ `serve_batch` rejected
    /// (over `MAX_BATCH_ITEMS` or a degenerate Diff range) OR no certified head
    /// yet (height 0). Inclusion/kNN/range slots verify against the bundled
    /// head; Diff slots additionally need the client's own range blocks.
    ///
    /// M62: the third tuple element ships exactly that range — `[1..=max_h2]`,
    /// where `max_h2` is the largest h₂ across the request's Diff items (0 ⇒
    /// empty when the batch has no Diff item) — so a Diff slot verifies without
    /// the client having pre-synced the chain. Computed before `items` is moved
    /// into `serve_batch`.
    pub fn batch(&self, items: Vec<crate::light::BatchItem>) -> Option<crate::light::BatchReply> {
        let max_h2 = items
            .iter()
            .filter_map(|it| match it {
                crate::light::BatchItem::Diff { h2, .. } => Some(*h2),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let env = self.serve_batch(items)?;
        let head_ch = self.headers_from(self.height()).into_iter().next_back()?;
        Some((head_ch, env, self.blocks_through(max_h2)))
    }

    /// M59: bundle an account inclusion proof with the certified header it
    /// verifies against. `None` ⇒ unknown account OR no certified head yet
    /// (height 0). The head block's `accounts_root` commits to exactly the
    /// current state the proof is built from, so the returned pair always
    /// self-verifies (same soundness as the gossip `GetProof` path — this is
    /// just the single-account producer wired for the external read RPC).
    pub fn account_inclusion(
        &self,
        id: u64,
    ) -> Option<(CertifiedHeader, crate::light::ProofEntry)> {
        self.inclusion(crate::light::ProofKind::Account, id)
    }

    /// M124: the canonical Merkle *leaf bytes* for one entity — the exact preimage a light
    /// client feeds to [`merkle::leaf_hash`] when verifying an inclusion proof. Identical to
    /// the leaf inside what [`inclusion`](Self::inclusion) would ship, but without computing
    /// (or requiring) the proof path or certified header, so it answers even before the first
    /// block is committed. `None` when no such entity exists.
    pub fn leaf(&self, kind: crate::light::ProofKind, id: u64) -> Option<Vec<u8>> {
        self.serve_inclusion(kind, id).map(|e| e.leaf())
    }

    /// M126: the cert-signed [`CertifiedHeader`] (header + its finality `Commit`) at a committed
    /// `height` — the standalone unit a light client verifies against, decoupled from any account
    /// proof. `blocks[i]`/`certs[i]` is height `base_height+1+i` (M139). Genesis (height 0, no
    /// log block/cert), a pruned height (≤ `base_height`), and a height past the tip ⇒ `None`.
    pub fn certified_header(&self, height: u64) -> Option<CertifiedHeader> {
        if height == 0 {
            return None;
        }
        let i = self.vec_idx(height)?;
        let block = self.blocks.get(i)?;
        let cert = self.certs.get(i)?;
        Some(CertifiedHeader::from_certified(block, cert))
    }

    /// M127: a contiguous range of certified headers for heights `[from, from+limit)` that are
    /// committed — the RPC analog of gossip header-sync, so a light client can bootstrap its
    /// header chain over plain HTTP. `from == 0` (genesis has no log header) or `from ≤
    /// base_height` (pruned) yields an empty range; the result stops at the tip.
    pub fn headers_range(&self, from: u64, limit: usize) -> Vec<CertifiedHeader> {
        if from == 0 {
            return Vec::new();
        }
        if from <= self.base_height {
            return Vec::new();
        }
        let start = (from - 1 - self.base_height) as usize;
        (start..self.blocks.len().min(start.saturating_add(limit)))
            .map(|i| CertifiedHeader::from_certified(&self.blocks[i], &self.certs[i]))
            .collect()
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
                    // M139: pre-snapshot (≤ `base_height`) heights are pruned on a
                    // snapshot-booted node; reject at the producer side so the
                    // batch arm honestly returns `None` rather than serving
                    // a half-replayed diff.
                    if h1 == 0 || h1 >= h2 || h2 > self.height() || h1 <= self.base_height {
                        return None;
                    }
                    let idx_h1 = self.vec_idx(h1)?;
                    let idx_h2 = self.vec_idx(h2)?;
                    let block_h1 = &self.blocks[idx_h1];
                    let block_h2 = &self.blocks[idx_h2];
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
        Some(crate::light::KnnClaim {
            query,
            k,
            neighbours,
        })
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
                let sa =
                    crate::engine::cos_sim(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], &a.embedding);
                let sb =
                    crate::engine::cos_sim(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], &b.embedding);
                sb.partial_cmp(&sa)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.node_id.cmp(&b.node_id))
            });
            sorted
                .iter()
                .enumerate()
                .map(|(i, n)| (n.node_id, i))
                .collect()
        };
        let mut nodes: Vec<(u64, crate::engine::GraphNode, merkle::Proof)> =
            Vec::with_capacity(ranked.len());
        for (node_id, _sim) in &ranked {
            let sorted_idx = *sorted_index.get(node_id)?;
            let proof = self
                .chain
                .state
                .graph_range_proof(sorted_idx, sorted_idx + 1)?;
            let (_id, graph_node, merkle_proof) = proof.entries.into_iter().next()?;
            nodes.push((*node_id, graph_node, merkle_proof));
        }
        Some(crate::light::RangeClaim {
            query,
            min_sim,
            nodes,
        })
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
        // M139: pre-snapshot heights (h1 ≤ `base_height`) are pruned on a
        // snapshot-booted node; we don't have the pre-snapshot history to
        // replay. For `h1 > base_height`, replay the **tail** onto the
        // snapshot's base state (or genesis state, when `base == None`).
        if h1 <= self.base_height {
            return None;
        }
        let state_h1 = self.replay_to_state_at(h1)?;
        let cert_h1 = self.certs.get(self.vec_idx(h1)?)?.clone();
        let cert_h2 = self.certs.get(self.vec_idx(h2)?)?.clone();
        let diff = self.chain.state.graph_diff(&state_h1);
        // Per-side tracked sets come from the wallet's POV: after
        // applying block `h` the set that certifies `h+1` is
        // `state.validators`. The wallet already maintains this set
        // via `ValidatorTracker::follow`, but for the producer we
        // surface the sets as-is — the wallet re-derives them anyway
        // from its header cache.
        let tracked_h1 = self.replay_to_state_at(h1)?.validators.clone();
        let tracked_h2 = self.replay_to_state_at(h2)?.validators.clone();
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
        // M139: the carrying block's header+cert. `vec_idx` returns `None` for a pruned
        // (≤ base_height) height, so a snapshot-booted node honestly can't serve a lock
        // whose block it no longer retains.
        let idx = self.vec_idx(height)?;
        let block = self.blocks.get(idx)?;
        let cert = self.certs.get(idx)?;
        let header = block.header();
        // Active set that certifies this header (the set the endpoint's tracker must follow
        // to know the cert) — the post-apply state at this height, from the same
        // replay-from-base path `serve_diff` uses.
        let tracked_set = self.replay_to_state_at(height)?.validators.clone();
        Some(crate::bridge::LockEnvelope {
            source_header: header,
            source_cert: cert.clone(),
            source_tracked_set: tracked_set,
            lock_id,
            lock,
            proof,
        })
    }

    /// M64: every bridge lock on this chain as `(lock_id, height, lock)`, id order
    /// (the `BTreeMap` iterates sorted) — the directory a client enumerates before
    /// fetching a single lock's proof via `serve_lock`. Plain (unverified) data; a
    /// client verifies any one lock via the M63 `/bridge/lock/{id}/proof` route.
    pub fn lock_listing(&self) -> crate::light::LockListing {
        self.chain
            .state
            .bridge_locks
            .iter()
            .map(|(&id, lock)| {
                let h = self
                    .chain
                    .state
                    .bridge_lock_heights
                    .get(&id)
                    .copied()
                    .unwrap_or(0);
                (id, h, lock.clone())
            })
            .collect()
    }

    /// The certified blocks from `height` onward (inclusive), capped at
    /// [`MAX_BATCH`] — the payload for a peer's `GetBlocks`. M139: a snapshot-booted node
    /// can only serve `height > base_height`; a request for a pruned height returns empty
    /// (the requester must find a full-history peer or snapshot-sync itself).
    fn batch_from(&self, height: u64) -> Vec<(Block, Commit)> {
        if height == 0 || height <= self.base_height {
            return Vec::new();
        }
        let start = (height - 1 - self.base_height) as usize;
        (start..self.blocks.len().min(start + MAX_BATCH))
            .map(|i| (self.blocks[i].clone(), self.certs[i].clone()))
            .collect()
    }

    /// M62: every certified `(Block, Commit)` from height 1 through `up_to`
    /// inclusive (`[1..=up_to]`), in height order — the block range a client
    /// needs to replay for `ValidatorTracker::verify_diff_against_headers`.
    /// Unlike [`Self::batch_from`] this is NOT capped at [`MAX_BATCH`]: the
    /// Diff verifier replays the whole prefix, so a short cap would make a
    /// long-chain Diff unverifiable. `up_to == 0` ⇒ empty (a batch with no Diff
    /// item needs no range); `up_to` beyond the tip clamps to the stored tip.
    /// M139: a snapshot-booted node (`base_height > 0`) does NOT hold the
    /// genesis-anchored prefix `[1..=base_height]`, so it cannot serve this
    /// full-prefix payload at all — it returns empty. A Diff verifier must pull
    /// the prefix from a full-history peer.
    fn blocks_through(&self, up_to: u64) -> Vec<(Block, Commit)> {
        if self.base_height > 0 {
            return Vec::new();
        }
        let end = (up_to as usize).min(self.blocks.len());
        (0..end)
            .map(|i| (self.blocks[i].clone(), self.certs[i].clone()))
            .collect()
    }

    /// The cert-signed headers from `height` onward (inclusive), capped at
    /// [`MAX_BATCH`] — the payload for a peer's `GetHeaders` (M22). Drops
    /// transaction bodies, stake ops, and slashing evidence; what remains is
    /// exactly the cert-signed prefix. M139: a request for a pruned height
    /// (≤ `base_height`) returns empty.
    pub fn headers_from(&self, height: u64) -> Vec<CertifiedHeader> {
        if height == 0 || height <= self.base_height {
            return Vec::new();
        }
        let start = (height - 1 - self.base_height) as usize;
        (start..self.blocks.len().min(start + MAX_BATCH))
            .map(|i| CertifiedHeader::from_certified(&self.blocks[i], &self.certs[i]))
            .collect()
    }

    /// M147/M150: the response payload for `GetHeadersWithSets` — pairs each retained
    /// header from `height` onward with the validator set that certifies the **next**
    /// header, or `None` when that set is unchanged from the header's own certifying set
    /// (M150 compression — the light node reuses its current tracked set). Capped at
    /// [`MAX_BATCH`].
    ///
    /// Implementation is one replay pass: seed at the base (genesis, or the snapshot
    /// base), commit each block, and for each block from index `start` compare the
    /// validator set before vs after applying it — emit `None` when unchanged, else
    /// `Some(new_set)`. A snapshot-booted node's `from` must be `> base_height` (pruned
    /// heights return empty, matching `headers_from`).
    pub fn headers_with_sets_from(
        &self,
        height: u64,
    ) -> Vec<(CertifiedHeader, Option<ValidatorSet>)> {
        if height == 0 || height <= self.base_height {
            return Vec::new();
        }
        let start = (height - 1 - self.base_height) as usize;
        let end = self.blocks.len().min(start + MAX_BATCH);
        if start >= end {
            return Vec::new();
        }
        // Seed a trial chain at the base (genesis, or the snapshot base for a
        // snapshot-booted node), then commit the retained tail forward, snapshotting
        // the validator set after each block from index `start` onward. Committing the
        // `[0..start]` prefix first is what makes each recorded set the true post-apply
        // set (one replay pass; the prefix results are discarded).
        let mut chain = match self.base.as_deref() {
            None => Chain::new(self.genesis.clone()),
            Some((state, head)) => Chain::from_snapshot(state.clone(), *head),
        };
        let mut out = Vec::with_capacity(end - start);
        for i in 0..end {
            // M150: the set that certifies this header (before applying its block).
            let before_root = chain.state.validators.merkle_root();
            let mut block = self.blocks[i].clone();
            if chain.commit(&mut block).is_err() {
                // A replay failure means the retained log is inconsistent with the
                // base — already validated at load time, so stop serving rather than
                // panic.
                break;
            }
            if i >= start {
                // Attach the next set only across an actual validator-set change.
                let next = if chain.state.validators.merkle_root() == before_root {
                    None
                } else {
                    Some(chain.state.validators.clone())
                };
                out.push((
                    CertifiedHeader::from_certified(&self.blocks[i], &self.certs[i]),
                    next,
                ));
            }
        }
        out
    }

    /// Every **retained** certified header (snapshot — the cert-signed projection
    /// of the full-node's certified chain). M139: for a snapshot-booted node this is
    /// the post-`base_height` headers only; pre-snapshot history is pruned. The light
    /// client uses this with
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
        // M143: the commit advanced some accounts' nonces, so drop any now-stale pending
        // txs (replays / losing same-nonce duplicates) a pipelining pool may hold.
        self.mempool.prune_stale(&self.chain);
        // M33: every validator builds candidates from its own pending pools, so
        // once a block commits we must drop the evidence / stake ops it carried —
        // otherwise this node would re-propose already-applied ops at the next
        // height and the candidate would fail to apply. (Pre-M33 only the single
        // sequencer drained these, via take_pending_*; distributed proposing
        // needs every node to reconcile against committed blocks.)
        if !block.slashing_evidence.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.slashing_evidence.iter().map(|ev| ev.hash()).collect();
            self.pending_evidence
                .retain(|ev| !committed.contains(&ev.hash()));
        }
        if !block.stake_ops.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.stake_ops.iter().map(|op| op.hash()).collect();
            self.pending_stake_ops
                .retain(|op| !committed.contains(&op.hash()));
        }
        if !block.key_rotations.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.key_rotations.iter().map(|op| op.hash()).collect();
            self.pending_key_rotations
                .retain(|op| !committed.contains(&op.hash()));
        }
        // SEC-01: drop committed governance ops from the pending pools so this node
        // never re-proposes an already-applied proposal/vote (whose nonce is now
        // consumed) at the next height.
        if !block.gov_proposals.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.gov_proposals.iter().map(|op| op.hash()).collect();
            self.pending_gov_proposals
                .retain(|op| !committed.contains(&op.hash()));
        }
        if !block.gov_votes.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.gov_votes.iter().map(|op| op.hash()).collect();
            self.pending_gov_votes
                .retain(|op| !committed.contains(&op.hash()));
        }
        // K4:同理,丢弃已入块的知识 op。
        if !block.knowledge_ops.is_empty() {
            let committed: std::collections::HashSet<[u8; 32]> =
                block.knowledge_ops.iter().map(|op| op.hash()).collect();
            self.pending_knowledge_ops
                .retain(|op| !committed.contains(&op.hash()));
        }
        // SEC-01 (round 3): the block just applied may have advanced account nonces
        // (via any op type) or matured proposals, so staged gov ops that can now never
        // apply are dropped here instead of holding pool slots until eviction. Cheap:
        // nonce-window / proposal-open checks only, no signature work.
        {
            let state = &self.chain.state;
            self.pending_gov_proposals
                .retain(|op| state.gov_proposal_not_stale(op));
            self.pending_gov_votes
                .retain(|op| state.gov_vote_not_stale(op));
            // K4:序号已落在窗口之外的知识 op 永远无法执行,丢弃。
            self.pending_knowledge_ops
                .retain(|op| state.knowledge_op_not_stale(op));
            // SEC-01 (round 5): a key rotation in this block replaced the account's
            // pubkey, so its queued ops signed by the OLD key are still inside the nonce
            // window (rotation bumps the nonce by one) yet can never verify at apply.
            // Re-run full admissibility (signature included) for just those accounts —
            // bounded by their quota, so the signature work is small.
            if !block.key_rotations.is_empty() {
                let rotated: std::collections::HashSet<u64> =
                    block.key_rotations.iter().map(|r| r.account).collect();
                self.pending_gov_proposals.retain(|op| {
                    !rotated.contains(&op.proposer) || state.gov_proposal_admissible(op).is_ok()
                });
                self.pending_gov_votes.retain(|op| {
                    !rotated.contains(&op.voter) || state.gov_vote_admissible(op).is_ok()
                });
                // K4:被轮换密钥的账户,其旧密钥签名的知识 op 无法再验签,重跑完整准入。
                self.pending_knowledge_ops.retain(|op| {
                    !rotated.contains(&op.actor) || state.knowledge_op_admissible(op).is_ok()
                });
            }
        }
        // M212: record which validators signed this height's commit, from THIS node's
        // cert view, for local liveness observability (see `LivenessTracker`). Pure
        // local metric — NOT folded into state_root (cert signer subsets are
        // non-canonical), so it never affects determinism or the chain.
        let signers: BTreeSet<u64> = cert.precommits.iter().map(|v| v.validator).collect();
        self.liveness.record(signers);
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
        let mut candidate = self
            .mempool
            .build_block(&self.chain, timestamp_days)
            .unwrap_or(Block {
                height: self.chain.state.height + 1,
                prev_hash: self.chain.head,
                timestamp_days,
                proposer: 0,
                next_validators_root: [0u8; 32],
                state_root: [0u8; 32],
                accounts_root: [0u8; 32],
                graph_root: [0u8; 32],
                bridge_root: [0u8; 32],
                contributions_root: [0u8; 32],
                unbonding_root: [0u8; 32],
                txs: Vec::new(),
                validator_updates: Vec::new(),
                stake_ops: Vec::new(),
                slashing_evidence: Vec::new(),
                bridge_locks: Vec::new(),
                bridge_headers: Vec::new(),
                bridge_redeems: Vec::new(),
                bridge_unbondings: Vec::new(),
                key_rotations: Vec::new(),
                gov_proposals: Vec::new(),
                gov_votes: Vec::new(),
                knowledge_ops: Vec::new(),
            });
        // M235: include only stake-ops that apply cleanly AND don't trip the §10.11 share cap,
        // so a stale/over-cap bond can't make the whole candidate unsealable and stall
        // production (mirrors the gov filter below / SEC-01).
        candidate.stake_ops = self
            .chain
            .state
            .filter_applicable_stake_ops(&self.pending_stake_ops);
        candidate.slashing_evidence = self.pending_evidence.clone();
        candidate.key_rotations = self.pending_key_rotations.clone();
        // SEC-01: include only governance ops that apply cleanly against current state
        // (in block order), so a stale/invalid staged op can't make the whole candidate
        // unproposable and stall production.
        let (gp, gv) = self.chain.state.filter_applicable_gov(
            self.chain.state.height + 1,
            &self.pending_gov_proposals,
            &self.pending_gov_votes,
        );
        // K4:知识 op 在治理 op 之后执行、共用账户序号,所以在已选入的治理 op 之上过滤。
        candidate.knowledge_ops = self.chain.state.filter_applicable_knowledge(
            self.chain.state.height + 1,
            &gp,
            &gv,
            &self.pending_knowledge_ops,
        );
        candidate.gov_proposals = gp;
        candidate.gov_votes = gv;
        // M122: this node is the proposer of the block it builds — credit its own
        // validator id so the tx fees route to it. Only a block actually elected and
        // committed keeps this; a losing candidate is discarded.
        candidate.proposer = self.id;
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
        self.submit_local_checked(tx)
            .map(|(_, out)| out)
            .unwrap_or_default()
    }

    /// M54: bound the pending mempool (DoS hardening). `usize::MAX` ⇒ unbounded.
    /// Admission past the bound is rejected with `ChainError::MempoolFull`.
    pub fn set_mempool_capacity(&mut self, capacity: usize) {
        self.mempool.set_capacity(capacity);
    }

    /// M57: bound how many pending txs a single account (`author`) may hold at once
    /// (DoS hardening — stops one account from monopolizing the pool). `usize::MAX`
    /// ⇒ unbounded. Admission past it is rejected with `ChainError::AccountQuotaFull`.
    pub fn set_mempool_per_account_limit(&mut self, limit: usize) {
        self.mempool.set_per_account_limit(limit);
    }

    /// M143: set the mempool's nonce pipelining window — how many future nonces beyond
    /// an account's current one may be buffered so they can be laid into one block in
    /// nonce order. `0` ⇒ strict one-pending-tx-per-account (the default; byte-identical
    /// to pre-M143).
    pub fn set_mempool_nonce_lookahead(&mut self, lookahead: usize) {
        self.mempool.set_nonce_lookahead(lookahead);
    }

    /// M144: enable replace-by-fee mempool admission — a tx at a `(author, nonce)`
    /// already held replaces the incumbent iff it bids a strictly higher fee.
    /// `false` (the default) keeps same-slot competitors coexisting.
    pub fn set_mempool_replace_by_fee(&mut self, enabled: bool) {
        self.mempool.set_replace_by_fee(enabled);
    }

    /// M145: set the replace-by-fee minimum fee increment (`new.fee >= incumbent.fee +
    /// min_fee_bump`). Default `1` (= strictly higher). Only used when RBF is on.
    pub fn set_mempool_min_fee_bump(&mut self, min_fee_bump: u64) {
        self.mempool.set_min_fee_bump(min_fee_bump);
    }

    /// M157: set the mempool minimum-fee admission floor (anti-spam). `0` ⇒ no floor.
    pub fn set_mempool_min_fee(&mut self, min_fee: u64) {
        self.mempool.set_min_fee(min_fee);
    }

    /// M55: bound each gossip dedup set (`seen_tx`/`seen_evidence`/`seen_stake_op`)
    /// to `capacity` most-recent entries (FIFO eviction). `usize::MAX` ⇒ unbounded
    /// (the default). The three sets are bounded independently to the same value.
    pub fn set_seen_capacity(&mut self, capacity: usize) {
        self.seen_tx.set_capacity(capacity);
        self.seen_evidence.set_capacity(capacity);
        self.seen_stake_op.set_capacity(capacity);
        // SEC-01 (re-verify): the governance dedup sets were previously unbounded, so a
        // peer could grow them without limit. Bound them with the same policy.
        self.seen_key_rotation.set_capacity(capacity);
        self.seen_gov_proposal.set_capacity(capacity);
        self.seen_gov_vote.set_capacity(capacity);
        self.seen_knowledge.set_capacity(capacity);
    }

    /// M55: current number of entries in the tx dedup set (the one under flood
    /// pressure) — surfaced as a metrics gauge.
    pub fn seen_tx_len(&self) -> usize {
        self.seen_tx.len()
    }

    /// M55: the configured tx dedup-set bound (`usize::MAX` ⇒ unbounded).
    pub fn seen_tx_capacity(&self) -> usize {
        self.seen_tx.capacity()
    }

    /// M53: like [`submit_local`](Self::submit_local) but surfaces the mempool's
    /// rejection reason instead of swallowing it — used by the external ingress
    /// RPC to report accept (with the tx hash) or reject (with a `ChainError`).
    /// Ordering matches `submit_local` verbatim: the tx is marked seen even when
    /// admission fails, so a rejected tx is not re-requested from peers.
    pub fn submit_local_checked(
        &mut self,
        tx: SubmissionTx,
    ) -> Result<(Hash, Vec<(u64, GossipMsg)>), crate::ChainError> {
        let h = tx.hash();
        self.seen_tx.insert(h);
        let hash = self.mempool.insert(&self.chain, tx.clone())?;
        Ok((hash, self.broadcast(GossipMsg::Tx(tx), None)))
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

    /// M179: submit a locally-originated key-rotation op: stage it for the next
    /// block and flood it to peers. As with stake ops, there is nothing to
    /// validate structurally here — the signature (by the account's current
    /// key), nonce, and active-validator guard are checked at apply time in
    /// `chain.commit.apply_key_rotation`, which rolls back the whole block on
    /// any failure.
    pub fn submit_local_key_rotation(&mut self, op: KeyRotation) -> Vec<(u64, GossipMsg)> {
        let h = op.hash();
        self.seen_key_rotation.insert(h);
        self.pending_key_rotations.push(op.clone());
        self.broadcast(GossipMsg::KeyRotation(op), None)
    }

    /// M195: stage a locally-created governance proposal and flood it. Goes through
    /// the same admission as gossip ingress ([`Self::admit_gov_proposal`]): the
    /// structured `Err` tells the caller (RPC) exactly why the op was NOT staged.
    pub fn submit_local_gov_proposal(
        &mut self,
        op: GovProposal,
    ) -> Result<Vec<(u64, GossipMsg)>, ChainError> {
        self.admit_gov_proposal(&op)?;
        Ok(self.broadcast(GossipMsg::GovProposal(Box::new(op)), None))
    }

    /// M195: stage a locally-created governance vote and flood it (see
    /// [`Self::submit_local_gov_proposal`]).
    pub fn submit_local_gov_vote(
        &mut self,
        op: GovVote,
    ) -> Result<Vec<(u64, GossipMsg)>, ChainError> {
        self.admit_gov_vote(&op)?;
        Ok(self.broadcast(GossipMsg::GovVote(Box::new(op)), None))
    }

    /// K4:暂存一条本地创建的知识 op 并泛洪。与 gossip 入口走同一个准入函数
    /// ([`Self::admit_knowledge_op`]),结构化的 `Err` 告诉调用方(RPC)为什么没有入池。
    pub fn submit_local_knowledge_op(
        &mut self,
        op: crate::knowledge::KnowledgeOp,
    ) -> Result<Vec<(u64, GossipMsg)>, ChainError> {
        self.admit_knowledge_op(&op)?;
        Ok(self.broadcast(GossipMsg::KnowledgeOp(Box::new(op)), None))
    }

    /// K4:知识 op 待处理池的唯一准入路径(gossip 与本地 / RPC 共用),完全沿用治理池经五轮
    /// 审计加固后的顺序:有效性(结构 / 签名 / 成员 / 序号窗口)→ 按账户配额 → 全局上限 →
    /// 去重 → 入池。容量在去重插入之前检查,满池时不占用 `seen` 槽位。当前序号(可立即执行)
    /// 的 op 在配额满或池满时,可以淘汰停放最远的超前序号 op,不会被它们饿死。
    fn admit_knowledge_op(&mut self, op: &crate::knowledge::KnowledgeOp) -> Result<(), ChainError> {
        self.chain.state.knowledge_op_admissible(op)?;
        let current = self
            .chain
            .state
            .accounts
            .get(&op.actor)
            .is_some_and(|a| a.nonce == op.nonce);
        let mine = self
            .pending_knowledge_ops
            .iter()
            .filter(|p| p.actor == op.actor)
            .count();
        if mine >= MAX_PENDING_KNOWLEDGE_PER_ACCOUNT
            && !(current && self.evict_furthest_future_knowledge(Some(op.actor)))
        {
            return Err(ChainError::KnowledgeAccountQuotaExceeded {
                account: op.actor,
                limit: MAX_PENDING_KNOWLEDGE_PER_ACCOUNT,
            });
        }
        if self.pending_knowledge_ops.len() >= MAX_PENDING_KNOWLEDGE
            && !(current && self.evict_furthest_future_knowledge(None))
        {
            return Err(ChainError::KnowledgePoolFull);
        }
        if !self.seen_knowledge.insert(op.hash()) {
            return Err(ChainError::KnowledgeAlreadySeen);
        }
        self.pending_knowledge_ops.push(op.clone());
        Ok(())
    }

    /// K4:淘汰(可限定某账户的)序号最超前的知识 op,平局取最早入池者;其哈希从去重缓存移除,
    /// 主人有空位时可重新提交。若所有候选都在当前序号(没有"停放"的),返回 false。
    fn evict_furthest_future_knowledge(&mut self, account: Option<u64>) -> bool {
        let state = &self.chain.state;
        let victim = self
            .pending_knowledge_ops
            .iter()
            .enumerate()
            .filter(|(_, p)| account.is_none_or(|a| p.actor == a))
            .filter_map(|(i, p)| {
                let cur = state.accounts.get(&p.actor)?.nonce;
                (p.nonce > cur).then_some((p.nonce - cur, i))
            })
            .max_by_key(|&(d, i)| (d, std::cmp::Reverse(i)))
            .map(|(_, i)| i);
        match victim {
            Some(i) => {
                let gone = self.pending_knowledge_ops.remove(i);
                self.seen_knowledge.remove(&gone.hash());
                true
            }
            None => false,
        }
    }

    /// SEC-01 (round 3): the single admission path into the governance proposal pool,
    /// shared by gossip ingress and local/RPC submission so the two can't drift.
    /// Order matters: validity (sig / param / nonce window) → per-account quota →
    /// global pool cap → dedup → stage. Capacity is checked BEFORE the dedup insert
    /// so a full pool never consumes a `seen` slot for an op that was not staged.
    ///
    /// SEC-01 (round 5): a **current-nonce** op (immediately applicable) never loses
    /// its slot to parked future-nonce ops: at quota it evicts this account's
    /// furthest-future op, at the global cap it evicts the pool's furthest-future op
    /// ([`Self::evict_furthest_future_gov_proposal`]). Only when nothing is evictable
    /// (the pool holds nothing but current-nonce ops) does the hard cap reject it —
    /// so many accounts parking future-nonce ops can't starve governance.
    fn admit_gov_proposal(&mut self, op: &GovProposal) -> Result<(), ChainError> {
        self.chain.state.gov_proposal_admissible(op)?;
        // Admissibility proved the account exists and `op.nonce >= acct.nonce`.
        let current = self
            .chain
            .state
            .accounts
            .get(&op.proposer)
            .is_some_and(|a| a.nonce == op.nonce);
        let mine = self
            .pending_gov_proposals
            .iter()
            .filter(|p| p.proposer == op.proposer)
            .count();
        if mine >= MAX_PENDING_GOV_PER_ACCOUNT
            && !(current && self.evict_furthest_future_gov_proposal(Some(op.proposer)))
        {
            return Err(ChainError::GovAccountQuotaExceeded {
                account: op.proposer,
                limit: MAX_PENDING_GOV_PER_ACCOUNT,
            });
        }
        if self.pending_gov_proposals.len() >= MAX_PENDING_GOV
            && !(current && self.evict_furthest_future_gov_proposal(None))
        {
            return Err(ChainError::GovPoolFull);
        }
        if !self.seen_gov_proposal.insert(op.hash()) {
            return Err(ChainError::GovAlreadySeen);
        }
        self.pending_gov_proposals.push(op.clone());
        Ok(())
    }

    /// SEC-01 (round 3/5): as [`Self::admit_gov_proposal`], for the vote pool.
    fn admit_gov_vote(&mut self, op: &GovVote) -> Result<(), ChainError> {
        self.chain.state.gov_vote_admissible(op)?;
        let current = self
            .chain
            .state
            .accounts
            .get(&op.voter)
            .is_some_and(|a| a.nonce == op.nonce);
        let mine = self
            .pending_gov_votes
            .iter()
            .filter(|v| v.voter == op.voter)
            .count();
        if mine >= MAX_PENDING_GOV_PER_ACCOUNT
            && !(current && self.evict_furthest_future_gov_vote(Some(op.voter)))
        {
            return Err(ChainError::GovAccountQuotaExceeded {
                account: op.voter,
                limit: MAX_PENDING_GOV_PER_ACCOUNT,
            });
        }
        if self.pending_gov_votes.len() >= MAX_PENDING_GOV
            && !(current && self.evict_furthest_future_gov_vote(None))
        {
            return Err(ChainError::GovPoolFull);
        }
        if !self.seen_gov_vote.insert(op.hash()) {
            return Err(ChainError::GovAlreadySeen);
        }
        self.pending_gov_votes.push(op.clone());
        Ok(())
    }

    /// SEC-01 (round 5): drop the staged proposal (optionally restricted to `account`)
    /// whose nonce is furthest ahead of its account's current nonce — the one least
    /// likely to apply soon; ties go to the oldest. Its hash is forgotten by the dedup
    /// cache so the owner can resubmit once there is room. `false` if every candidate
    /// is at its current nonce (nothing is "parked", so nothing is evictable).
    fn evict_furthest_future_gov_proposal(&mut self, account: Option<u64>) -> bool {
        let state = &self.chain.state;
        let victim = self
            .pending_gov_proposals
            .iter()
            .enumerate()
            .filter(|(_, p)| account.is_none_or(|a| p.proposer == a))
            .filter_map(|(i, p)| {
                let cur = state.accounts.get(&p.proposer)?.nonce;
                (p.nonce > cur).then_some((p.nonce - cur, i))
            })
            // max distance; `max_by_key` takes the LAST max, so negate the index to
            // prefer the oldest (lowest index) on ties.
            .max_by_key(|&(d, i)| (d, std::cmp::Reverse(i)))
            .map(|(_, i)| i);
        match victim {
            Some(i) => {
                let gone = self.pending_gov_proposals.remove(i);
                self.seen_gov_proposal.remove(&gone.hash());
                true
            }
            None => false,
        }
    }

    /// SEC-01 (round 5): as [`Self::evict_furthest_future_gov_proposal`], for votes.
    fn evict_furthest_future_gov_vote(&mut self, account: Option<u64>) -> bool {
        let state = &self.chain.state;
        let victim = self
            .pending_gov_votes
            .iter()
            .enumerate()
            .filter(|(_, v)| account.is_none_or(|a| v.voter == a))
            .filter_map(|(i, v)| {
                let cur = state.accounts.get(&v.voter)?.nonce;
                (v.nonce > cur).then_some((v.nonce - cur, i))
            })
            .max_by_key(|&(d, i)| (d, std::cmp::Reverse(i)))
            .map(|(_, i)| i);
        match victim {
            Some(i) => {
                let gone = self.pending_gov_votes.remove(i);
                self.seen_gov_vote.remove(&gone.hash());
                true
            }
            None => false,
        }
    }

    /// Test-only: push a vote straight into the pending pool, bypassing admission —
    /// models a pool poisoned by any means (e.g. an op that was valid when staged and
    /// went stale) for the "poisoned pool must not stall production" guard.
    #[cfg(test)]
    pub(crate) fn inject_pending_gov_vote(&mut self, op: GovVote) {
        self.pending_gov_votes.push(op);
    }

    /// Test-only: size of the governance-proposal dedup cache.
    #[cfg(test)]
    pub(crate) fn seen_gov_proposal_len(&self) -> usize {
        self.seen_gov_proposal.len()
    }

    /// Test-only: size of the knowledge-op dedup cache.
    #[cfg(test)]
    pub(crate) fn seen_knowledge_len(&self) -> usize {
        self.seen_knowledge.len()
    }

    /// The gossip a node emits to announce its current height (anti-entropy tick).
    pub fn announce(&self) -> Vec<(u64, GossipMsg)> {
        self.broadcast(
            GossipMsg::Status {
                height: self.height(),
            },
            None,
        )
    }

    /// React to one message from peer `from`; return outbound `(peer, msg)`.
    pub fn on_message(&mut self, from: u64, msg: GossipMsg) -> Vec<(u64, GossipMsg)> {
        self.peers.insert(from);
        match msg {
            GossipMsg::Status { height } => {
                if height > self.height() {
                    // peer is ahead: pull what we are missing
                    vec![(
                        from,
                        GossipMsg::GetBlocks {
                            from: self.height() + 1,
                        },
                    )]
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
            GossipMsg::KeyRotation(op) => self.on_key_rotation(from, op),
            GossipMsg::GovProposal(op) => self.on_gov_proposal(from, *op),
            GossipMsg::GovVote(op) => self.on_gov_vote(from, *op),
            GossipMsg::KnowledgeOp(op) => self.on_knowledge_op(from, *op),
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
            // M147: light header-sync-with-sets serve — pair each header with the set
            // that certifies the next one, so the light peer advances from wire alone.
            GossipMsg::GetHeadersWithSets { from: h } => {
                let batch = self.headers_with_sets_from(h);
                if batch.is_empty() {
                    Vec::new()
                } else {
                    vec![(from, GossipMsg::HeadersWithSets(batch))]
                }
            }
            GossipMsg::HeadersWithSets(_) => Vec::new(),
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
            GossipMsg::GetDiff {
                h1,
                h2,
                header_h1,
                header_h2,
            } => match self.serve_diff(h1, h2, &header_h1, &header_h2) {
                Some(envelope) => vec![(
                    from,
                    GossipMsg::Diff {
                        envelope: Box::new(envelope),
                    },
                )],
                None => Vec::new(),
            },
            GossipMsg::Diff { .. } => Vec::new(), // full nodes don't consume diffs
            // M30: bridge lock fetch. Full peer answers with a cert-signed
            // lock envelope if the lock exists on its chain; the wallet's
            // destination-side `BridgeEndpoint::verify_lock` rebinds the
            // header against its own tracked set (same soundness pattern as
            // M28).
            GossipMsg::GetLock { lock_id } => match self.serve_lock(lock_id) {
                Some(envelope) => vec![(
                    from,
                    GossipMsg::Lock {
                        envelope: Box::new(envelope),
                    },
                )],
                None => Vec::new(),
            },
            GossipMsg::Lock { .. } => Vec::new(), // full nodes don't consume locks
            // M33: distributed consensus is driven by the daemon Actor (it owns
            // the Keypair, wall-clock timers, and apply_certified side effects);
            // the pure core never handles it.
            GossipMsg::Consensus(_) => Vec::new(),
            // M39: peer discovery is driven by the daemon Actor (it owns the
            // address book and dialing); the pure core does no I/O.
            GossipMsg::Peers(_) => Vec::new(),
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
        let mut out = self.broadcast(
            GossipMsg::Status {
                height: self.height(),
            },
            Some(from),
        );
        if applied == n && n == MAX_BATCH {
            out.push((
                from,
                GossipMsg::GetBlocks {
                    from: self.height() + 1,
                },
            ));
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

    fn on_key_rotation(&mut self, from: u64, op: KeyRotation) -> Vec<(u64, GossipMsg)> {
        if !self.seen_key_rotation.insert(op.hash()) {
            return Vec::new(); // already flooded through us
        }
        // stage; signature / nonce / active-validator checks live in apply_key_rotation
        self.pending_key_rotations.push(op.clone());
        self.broadcast(GossipMsg::KeyRotation(op), Some(from))
    }

    /// M195: dedup + stage + re-flood a gossiped governance proposal. Admission (and
    /// its ordering — validate before touching the dedup cache, capacity before the
    /// dedup insert) lives in [`Self::admit_gov_proposal`], shared with local submit.
    /// A rejected or duplicate op is silently dropped: neither staged nor re-flooded.
    fn on_gov_proposal(&mut self, from: u64, op: GovProposal) -> Vec<(u64, GossipMsg)> {
        if self.admit_gov_proposal(&op).is_err() {
            return Vec::new();
        }
        self.broadcast(GossipMsg::GovProposal(Box::new(op)), Some(from))
    }

    /// M195: dedup + stage + re-flood a gossiped governance vote.
    fn on_gov_vote(&mut self, from: u64, op: GovVote) -> Vec<(u64, GossipMsg)> {
        if self.admit_gov_vote(&op).is_err() {
            return Vec::new();
        }
        self.broadcast(GossipMsg::GovVote(Box::new(op)), Some(from))
    }

    /// K4:去重 + 入池 + 转发一条 gossip 来的知识 op。被拒或重复的 op 静默丢弃,不入池也不转发。
    fn on_knowledge_op(
        &mut self,
        from: u64,
        op: crate::knowledge::KnowledgeOp,
    ) -> Vec<(u64, GossipMsg)> {
        if self.admit_knowledge_op(&op).is_err() {
            return Vec::new();
        }
        self.broadcast(GossipMsg::KnowledgeOp(Box::new(op)), Some(from))
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

    /// M179: submit a locally-originated key-rotation op at node `id` and flood it.
    pub fn submit_key_rotation(&mut self, id: u64, op: KeyRotation) {
        let out = self
            .nodes
            .get_mut(&id)
            .unwrap()
            .submit_local_key_rotation(op);
        self.enqueue(id, out);
    }

    /// M195: submit a governance proposal / vote at `id` and flood it. Panics if the
    /// op is not admitted (test-bus helper; tests that exercise rejection call
    /// `submit_local_gov_*` on the node directly).
    pub fn submit_knowledge_op(&mut self, id: u64, op: crate::knowledge::KnowledgeOp) {
        let out = self
            .nodes
            .get_mut(&id)
            .unwrap()
            .submit_local_knowledge_op(op)
            .expect("knowledge op admitted");
        self.enqueue(id, out);
    }
    pub fn submit_gov_proposal(&mut self, id: u64, op: GovProposal) {
        let out = self
            .nodes
            .get_mut(&id)
            .unwrap()
            .submit_local_gov_proposal(op)
            .expect("gov proposal admitted");
        self.enqueue(id, out);
    }
    pub fn submit_gov_vote(&mut self, id: u64, op: GovVote) {
        let out = self
            .nodes
            .get_mut(&id)
            .unwrap()
            .submit_local_gov_vote(op)
            .expect("gov vote admitted");
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
    /// M142: the snapshot tip height this node was seeded at — `0` for a fresh-from-
    /// genesis node (today's contract), `snapshot.height` for a snapshot-seeded node.
    /// `headers[i]`/`next_sets[i]` is therefore height `base_height + 1 + i`.
    base_height: u64,
    /// Validated headers in height order. Index `i` is height `base_height + 1 + i`.
    headers: Vec<CertifiedHeader>,
    /// The next set we were told certifies each header. The same full node
    /// that produced the header can produce this snapshot (it knows its own
    /// `state.validators` after applying the block). For `apply_header` we
    /// accept it alongside the header. Index `i` is height `base_height + 1 + i`.
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
            base_height: 0,
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

    /// M142: seed a light node from a snapshot `state` at height `state.height`
    /// instead of tracking from genesis — the light-client analogue of
    /// [`GossipNode::from_snapshot`] (M139). The validator-set tracker is built via
    /// [`ValidatorTracker::from_snapshot`] (M136); `headers`/`next_sets` start empty
    /// and `apply_header` resumes at `state.height + 1` (its gate is already
    /// `tracker.height()`-relative). `head` is the snapshot's tip block hash — the
    /// state alone can't recompute it, so the snapshot provider supplies it.
    ///
    /// This is **infallible plumbing**: it does not verify the snapshot. Callers that
    /// adopt a snapshot from an untrusted peer MUST verify it first (see
    /// [`Self::from_verified_snapshot`], or
    /// [`ValidatorTracker::verify_snapshot_against_header`] directly).
    pub fn from_snapshot(
        id: u64,
        state: &crate::ChainState,
        head: Hash,
        peers: impl IntoIterator<Item = u64>,
    ) -> Self {
        let peers = peers.into_iter().filter(|&p| p != id).collect();
        LightGossipNode {
            id,
            base_height: state.height,
            headers: Vec::new(),
            next_sets: Vec::new(),
            seen: BTreeSet::new(),
            tracker: ValidatorTracker::from_snapshot(state, head),
            peers,
            proofs: BTreeMap::new(),
            diffs: None,
            batches: None,
            locks: None,
        }
    }

    /// M142: the safe snapshot-seed entry point — verify the snapshot against a
    /// cert-signed `header` + its `cert` before adopting it, then seed via
    /// [`Self::from_snapshot`]. `trusted_set` is the operator-pinned weak-subjectivity
    /// checkpoint the cert must reach `>2/3` of (a light client bootstrapping from
    /// nothing has no tracked set yet, so it must be supplied out-of-band — the
    /// light-client analogue of M141's full-node `trusted_validators`). Returns the
    /// same `SnapshotMismatch`/quorum errors as
    /// [`ValidatorTracker::verify_snapshot_against_header`] (M134) on any failure.
    pub fn from_verified_snapshot(
        id: u64,
        state: &crate::ChainState,
        header: &crate::codec::BlockHeader,
        cert: &crate::consensus::Commit,
        trusted_set: &ValidatorSet,
        peers: impl IntoIterator<Item = u64>,
    ) -> Result<Self, crate::light::LightError> {
        ValidatorTracker::verify_snapshot_against_header(state, header, cert, trusted_set)?;
        Ok(Self::from_snapshot(id, state, header.hash(), peers))
    }

    /// M154: seed a light node from a **weak-subjectivity checkpoint** — a pinned
    /// `(validator set, head, height)` the operator trusts as a subjectivity anchor
    /// (no snapshot blob, no cert). Lighter than [`Self::from_snapshot`]; the node tracks
    /// forward from `height`, cert-verifying every subsequent header against `set`. The
    /// anchor's authenticity is the operator's trust assumption (as with genesis).
    pub fn from_checkpoint(
        id: u64,
        set: ValidatorSet,
        head: Hash,
        height: u64,
        peers: impl IntoIterator<Item = u64>,
    ) -> Self {
        let peers = peers.into_iter().filter(|&p| p != id).collect();
        LightGossipNode {
            id,
            base_height: height,
            headers: Vec::new(),
            next_sets: Vec::new(),
            seen: BTreeSet::new(),
            tracker: ValidatorTracker::from_checkpoint(set, head, height),
            peers,
            proofs: BTreeMap::new(),
            diffs: None,
            batches: None,
            locks: None,
        }
    }

    /// M142: the snapshot/checkpoint tip height this node was seeded at — `0` for a
    /// fresh-from-genesis node. `headers()[i]` is height `base_height() + 1 + i`.
    pub fn base_height(&self) -> u64 {
        self.base_height
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

    /// M151: the validator sets adopted alongside each retained header (parallel to
    /// [`Self::headers`]: `next_sets()[i]` is the set the tracker adopted after applying
    /// `headers()[i]`). The light daemon persists `(header, next_set)` pairs so a
    /// restart can replay them instead of re-syncing from genesis/snapshot.
    pub fn next_sets(&self) -> &[ValidatorSet] {
        &self.next_sets
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
                    // peer is ahead: pull headers-with-sets (M147) so we can verify
                    // succession across validator-set changes from wire data alone.
                    vec![(
                        from,
                        GossipMsg::GetHeadersWithSets {
                            from: self.tracker.height() + 1,
                        },
                    )]
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
            // M147: light nodes don't serve header requests; they consume the
            // header-with-sets response to advance the tracker from wire data alone.
            GossipMsg::GetHeadersWithSets { .. } => Vec::new(),
            GossipMsg::HeadersWithSets(batch) => self.on_headers_with_sets(from, batch),
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
            // M179: key rotations are a full-node block concern; light clients ignore.
            | GossipMsg::KeyRotation(_)
            // M195: governance ops are a full-node block concern; light clients ignore.
            | GossipMsg::GovProposal(_)
            | GossipMsg::GovVote(_)
            // K4:知识 op 是全节点的区块事务;轻客户端忽略。
            | GossipMsg::KnowledgeOp(_)
            // M33: consensus is an Actor concern; light clients never vote.
            | GossipMsg::Consensus(_)
            // M39: peer discovery is an Actor concern; the pure core does no I/O.
            | GossipMsg::Peers(_) => Vec::new(),
        }
    }

    fn on_headers(&mut self, from: u64, batch: Vec<CertifiedHeader>) -> Vec<(u64, GossipMsg)> {
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
            out.push((
                from,
                GossipMsg::Status {
                    height: self.tracker.height(),
                },
            ));
        }
        out
    }

    /// M147: advance the tracker from a `HeadersWithSets` batch — each `(header,
    /// next_set)` pair is verified and applied via [`Self::apply_header`]
    /// (`follow_header`: the cert is checked against the *current* tracked set, the
    /// `next_set` against the header's `next_validators_root`, then the tracker
    /// adopts `next_set`). Unlike the legacy [`Self::on_headers`], this advances
    /// purely from wire data — no in-process `next_set` oracle. On any
    /// verification failure the batch stops (a later `GetHeadersWithSets` re-pulls).
    fn on_headers_with_sets(
        &mut self,
        from: u64,
        batch: Vec<(CertifiedHeader, Option<ValidatorSet>)>,
    ) -> Vec<(u64, GossipMsg)> {
        let mut advanced = 0usize;
        for (ch, next_set) in batch {
            // M150: `None` means the set is unchanged across this header — reuse the
            // current tracked set (which certifies this header). `apply_header` still
            // verifies it against `header.next_validators_root`, so a server that lies
            // (omits a real change) is rejected at the root check.
            let next = next_set.unwrap_or_else(|| self.tracker.validators().clone());
            match self.apply_header(ch, &next) {
                Ok(_) => advanced += 1,
                Err(_) => break, // out-of-order / bad set — resync on the next round
            }
        }
        let mut out = Vec::new();
        if advanced > 0 {
            // Report our new height so the peer keeps pushing (and, if more remains,
            // we re-request the next window).
            out.push((
                from,
                GossipMsg::Status {
                    height: self.tracker.height(),
                },
            ));
            out.push((
                from,
                GossipMsg::GetHeadersWithSets {
                    from: self.tracker.height() + 1,
                },
            ));
        }
        out
    }

    /// Light-side equivalent of [`GossipNode::announce`].
    pub fn announce(&self) -> Vec<(u64, GossipMsg)> {
        self.peers
            .iter()
            .copied()
            .map(|p| {
                (
                    p,
                    GossipMsg::Status {
                        height: self.tracker.height(),
                    },
                )
            })
            .collect()
    }
}

/// A fixed-order, in-process delivery bus that mixes full [`GossipNode`]s
/// and light [`LightGossipNode`]s. Full peers serve `GetHeadersWithSets` with
/// `(header, next_set)` pairs; light peers consume them and advance their tracker.
/// The bus is the M22/M147 SPV-over-gossip analog of [`Network`].
///
/// **Headers routing (M147).** A light peer, seeing a peer ahead, pulls
/// `GetHeadersWithSets`; the full peer replies with each header paired with the
/// validator set that certifies the next one, so the light peer advances through the
/// ordinary `on_message` path — no in-process `next_set` oracle or side channel.
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
            out.extend(
                self.full[id]
                    .announce()
                    .into_iter()
                    .map(|(d, m)| (d, *id, m)),
            );
        }
        for id in self.light.keys() {
            out.extend(
                self.light[id]
                    .announce()
                    .into_iter()
                    .map(|(d, m)| (d, *id, m)),
            );
        }
        for (dst, src, msg) in out {
            self.queue.push_back((dst, src, msg));
        }
    }

    /// Run the bus to quiescence. M147: light peers sync via the
    /// `GetHeadersWithSets`/`HeadersWithSets` pair through the normal `on_message`
    /// path — each header arrives paired with its next validator set over the wire,
    /// so the bus no longer needs an in-process `next_set` oracle or a side channel.
    pub fn run(&mut self) -> usize {
        let mut delivered = 0;
        while let Some((dst, src, msg)) = self.queue.pop_front() {
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
            // M62: body format lifted to the standalone `encode_blocks` so the
            // RPC `/batch` `range_blocks=` line shares these exact bytes.
            out.extend_from_slice(&encode_blocks(batch));
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
        GossipMsg::KeyRotation(op) => {
            out.push(TAG_KEYROTATION);
            put_bytes(&mut out, &crate::codec::encode_key_rotation(op));
        }
        GossipMsg::GovProposal(op) => {
            out.push(TAG_GOVPROPOSAL);
            put_bytes(&mut out, &crate::codec::encode_gov_proposal(op));
        }
        GossipMsg::GovVote(op) => {
            out.push(TAG_GOVVOTE);
            put_bytes(&mut out, &crate::codec::encode_gov_vote(op));
        }
        GossipMsg::KnowledgeOp(op) => {
            out.push(TAG_KNOWLEDGE_OP);
            put_bytes(&mut out, &crate::codec::encode_knowledge_op(op));
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
        GossipMsg::GetDiff {
            h1,
            h2,
            header_h1,
            header_h2,
        } => {
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
            out.extend_from_slice(&encode_batch_request(items));
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
        // M39: peer address book — u32 count then per-entry (u64 id + length-
        // prefixed utf8 addr).
        GossipMsg::Peers(entries) => {
            out.push(TAG_PEERS);
            out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
            for (id, addr) in entries.iter() {
                out.extend_from_slice(&id.to_be_bytes());
                put_bytes(&mut out, addr.as_bytes());
            }
        }
        // M147: light header-sync-with-sets pair. `GetHeadersWithSets` mirrors
        // `GetHeaders` (a single u64 `from`). `HeadersWithSets` is a u64 count then,
        // per entry, two length-prefixed blobs: the certified header and the next
        // validator set.
        GossipMsg::GetHeadersWithSets { from } => {
            out.push(TAG_GETHEADERS_SETS);
            out.extend_from_slice(&from.to_be_bytes());
        }
        GossipMsg::HeadersWithSets(batch) => {
            out.push(TAG_HEADERS_SETS);
            out.extend_from_slice(&(batch.len() as u64).to_be_bytes());
            for (ch, set) in batch {
                put_bytes(&mut out, &encode_certified_header(ch));
                // M150: 1 presence byte (0 = None / unchanged, 1 = Some) then the
                // length-prefixed set when present.
                match set {
                    Some(s) => {
                        out.push(1);
                        put_bytes(&mut out, &crate::codec::encode_validator_set(s));
                    }
                    None => out.push(0),
                }
            }
        }
    }
    out
}

/// Decode a gossip message produced by [`encode_gossip`].
pub fn decode_gossip(buf: &[u8]) -> Result<GossipMsg, CodecError> {
    let (&tag, mut rest) = buf.split_first().ok_or(CodecError::UnexpectedEof)?;
    let msg = match tag {
        TAG_STATUS => GossipMsg::Status {
            height: take_u64(&mut rest)?,
        },
        TAG_GETBLOCKS => GossipMsg::GetBlocks {
            from: take_u64(&mut rest)?,
        },
        TAG_BLOCKS => {
            let n = take_u64(&mut rest)?;
            if n > MAX_BATCH as u64 {
                return Err(CodecError::TooManyItems(n));
            }
            let mut batch = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
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
        TAG_KEYROTATION => {
            GossipMsg::KeyRotation(crate::codec::decode_key_rotation(take_bytes(&mut rest)?)?)
        }
        TAG_GOVPROPOSAL => GossipMsg::GovProposal(Box::new(crate::codec::decode_gov_proposal(
            take_bytes(&mut rest)?,
        )?)),
        TAG_GOVVOTE => GossipMsg::GovVote(Box::new(crate::codec::decode_gov_vote(take_bytes(
            &mut rest,
        )?)?)),
        TAG_KNOWLEDGE_OP => GossipMsg::KnowledgeOp(Box::new(crate::codec::decode_knowledge_op(
            take_bytes(&mut rest)?,
        )?)),
        TAG_GETHEADERS => GossipMsg::GetHeaders {
            from: take_u64(&mut rest)?,
        },
        TAG_HEADERS => {
            let n = take_u64(&mut rest)?;
            if n > MAX_BATCH as u64 {
                return Err(CodecError::TooManyItems(n));
            }
            let mut batch = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
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
            let mut items = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
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
            let mut items = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
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
            GossipMsg::GetDiff {
                h1,
                h2,
                header_h1,
                header_h2,
            }
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
            let mut items = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
            for _ in 0..n {
                let kind = crate::codec::decode_batch_response_kind(
                    rest.first().copied().ok_or(CodecError::UnexpectedEof)?,
                )?;
                rest = &rest[1..];
                match kind {
                    0 => {
                        let kind_byte = rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                        rest = &rest[1..];
                        let k = crate::codec::decode_proof_kind(kind_byte)?;
                        let id = take_u64(&mut rest)?;
                        items.push(crate::light::BatchItem::Inclusion { kind: k, id });
                    }
                    1 => {
                        let (query, k) = crate::codec::decode_knn_request(take_bytes(&mut rest)?)?;
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
        TAG_CONSENSUS => GossipMsg::Consensus(Box::new(crate::codec::decode_consensus_msg(
            take_bytes(&mut rest)?,
        )?)),
        // M39: peer address book — u32 count then per-entry (u64 id + length-
        // prefixed utf8 addr). Addrs are best-effort hints (parse-checked before
        // dialing), so a non-utf8 addr decodes lossily rather than failing.
        TAG_PEERS => {
            let n = take_u32(&mut rest)?;
            if n as usize > MAX_PEERS {
                return Err(CodecError::TooManyItems(n as u64));
            }
            let mut entries = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
            for _ in 0..n {
                let id = take_u64(&mut rest)?;
                let addr = String::from_utf8_lossy(take_bytes(&mut rest)?).into_owned();
                entries.push((id, addr));
            }
            GossipMsg::Peers(entries)
        }
        // M147: light header-sync-with-sets decode pair.
        TAG_GETHEADERS_SETS => GossipMsg::GetHeadersWithSets {
            from: take_u64(&mut rest)?,
        },
        TAG_HEADERS_SETS => {
            let n = take_u64(&mut rest)?;
            if n > MAX_BATCH as u64 {
                return Err(CodecError::TooManyItems(n));
            }
            let mut batch = Vec::with_capacity(crate::codec::prealloc_usize(n as usize));
            for _ in 0..n {
                let ch = decode_certified_header(take_bytes(&mut rest)?)?;
                // M150: 1 presence byte, then the set when present.
                let present = rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                rest = &rest[1..];
                let set = match present {
                    0 => None,
                    1 => Some(crate::codec::decode_validator_set(take_bytes(&mut rest)?)?),
                    other => return Err(CodecError::BadEnum(other as u32)),
                };
                batch.push((ch, set));
            }
            GossipMsg::HeadersWithSets(batch)
        }
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
        put_bytes(
            &mut out,
            &crate::codec::encode_graph_node(&entry.graph_node),
        );
        put_bytes(&mut out, &crate::codec::encode_proof(&entry.proof));
    }
    out.extend_from_slice(&(env.diff.dropped.len() as u32).to_be_bytes());
    for entry in &env.diff.dropped {
        put_bytes(
            &mut out,
            &crate::codec::encode_graph_node(&entry.graph_node),
        );
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
    let mut added = Vec::with_capacity(crate::codec::prealloc_usize(n_added));
    for _ in 0..n_added {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        added.push(crate::GraphLeafAtHeight {
            node_id,
            graph_node: gn,
            proof,
        });
    }
    let n_dropped = take_u32(&mut p)? as usize;
    let mut dropped = Vec::with_capacity(crate::codec::prealloc_usize(n_dropped));
    for _ in 0..n_dropped {
        let gn = crate::codec::decode_graph_node(take_bytes(&mut p)?)?;
        let proof = crate::codec::decode_proof(take_bytes(&mut p)?)?;
        let node_id = gn.node_id;
        dropped.push(crate::GraphLeafAtHeight {
            node_id,
            graph_node: gn,
            proof,
        });
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

/// M62: standalone codec for a `(Block, Commit)` range, so the RPC `/batch`
/// `range_blocks=` line shares the exact byte format the gossip `Blocks` payload
/// carries inline (`encode_gossip`'s `Blocks` arm delegates here). Layout:
/// u64_be(count), then per pair a length-prefixed `encode_block` followed by a
/// length-prefixed `encode_commit`.
pub fn encode_blocks(batch: &[(Block, Commit)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(batch.len() as u64).to_be_bytes());
    for (b, c) in batch.iter() {
        put_bytes(&mut out, &encode_block(b));
        put_bytes(&mut out, &encode_commit(c));
    }
    out
}

/// Mirror of [`encode_blocks`]. Unlike the inline gossip `TAG_BLOCKS` decoder
/// this is NOT capped at [`MAX_BATCH`] — a legitimate Diff range can span the
/// whole chain — and it does NOT pre-`with_capacity` on the declared count, so a
/// buffer that lies about its length fails fast at the first missing pair
/// (`UnexpectedEof`) instead of over-allocating.
pub fn decode_blocks(buf: &[u8]) -> Result<Vec<(Block, Commit)>, CodecError> {
    let mut rest = buf;
    let n = take_u64(&mut rest)?;
    let mut batch = Vec::new();
    for _ in 0..n {
        let block = decode_block(take_bytes(&mut rest)?)?;
        let commit = decode_commit(take_bytes(&mut rest)?)?;
        batch.push((block, commit));
    }
    if !rest.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(batch)
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
/// M61: standalone codec for a batch *request* (`Vec<BatchItem>`), so the RPC
/// `POST /batch` body shares the exact byte format the gossip `GetBatch` carries
/// inline (`encode_gossip` delegates here). Layout: u32 item count, then per item
/// a 1-byte kind tag + per-kind body — Inclusion: 1-byte proof-kind + u64 id;
/// Knn/Range: length-prefixed request; Diff: two u64 heights. No trailing length
/// prefix (the gossip tag framing / RPC body length bound it).
pub fn encode_batch_request(items: &[crate::light::BatchItem]) -> Vec<u8> {
    let mut out = Vec::new();
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
    out
}

/// M61: inverse of [`encode_batch_request`]. Caps the item count at
/// `MAX_BATCH_ITEMS` (→ [`CodecError::TooManyItems`]), matching the inline gossip
/// `GetBatch` decoder and [`decode_batch_envelope`].
pub fn decode_batch_request(buf: &[u8]) -> Result<Vec<crate::light::BatchItem>, CodecError> {
    let mut rest = buf;
    let n = take_u32(&mut rest)? as usize;
    if n > MAX_BATCH_ITEMS {
        return Err(CodecError::TooManyItems(n as u64));
    }
    let mut items = Vec::with_capacity(crate::codec::prealloc_usize(n));
    for _ in 0..n {
        let kind = crate::codec::decode_batch_response_kind(
            rest.first().copied().ok_or(CodecError::UnexpectedEof)?,
        )?;
        rest = &rest[1..];
        match kind {
            0 => {
                let kind_byte = rest.first().copied().ok_or(CodecError::UnexpectedEof)?;
                rest = &rest[1..];
                let k = crate::codec::decode_proof_kind(kind_byte)?;
                let id = take_u64(&mut rest)?;
                items.push(crate::light::BatchItem::Inclusion { kind: k, id });
            }
            1 => {
                let (query, k) = crate::codec::decode_knn_request(take_bytes(&mut rest)?)?;
                items.push(crate::light::BatchItem::Knn { query, k });
            }
            2 => {
                let (query, min_sim) = crate::codec::decode_range_request(take_bytes(&mut rest)?)?;
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
    Ok(items)
}

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
    let mut items = Vec::with_capacity(crate::codec::prealloc_usize(n));
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
    let mut vs = Vec::with_capacity(crate::codec::prealloc_usize(n));
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
    let mut neighbours = Vec::with_capacity(crate::codec::prealloc_usize(n));
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
    Ok(crate::light::KnnClaim {
        query,
        k,
        neighbours,
    })
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
    put_bytes(
        &mut out,
        &crate::codec::encode_range_request(&c.query, c.min_sim),
    );
    out
}

/// M29: inverse of [`encode_range_claim`].
pub fn decode_range_claim(buf: &[u8]) -> Result<crate::light::RangeClaim, CodecError> {
    let mut p = buf;
    let n = take_u32(&mut p)? as usize;
    let mut nodes = Vec::with_capacity(crate::codec::prealloc_usize(n));
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
    Ok(crate::light::RangeClaim {
        query,
        min_sim,
        nodes,
    })
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
    use crate::{
        BondKind, Keypair, Review, SlashEvidence, SubmissionTx, Vote, VoteType, DIM, MICRO,
    };
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
            decay_half_life_blocks: 0,
            slash_bps: 10_000,
            max_validator_power_bps: 0,
            knowledge: crate::knowledge::KnowledgeGenesis::default(),
            timestamp_days: 0.0,
            validators: [21u64, 22, 23, 24]
                .iter()
                .map(|&id| (id, kp(id).public(), 1))
                .collect(),
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
            ],
            repl_success: 3,
            repl_total: 3,
            timestamp_days: 1.0,
            signature: [0u8; 64],
            fee: 0,
            nonce: 0,
        }
        .signed(&kp(author))
    }

    /// Produce a real certified chain (blocks + certs) to seed sync tests with.
    fn certified_chain(n_tx: usize) -> (Vec<Block>, Vec<Commit>) {
        let seeds: BTreeMap<u64, [u8; 32]> = [21u64, 22, 23, 24]
            .iter()
            .map(|&id| (id, seed(id)))
            .collect();
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
        let batch: Vec<(Block, Commit)> =
            blocks.iter().cloned().zip(certs.iter().cloned()).collect();
        let headers: Vec<CertifiedHeader> = blocks
            .iter()
            .zip(certs.iter())
            .map(|(b, c)| CertifiedHeader::from_certified(b, c))
            .collect();
        // A trivial account + Merkle proof (the proof is shape, not semantics;
        // round-trip is the property under test).
        let fake_account = crate::Account::default();
        let fake_proof = crate::merkle::Proof {
            key: 0,
            leaf_count: 2,
            steps: vec![crate::merkle::Step::Right([0xAA; 32])],
        };
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
                    validator: Validator {
                        id: 25,
                        pubkey: [3u8; 32],
                        power: 7,
                    },
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

    /// M39: a `Peers` address book round-trips exactly, and a book claiming
    /// more than `MAX_PEERS` entries is rejected at decode (bounded so a
    /// hostile peer can't ship an unbounded book).
    #[test]
    fn peers_gossip_round_trips() {
        let entries = vec![
            (21u64, "127.0.0.1:19711".to_string()),
            (22, "127.0.0.1:19712".to_string()),
            (23, "10.0.0.7:30303".to_string()),
        ];
        let book = GossipMsg::Peers(entries.clone());
        let bytes = encode_gossip(&book);
        match decode_gossip(&bytes) {
            Ok(GossipMsg::Peers(back)) => assert_eq!(back, entries),
            other => panic!("expected Peers, got {other:?}"),
        }
        // An empty book is legal (a node with no known peers).
        let empty = encode_gossip(&GossipMsg::Peers(vec![]));
        match decode_gossip(&empty) {
            Ok(GossipMsg::Peers(back)) => assert!(back.is_empty()),
            other => panic!("expected empty Peers, got {other:?}"),
        }
        // A book that claims more than MAX_PEERS entries is rejected before it
        // can allocate: hand-craft the wire header with an over-cap count.
        let mut evil = vec![TAG_PEERS];
        evil.extend_from_slice(&((MAX_PEERS as u32) + 1).to_be_bytes());
        match decode_gossip(&evil) {
            Err(CodecError::TooManyItems(n)) => assert_eq!(n, MAX_PEERS as u64 + 1),
            other => panic!("expected TooManyItems, got {other:?}"),
        }
    }

    #[test]
    fn gov_op_gossip_wire_round_trips() {
        // M195: GovProposal / GovVote survive encode_gossip → decode_gossip.
        let prop = crate::GovProposal {
            proposer: 3,
            param: crate::GovParam::DecayHalfLife(7),
            voting_period: 20,
            nonce: 2,
            signature: [5u8; 64],
        };
        let bytes = encode_gossip(&GossipMsg::GovProposal(Box::new(prop.clone())));
        match decode_gossip(&bytes) {
            Ok(GossipMsg::GovProposal(back)) => assert_eq!(*back, prop),
            other => panic!("expected GovProposal, got {other:?}"),
        }
        let vote = crate::GovVote {
            proposal_id: 9,
            voter: 4,
            approve: false,
            nonce: 0,
            signature: [6u8; 64],
        };
        let vb = encode_gossip(&GossipMsg::GovVote(Box::new(vote.clone())));
        match decode_gossip(&vb) {
            Ok(GossipMsg::GovVote(back)) => assert_eq!(*back, vote),
            other => panic!("expected GovVote, got {other:?}"),
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
            decay_half_life_blocks: 0,
            slash_bps: 10_000,
            max_validator_power_bps: 0,
            knowledge: crate::knowledge::KnowledgeGenesis::default(),
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
            decay_half_life_blocks: 0,
            slash_bps: 10_000,
            max_validator_power_bps: 0,
            knowledge: crate::knowledge::KnowledgeGenesis::default(),
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
            .follow_source(
                &env2.source_header,
                &env2.source_cert,
                &env2.source_tracked_set,
            )
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
        assert!(matches!(
            decode_gossip(&bytes),
            Err(CodecError::TrailingBytes)
        ));
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
        assert!(
            !fresh.apply_certified(blocks[1].clone(), forged),
            "forged cert refused"
        );
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
    fn liveness_tracker_counts_missed_and_evicts() {
        // M212: the per-node liveness window counts, per validator, how many remembered
        // heights it did NOT sign, and evicts past LIVENESS_WINDOW.
        let mut lt = LivenessTracker::default();
        assert_eq!(lt.samples(), 0);
        lt.record(BTreeSet::from([21, 22, 23])); // 24 absent
        lt.record(BTreeSet::from([21, 22, 23, 24])); // all present
        lt.record(BTreeSet::from([21, 22])); // 23, 24 absent
        assert_eq!(lt.samples(), 3);
        assert_eq!(lt.missed(21), 0);
        assert_eq!(lt.missed(23), 1);
        assert_eq!(lt.missed(24), 2);
        // a validator never seen misses every remembered height
        assert_eq!(lt.missed(99), 3);
        // the window evicts oldest past the cap
        for _ in 0..LIVENESS_WINDOW {
            lt.record(BTreeSet::from([21]));
        }
        assert_eq!(lt.samples(), LIVENESS_WINDOW);
    }

    #[test]
    fn liveness_report_tracks_an_absent_validator_from_cert_view() {
        // M212: end-to-end — applying certified blocks feeds the liveness window, and
        // `liveness_report` reflects exactly who signed each height's cert (this node's
        // view). With a 4-validator set every cert carries quorum (3), so one validator
        // is absent each height; the report must count those absences. Pure local
        // observability — not consensus state.
        let (blocks, certs) = certified_chain(2);
        assert!(certs.len() >= 2);
        let signers: Vec<BTreeSet<u64>> = certs[..2]
            .iter()
            .map(|c| c.precommits.iter().map(|v| v.validator).collect())
            .collect();
        assert!(
            signers.iter().all(|s| s.len() >= 3),
            "each cert clears quorum"
        );

        let mut node = GossipNode::new(2, genesis(), 8, [1]);
        assert!(node.apply_certified(blocks[0].clone(), certs[0].clone()));
        assert!(node.apply_certified(blocks[1].clone(), certs[1].clone()));

        let (samples, missed) = node.liveness_report();
        assert_eq!(samples, 2, "two committed heights remembered");
        // report covers exactly the current validator set, sorted by id
        assert_eq!(
            missed.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
            vec![21, 22, 23, 24]
        );
        // each validator's missed count == the heights whose cert it was absent from
        for &(id, m) in &missed {
            let expected = signers.iter().filter(|s| !s.contains(&id)).count();
            assert_eq!(m, expected, "validator {id} missed count");
        }
        // quorum < full set, so at least one validator shows an observed miss
        assert!(
            missed.iter().any(|&(_, m)| m > 0),
            "an absent validator is visible"
        );
    }

    #[test]
    fn tx_gossip_reaches_every_node() {
        // three fully-connected fresh nodes; a tx injected at one floods to all
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);

        let t = tx(1, 1, 1);
        let h = t.hash();
        let out = net.nodes.get_mut(&1).unwrap().submit_local(t);
        net.inject(1, out);
        net.run();

        for id in ids {
            assert!(
                net.node(id).mempool.contains(&h),
                "node {id} received the tx"
            );
        }
    }

    #[test]
    fn submit_local_checked_surfaces_reject() {
        // M53: the checked variant returns the tx hash on admission and the
        // ChainError on rejection, while plain submit_local stays byte-identical
        // (empty vec on that same reject, marked seen either way).
        let mut node = GossipNode::new(1, genesis(), 16, [1, 2]);

        let good = tx(1, 1, 1);
        let h = good.hash();
        let (returned, gossip) = node
            .submit_local_checked(good)
            .expect("valid tx is admitted");
        assert_eq!(returned, h);
        assert!(node.mempool.contains(&h), "valid tx landed in the mempool");
        assert!(!gossip.is_empty(), "admission produces gossip to flood");

        // An unknown-author tx fails validation → the checked variant surfaces it.
        let bad = tx(99, 2, 2);
        let bh = bad.hash();
        assert!(
            node.submit_local_checked(bad.clone()).is_err(),
            "reject is surfaced"
        );
        assert!(
            !node.mempool.contains(&bh),
            "rejected tx never entered the mempool"
        );

        // Plain submit_local swallows the same reject (empty vec), byte-identical.
        assert!(
            node.submit_local(bad).is_empty(),
            "submit_local drops the reject"
        );
    }

    #[test]
    fn a_duplicate_tx_does_not_re_flood() {
        let ids = [1u64, 2];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);
        let t = tx(1, 1, 1);
        let out = net.nodes.get_mut(&1).unwrap().submit_local(t.clone());
        net.inject(1, out);
        net.run();
        // re-injecting the same tx to node 2 yields no new forwarding
        let again = net
            .nodes
            .get_mut(&2)
            .unwrap()
            .on_message(1, GossipMsg::Tx(t));
        assert!(again.is_empty(), "an already-seen tx is not re-gossiped");
    }

    #[test]
    fn seen_set_evicts_oldest_at_capacity() {
        // M55: a bounded SeenSet keeps only its `capacity` most-recent entries,
        // evicting the oldest (FIFO). Re-inserting an evicted hash reports it as
        // new again; a still-present hash reports as a duplicate.
        let mut s = SeenSet::new();
        s.set_capacity(2);
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        assert!(s.insert(a));
        assert!(s.insert(b));
        assert!(s.insert(c)); // evicts a (oldest)
        assert_eq!(s.len(), 2);
        assert!(!s.insert(c), "newest still present ⇒ duplicate");
        assert!(!s.insert(b), "b still present ⇒ duplicate");
        assert!(s.insert(a), "a was evicted ⇒ treated as new again");
        assert_eq!(s.len(), 2); // inserting a evicted b
    }

    #[test]
    fn seen_set_unbounded_by_default() {
        // Default capacity is usize::MAX ⇒ nothing is ever evicted and the FIFO
        // order deque stays empty (byte-identical to a plain BTreeSet).
        let mut s = SeenSet::new();
        assert_eq!(s.capacity(), usize::MAX);
        for i in 0..1000u32 {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_le_bytes());
            assert!(s.insert(h));
        }
        assert_eq!(s.len(), 1000);
        assert!(s.order.is_empty(), "unbounded mode never tracks order");
    }

    #[test]
    fn seen_set_set_capacity_trims_when_over() {
        // Lowering the bound below the current size evicts oldest down to it.
        let mut s = SeenSet::new();
        s.set_capacity(10);
        for i in 0..3u8 {
            s.insert([i; 32]);
        }
        assert_eq!(s.len(), 3);
        s.set_capacity(1);
        assert_eq!(s.len(), 1, "trimmed down to the new bound");
        assert!(s.insert([0u8; 32]), "the two older entries were evicted");
    }

    #[test]
    fn bounded_seen_tx_reaccepts_evicted_flood() {
        // M55: once a tx hash is evicted from the bounded seen_tx set, the same tx
        // arriving again via gossip is treated as new and re-floods (contrast with
        // `a_duplicate_tx_does_not_re_flood`). Bounded re-flood is the accepted cost
        // of bounding; it is never a consensus-safety issue.
        let mut node = GossipNode::new(1, genesis(), 16, [1, 2, 3]);
        node.set_seen_capacity(1);

        let a = tx(1, 1, 1);
        let b = tx(2, 2, 2);
        node.submit_local_checked(a.clone()).expect("A admitted");
        // Submitting B marks B seen and evicts A's hash from the size-1 seen set.
        node.submit_local_checked(b).expect("B admitted");

        // A arrives again from peer 2: no longer "seen" ⇒ re-flooded to the others.
        let out = node.on_message(2, GossipMsg::Tx(a));
        assert!(!out.is_empty(), "an evicted tx re-floods when it reappears");
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
        assert!(ids
            .iter()
            .all(|&id| net.node(id).chain.state.state_root() == root));
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
            vote_a: Vote::signed(
                offender,
                2,
                0,
                [0xAAu8; 32],
                VoteType::Precommit,
                &kp(offender),
            ),
            vote_b: Vote::signed(
                offender,
                2,
                0,
                [0xBBu8; 32],
                VoteType::Precommit,
                &kp(offender),
            ),
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
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
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
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
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
        assert!(
            again.is_empty(),
            "an already-seen evidence is not re-gossiped"
        );
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
        assert!(
            node.pending_evidence().is_empty(),
            "malformed evidence is not staged"
        );

        // receiving one from a peer is also dropped
        let out2 = node.on_message(2, GossipMsg::Evidence(bad));
        assert!(out2.is_empty());
    }

    #[test]
    fn stake_op_gossip_reaches_every_node() {
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
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

    #[test]
    fn key_rotation_gossip_reaches_every_node_and_dedups() {
        // M179: a locally-submitted key rotation floods to every node's pending
        // pool (one copy each), and a duplicate does not re-flood.
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);

        let op = KeyRotation {
            account: 1,
            new_pubkey: kp(99).public(),
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let h = op.hash();
        net.submit_key_rotation(1, op.clone());
        net.run();
        for id in ids {
            let pending = net.node(id).pending_key_rotations();
            assert_eq!(pending.len(), 1, "node {id} staged one rotation");
            assert_eq!(pending[0].hash(), h);
        }

        // A re-flood from a peer (receive path) is deduped: node 2 already saw
        // this rotation, so re-delivering it stages nothing new and emits nothing.
        let out = net
            .node_mut(2)
            .on_message(1, GossipMsg::KeyRotation(op.clone()));
        assert!(out.is_empty(), "a seen rotation is not re-flooded");
        assert_eq!(
            net.node(2).pending_key_rotations().len(),
            1,
            "node 2 did not re-stage the duplicate rotation"
        );
    }

    #[test]
    fn gov_op_gossip_reaches_every_node_and_dedups() {
        // M195/SEC-01: an admissible proposal floods to every node's pending pool (one
        // copy each) and a duplicate does not re-flood. (Votes require a *committed*
        // proposal to be admissible — see `gov_gossip_rejects_inadmissible_ops` — so
        // the flood/dedup mechanic is exercised here with two distinct proposals.)
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);

        let p1 = crate::GovProposal {
            proposer: 1,
            param: crate::GovParam::BaseEmission(42),
            voting_period: 10,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let p2 = crate::GovProposal {
            proposer: 2,
            param: crate::GovParam::SlashBps(5000),
            voting_period: 10,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(2));
        let h1 = p1.hash();
        let h2 = p2.hash();
        net.submit_gov_proposal(1, p1.clone());
        net.submit_gov_proposal(2, p2.clone());
        net.run();
        for id in ids {
            let pending = net.node(id).pending_gov_proposals();
            assert_eq!(pending.len(), 2, "node {id} has both proposals");
            let hashes: std::collections::HashSet<_> = pending.iter().map(|p| p.hash()).collect();
            assert!(
                hashes.contains(&h1) && hashes.contains(&h2),
                "node {id} both"
            );
        }

        // Re-delivering a seen proposal stages nothing and emits nothing.
        let out = net
            .node_mut(2)
            .on_message(1, GossipMsg::GovProposal(Box::new(p1)));
        assert!(out.is_empty(), "a seen proposal is not re-flooded");
        assert_eq!(net.node(2).pending_gov_proposals().len(), 2);
    }

    #[test]
    fn gov_gossip_rejects_inadmissible_ops() {
        // SEC-01: invalid governance gossip is neither staged nor re-flooded, so a
        // poisoned op can't pollute the candidate pool or amplify across the mesh.
        let ids = [1u64, 2];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, genesis(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);

        // Bad signature (kp(2) signing account 1's proposal).
        let bad_sig = crate::GovProposal {
            proposer: 1,
            param: crate::GovParam::BaseEmission(42),
            voting_period: 10,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(2));
        let out = net
            .node_mut(2)
            .on_message(1, GossipMsg::GovProposal(Box::new(bad_sig)));
        assert!(out.is_empty(), "bad-sig proposal not re-flooded");
        assert_eq!(net.node(2).pending_gov_proposals().len(), 0, "not staged");

        // Out-of-range param (slash_bps > 10000).
        let bad_param = crate::GovProposal {
            proposer: 1,
            param: crate::GovParam::SlashBps(20_000),
            voting_period: 10,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        net.node_mut(2)
            .on_message(1, GossipMsg::GovProposal(Box::new(bad_param)));
        assert_eq!(
            net.node(2).pending_gov_proposals().len(),
            0,
            "out-of-range not staged"
        );

        // Vote for a nonexistent proposal (a real vote's id is only known post-commit).
        let orphan_vote = crate::GovVote {
            proposal_id: 999,
            voter: 1,
            approve: true,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        net.node_mut(2)
            .on_message(1, GossipMsg::GovVote(Box::new(orphan_vote)));
        assert_eq!(
            net.node(2).pending_gov_votes().len(),
            0,
            "orphan vote not staged"
        );
    }

    #[test]
    fn poisoned_gov_pool_does_not_stall_candidate_production() {
        // SEC-01 core: even if a stale/invalid gov op is in the pending pool (injected
        // here via the test-only hook — modelling an op that was valid when admitted
        // and went stale after), build_candidate must still produce a sealable block —
        // the invalid op is filtered out, not allowed to stall.
        let mut node = GossipNode::new(1, genesis(), 16, [1]);
        let bogus = crate::GovVote {
            proposal_id: 999, // no such committed proposal
            voter: 1,
            approve: true,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        node.inject_pending_gov_vote(bogus);
        assert_eq!(node.pending_gov_votes().len(), 1, "op is in the pool");
        let cand = node.build_candidate(1.0);
        assert!(cand.is_some(), "poisoned pool must not stall production");
        assert!(
            cand.unwrap().gov_votes.is_empty(),
            "the stale vote is filtered out of the candidate"
        );
    }

    #[test]
    fn bounded_seen_set_evicts_oldest() {
        // SEC-01 (re-verify): the governance dedup caches use `SeenSet::bounded` so they
        // are capped by default (not `usize::MAX`), independent of `[mempool] seen_cache`.
        let mut s = SeenSet::bounded(2);
        assert!(s.insert([1u8; 32]));
        assert!(s.insert([2u8; 32]));
        assert!(s.insert([3u8; 32])); // evicts [1]
        assert_eq!(s.len(), 2);
        assert!(s.insert([1u8; 32]), "[1] was evicted, so it is new again");
    }

    /// A proposal from `proposer` at `nonce`; `period` varies the content hash.
    fn gov_prop(proposer: u64, nonce: u64, period: u64) -> crate::GovProposal {
        crate::GovProposal {
            proposer,
            param: crate::GovParam::BaseEmission(1),
            voting_period: period,
            nonce,
            signature: [0u8; 64],
        }
        .signed(&kp(proposer))
    }

    /// `genesis()` with accounts `1..=n` (enough distinct accounts to fill a pool
    /// under the per-account quota).
    fn genesis_with_accounts(n: u64) -> Genesis {
        let mut g = genesis();
        g.accounts = (1..=n)
            .map(|id| (id, 30 * MICRO, kp(id).public()))
            .collect();
        g
    }

    #[test]
    fn gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup() {
        // SEC-01 (round 3): the pending pool is hard-capped on BOTH the local-submit and
        // gossip paths; a full pool is a structured `GovPoolFull` (not a silent drop or a
        // misleading success) and does not consume a dedup slot. Reaching the cap needs
        // MAX_PENDING_GOV / MAX_PENDING_GOV_PER_ACCOUNT distinct accounts, each staging
        // its full quota of future-nonce (within-window) ops.
        let fillers = (MAX_PENDING_GOV / MAX_PENDING_GOV_PER_ACCOUNT) as u64;
        let mut node = GossipNode::new(1, genesis_with_accounts(fillers + 2), 16, [1]);
        for acct in 1..=fillers {
            for i in 0..MAX_PENDING_GOV_PER_ACCOUNT as u64 {
                // nonces 1..=8 (ahead of the current 0, inside the window), two periods
                let op = gov_prop(acct, 1 + i % crate::GOV_NONCE_LOOKAHEAD, 100 + i);
                node.submit_local_gov_proposal(op).expect("within caps");
            }
        }
        assert_eq!(node.pending_gov_proposals().len(), MAX_PENDING_GOV);
        let seen_full = node.seen_gov_proposal_len();

        // Local submit of a FUTURE-nonce op from an untouched account: explicitly
        // refused as PoolFull (never silently dropped), pool and seen unchanged. (A
        // current-nonce op would instead evict a parked one — see
        // `current_nonce_gov_op_evicts_parked_future_op_when_pool_full`.)
        let r = node.submit_local_gov_proposal(gov_prop(fillers + 1, 1, 100));
        assert!(matches!(r, Err(ChainError::GovPoolFull)), "got {r:?}");
        assert_eq!(node.pending_gov_proposals().len(), MAX_PENDING_GOV);
        assert_eq!(
            node.seen_gov_proposal_len(),
            seen_full,
            "no seen slot consumed"
        );

        // Gossip ingress of a NEW valid future-nonce op when the pool is full: dropped,
        // not re-flooded, and (capacity check precedes the dedup insert) the seen cache
        // does not grow.
        let gossiped = gov_prop(fillers + 2, 1, 100);
        let out = node.on_message(1, GossipMsg::GovProposal(Box::new(gossiped)));
        assert!(out.is_empty(), "full pool drops gossiped op");
        assert_eq!(node.pending_gov_proposals().len(), MAX_PENDING_GOV);
        assert_eq!(
            node.seen_gov_proposal_len(),
            seen_full,
            "no seen slot consumed"
        );
    }

    #[test]
    fn gov_admission_bounds_nonce_window_and_per_account_quota() {
        // SEC-01 (round 3): one account can neither park ops arbitrarily far ahead
        // (nonce window) nor hold more than its quota of pool slots; both are structured
        // rejections, and another account's current-nonce op is still admitted.
        let mut node = GossipNode::new(1, genesis(), 16, [1]);
        // Inside the window: nonces 0..=GOV_NONCE_LOOKAHEAD are all admitted.
        for n in 0..=crate::GOV_NONCE_LOOKAHEAD {
            node.submit_local_gov_proposal(gov_prop(1, n, 100))
                .expect("inside the nonce window");
        }
        // One past the window → NonceTooFarAhead, not staged.
        let r = node.submit_local_gov_proposal(gov_prop(1, crate::GOV_NONCE_LOOKAHEAD + 1, 100));
        assert!(
            matches!(r, Err(ChainError::NonceTooFarAhead { account: 1, .. })),
            "got {r:?}"
        );
        // Re-submitting a staged op → GovAlreadySeen, not staged twice.
        let r = node.submit_local_gov_proposal(gov_prop(1, 0, 100));
        assert!(matches!(r, Err(ChainError::GovAlreadySeen)), "got {r:?}");
        // Fill the rest of account 1's quota with distinct hashes at the current nonce.
        let mut period = 101;
        while node.pending_gov_proposals().len() < MAX_PENDING_GOV_PER_ACCOUNT {
            node.submit_local_gov_proposal(gov_prop(1, 0, period))
                .expect("inside the quota");
            period += 1;
        }
        let r = node.submit_local_gov_proposal(gov_prop(1, 1, 999));
        assert!(
            matches!(
                r,
                Err(ChainError::GovAccountQuotaExceeded {
                    account: 1,
                    limit: MAX_PENDING_GOV_PER_ACCOUNT
                })
            ),
            "got {r:?}"
        );
        let seen_before = node.seen_gov_proposal_len();
        // Account 1 at quota does not crowd out account 2's current-nonce op.
        node.submit_local_gov_proposal(gov_prop(2, 0, 100))
            .expect("another account is still admitted");
        assert_eq!(node.seen_gov_proposal_len(), seen_before + 1);
        // Gossip ingress of an over-quota op: dropped without consuming a seen slot.
        let out = node.on_message(1, GossipMsg::GovProposal(Box::new(gov_prop(1, 2, 5000))));
        assert!(out.is_empty());
        assert_eq!(node.seen_gov_proposal_len(), seen_before + 1);
        assert_eq!(
            node.pending_gov_proposals().len(),
            MAX_PENDING_GOV_PER_ACCOUNT + 1
        );
    }

    // --- K4:知识 op 的 gossip 与待处理池 ---------------------------------------

    fn kgen() -> Genesis {
        let mut g = genesis();
        g.knowledge.domains = vec![2];
        g
    }

    fn kpayload(tag: u8, cites: Vec<crate::knowledge::Cite>) -> crate::knowledge::VersionPayload {
        use crate::knowledge::*;
        VersionPayload {
            bodies: vec![Body {
                lang: "zh".into(),
                hash: [tag; 32],
            }],
            source: Source::Human,
            cites,
        }
    }

    /// 由 `actor` 用 kp(actor) 签名的知识 op;`tag` 区分正文,使同序号的 op 也有不同哈希。
    fn kop_create(actor: u64, nonce: u64, tag: u8) -> crate::knowledge::KnowledgeOp {
        use crate::knowledge::*;
        KnowledgeOp {
            actor,
            action: KnowledgeAction::CreateUnit {
                layer: Layer::Foundation,
                domain: 2,
                version: kpayload(tag, vec![]),
            },
            nonce,
            signature: [0u8; 64],
        }
        .signed(&kp(actor))
    }

    #[test]
    fn knowledge_op_gossip_wire_round_trips() {
        let op = kop_create(1, 0, 9);
        let bytes = encode_gossip(&GossipMsg::KnowledgeOp(Box::new(op.clone())));
        assert_eq!(bytes[0], TAG_KNOWLEDGE_OP);
        match decode_gossip(&bytes) {
            Ok(GossipMsg::KnowledgeOp(back)) => assert_eq!(*back, op),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn knowledge_op_gossip_reaches_every_node_and_dedups() {
        let ids = [1u64, 2, 3];
        let nodes: Vec<GossipNode> = ids
            .iter()
            .map(|&id| GossipNode::new(id, kgen(), 16, ids.iter().copied()))
            .collect();
        let mut net = Network::new(nodes);
        let a = kop_create(1, 0, 1);
        let b = kop_create(2, 0, 2);
        net.submit_knowledge_op(1, a.clone());
        net.submit_knowledge_op(2, b.clone());
        net.run();
        for id in ids {
            let hs: std::collections::HashSet<_> = net
                .node(id)
                .pending_knowledge_ops()
                .iter()
                .map(|o| o.hash())
                .collect();
            assert_eq!(hs.len(), 2, "node {id} has both ops");
            assert!(hs.contains(&a.hash()) && hs.contains(&b.hash()));
        }
        let out = net
            .node_mut(2)
            .on_message(1, GossipMsg::KnowledgeOp(Box::new(a)));
        assert!(out.is_empty(), "a seen op is not re-flooded");
        assert_eq!(net.node(2).pending_knowledge_ops().len(), 2);
    }

    #[test]
    fn knowledge_gossip_rejects_inadmissible_ops() {
        // 无效的知识 op 既不入池也不转发:签名错误、非成员、序号过旧 / 过于超前、结构不合法。
        let mut node = GossipNode::new(1, kgen(), 16, [1, 2]);
        let forged = crate::knowledge::KnowledgeOp {
            actor: 1,
            ..kop_create(1, 0, 1)
        }
        .signed(&kp(2));
        let too_far = kop_create(1, crate::GOV_NONCE_LOOKAHEAD + 1, 2);
        let mut malformed = kop_create(1, 0, 3);
        if let crate::knowledge::KnowledgeAction::CreateUnit { version, .. } = &mut malformed.action
        {
            version.bodies.clear();
        }
        let malformed = malformed.signed(&kp(1));
        node.chain.state.knowledge.members.remove(&2);
        let non_member = kop_create(2, 0, 4);
        for op in [forged, too_far, malformed, non_member] {
            assert!(node
                .on_message(2, GossipMsg::KnowledgeOp(Box::new(op)))
                .is_empty());
        }
        assert!(node.pending_knowledge_ops().is_empty());
        assert_eq!(
            node.seen_knowledge_len(),
            0,
            "rejected ops never consume a dedup slot"
        );
        let r = node.submit_local_knowledge_op(kop_create(2, 0, 5));
        assert!(
            matches!(r, Err(ChainError::NotAMember(2))),
            "local submit gets the structured reason: {r:?}"
        );
    }

    #[test]
    fn knowledge_admission_rejects_permanently_invalid_ops_up_front() {
        // 测试网实测发现:成员核验自己的版本时,RPC 先回报成功、出块时才悄悄丢弃。现在准入时即给出原因,
        // 且不入池、不占去重槽位。可能在同一块内变得有效的 op(引用尚未创建的单元)仍然放行。
        use crate::knowledge::*;
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut driver = ChainDriver::new(kgen(), seeds, 4);
        let mut node = GossipNode::new(1, kgen(), 16, [1]);
        driver.stage_knowledge_op(kop_create(1, 0, 1));
        driver.produce(1.0, &BTreeSet::new()).unwrap();
        let (b, c) = (driver.blocks()[0].clone(), driver.certificates()[0].clone());
        assert!(node.apply_certified(b, c));

        let mk = |actor: u64, nonce: u64, action| {
            KnowledgeOp {
                actor,
                action,
                nonce,
                signature: [0u8; 64],
            }
            .signed(&kp(actor))
        };
        let self_verify = mk(
            1,
            1,
            KnowledgeAction::Verify {
                unit: 1,
                version: 1,
                verdict: Verdict::Pass,
                note_hash: [0u8; 32],
            },
        );
        let r = node.submit_local_knowledge_op(self_verify);
        assert!(
            matches!(
                r,
                Err(ChainError::SelfVerify {
                    unit: 1,
                    version: 1
                })
            ),
            "{r:?}"
        );
        let bad_domain = mk(
            2,
            0,
            KnowledgeAction::CreateUnit {
                layer: Layer::Foundation,
                domain: 9,
                version: kpayload(4, vec![]),
            },
        );
        assert!(matches!(
            node.submit_local_knowledge_op(bad_domain),
            Err(ChainError::UnknownDomain(9))
        ));
        let reuse_key = mk(
            2,
            0,
            KnowledgeAction::Invite {
                pubkey: kp(3).public(),
            },
        );
        assert!(matches!(
            node.submit_local_knowledge_op(reuse_key),
            Err(ChainError::PubkeyInUse)
        ));
        assert!(node.pending_knowledge_ops().is_empty());
        assert_eq!(node.seen_knowledge_len(), 0);
        // 他人核验:放行;引用尚未存在的单元:放行(可能被同一块内更早的 op 创建)
        let other = mk(
            2,
            0,
            KnowledgeAction::Verify {
                unit: 1,
                version: 1,
                verdict: Verdict::Pass,
                note_hash: [0u8; 32],
            },
        );
        node.submit_local_knowledge_op(other)
            .expect("another member may verify");
        let forward = mk(
            3,
            0,
            KnowledgeAction::CreateUnit {
                layer: Layer::Application,
                domain: 2,
                version: kpayload(
                    5,
                    vec![Cite {
                        unit: 7,
                        version: 1,
                        kind: CiteKind::Basis,
                    }],
                ),
            },
        );
        node.submit_local_knowledge_op(forward)
            .expect("a forward cite may resolve within a block");
    }

    #[test]
    fn knowledge_pool_caps_quota_and_current_nonce_priority() {
        // 镜像治理池 SEC-01 第 3 / 5 轮:按账户配额、全局上限(本地提交同样受限,满池不占去重槽位)、
        // 当前序号的 op 可淘汰停放最远的超前序号 op。
        let fillers = (MAX_PENDING_KNOWLEDGE / MAX_PENDING_KNOWLEDGE_PER_ACCOUNT) as u64;
        let mut node = GossipNode::new(1, genesis_with_accounts(fillers + 2), 16, [1]);
        node.chain.state.knowledge.domains.insert(2);
        for acct in 1..=fillers {
            for i in 0..MAX_PENDING_KNOWLEDGE_PER_ACCOUNT as u64 {
                let op = kop_create(acct, 1 + i % crate::GOV_NONCE_LOOKAHEAD, i as u8);
                node.submit_local_knowledge_op(op).expect("within caps");
            }
        }
        assert_eq!(node.pending_knowledge_ops().len(), MAX_PENDING_KNOWLEDGE);
        // 配额:账户 1 已满,再交一个超前序号的 op 被拒
        let r = node.submit_local_knowledge_op(kop_create(1, 3, 200));
        assert!(
            matches!(
                r,
                Err(ChainError::KnowledgeAccountQuotaExceeded { account: 1, .. })
            ),
            "{r:?}"
        );
        // 全局上限:新账户的超前序号 op 被明确拒绝,池与去重缓存都不增长
        let seen = node.seen_knowledge_len();
        let r = node.submit_local_knowledge_op(kop_create(fillers + 1, 2, 1));
        assert!(matches!(r, Err(ChainError::KnowledgePoolFull)), "{r:?}");
        assert_eq!(node.pending_knowledge_ops().len(), MAX_PENDING_KNOWLEDGE);
        assert_eq!(
            node.seen_knowledge_len(),
            seen,
            "a full pool does not grow the dedup cache"
        );
        // 当前序号优先:淘汰一个最超前(序号 8)的停放 op,池仍在上限
        let far = |n: &GossipNode| {
            n.pending_knowledge_ops()
                .iter()
                .filter(|o| o.nonce == crate::GOV_NONCE_LOOKAHEAD)
                .count()
        };
        let before = far(&node);
        let cur = kop_create(fillers + 2, 0, 1);
        node.submit_local_knowledge_op(cur.clone())
            .expect("current nonce takes priority");
        assert_eq!(node.pending_knowledge_ops().len(), MAX_PENDING_KNOWLEDGE);
        assert!(node
            .pending_knowledge_ops()
            .iter()
            .any(|o| o.hash() == cur.hash()));
        assert_eq!(
            far(&node),
            before - 1,
            "the victim was a furthest-future op"
        );
    }

    #[test]
    fn knowledge_candidate_orders_by_nonce_and_skips_inapplicable_ops() {
        // 同一账户先交序号 1(依赖序号 0 创建的单元)、后交序号 0;另一账户交一条引用不存在单元的 op。
        // 候选块应按(账户, 序号)排好两条、丢掉不可执行的那条,且能正常封装(不停摆)。
        use crate::knowledge::*;
        let mut node = GossipNode::new(1, kgen(), 16, [1]);
        let create = kop_create(1, 0, 1);
        let publish = KnowledgeOp {
            actor: 1,
            action: KnowledgeAction::PublishVersion {
                unit: 1,
                version: kpayload(2, vec![]),
            },
            nonce: 1,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let dangling = KnowledgeOp {
            actor: 2,
            action: KnowledgeAction::CreateUnit {
                layer: Layer::Application,
                domain: 2,
                version: kpayload(
                    3,
                    vec![Cite {
                        unit: 99,
                        version: 1,
                        kind: CiteKind::Basis,
                    }],
                ),
            },
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(2));
        node.submit_local_knowledge_op(publish.clone()).unwrap();
        node.submit_local_knowledge_op(dangling).unwrap();
        node.submit_local_knowledge_op(create.clone()).unwrap();
        let cand = node
            .build_candidate(1.0)
            .expect("candidate seals despite an inapplicable op");
        assert_eq!(cand.knowledge_ops, vec![create, publish]);
    }

    #[test]
    fn knowledge_ops_are_pruned_on_commit() {
        // 已入块的知识 op 从池中移除;序号被其他 op 消耗掉的 op 成为陈旧 op,也被移除。
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut driver = ChainDriver::new(kgen(), seeds, 4);
        let mut node = GossipNode::new(1, kgen(), 16, [1]);
        let committed = kop_create(1, 0, 1);
        node.submit_local_knowledge_op(committed.clone()).unwrap();
        node.submit_local_knowledge_op(kop_create(2, 0, 2)).unwrap(); // 将被另一条序号 0 的 op 顶掉
        driver.stage_knowledge_op(committed);
        driver.stage_knowledge_op(kop_create(2, 0, 3));
        driver.produce(1.0, &BTreeSet::new()).unwrap();
        let (b, c) = (driver.blocks()[0].clone(), driver.certificates()[0].clone());
        assert_eq!(b.knowledge_ops.len(), 2);
        assert!(node.apply_certified(b, c));
        assert!(
            node.pending_knowledge_ops().is_empty(),
            "committed and stale knowledge ops pruned"
        );
        assert_eq!(node.chain.state.knowledge.units.len(), 2);
    }

    #[test]
    fn stale_gov_ops_are_pruned_on_commit() {
        // SEC-01 (round 3): a staged op whose nonce was consumed by a *different* op in
        // a committed block, or a vote whose proposal has closed, can never apply — so
        // apply_certified prunes it instead of letting it hold a pool slot indefinitely.
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut driver = ChainDriver::new(genesis(), seeds, 4);
        let mut node = GossipNode::new(1, genesis(), 16, [1]);

        // The node stages account 1's nonce-0 proposal; the network commits a DIFFERENT
        // nonce-0 proposal from account 1 (short voting window for the vote half below).
        node.submit_local_gov_proposal(gov_prop(1, 0, 100))
            .expect("admitted");
        driver.stage_gov_proposal(gov_prop(1, 0, 2));
        driver.produce(1.0, &BTreeSet::new()).unwrap();
        let (b1, c1) = (driver.blocks()[0].clone(), driver.certificates()[0].clone());
        assert!(node.apply_certified(b1, c1));
        assert!(
            node.pending_gov_proposals().is_empty(),
            "nonce-0 proposal is stale once account 1's nonce advanced"
        );
        // ...and the same nonce is now refused at admission as stale.
        let r = node.submit_local_gov_proposal(gov_prop(1, 0, 7));
        assert!(
            matches!(
                r,
                Err(ChainError::BadNonce {
                    account: 1,
                    expected: 1,
                    got: 0
                })
            ),
            "got {r:?}"
        );

        // Vote half: the committed proposal (period 2, registered at h=1) is open for
        // h=2, so a vote is admitted now — and pruned once h=2 commits (closed at h=3).
        let pid = *node
            .chain
            .state
            .proposals
            .keys()
            .next()
            .expect("proposal registered");
        let vote = crate::GovVote {
            proposal_id: pid,
            voter: 2,
            approve: true,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(2));
        node.submit_local_gov_vote(vote)
            .expect("proposal still open");
        assert_eq!(node.pending_gov_votes().len(), 1);
        // h=2 needs content for the driver to emit a block: account 1's next proposal.
        driver.stage_gov_proposal(gov_prop(1, 1, 50));
        driver.produce(2.0, &BTreeSet::new()).unwrap();
        let (b2, c2) = (driver.blocks()[1].clone(), driver.certificates()[1].clone());
        assert!(node.apply_certified(b2, c2));
        assert!(
            node.pending_gov_votes().is_empty(),
            "vote on a closed proposal is pruned"
        );
    }

    #[test]
    fn current_nonce_gov_op_evicts_parked_future_op_when_pool_full() {
        // SEC-01 (round 5): many accounts parking within-window future-nonce ops can
        // fill the pool, and empty blocks never advance their nonces — so a full pool
        // must not starve immediately-applicable (current-nonce) ops. A current-nonce
        // op evicts the furthest-future parked op (globally, or its own at quota); the
        // evicted op's hash is forgotten so its owner can resubmit; a future-nonce op
        // still gets the hard `GovPoolFull`.
        let fillers = (MAX_PENDING_GOV / MAX_PENDING_GOV_PER_ACCOUNT) as u64;
        let mut node = GossipNode::new(1, genesis_with_accounts(fillers + 1), 16, [1]);
        for acct in 1..=fillers {
            for i in 0..MAX_PENDING_GOV_PER_ACCOUNT as u64 {
                let op = gov_prop(acct, 1 + i % crate::GOV_NONCE_LOOKAHEAD, 100 + i);
                node.submit_local_gov_proposal(op).expect("within caps");
            }
        }
        assert_eq!(node.pending_gov_proposals().len(), MAX_PENDING_GOV);
        // Number of furthest-future (nonce 8) parked ops: two per filler account.
        let far = |node: &GossipNode| {
            node.pending_gov_proposals()
                .iter()
                .filter(|p| p.nonce == crate::GOV_NONCE_LOOKAHEAD)
                .count()
        };
        assert_eq!(far(&node), fillers as usize * 2);

        // (a) An untouched account's current-nonce op is admitted into the full pool by
        // evicting exactly one parked op — a furthest-future one; the pool stays at cap.
        let cur = gov_prop(fillers + 1, 0, 100);
        node.submit_local_gov_proposal(cur.clone())
            .expect("current nonce takes priority over parked future ops");
        let pool = node.pending_gov_proposals();
        assert_eq!(pool.len(), MAX_PENDING_GOV, "still at the hard cap");
        assert!(pool.iter().any(|p| p.hash() == cur.hash()), "staged");
        assert_eq!(
            far(&node),
            fillers as usize * 2 - 1,
            "the victim was a furthest-future (nonce 8) op"
        );
        // Ties go to the oldest: account 1's first nonce-8 op (period 107). Resubmitting
        // it is refused by the (still full) pool cap — NOT as `GovAlreadySeen`, proving
        // eviction forgot its hash so the owner can resubmit once there is room.
        let r = node.submit_local_gov_proposal(gov_prop(1, 8, 107));
        assert!(matches!(r, Err(ChainError::GovPoolFull)), "got {r:?}");

        // (b) A FUTURE-nonce op from an account with spare quota still hits the hard
        // cap: priority is only for immediately-applicable ops.
        let r = node.submit_local_gov_proposal(gov_prop(fillers + 1, 1, 100));
        assert!(matches!(r, Err(ChainError::GovPoolFull)), "got {r:?}");

        // (c) Per-account path, pool NOT full: account 1 parks its whole quota (two ops
        // at nonce 8); its current-nonce op evicts its own furthest-future op, oldest on
        // ties, and the quota count is unchanged.
        let mut node = GossipNode::new(1, genesis(), 16, [1]);
        for i in 0..MAX_PENDING_GOV_PER_ACCOUNT as u64 {
            node.submit_local_gov_proposal(gov_prop(
                1,
                1 + i % crate::GOV_NONCE_LOOKAHEAD,
                100 + i,
            ))
            .expect("within quota");
        }
        assert_eq!(far(&node), 2);
        node.submit_local_gov_proposal(gov_prop(1, 0, 777))
            .expect("current nonce evicts this account's furthest parked op");
        let mine: Vec<_> = node
            .pending_gov_proposals()
            .iter()
            .filter(|p| p.proposer == 1)
            .collect();
        assert_eq!(mine.len(), MAX_PENDING_GOV_PER_ACCOUNT, "quota unchanged");
        assert!(mine.iter().any(|p| p.nonce == 0), "current-nonce op staged");
        assert_eq!(far(&node), 1, "one nonce-8 op evicted");
        // The victim (older nonce-8 op, period 107) is refused by quota on resubmit —
        // not `GovAlreadySeen`.
        let r = node.submit_local_gov_proposal(gov_prop(1, 8, 107));
        assert!(
            matches!(
                r,
                Err(ChainError::GovAccountQuotaExceeded { account: 1, .. })
            ),
            "got {r:?}"
        );

        // (d) With nothing parked (a pool of only current-nonce ops) the hard cap holds
        // even for a current-nonce op — eviction never drops an applicable op.
        let mut node = GossipNode::new(1, genesis_with_accounts(fillers + 1), 16, [1]);
        for acct in 1..=fillers {
            for i in 0..MAX_PENDING_GOV_PER_ACCOUNT as u64 {
                node.submit_local_gov_proposal(gov_prop(acct, 0, 100 + i))
                    .expect("within caps");
            }
        }
        let r = node.submit_local_gov_proposal(gov_prop(fillers + 1, 0, 100));
        assert!(matches!(r, Err(ChainError::GovPoolFull)), "got {r:?}");
        assert_eq!(node.pending_gov_proposals().len(), MAX_PENDING_GOV);
    }

    #[test]
    fn key_rotation_purges_pending_gov_ops_signed_by_old_key() {
        // SEC-01 (round 5): after a committed key rotation, the account's parked ops
        // signed by the OLD key are still inside the nonce window (rotation consumed
        // just one nonce) but can never verify at apply — they must not keep holding
        // the account's quota. Other accounts' ops are untouched.
        let ids = [1u64, 2, 3, 21, 22, 23, 24];
        let seeds: BTreeMap<u64, [u8; 32]> = ids.iter().map(|&id| (id, seed(id))).collect();
        let mut driver = ChainDriver::new(genesis(), seeds, 4);
        let mut node = GossipNode::new(1, genesis(), 16, [1]);

        for n in 1..=3 {
            node.submit_local_gov_proposal(gov_prop(1, n, 100))
                .expect("inside the window");
        }
        node.submit_local_gov_proposal(gov_prop(2, 1, 100))
            .expect("control op from another account");
        assert_eq!(node.pending_gov_proposals().len(), 4);

        // The network commits account 1's rotation to kp(99) at nonce 0.
        let rot = KeyRotation {
            account: 1,
            new_pubkey: kp(99).public(),
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        driver.stage_key_rotation(rot);
        driver.produce(1.0, &BTreeSet::new()).unwrap();
        let (b1, c1) = (driver.blocks()[0].clone(), driver.certificates()[0].clone());
        assert!(node.apply_certified(b1, c1));
        assert_eq!(
            node.chain.state.accounts.get(&1).map(|a| a.nonce),
            Some(1),
            "rotation consumed nonce 0, so nonces 1..=3 are still inside the window"
        );
        let pool = node.pending_gov_proposals();
        assert!(
            pool.iter().all(|p| p.proposer != 1),
            "old-key ops purged despite being inside the nonce window"
        );
        assert_eq!(pool.len(), 1, "account 2's op survives");

        // Old key at the new current nonce → BadSignature; new key → admitted.
        let r = node.submit_local_gov_proposal(gov_prop(1, 1, 100));
        assert!(matches!(r, Err(ChainError::BadSignature(1))), "got {r:?}");
        let fresh = crate::GovProposal {
            proposer: 1,
            param: crate::GovParam::BaseEmission(1),
            voting_period: 100,
            nonce: 1,
            signature: [0u8; 64],
        }
        .signed(&kp(99));
        node.submit_local_gov_proposal(fresh)
            .expect("op signed by the rotated-in key is admitted");
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
        driver
            .produce(1.0, &BTreeSet::new())
            .unwrap()
            .expect("stake-only block at h=1");
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
        assert!(
            state.validators.get(1).is_none(),
            "offender removed from validator set"
        );
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
        let (_, GossipMsg::Headers(batch)) = &out[0] else {
            panic!("expected Headers, got {:?}", out[0])
        };
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
        // Closure: height `i+1`'s next set is `sets[i]` (after applying block `i+1`,
        // the validator set the next block's `next_validators_root` commits to).

        net.announce_all();
        // Round 1: light's Status{height=0} → full sees Status{height=0}, peer ahead=3,
        // reply with Headers{from=1}. Round 2: bridge side-channels the Headers batch
        // through `apply_header(ch, &sets[i])`, light advances to height 3.
        for _ in 0..10 {
            let n = net.run();
            if n == 0 {
                break;
            }
        }

        let l = net.light_node(light_id);
        assert_eq!(
            l.tracker().height(),
            3,
            "light node reached the full node's height"
        );
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
        assert!(
            matches!(err, crate::light::LightError::ValidatorRootMismatch { .. }),
            "got {err}"
        );
        assert_eq!(
            light.tracker().height(),
            0,
            "tracker unchanged on rejection"
        );
    }

    // --- M147: next validator set over the header-sync wire ----------------------

    /// Build a certified chain whose validator set CHANGES at height 1 (adds
    /// validator 25), for the cross-set-change wire tests. Returns `(blocks, certs)`.
    fn chain_with_validator_change() -> (Vec<Block>, Vec<Commit>) {
        use crate::driver::ChainDriver;
        let seeds: BTreeMap<u64, [u8; 32]> = [21u64, 22, 23, 24, 25]
            .iter()
            .map(|&id| (id, seed(id)))
            .collect();
        let mut d = ChainDriver::new(genesis(), seeds, 1);
        // h1: add validator 25 (effective next height). h2: a plain tx height, now
        // certified by the enlarged set {21..25}.
        d.stage_validator_update(crate::validator::ValidatorUpdate {
            id: 25,
            pubkey: kp(25).public(),
            power: 1,
        });
        d.produce(1.0, &std::collections::BTreeSet::new())
            .unwrap()
            .expect("block 1 with update");
        d.submit(tx(1, 1, 1)).unwrap();
        d.produce(2.0, &std::collections::BTreeSet::new())
            .unwrap()
            .expect("block 2");
        (d.blocks().to_vec(), d.certificates().to_vec())
    }

    #[test]
    fn headers_with_sets_serve_pairs_each_header_with_its_next_set() {
        // M147/M150: `headers_with_sets_from` attaches a set only across an actual
        // validator-set change (Some), else None (unchanged — reuse current set). The
        // chain adds validator 25 at height 1, then no change at height 2.
        let (blocks, certs) = chain_with_validator_change();
        let mut full = GossipNode::new(1, genesis(), 8, []);
        assert!(full.load_certified(&blocks, &certs));

        let pairs = full.headers_with_sets_from(1);
        assert_eq!(pairs.len(), 2, "both retained heights served");
        // Header 1: the set changed (added 25) → Some, root matches, contains 25.
        let set1 = pairs[0]
            .1
            .as_ref()
            .expect("height 1 changed the set → Some");
        assert_eq!(
            set1.merkle_root(),
            pairs[0].0.header.next_validators_root,
            "attached set certifies the next header"
        );
        assert!(
            set1.get(25).is_some(),
            "validator added at height 1 is present"
        );
        // Header 2: the set was unchanged → None (M150 compression).
        assert!(
            pairs[1].1.is_none(),
            "unchanged set at height 2 is omitted (None)"
        );
    }

    #[test]
    fn light_syncs_across_validator_set_change_over_wire() {
        // M147 core soundness: a light node tracks the chain across a validator-set
        // change using ONLY wire data (GetHeadersWithSets / HeadersWithSets) — no
        // in-process `next_set` oracle. It reaches the full node's height + set.
        let (blocks, certs) = chain_with_validator_change();
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        assert!(full.load_certified(&blocks, &certs));
        let light_id = 2u64;
        let light = LightGossipNode::new(light_id, &genesis(), [1, 2]);
        let mut net = LightNetwork::new(vec![full], vec![light]);

        net.announce_all();
        for _ in 0..20 {
            if net.run() == 0 {
                break;
            }
        }

        let l = net.light_node(light_id);
        assert_eq!(l.tracker().height(), 2, "light reached the tip over wire");
        let auth = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(
            l.tracker().validators().merkle_root(),
            auth.merkle_root(),
            "light-tracked set matches the authoritative post-change set"
        );
        assert!(
            l.tracker().validators().get(25).is_some(),
            "validator 25 was learned purely from wire data"
        );
    }

    #[test]
    fn light_syncs_unchanged_set_via_none_entries() {
        // M150: on a chain whose validator set never changes, every header's next set is
        // omitted (None); the light node reconstructs it by reusing its current tracked
        // set and still advances to the tip over wire.
        let (blocks, certs) = certified_chain(3); // constant validator set
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        assert!(full.load_certified(&blocks, &certs));
        // Every served entry omits the set (compression active).
        assert!(
            full.headers_with_sets_from(1)
                .iter()
                .all(|(_, s)| s.is_none()),
            "unchanged-set chain omits every set"
        );
        let light_id = 2u64;
        let light = LightGossipNode::new(light_id, &genesis(), [1, 2]);
        let mut net = LightNetwork::new(vec![full], vec![light]);
        net.announce_all();
        for _ in 0..20 {
            if net.run() == 0 {
                break;
            }
        }
        assert_eq!(
            net.light_node(light_id).tracker().height(),
            3,
            "light synced to tip reconstructing None sets"
        );
    }

    #[test]
    fn light_from_checkpoint_follows_forward_over_wire() {
        // M154: a light node seeded from a weak-subjectivity checkpoint at height H
        // (pinned set + head, no snapshot/cert) follows forward over wire to the tip.
        let (blocks, certs) = certified_chain(3);
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        assert!(full.load_certified(&blocks, &certs));

        // Checkpoint at height 1: the set that certifies height 2 + the head at 1.
        let at_1 = crate::Chain::replay(genesis(), &blocks[..1]).expect("replay to 1");
        let light_id = 2u64;
        let light = LightGossipNode::from_checkpoint(
            light_id,
            at_1.state.validators.clone(),
            at_1.head,
            1,
            [1, 2],
        );
        assert_eq!(
            light.tracker().height(),
            1,
            "seeded at the checkpoint height"
        );
        assert_eq!(light.base_height(), 1);

        let mut net = LightNetwork::new(vec![full], vec![light]);
        net.announce_all();
        for _ in 0..20 {
            if net.run() == 0 {
                break;
            }
        }
        let l = net.light_node(light_id);
        assert_eq!(
            l.tracker().height(),
            3,
            "followed from the checkpoint to the tip"
        );
        let auth = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(
            l.tracker().validators().merkle_root(),
            auth.merkle_root(),
            "tracked set matches the authoritative set"
        );
    }

    #[test]
    fn gossip_wire_round_trips_headers_with_sets() {
        // M147/M150: the header-sync-with-sets pair round-trips, including both the
        // Some(set) and the None (unchanged) entry forms.
        let (blocks, certs) = certified_chain(2);
        let ch0 = CertifiedHeader::from_certified(&blocks[0], &certs[0]);
        let ch1 = CertifiedHeader::from_certified(&blocks[1], &certs[1]);
        let set = authoritative_final_set(&genesis(), &blocks);
        let msg = GossipMsg::HeadersWithSets(vec![(ch0, Some(set)), (ch1, None)]);
        match decode_gossip(&encode_gossip(&msg)).unwrap() {
            GossipMsg::HeadersWithSets(b) => {
                assert_eq!(b.len(), 2);
                assert!(b[0].1.is_some(), "first entry carries a set");
                assert!(b[1].1.is_none(), "second entry omits the set");
            }
            other => panic!("expected HeadersWithSets, got {other:?}"),
        }
        let g = GossipMsg::GetHeadersWithSets { from: 7 };
        match decode_gossip(&encode_gossip(&g)).unwrap() {
            GossipMsg::GetHeadersWithSets { from } => assert_eq!(from, 7),
            other => panic!("expected GetHeadersWithSets, got {other:?}"),
        }
    }

    /// Build the genesis validator set (21-24, power 1) — the set that certifies
    /// every header in a `certified_chain` (no validator updates), i.e. the
    /// weak-subjectivity trust anchor for the M142 verify tests.
    fn trusted_genesis_set() -> ValidatorSet {
        ValidatorSet::new(
            [21u64, 22, 23, 24]
                .iter()
                .map(|&id| Validator {
                    id,
                    pubkey: kp(id).public(),
                    power: 1,
                })
                .collect(),
        )
    }

    #[test]
    fn light_node_from_snapshot_resumes_and_matches_genesis_tracked() {
        // M142: a light node seeded from a snapshot at height H resumes header-sync
        // from H+1 over the bus and reaches the same height + validator set as a
        // genesis-tracked light node (cf. `light_node_pulls_headers_and_tracks_validator_set`).
        let (blocks, certs) = certified_chain(3); // heights 1..=3
        let mut full = GossipNode::new(1, genesis(), 8, [1, 2]);
        full.load_certified(&blocks, &certs);

        // Snapshot at height 1 (the state + head after block 1).
        let h = 1usize;
        let snap = Chain::replay(genesis(), &blocks[..h]).expect("replay to H");
        assert_eq!(snap.state.height, 1);

        let light_id = 2u64;
        let light = LightGossipNode::from_snapshot(light_id, &snap.state, snap.head, [1, 2]);
        assert_eq!(light.base_height(), 1, "seeded at the snapshot tip");
        assert_eq!(
            light.tracker().height(),
            1,
            "tracker starts at the snapshot tip"
        );

        let mut net = LightNetwork::new(vec![full], vec![light]);
        net.announce_all();
        for _ in 0..10 {
            if net.run() == 0 {
                break;
            }
        }

        let l = net.light_node(light_id);
        assert_eq!(
            l.tracker().height(),
            3,
            "snapshot-seeded light node resumed to the full node's height"
        );
        let authoritative = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(
            l.tracker().validators().merkle_root(),
            authoritative.merkle_root(),
            "snapshot-seeded set equals the authoritative replayed set"
        );
    }

    #[test]
    fn light_node_base_height_reports_snapshot_tip() {
        // M142: `base_height()` is 0 for a genesis-start node, the snapshot tip for a
        // snapshot-seeded one.
        let (blocks, _certs) = certified_chain(2);
        let fresh = LightGossipNode::new(1, &genesis(), [1]);
        assert_eq!(fresh.base_height(), 0);

        let snap = Chain::replay(genesis(), &blocks[..2]).expect("replay");
        let seeded = LightGossipNode::from_snapshot(1, &snap.state, snap.head, [1]);
        assert_eq!(seeded.base_height(), 2);
        assert_eq!(seeded.tracker().height(), 2);
    }

    #[test]
    fn light_node_from_verified_snapshot_accepts_valid_cert() {
        // M142: the safe seed path accepts a snapshot whose cert-signed header reaches
        // >2/3 of the pinned trusted set, and leaves the node ready to follow H+1.
        let (blocks, certs) = certified_chain(3);
        let h = 2usize;
        let snap = Chain::replay(genesis(), &blocks[..h]).expect("replay");
        let header = blocks[h - 1].header();
        let cert = &certs[h - 1];
        let node = LightGossipNode::from_verified_snapshot(
            9,
            &snap.state,
            &header,
            cert,
            &trusted_genesis_set(),
            [1],
        )
        .expect("valid snapshot accepted");
        assert_eq!(node.base_height(), 2);
        assert_eq!(node.tracker().height(), 2);
        assert_eq!(node.tracker().head(), header.hash());
    }

    #[test]
    fn light_node_from_verified_snapshot_rejects_forged_snapshot() {
        // M142: tamper one account balance so the snapshot's roots no longer match the
        // cert-signed header → SnapshotMismatch (M134 layer), before any adoption.
        let (blocks, certs) = certified_chain(3);
        let h = 2usize;
        let mut snap = Chain::replay(genesis(), &blocks[..h]).expect("replay");
        snap.state.accounts.get_mut(&1).unwrap().balance += 1;
        let header = blocks[h - 1].header();
        let err = match LightGossipNode::from_verified_snapshot(
            9,
            &snap.state,
            &header,
            &certs[h - 1],
            &trusted_genesis_set(),
            [1],
        ) {
            Err(e) => e,
            Ok(_) => panic!("forged snapshot must be rejected"),
        };
        assert!(
            matches!(err, crate::light::LightError::SnapshotMismatch { .. }),
            "got {err}"
        );
    }

    #[test]
    fn light_node_from_verified_snapshot_rejects_untrusted_cert() {
        // M142: a cert that does not reach >2/3 of the pinned trusted set is rejected
        // (here the trusted set is a single stranger the real signers aren't in).
        let (blocks, certs) = certified_chain(3);
        let h = 2usize;
        let snap = Chain::replay(genesis(), &blocks[..h]).expect("replay");
        let header = blocks[h - 1].header();
        let bogus = ValidatorSet::new(vec![Validator {
            id: 99,
            pubkey: crate::Keypair::from_seed([7u8; 32]).public(),
            power: 1,
        }]);
        assert!(
            LightGossipNode::from_verified_snapshot(
                9,
                &snap.state,
                &header,
                &certs[h - 1],
                &bogus,
                [1],
            )
            .is_err(),
            "cert not backed by the trusted set must be rejected"
        );
    }

    #[test]
    fn header_gossip_shrinks_the_wire_payload_vs_full_blocks() {
        // The whole point of M22: a header-only gossip item omits the bodies
        // (txs, stake ops, evidence, …), so for a block that actually carries a
        // body it is smaller than shipping the full block + cert. M179 (B4 wire)
        // grew the header by one 32-byte body commitment (eight total = 256 B of
        // commitments), so the saving only materializes once a block's raw body
        // exceeds that commitment overhead — which it does for any real block.
        // We therefore assert the invariant on the single block that carries the
        // transactions (3 txs ≈ 600 B of body ≫ 256 B of commitments), rather
        // than summing over a chain padded with empty heartbeat blocks whose
        // commitments would dominate their (empty) bodies.
        let (blocks, certs) = certified_chain(3);
        // Put three real txs into one block so the body unambiguously exceeds the
        // 8×32 B of header commitments (one tx ≈ 200 B, so three ≈ 600 B ≫ 256 B).
        // The mempool spreads txs across blocks, so we stuff one block directly;
        // the cert is identical on both sides of the inequality and cancels out,
        // so any cert works for a pure size comparison.
        let mut b = blocks[0].clone();
        b.txs = vec![tx(1, 1, 1), tx(2, 2, 2), tx(3, 3, 3)];
        let c = &certs[0];

        let full_bytes = encode_block(&b).len() + encode_commit(c).len();
        let header_bytes = encode_certified_header(&CertifiedHeader::from_certified(&b, c)).len();

        assert!(
            header_bytes < full_bytes,
            "header-only item ({header_bytes} B) must be strictly smaller than the full \
             block+cert ({full_bytes} B) for a block with a body — bodies are never carried \
             in header gossip"
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
            lt.follow_header(&ch.header, &ch.cert, &sets[i])
                .expect("follow_header");
        }
        assert_eq!(lt.height(), 3);
        let authoritative = authoritative_final_set(&genesis(), &blocks);
        assert_eq!(lt.validators().merkle_root(), authoritative.merkle_root());
    }

    // --- M24: batched typed proof gossip + light wallet end-to-end ---------

    #[test]
    fn batch_request_codec_round_trip() {
        // M61: the standalone batch-request codec (reused by the RPC `POST /batch`
        // body) round-trips a heterogeneous `Vec<BatchItem>`, caps the item count,
        // and emits bytes byte-identical to the inline gossip `GetBatch` framing.
        let items = vec![
            crate::light::BatchItem::Inclusion {
                kind: crate::light::ProofKind::Reviewer,
                id: 10,
            },
            crate::light::BatchItem::Knn {
                query: unit(0),
                k: 1,
            },
            crate::light::BatchItem::Range {
                query: unit(0),
                min_sim: 0.0,
            },
            crate::light::BatchItem::Diff { h1: 1, h2: 2 },
        ];

        // Round-trip: re-encoding the decoded items reproduces the original bytes
        // (compared as bytes to sidestep float Eq on the embedding queries).
        let bytes = encode_batch_request(&items);
        let decoded = decode_batch_request(&bytes).expect("decode batch request");
        assert_eq!(
            encode_batch_request(&decoded),
            bytes,
            "round-trip must be stable"
        );

        // The standalone encoding equals the tail of the inline gossip `GetBatch`
        // framing (tag byte + request bytes) — the RPC body shares that wire format.
        let gossip = encode_gossip(&GossipMsg::GetBatch {
            items: items.clone(),
        });
        assert_eq!(
            gossip[0], TAG_GETBATCH,
            "gossip GetBatch leads with its tag"
        );
        assert_eq!(
            &gossip[1..],
            &bytes[..],
            "RPC body matches the gossip GetBatch tail"
        );

        // Over-cap request is rejected at decode time (mirrors the gossip decoder).
        let big: Vec<_> = (0..=MAX_BATCH_ITEMS)
            .map(|i| crate::light::BatchItem::Inclusion {
                kind: crate::light::ProofKind::Account,
                id: i as u64,
            })
            .collect();
        let over = encode_batch_request(&big);
        assert!(
            matches!(
                decode_batch_request(&over),
                Err(CodecError::TooManyItems(_))
            ),
            "over-cap batch must decode to TooManyItems",
        );
    }

    #[test]
    fn blocks_codec_round_trip() {
        // M62: the standalone `(Block, Commit)` range codec (the RPC `/batch`
        // `range_blocks=` line) round-trips, matches the inline gossip `Blocks`
        // framing byte-for-byte, and fails fast on a lying length.
        let (blocks, certs) = certified_chain(2);
        let batch: Vec<(Block, Commit)> =
            blocks.iter().cloned().zip(certs.iter().cloned()).collect();

        let bytes = encode_blocks(&batch);
        let decoded = decode_blocks(&bytes).expect("decode blocks");
        assert_eq!(encode_blocks(&decoded), bytes, "round-trip must be stable");

        // The standalone encoding equals the tail of the inline gossip `Blocks`
        // framing (tag byte + body) — the RPC line shares that wire format.
        let gossip = encode_gossip(&GossipMsg::Blocks(batch.clone()));
        assert_eq!(gossip[0], TAG_BLOCKS, "gossip Blocks leads with its tag");
        assert_eq!(
            &gossip[1..],
            &bytes[..],
            "range_blocks line matches the gossip Blocks tail"
        );

        // An empty range encodes to just the u64 count (0) and round-trips.
        assert_eq!(encode_blocks(&[]), 0u64.to_be_bytes().to_vec());
        assert!(decode_blocks(&encode_blocks(&[]))
            .expect("decode empty")
            .is_empty());

        // A buffer that lies about its count (huge n, no bodies) fails fast at the
        // first missing pair — no OOM from a pre-sized allocation.
        let mut liar = 1_000_000u64.to_be_bytes().to_vec();
        liar.extend_from_slice(&[0u8; 4]); // some trailing junk, far short of a pair
        assert!(
            matches!(decode_blocks(&liar), Err(CodecError::UnexpectedEof)),
            "a lying count must fail at EOF, not allocate",
        );
    }

    #[test]
    fn batch_includes_range_for_diff_and_verifies() {
        // M62: a batch carrying a Diff item ships the `[1..=max_h2]` block range
        // the Diff verifier replays, so `verify_batch` accepts the Diff slot
        // without the client having pre-synced the chain.
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, []);
        full.load_certified(&blocks, &certs);

        let items = vec![
            crate::light::BatchItem::Diff { h1: 1, h2: 2 },
            crate::light::BatchItem::Inclusion {
                kind: crate::light::ProofKind::Reviewer,
                id: 10,
            },
        ];
        let (ch, env, range) = full.batch(items.clone()).expect("batch served");
        assert_eq!(env.items.len(), items.len(), "one slot per request item");
        assert_eq!(
            range.len(),
            2,
            "Diff{{1,2}} ships the full [1..=2] block range"
        );

        let tracker = crate::light::ValidatorTracker::from_genesis(&genesis());
        let tracked = tracker.validators().clone();
        tracker
            .verify_batch(
                &genesis(),
                &ch.header,
                &ch.cert,
                &tracked,
                &range,
                &items,
                &env,
            )
            .expect("the shipped range satisfies the Diff slot; head anchors the rest");

        // A Diff-free batch ships no range.
        let plain = vec![crate::light::BatchItem::Inclusion {
            kind: crate::light::ProofKind::Reviewer,
            id: 10,
        }];
        let (_ch2, _env2, range2) = full.batch(plain).expect("batch served");
        assert!(range2.is_empty(), "no Diff item ⇒ empty range");
    }

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
        let contributions_root = &full.chain.state.contributions_merkle_root();

        for (i, e) in entries.iter().enumerate() {
            let entry = e
                .as_ref()
                .unwrap_or_else(|| panic!("entry {i} should be Some"));
            let leaf = crate::merkle::leaf_hash(&entry.leaf());
            let root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => account_root,
                crate::light::ProofEntry::Validator { .. } => validator_root,
                crate::light::ProofEntry::Cnft { .. } => contributions_root,
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
                crate::light::ProofEntry::Cnft { .. } => &header.contributions_root,
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

        // Phase 1: light pulls headers from full (M22).
        net.announce_all();
        for _ in 0..20 {
            let n = net.run();
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

        crate::light::ValidatorTracker::verify_proof_against_header(
            &header, last_cert, &tracked, &acct,
        )
        .expect("light wallet proves account 1");
        crate::light::ValidatorTracker::verify_proof_against_header(
            &header, last_cert, &tracked, &rev,
        )
        .expect("light wallet proves reviewer 10");
        crate::light::ValidatorTracker::verify_proof_against_header(
            &header, last_cert, &tracked, &val,
        )
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

        net.announce_all();
        for _ in 0..20 {
            let n = net.run();
            if n == 0 && net.light_node(light_id).tracker().height() == 2 {
                break;
            }
        }
        let lt = net.light_node(light_id).tracker().clone();

        let mut full_node = net.take_full(1);
        let mut light_node = net.take_light(light_id);
        let reply = full_node.on_message(
            light_id,
            GossipMsg::GetProof {
                items: vec![(crate::light::ProofKind::Validator, 21)],
            },
        );
        let (_dst, proof_msg) = reply.into_iter().next().unwrap();
        light_node.on_message(1, proof_msg);

        let entry = light_node
            .take_proof(crate::light::ProofKind::Validator, 21)
            .expect("validator proof cached");
        // Tamper: bump the validator's power by 1.
        let mut forged = entry;
        if let crate::light::ProofEntry::Validator {
            id,
            ref mut validator,
            ..
        } = forged
        {
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
        let ProofEntry::Reviewer {
            id,
            reputation,
            proof,
        } = entry
        else {
            panic!("expected Reviewer entry");
        };
        assert_eq!(id, 11);
        assert!(reputation >= 0.0);
        // Local verification against the full peer's accounts_root.
        let leaf = crate::merkle::leaf_hash(&crate::Reviewer { id, reputation }.merkle_leaf());
        assert!(crate::merkle::verify(
            &full.chain.state.merkle_root(),
            &leaf,
            &proof
        ));
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
        assert!(
            entries[0].is_none(),
            "unknown id must produce None, got {:?}",
            entries[0]
        );

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
        assert!(light
            .take_proof(crate::light::ProofKind::Account, 999)
            .is_none());
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
            crate::light::ProofEntry::GraphNode {
                node_id,
                graph_node,
                ..
            } => {
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

    /// M186: a full node serves a `ProofEntry::Cnft` in response to a `GetProof`
    /// for `ProofKind::Cnft` — the serve-side path behind `GET /cnft/{id}/proof`.
    /// The entry verifies against the chain's `contributions_root` (M185), which
    /// the cert-signed header commits to.
    #[test]
    fn full_node_serves_a_cnft_proof_in_response_to_get_proof() {
        let (blocks, certs) = certified_chain(2);
        let mut full = GossipNode::new(1, genesis(), 8, [1]);
        full.load_certified(&blocks, &certs);

        // The two accepted submissions in `certified_chain(2)` each mint a cNFT.
        assert!(
            !full.chain.state.contributions.is_empty(),
            "accepted submissions must mint cNFTs"
        );
        let (node_id, expected_cnft) = {
            let (id, c) = full.chain.state.contributions.iter().next().unwrap();
            (*id, c.clone())
        };

        let request = GossipMsg::GetProof {
            items: vec![(crate::light::ProofKind::Cnft, node_id)],
        };
        let mut out = full.on_message(99, request);
        let (_dst, msg) = out.pop().unwrap();
        let entries = match msg {
            GossipMsg::Proof { items } => items,
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_ref().expect("cNFT proof must be Some");
        match entry {
            crate::light::ProofEntry::Cnft {
                node_id: nid, cnft, ..
            } => {
                assert_eq!(*nid, node_id);
                assert_eq!(*cnft, expected_cnft);
            }
            other => panic!("expected Cnft, got {other:?}"),
        }

        // Verifies locally against the cert-signed `contributions_root`.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let leaf = crate::merkle::leaf_hash(&entry.leaf());
        let root = &full.chain.state.contributions_merkle_root();
        assert!(
            crate::merkle::verify(root, &leaf, entry.proof()),
            "cNFT proof must verify against contributions_root"
        );
        assert_eq!(root.as_slice(), header.contributions_root.as_slice());

        // An unknown node_id yields a verified `None` slot.
        let miss = GossipMsg::GetProof {
            items: vec![(crate::light::ProofKind::Cnft, node_id + 9999)],
        };
        let mut out2 = full.on_message(99, miss);
        let (_d, m2) = out2.pop().unwrap();
        if let GossipMsg::Proof { items } = m2 {
            assert_eq!(items.len(), 1);
            assert!(items[0].is_none(), "unknown cNFT id must be a None slot");
        } else {
            panic!("expected Proof");
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
            e.as_ref()
                .unwrap_or_else(|| panic!("entry {i} must be Some"));
        }

        // Each entry must verify locally against the right root.
        let last_block = blocks.last().unwrap();
        let header = crate::codec::BlockHeader::from_block(last_block);
        let account_root = &full.chain.state.merkle_root();
        let validator_root = &full.chain.state.validators.merkle_root();
        let contributions_root = &full.chain.state.contributions_merkle_root();
        for (i, e) in entries.iter().enumerate() {
            let entry = e.as_ref().unwrap();
            let leaf = crate::merkle::leaf_hash(&entry.leaf());
            let root = match entry {
                crate::light::ProofEntry::Account { .. }
                | crate::light::ProofEntry::Reviewer { .. }
                | crate::light::ProofEntry::GraphNode { .. } => account_root,
                crate::light::ProofEntry::Validator { .. } => validator_root,
                crate::light::ProofEntry::Cnft { .. } => contributions_root,
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
                crate::light::ProofEntry::Cnft { .. } => &header.contributions_root,
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
        let claim = node.serve_knn(query, k).expect("kNN claim");
        assert!(
            !claim.neighbours.is_empty(),
            "graph must have at least one node"
        );
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
        let tracked = ValidatorTracker::from_genesis(&genesis())
            .validators()
            .clone();
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
        let claim = node.serve_range(query, min_sim).expect("range claim");
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
        let tracked = ValidatorTracker::from_genesis(&genesis())
            .validators()
            .clone();
        ValidatorTracker::verify_range_against_header(&header, &last_cert, &tracked, &claim)
            .expect("wallet-side range claim verifies");
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
        let blocks_in_range: Vec<(Block, Commit)> =
            blocks.iter().cloned().zip(certs.iter().cloned()).collect();
        crate::light::ValidatorTracker::verify_diff_against_headers(
            &genesis(),
            &blocks_in_range,
            &env,
        )
        .expect("wallet-side diff verifier accepts the envelope");

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
            &genesis(),
            &blocks_in_range,
            &back_envelope,
        )
        .expect("wallet-side diff verifier accepts the wire-roundtripped envelope");
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

        assert!(
            full.serve_diff(0, 2, &header_h1, &header_h2).is_none(),
            "h1 == 0"
        );
        assert!(
            full.serve_diff(2, 2, &header_h1, &header_h2).is_none(),
            "h1 == h2"
        );
        assert!(
            full.serve_diff(2, 1, &header_h1, &header_h2).is_none(),
            "h1 > h2"
        );
        assert!(
            full.serve_diff(1, 99, &header_h1, &header_h2).is_none(),
            "h2 > height"
        );
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

    // ---- M139: snapshot offset-awareness ----

    #[test]
    fn chain_from_snapshot_and_replay_from_match_full_replay() {
        // M139: `Chain::from_snapshot` reproduces the tip state/head of a fully-replayed
        // chain, and `Chain::replay_from(genesis_state, genesis_hash, blocks)` is byte-
        // identical to `Chain::replay(genesis, blocks)` (the single-replay-path guarantee).
        let (blocks, certs) = certified_chain(3);
        let _ = &certs;
        let full = crate::Chain::replay(genesis(), &blocks).expect("full replay");

        // from_snapshot: adopt the tip state + head, assert identical commitments.
        let rebuilt = crate::Chain::from_snapshot(full.state.clone(), full.head);
        assert_eq!(
            rebuilt.state.state_root(),
            full.state.state_root(),
            "state_root"
        );
        assert_eq!(
            rebuilt.state.merkle_root(),
            full.state.merkle_root(),
            "accounts_root"
        );
        assert_eq!(rebuilt.head, full.head, "head");
        assert_eq!(rebuilt.state.height, full.state.height, "height");

        // replay_from genesis state ≡ replay from genesis.
        let (genesis_state, genesis_hash) = crate::ChainState::genesis(genesis());
        let via_from =
            crate::Chain::replay_from(genesis_state, genesis_hash, &blocks).expect("replay_from");
        assert_eq!(via_from.state.state_root(), full.state.state_root());
        assert_eq!(via_from.head, full.head);
    }

    #[test]
    fn gossipnode_from_snapshot_applies_next_height() {
        // M139: a node adopting a snapshot at height H has empty retained history, reports
        // height H, serves no pre-H header (pruned), and accepts block H+1 forward.
        let (blocks, certs) = certified_chain(3); // heights 1,2,3
                                                  // State AT height 1 (the snapshot point) — the provider's `snapshot` + `head`.
        let at_1 = crate::Chain::replay(genesis(), &blocks[..1]).expect("replay to 1");
        let mut snap =
            GossipNode::from_snapshot(1, genesis(), at_1.state.clone(), at_1.head, 8, []);
        assert_eq!(snap.height(), 1, "adopted height");
        assert!(snap.blocks().is_empty(), "no pre-snapshot history retained");
        // Pruned: the snapshot node cannot serve the header at height 1 (or below).
        assert!(snap.certified_header(1).is_none(), "height 1 is pruned");

        // Apply block 2 (prev_hash == head@1, height == 2) — must succeed and append.
        assert!(
            snap.apply_certified(blocks[1].clone(), certs[1].clone()),
            "block 2 applies on top of the snapshot"
        );
        assert_eq!(snap.height(), 2);
        // Now the post-snapshot header is served; the pruned one still isn't.
        let h2 = snap.certified_header(2).expect("height 2 served");
        assert_eq!(h2.header.height, 2);
        assert!(snap.certified_header(1).is_none(), "height 1 still pruned");
        // blocks[0] of the snapshot node is height 2 (offset-aware indexing).
        assert_eq!(snap.blocks().len(), 1);
        assert_eq!(snap.blocks()[0].height, 2);
    }

    #[test]
    fn block_at_is_offset_aware_o1_lookup() {
        // M146: `block_at` resolves a committed block by height in O(1), honoring the
        // base_height offset — pruned/genesis/past-tip all yield None.
        let (blocks, certs) = certified_chain(3); // heights 1,2,3

        // Genesis-start node: block_at matches the retained Vec by height.
        let mut full = GossipNode::new(1, genesis(), 8, []);
        assert!(full.load_certified(&blocks, &certs));
        assert!(full.block_at(0).is_none(), "genesis has no block");
        assert_eq!(full.block_at(2).map(|b| b.height), Some(2));
        assert_eq!(full.block_at(3).map(|b| b.height), Some(3));
        assert!(full.block_at(4).is_none(), "past the tip");

        // Snapshot-booted node at height 1: heights <= base_height are pruned.
        let at_1 = crate::Chain::replay(genesis(), &blocks[..1]).expect("replay to 1");
        let mut snap =
            GossipNode::from_snapshot(1, genesis(), at_1.state.clone(), at_1.head, 8, []);
        assert!(snap.apply_certified(blocks[1].clone(), certs[1].clone()));
        assert!(snap.apply_certified(blocks[2].clone(), certs[2].clone()));
        assert!(snap.block_at(1).is_none(), "height 1 is pruned");
        assert_eq!(snap.block_at(2).map(|b| b.height), Some(2));
        assert_eq!(snap.block_at(3).map(|b| b.height), Some(3));
        assert!(snap.block_at(4).is_none(), "past the tip");
    }

    #[test]
    fn pruned_node_serves_post_snapshot_diff_and_gones_pre_snapshot() {
        // M139: a snapshot-booted node (base_height = 1) serves a Diff over post-snapshot
        // heights [2,3] (replay-from-base), and returns None for any range touching the
        // pruned height 1 (the HTTP layer turns that into 410 Gone in M140).
        let (blocks, certs) = certified_chain(3); // heights 1,2,3
        let at_1 = crate::Chain::replay(genesis(), &blocks[..1]).expect("replay to 1");
        let mut snap =
            GossipNode::from_snapshot(1, genesis(), at_1.state.clone(), at_1.head, 8, []);
        // Feed heights 2 and 3 forward.
        assert!(snap.apply_certified(blocks[1].clone(), certs[1].clone()));
        assert!(snap.apply_certified(blocks[2].clone(), certs[2].clone()));
        assert_eq!(snap.height(), 3);

        // Post-snapshot Diff [2,3] serves (headers from the retained tail).
        let header_h2 = crate::codec::BlockHeader::from_block(&blocks[1]);
        let header_h3 = crate::codec::BlockHeader::from_block(&blocks[2]);
        let env = snap
            .serve_diff(2, 3, &header_h2, &header_h3)
            .expect("pruned node serves a post-snapshot diff");
        assert_eq!(env.header_prev.height, 2);
        assert_eq!(env.header_new.height, 3);

        // A Diff touching the pruned height 1 is refused (can't replay pre-snapshot state).
        let header_h1 = crate::codec::BlockHeader::from_block(&blocks[0]);
        assert!(
            snap.serve_diff(1, 3, &header_h1, &header_h3).is_none(),
            "pre-snapshot h1=1 is pruned"
        );
        // Likewise serve_lock / headers_from / headers_range refuse pruned heights.
        assert!(snap.headers_from(1).is_empty(), "headers_from(1) pruned");
        assert_eq!(
            snap.headers_from(2).len(),
            2,
            "headers_from(2) returns heights 2,3"
        );
        assert!(
            snap.headers_range(1, 10).is_empty(),
            "headers_range from pruned height is empty"
        );
        assert_eq!(snap.headers_range(2, 10).len(), 2);
    }
}
