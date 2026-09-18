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
}

/// M24: maximum items in a single [`GossipMsg::GetProof`] / [`GossipMsg::Proof`]
/// — over the cap is a codec error (`TooManyItems`). Keeps a single request
/// bounded so a hostile peer can't fan out O(n) work on the responder.
pub const MAX_PROOF_BATCH: usize = 32;

/// Wire-tag assignments. Each GossipMsg variant is one byte; bumping a tag
/// outside the existing range (0..=9) requires a major-version bump.
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
            chain: Chain::new(genesis),
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
        self.blocks.push(block);
        self.certs.push(cert);
        true
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
                    let entry = match kind {
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
                        crate::light::ProofKind::Validator => self
                            .chain
                            .state
                            .validators
                            .proof(id)
                            .and_then(|p| {
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
                            }),
                        crate::light::ProofKind::GraphNode => {
                            // For graph nodes the request id is the **insertion
                            // index** — what the wallet observed from genesis
                            // forward or from a prior block's graph length. The
                            // full peer resolves it to a GraphNode by index and
                            // packages its node_id along with the proof.
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
                    };
                    out.push(entry);
                }
                vec![(from, GossipMsg::Proof { items: out })]
            }
            GossipMsg::Proof { .. } => Vec::new(), // full nodes don't consume proofs
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
            // everything else: light clients forward tx gossip but never store it
            // — for the M22 demo we just drop, mirroring the "I don't care about
            // bodies" SPV stance.
            GossipMsg::Blocks(_)
            | GossipMsg::GetBlocks { .. }
            | GossipMsg::Tx(_)
            | GossipMsg::Evidence(_)
            | GossipMsg::StakeOp(_) => Vec::new(),
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
        other => return Err(CodecError::BadEnum(other as u32)),
    };
    if !rest.is_empty() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(msg)
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
        ];
        for m in &msgs {
            let bytes = encode_gossip(m);
            match decode_gossip(&bytes) {
                Ok(back) => assert_eq!(encode_gossip(&back), bytes, "re-encoding is stable"),
                Err(e) => panic!("decode failed: {e:?} for {m:?}"),
            }
        }
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
}
