//! M32/M33: the real networked node daemon (tokio TCP P2P transport).
//!
//! Everything below M31 ran in one process: the `Network`/`Sim` buses are
//! in-process `VecDeque`s. This module is the first **long-running, multi-machine
//! daemon**. It reuses the existing pure state machines unchanged — only the
//! *transport* changes from an in-process deque to real sockets:
//!
//!   * [`crate::net::GossipNode::on_message`] stays the sync/anti-entropy core
//!     (no I/O); this module just carries its `Vec<(peer_id, GossipMsg)>` output
//!     over TCP.
//!   * Frames are `u32` big-endian length + [`crate::net::encode_gossip`] body —
//!     the same wire format as the blocking `read_msg`/`write_msg`, reimplemented
//!     over tokio [`AsyncReadExt`]/[`AsyncWriteExt`] with a [`MAX_FRAME`] cap.
//!   * Persistence reuses [`BlockLog`]/[`CertLog`]; boot recovery replays the log
//!     (`load_certified`), re-verifying finality.
//!
//! ## M33: distributed BFT voting (no sequencer)
//!
//! Consensus is **decentralized**. Every validator node owns exactly one
//! [`Keypair`] and drives one [`RoundState`] per height, gossiping
//! proposals/prevotes/precommits over the same TCP transport as
//! [`GossipMsg::Consensus`] and advancing rounds with **wall-clock timeouts**.
//! There is no designated producer: at each height every in-set validator builds
//! and seals its own candidate ([`GossipNode::build_candidate`]); the round's
//! elected proposer's is the one that gets voted on. A decided block routes
//! through the existing [`GossipNode::apply_certified`] (its hash is unchanged by
//! commit, so the certificate still verifies). Nodes without a key are pure
//! followers: they sync and verify certificates but never vote.
//!
//! **Sync always wins.** A validator only runs consensus for `height()+1`;
//! anything it learns via anti-entropy sync supersedes an in-flight round for a
//! now-committed height (`reconcile_after_sync`). Liveness survives ≤ 1/3 faults
//! via round changes; safety holds past 1/3 as a safe stall (no forged commit).
//!
//! The offline `ChainDriver`/`Sim` single-process path is retired from the daemon
//! but kept for the `cmd_bft`/`cmd_live`/`cmd_chain` demos and unit tests.
//!
//! ## Architecture (single-owner actor, no locks)
//!
//! One **actor** task owns the [`GossipNode`], its [`RoundState`], the signing
//! [`Keypair`], and a `peer_id -> Sender` table. Each TCP connection is a pair of
//! tasks (reader + writer); the reader forwards `Inbound { from, msg }` commands
//! to the actor, the writer drains a per-peer queue. The actor is the sole writer
//! of this node's block/cert log and the sole owner of consensus state — it
//! self-schedules via `self_tx` (timeouts, next-height starts), so there are no
//! locks and no shared consensus state across tasks.
//!
//! To avoid duplicate links, a node only dials peers with a **higher id**; the
//! lower-id side accepts. Every pair thus forms exactly one connection.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::{ConfigError, NodeConfig};
use crate::crypto::verify;
use crate::net::{decode_gossip, encode_gossip, GossipMsg, GossipNode};
use crate::round::{Action, Msg, RoundState, Step};
use crate::store::{BlockLog, CertLog};
use crate::{Genesis, Hash, Keypair, PubKey, SlashEvidence, SubmissionTx};

/// Hard cap on a single wire frame (16 MiB). The blocking `read_msg` has no cap
/// (a hostile `u32` length would allocate up to 4 GiB); a real transport must.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// M35: per-node consensus timing + empty-block policy, resolved from
/// [`crate::config::ConsensusConfig`] at boot (was hard-coded module consts
/// pre-M35). Linear back-off `base + round*delta` gives eventual synchrony:
/// rounds lengthen until they outlast message delay.
#[derive(Clone, Copy)]
struct Timing {
    propose_ms: u64,
    prevote_ms: u64,
    precommit_ms: u64,
    delta_ms: u64,
    /// Pacing between committing one height and starting the next (the empty-block
    /// heartbeat interval when `create_empty_blocks` is true).
    block_interval_ms: u64,
    /// When false, a height is started only when there is pending work.
    create_empty_blocks: bool,
}

fn timeout_for(t: &Timing, step: Step, round: u32) -> Duration {
    let base = match step {
        Step::Propose => t.propose_ms,
        Step::Prevote => t.prevote_ms,
        Step::Precommit => t.precommit_ms,
    };
    Duration::from_millis(base + round as u64 * t.delta_ms)
}

// ----------------------------------------------------------------------------
// async wire framing (u32 BE length + encode_gossip body)
// ----------------------------------------------------------------------------

/// Write one length-prefixed [`GossipMsg`] frame.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &GossipMsg) -> io::Result<()> {
    let body = encode_gossip(msg);
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "outbound frame exceeds MAX_FRAME"));
    }
    let len = body.len() as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed [`GossipMsg`] frame.
pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<GossipMsg> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb).await?;
    let len = u32::from_be_bytes(lenb) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "inbound frame exceeds MAX_FRAME"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    decode_gossip(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Transport-level handshake: each side announces its node id as 8 bytes BE on
/// connection open. Kept outside [`GossipMsg`] so the wire-tag range is untouched.
async fn write_hello<W: AsyncWriteExt + Unpin>(w: &mut W, id: u64) -> io::Result<()> {
    w.write_all(&id.to_be_bytes()).await?;
    w.flush().await
}

async fn read_hello<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).await?;
    Ok(u64::from_be_bytes(b))
}

// ----------------------------------------------------------------------------
// M40: authenticated handshake (opt-in `[network] require_peer_auth`)
// ----------------------------------------------------------------------------

/// Domain-separation tag for the authenticated handshake. Prefixing every signed
/// handshake transcript with this ensures a handshake signature can never be
/// mistaken for (or replayed as) a consensus vote / transaction signature — those
/// sign different, non-prefixed byte layouts.
const AUTH_DOMAIN: &[u8] = b"zhixing-node-auth-v1";

/// M40: the exact bytes a peer signs to prove it holds the genesis key for
/// `signer_id`. Binding *both* sides' fresh per-session nonces makes a captured
/// `(nonce, signature)` pair non-replayable and stops a relay from splicing two
/// sessions: `AUTH_DOMAIN || signer_id(8 BE) || signer_nonce || peer_id(8 BE) ||
/// peer_nonce`. Each side signs with itself as `signer`; the verifier reconstructs
/// the peer's transcript with the peer as `signer`.
fn auth_transcript(signer_id: u64, signer_nonce: &[u8; 32], peer_id: u64, peer_nonce: &[u8; 32]) -> Vec<u8> {
    let mut t = Vec::with_capacity(AUTH_DOMAIN.len() + 8 + 32 + 8 + 32);
    t.extend_from_slice(AUTH_DOMAIN);
    t.extend_from_slice(&signer_id.to_be_bytes());
    t.extend_from_slice(signer_nonce);
    t.extend_from_slice(&peer_id.to_be_bytes());
    t.extend_from_slice(peer_nonce);
    t
}

/// M40: read-only handshake auth context, shared (`Arc`) across every connection
/// task and built once in [`Node::start`]. `kp` is a *clone* of the validator
/// signing key used only to sign handshake transcripts (consensus keeps its own
/// owned copy in the [`Actor`]); `validators` is the genesis id→pubkey registry;
/// `require` is the `[network] require_peer_auth` toggle.
struct AuthContext {
    my_id: u64,
    kp: Option<Keypair>,
    validators: HashMap<u64, PubKey>,
    require: bool,
}

/// M40: run the mutually-authenticated handshake and return the authenticated
/// peer id. Both sides send a `HelloInit` (`id(8) || pubkey(32) || nonce(32)`),
/// then each signs the transcript binding both nonces and sends the 64-byte
/// signature. Any I/O failure, an unknown/non-genesis peer id, a pubkey that
/// doesn't match genesis, or a bad signature yields `Err` — the caller then drops
/// the connection. Symmetric (write-then-read for both messages), so two peers
/// dialing each other never deadlock; the payloads (72 B, 64 B) are tiny.
async fn auth_handshake<R, W>(rd: &mut R, wr: &mut W, ctx: &AuthContext) -> io::Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let kp = ctx.kp.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "peer auth required but this node has no signing key")
    })?;
    let my_pk = kp.public();
    let mut my_nonce = [0u8; 32];
    getrandom::getrandom(&mut my_nonce)
        .map_err(|e| io::Error::other(format!("handshake nonce rng failed: {e}")))?;

    // send our HelloInit
    let mut init = Vec::with_capacity(72);
    init.extend_from_slice(&ctx.my_id.to_be_bytes());
    init.extend_from_slice(&my_pk);
    init.extend_from_slice(&my_nonce);
    wr.write_all(&init).await?;
    wr.flush().await?;

    // read the peer's HelloInit
    let mut pi = [0u8; 72];
    rd.read_exact(&mut pi).await?;
    let peer_id = u64::from_be_bytes(pi[..8].try_into().unwrap());
    let mut peer_pk = [0u8; 32];
    peer_pk.copy_from_slice(&pi[8..40]);
    let mut peer_nonce = [0u8; 32];
    peer_nonce.copy_from_slice(&pi[40..72]);

    // sign our transcript and send the signature
    let sig = kp.sign(&auth_transcript(ctx.my_id, &my_nonce, peer_id, &peer_nonce));
    wr.write_all(&sig).await?;
    wr.flush().await?;

    // read the peer's signature
    let mut peer_sig = [0u8; 64];
    rd.read_exact(&mut peer_sig).await?;

    // verify: the peer must be a genesis validator, present the pubkey genesis
    // binds to its id, and sign its own transcript with that key.
    let expected = ctx.validators.get(&peer_id).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, format!("peer {peer_id} is not a genesis validator"))
    })?;
    if peer_pk != *expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer {peer_id} pubkey does not match its genesis validator key"),
        ));
    }
    let transcript = auth_transcript(peer_id, &peer_nonce, ctx.my_id, &my_nonce);
    if !verify(&peer_pk, &transcript, &peer_sig) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("peer {peer_id} handshake signature invalid"),
        ));
    }
    Ok(peer_id)
}

// ----------------------------------------------------------------------------
// actor commands + handle
// ----------------------------------------------------------------------------

enum Cmd {
    /// A peer sent us a message.
    Inbound { from: u64, msg: Box<GossipMsg> },
    /// A connection finished its handshake; register its outbound queue.
    Register { id: u64, tx: mpsc::UnboundedSender<GossipMsg> },
    /// A connection dropped.
    Unregister { id: u64 },
    /// M33: begin (or attempt to begin) consensus for this height. Self-sent on
    /// boot, after each commit, and after sync advances us. Idempotent: ignored
    /// unless we are an in-set validator and `height == node.height()+1`.
    StartHeight { height: u64 },
    /// M33: a previously-armed consensus timeout for (height, step, round) fired.
    Timeout { height: u64, step: Step, round: u32 },
    /// A locally-submitted transaction (from the CLI/demo handle).
    LocalTx(Box<SubmissionTx>),
    /// Periodic anti-entropy heartbeat.
    Announce,
    /// Read this node's (height, head) — used by the demo/tests.
    Query(oneshot::Sender<(u64, Hash)>),
    /// M38: read a richer runtime snapshot for the metrics/health endpoint.
    Metrics(oneshot::Sender<Metrics>),
}

/// M38: a read-only snapshot of the daemon's runtime state, rendered to the
/// Prometheus text-exposition format by [`render_prometheus`]. Built inside the
/// actor (the single owner of all this state) in response to [`Cmd::Metrics`].
#[derive(Debug, Clone)]
pub struct Metrics {
    /// Certified chain height.
    pub height: u64,
    /// Certified chain head hash.
    pub head: Hash,
    /// Number of connected peers (outbound queues).
    pub peers: usize,
    /// This process owns a signing key (in-set validator).
    pub is_validator: bool,
    /// A consensus instance is in flight for `height+1`.
    pub consensus_active: bool,
    /// Pending transactions in the mempool.
    pub mempool: usize,
    /// Pending stake operations awaiting inclusion.
    pub pending_stake_ops: usize,
    /// Pending slashing evidence awaiting inclusion.
    pub pending_evidence: usize,
}

/// A handle to a running node (for the in-process `localnet` demo and tests).
#[derive(Clone)]
pub struct Node {
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Node {
    /// Submit a transaction as if received from the network: it floods to peers
    /// and enters this node's mempool for inclusion in a future candidate.
    pub fn submit(&self, tx: SubmissionTx) {
        let _ = self.cmd.send(Cmd::LocalTx(Box::new(tx)));
    }

    /// Current (height, head) of this node's certified chain.
    pub async fn status(&self) -> Option<(u64, Hash)> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Query(tx)).ok()?;
        rx.await.ok()
    }

    /// M38: a read-only runtime snapshot for the metrics/health endpoint.
    /// Returns `None` if the actor has stopped.
    pub async fn metrics(&self) -> Option<Metrics> {
        let (tx, rx) = oneshot::channel();
        self.cmd.send(Cmd::Metrics(tx)).ok()?;
        rx.await.ok()
    }
}

// ----------------------------------------------------------------------------
// actor
// ----------------------------------------------------------------------------

/// M33: this node's live consensus state for a single height.
struct Consensus {
    /// The height being decided (`node.height()+1`).
    height: u64,
    /// The single-validator BFT state machine.
    round: RoundState,
}

struct Actor {
    node: GossipNode,
    /// One outbound queue per connected peer id.
    outbound: HashMap<u64, mpsc::UnboundedSender<GossipMsg>>,
    /// This process's signing key, or `None` for a pure follower (never votes).
    kp: Option<Keypair>,
    /// Self-scheduling channel (consensus timeouts + next-height starts).
    self_tx: mpsc::UnboundedSender<Cmd>,
    /// Live consensus for `node.height()+1`, if this validator is running one.
    cons: Option<Consensus>,
    /// This node is the sole writer of its own log.
    blog: BlockLog,
    clog: CertLog,
    /// Number of blocks already persisted (index into `node.blocks()`).
    appended: usize,
    /// M35: consensus timing + empty-block policy, resolved from config at boot.
    timing: Timing,
    /// M39: known peer listen addresses (id → "host:port"), seeded from config
    /// (self + configured peers) and grown by address-book gossip. First-wins:
    /// a configured/self addr is authoritative and can't be overwritten by a
    /// peer's claim.
    addrs: HashMap<u64, String>,
    /// M39: ids we've already spawned a connector for (dedup — at most one
    /// outbound dial per peer, whether from config boot or discovery).
    dialing: HashSet<u64>,
    /// M39: whether peer discovery is on (config `[network] enable_peer_exchange`).
    peer_exchange: bool,
    /// M40: shared read-only handshake auth context. Cloned into each connector
    /// (boot + discovered) and the listener so every link runs the same policy.
    auth: Arc<AuthContext>,
}

impl Actor {
    fn route(&self, out: Vec<(u64, GossipMsg)>) {
        for (dst, msg) in out {
            if let Some(tx) = self.outbound.get(&dst) {
                let _ = tx.send(msg);
            }
        }
    }

    /// Broadcast a fresh `Status` to every connected peer (kicks anti-entropy).
    fn broadcast_status(&self) {
        let h = self.node.height();
        for tx in self.outbound.values() {
            let _ = tx.send(GossipMsg::Status { height: h });
        }
    }

    /// M33: flood one consensus message to every connected peer. (Our own vote is
    /// already self-ingested by the `RoundState`, so peers only.)
    fn broadcast_consensus(&self, m: &Msg) {
        for tx in self.outbound.values() {
            let _ = tx.send(GossipMsg::Consensus(Box::new(m.clone())));
        }
    }

    /// M39: snapshot our address book (id → listen) as a gossip message. Includes
    /// our own `(id, my_listen)` so neighbors learn how to dial us — that's what
    /// lets discovery work without changing the hello handshake.
    fn peers_msg(&self) -> GossipMsg {
        GossipMsg::Peers(self.addrs.iter().map(|(id, a)| (*id, a.clone())).collect())
    }

    /// M39: propagate the address book to every connected peer (periodic, so a
    /// newly-learned entry reaches the whole mesh transitively). No-op if peer
    /// exchange is disabled.
    fn gossip_peers(&self) {
        if !self.peer_exchange {
            return;
        }
        let msg = self.peers_msg();
        for tx in self.outbound.values() {
            let _ = tx.send(msg.clone());
        }
    }

    /// M39: ingest a peer's address book. First-wins on the book (config/self
    /// addrs stay authoritative), and any newly-learned higher-id peer we're not
    /// already dialing gets an auto-dial connector — preserving the dial-higher-id
    /// invariant (the lower-id side learns *our* addr from the same gossip and
    /// dials us). No-op if peer exchange is disabled.
    fn on_peers(&mut self, book: Vec<(u64, String)>) {
        if !self.peer_exchange {
            return;
        }
        let my_id = self.node.id;
        for (id, addr) in book {
            if id == my_id {
                continue;
            }
            self.addrs.entry(id).or_insert_with(|| addr.clone());
            if id > my_id && !self.dialing.contains(&id) {
                if let Ok(sa) = addr.parse::<SocketAddr>() {
                    self.dialing.insert(id);
                    let tx = self.self_tx.clone();
                    tokio::spawn(run_connector(sa, self.auth.clone(), tx));
                    info!(node = my_id, peer = id, %addr, "discovered peer, dialing");
                }
            }
        }
    }

    /// Append any newly-certified blocks the node gained (single-writer durability).
    fn persist(&mut self) {
        let blocks = self.node.blocks();
        let certs = self.node.certificates();
        while self.appended < blocks.len() {
            if let Err(e) = self.blog.append(&blocks[self.appended]) {
                error!(node = self.node.id, error = %e, "append block failed");
                return;
            }
            if let Err(e) = self.clog.append(&certs[self.appended]) {
                error!(node = self.node.id, error = %e, "append cert failed");
                return;
            }
            self.appended += 1;
            debug!(node = self.node.id, height = self.appended as u64, "block committed");
        }
    }

    /// M33: arm a wall-clock timeout; when it elapses, self-send a `Timeout`.
    fn arm_timer(&self, height: u64, step: Step, round: u32) {
        let tx = self.self_tx.clone();
        let d = timeout_for(&self.timing, step, round);
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            let _ = tx.send(Cmd::Timeout { height, step, round });
        });
    }

    /// M33: schedule a `StartHeight` after `delay_ms` (paces the empty-block
    /// heartbeat and gives the mesh time to connect at boot).
    fn schedule_start(&self, height: u64, delay_ms: u64) {
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let _ = tx.send(Cmd::StartHeight { height });
        });
    }

    /// M33: perform the side effects a `RoundState` asked for.
    fn apply_actions(&mut self, height: u64, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Broadcast(m) => self.broadcast_consensus(&m),
                Action::Schedule(step, round) => self.arm_timer(height, step, round),
                Action::Decided(commit) => self.on_decided(commit),
                Action::Equivocation(ev) => self.on_equivocation(ev),
            }
        }
    }

    /// M34: we observed two conflicting precommits from the same validator over
    /// gossip. Turn them into slashing evidence, stage it locally (so our own
    /// next proposal carries it), and flood it — the existing M19 evidence
    /// pipeline delivers it into the next block, where `Chain::apply_evidence`
    /// re-verifies both signatures and burns the offender's bond. Repeated
    /// detections are idempotent (`submit_local_evidence` dedups on `hash()`).
    fn on_equivocation(&mut self, ev: SlashEvidence) {
        let out = self.node.submit_local_evidence(ev);
        self.route(out);
    }

    /// M35: scheduled entry point for a height (heartbeat / next-height pacing /
    /// boot). When `create_empty_blocks` is false and there is no pending work,
    /// we do NOT start a round — an idle chain simply pauses at the current height
    /// and re-polls after `block_interval_ms`. When work arrives (gossiped in), the
    /// next tick opens the gate; peers that receive the resulting proposal join via
    /// `on_consensus`'s ungated lazy-start, so liveness holds without every node
    /// independently observing the work first (see `on_consensus`).
    fn on_start_tick(&mut self, height: u64) {
        if self.timing.create_empty_blocks || self.node.has_pending_work() {
            self.start_height(height);
        } else if self.kp.is_some() && self.node.height() + 1 == height {
            // Nothing to propose yet: hold the height and check again later.
            self.schedule_start(height, self.timing.block_interval_ms);
        }
    }

    /// M33: begin consensus for `height`, if we are an eligible in-set validator
    /// and this is exactly our next height. Idempotent and self-guarding.
    ///
    /// This is the ungated core: `on_start_tick` applies the `create_empty_blocks`
    /// gate before calling here, while `on_consensus` calls here directly (a peer's
    /// proposal already implies work).
    fn start_height(&mut self, height: u64) {
        if self.kp.is_none() {
            return; // pure follower
        }
        if self.node.height() + 1 != height {
            return; // stale / ahead — driven only for the immediate next height
        }
        if self.cons.as_ref().is_some_and(|c| c.height == height) {
            return; // already running this height
        }
        let active = self.node.chain.state.validators.clone();
        let val_id = self.node.id;
        if active.get(val_id).is_none() {
            return; // not in the active set for this height
        }
        let candidate = match self.node.build_candidate(height as f32) {
            Some(b) => b,
            None => {
                // unproposable candidate (staged op fails to apply); retry shortly
                self.schedule_start(height, self.timing.block_interval_ms);
                return;
            }
        };
        let mut round = RoundState::new(active, val_id, height, candidate);
        let actions = match self.kp.as_ref() {
            Some(kp) => round.start(kp),
            None => return,
        };
        self.cons = Some(Consensus { height, round });
        self.apply_actions(height, actions);
    }

    /// M33: ingest a gossiped consensus message into the live round.
    fn on_consensus(&mut self, m: Msg) {
        // Lazily start our own round for this height if a peer's timer beat ours:
        // the round-0 proposer broadcasts the moment it commits the previous
        // height, which can reach slower peers before their own `StartHeight`
        // fires. Without this, that early proposal/vote hits `cons == None` and is
        // dropped — the peer then times out and prevotes nil, needlessly failing
        // round 0. (`start_height` self-guards on height and set membership.)
        let mh = match &m {
            Msg::Proposal(p) => p.height,
            Msg::Vote(v) => v.height,
        };
        if self.kp.is_some()
            && mh == self.node.height() + 1
            && self.cons.as_ref().is_none_or(|c| c.height != mh)
        {
            self.start_height(mh);
        }
        // Byzantine-proposer liveness guard: never prevote a proposal whose block
        // cannot actually apply (RoundState only checks height). Dropping it makes
        // honest nodes time out → prevote nil → next proposer.
        if let Msg::Proposal(p) = &m {
            if !self.node.chain.would_accept(&p.block) {
                return;
            }
        }
        let (height, actions) = match (self.kp.as_ref(), self.cons.as_mut()) {
            (Some(kp), Some(cons)) => (cons.height, cons.round.on_message(kp, m)),
            _ => return,
        };
        self.apply_actions(height, actions);
    }

    /// M33: a consensus timeout fired — advance the round if it is still current.
    fn on_timeout(&mut self, height: u64, step: Step, round: u32) {
        let (h, actions) = match (self.kp.as_ref(), self.cons.as_mut()) {
            (Some(kp), Some(cons)) if cons.height == height => {
                (cons.height, cons.round.on_timeout(kp, step, round))
            }
            _ => return, // stale timer for a height we already left
        };
        self.apply_actions(h, actions);
    }

    /// M33: consensus finalized a block — commit it, persist, tell peers, and
    /// queue the next height.
    fn on_decided(&mut self, commit: crate::consensus::Commit) {
        let block = match self.cons.as_ref().and_then(|c| c.round.decided_block().cloned()) {
            Some(b) => b,
            None => return,
        };
        // hash is unchanged by commit (block was sealed), so the certificate still
        // verifies against the active set inside apply_certified.
        if self.node.apply_certified(block, commit) {
            self.persist();
            self.broadcast_status();
        }
        self.cons = None;
        self.schedule_start(self.node.height() + 1, self.timing.block_interval_ms);
    }

    /// M33: sync always wins. Called after an inbound advanced our height: any
    /// round we were running for a now-committed height is obsolete, so drop it
    /// and (re)arm consensus for the new next height.
    fn reconcile_after_sync(&mut self) {
        if self.kp.is_none() {
            return; // pure follower never runs consensus
        }
        let stale = self.cons.as_ref().is_none_or(|c| self.node.height() >= c.height);
        if stale {
            self.cons = None;
            self.schedule_start(self.node.height() + 1, self.timing.block_interval_ms);
        }
    }
}

async fn run_actor(mut actor: Actor, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Register { id, tx } => {
                // Kick anti-entropy: tell the new peer our height immediately.
                let _ = tx.send(GossipMsg::Status { height: actor.node.height() });
                // M39: kick discovery — hand the new peer our address book (incl.
                // our own listen addr) so it can learn + dial the rest of the mesh.
                if actor.peer_exchange {
                    let _ = tx.send(actor.peers_msg());
                }
                actor.outbound.insert(id, tx);
            }
            Cmd::Unregister { id } => {
                actor.outbound.remove(&id);
            }
            Cmd::Inbound { from, msg } => {
                let msg = *msg;
                // M33: consensus messages bypass the pure gossip core (which drops
                // them) and drive this node's RoundState directly.
                if let GossipMsg::Consensus(m) = msg {
                    actor.on_consensus(*m);
                    continue;
                }
                // M39: address-book gossip is likewise Actor-handled (the pure
                // core drops it); it drives peer discovery + auto-dial.
                if let GossipMsg::Peers(book) = msg {
                    actor.on_peers(book);
                    continue;
                }
                let before = actor.node.height();
                let out = actor.node.on_message(from, msg);
                actor.route(out);
                actor.persist();
                if actor.node.height() > before {
                    actor.broadcast_status();
                    actor.reconcile_after_sync();
                }
            }
            Cmd::LocalTx(tx) => {
                let out = actor.node.submit_local(*tx);
                actor.route(out);
            }
            Cmd::StartHeight { height } => actor.on_start_tick(height),
            Cmd::Timeout { height, step, round } => actor.on_timeout(height, step, round),
            Cmd::Announce => {
                actor.broadcast_status();
                // M39: re-propagate the address book so newly-learned peers reach
                // the whole mesh transitively (no-op when exchange is disabled).
                actor.gossip_peers();
            }
            Cmd::Query(reply) => {
                let _ = reply.send((actor.node.height(), actor.node.head()));
            }
            Cmd::Metrics(reply) => {
                let _ = reply.send(Metrics {
                    height: actor.node.height(),
                    head: actor.node.head(),
                    peers: actor.outbound.len(),
                    is_validator: actor.kp.is_some(),
                    consensus_active: actor.cons.is_some(),
                    mempool: actor.node.mempool.len(),
                    pending_stake_ops: actor.node.pending_stake_ops().len(),
                    pending_evidence: actor.node.pending_evidence().len(),
                });
            }
        }
    }
}

// ----------------------------------------------------------------------------
// connection tasks
// ----------------------------------------------------------------------------

/// Drive one TCP connection: handshake, then split into a reader loop (forwards
/// `Inbound` to the actor) and a writer task (drains a per-peer queue). Returns
/// when the connection ends. M40: when `ctx.require` is set the handshake is the
/// mutually-authenticated [`auth_handshake`]; otherwise it is the pre-M40
/// cleartext [`write_hello`]/[`read_hello`] (byte-identical back-compat).
async fn handle_conn(stream: TcpStream, ctx: Arc<AuthContext>, cmd: mpsc::UnboundedSender<Cmd>) {
    let my_id = ctx.my_id;
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();

    let peer_id = if ctx.require {
        match auth_handshake(&mut rd, &mut wr, &ctx).await {
            Ok(id) => id,
            Err(e) => {
                warn!(node = my_id, error = %e, "authenticated handshake rejected");
                return;
            }
        }
    } else {
        if write_hello(&mut wr, my_id).await.is_err() {
            return;
        }
        match read_hello(&mut rd).await {
            Ok(id) => id,
            Err(_) => return,
        }
    };
    info!(node = my_id, peer = peer_id, "peer connected");

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<GossipMsg>();
    if cmd.send(Cmd::Register { id: peer_id, tx: out_tx }).is_err() {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut wr, &msg).await.is_err() {
                break;
            }
        }
    });

    while let Ok(msg) = read_frame(&mut rd).await {
        if cmd.send(Cmd::Inbound { from: peer_id, msg: Box::new(msg) }).is_err() {
            break;
        }
    }

    let _ = cmd.send(Cmd::Unregister { id: peer_id });
    info!(node = my_id, peer = peer_id, "peer disconnected");
    writer.abort();
}

async fn run_listener(listener: TcpListener, ctx: Arc<AuthContext>, cmd: mpsc::UnboundedSender<Cmd>) {
    let my_id = ctx.my_id;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(handle_conn(stream, ctx.clone(), cmd.clone()));
            }
            Err(e) => warn!(node = my_id, error = %e, "accept error"),
        }
    }
}

/// Dial a higher-id peer, reconnecting with capped backoff after any drop.
async fn run_connector(addr: SocketAddr, ctx: Arc<AuthContext>, cmd: mpsc::UnboundedSender<Cmd>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        if let Ok(stream) = TcpStream::connect(addr).await {
            backoff = Duration::from_millis(500);
            handle_conn(stream, ctx.clone(), cmd.clone()).await; // returns on disconnect
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(8));
    }
}

// ----------------------------------------------------------------------------
// M38: metrics / health endpoint
// ----------------------------------------------------------------------------

/// Render a [`Metrics`] snapshot to the Prometheus text-exposition format
/// (v0.0.4). Pure — this is the unit-testable core of the endpoint. Every gauge
/// gets a `# HELP`/`# TYPE` pair; the head hash rides a `zhixing_head_info`
/// info-gauge label so it is queryable without being a numeric metric.
fn render_prometheus(m: &Metrics) -> String {
    let head = crate::hash::hex(&m.head);
    let mut s = String::with_capacity(1024);
    let gauge = |s: &mut String, name: &str, help: &str, value: u64| {
        s.push_str(&format!("# HELP {name} {help}\n"));
        s.push_str(&format!("# TYPE {name} gauge\n"));
        s.push_str(&format!("{name} {value}\n"));
    };
    gauge(&mut s, "zhixing_height", "Certified chain height.", m.height);
    gauge(&mut s, "zhixing_peers_connected", "Connected peers.", m.peers as u64);
    gauge(
        &mut s,
        "zhixing_is_validator",
        "1 if this node owns a signing key (in-set validator), else 0.",
        m.is_validator as u64,
    );
    gauge(
        &mut s,
        "zhixing_consensus_active",
        "1 if a consensus instance is in flight, else 0.",
        m.consensus_active as u64,
    );
    gauge(&mut s, "zhixing_mempool_txs", "Pending transactions in the mempool.", m.mempool as u64);
    gauge(
        &mut s,
        "zhixing_pending_stake_ops",
        "Pending stake operations awaiting inclusion.",
        m.pending_stake_ops as u64,
    );
    gauge(
        &mut s,
        "zhixing_pending_evidence",
        "Pending slashing evidence awaiting inclusion.",
        m.pending_evidence as u64,
    );
    s.push_str("# HELP zhixing_head_info Certified chain head hash (as a label).\n");
    s.push_str("# TYPE zhixing_head_info gauge\n");
    s.push_str(&format!("zhixing_head_info{{head=\"{head}\"}} 1\n"));
    s
}

/// Accept loop for the metrics/health endpoint. Mirrors [`run_listener`]: each
/// connection is handled on its own task; an accept error is logged and the loop
/// continues (a transient error must not take the endpoint down).
async fn run_metrics(listener: TcpListener, my_id: u64, cmd: mpsc::UnboundedSender<Cmd>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(serve_metrics_conn(stream, cmd.clone()));
            }
            Err(e) => warn!(node = my_id, error = %e, "metrics accept error"),
        }
    }
}

/// Serve one metrics request: best-effort discard the HTTP request (bounded), ask
/// the actor for a snapshot, and write a fixed-shape `HTTP/1.1 200 OK` reply whose
/// body is the Prometheus exposition. Any path returns metrics, so a bare `GET /`
/// doubles as a health check (`200` ⇒ alive). Errors are swallowed — a broken
/// client connection must never affect the node.
async fn serve_metrics_conn(mut stream: TcpStream, cmd: mpsc::UnboundedSender<Cmd>) {
    // Drain the request headers so the client's write side is satisfied, but cap
    // the read so a malformed/never-terminated request can't hang or grow the
    // buffer without bound. We don't parse it — every request returns metrics.
    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break, // client closed
            Ok(n) => {
                total += n;
                // End of request headers, or the read cap — stop reading either way.
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || total >= 8192 {
                    break;
                }
            }
            Err(_) => return,
        }
    }

    let (tx, rx) = oneshot::channel();
    if cmd.send(Cmd::Metrics(tx)).is_err() {
        return; // actor gone
    }
    let Ok(snapshot) = rx.await else { return };
    let body = render_prometheus(&snapshot);
    let resp = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/plain; version=0.0.4\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        body.len(),
        body,
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

// ----------------------------------------------------------------------------
// startup + run
// ----------------------------------------------------------------------------

fn cfg_io(e: ConfigError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}

impl Node {
    /// Boot a node from a parsed config: open logs, recover state, spawn the
    /// actor + listener + peer connectors, and (for an enabled validator) queue
    /// the first consensus height. Tasks are detached and run until the tokio
    /// runtime is dropped. Returns a handle for local tx submission / status
    /// queries.
    ///
    /// `validator_key` is this process's single signing key (M33: one key per
    /// node, no sequencer). `None` ⇒ a pure follower that syncs and verifies
    /// certificates but never votes. If present, its public key MUST match this
    /// node's entry in `genesis.validators`, or startup fails fast.
    pub async fn start(
        cfg: NodeConfig,
        genesis: Genesis,
        validator_key: Option<Keypair>,
    ) -> io::Result<Node> {
        let my_id = cfg.node.id;
        let listen = cfg.listen_addr().map_err(cfg_io)?;

        // Fail fast on a misconfigured validator key: it must be the key genesis
        // assigns this node's id, else this node could never cast a valid vote.
        if let Some(kp) = &validator_key {
            match genesis.validators.iter().find(|(id, _, _)| *id == my_id) {
                Some((_, pk, _)) if *pk == kp.public() => {}
                Some(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("validator key for node {my_id} does not match its genesis pubkey"),
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("node {my_id} has a validator key but is not in genesis.validators"),
                    ));
                }
            }
        }

        // M40: strict peer auth requires this node to prove its own identity in the
        // handshake, which it can only do with a signing key. A keyless follower
        // could never complete an authenticated handshake, so refuse to start
        // rather than silently fail every dial (fail-fast, mirrors the check above).
        if cfg.network.require_peer_auth && validator_key.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("node {my_id} sets require_peer_auth but has no validator signing key"),
            ));
        }

        // logs + boot recovery
        let bpath = format!("{}/blocks.log", cfg.node.data_dir);
        let cpath = format!("{}/certs.log", cfg.node.data_dir);
        let blog = BlockLog::open(&bpath)?;
        let clog = CertLog::open(&cpath)?;
        let blocks = blog.read_all()?;
        let certs = clog.read_all()?;

        let peer_ids: Vec<u64> = cfg.peers.iter().map(|p| p.id).collect();

        let mut node = GossipNode::new(my_id, genesis.clone(), 64, peer_ids.iter().copied());
        if !blocks.is_empty() && !node.load_certified(&blocks, &certs) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "persisted log failed verified load (torn or unproven chain)",
            ));
        }
        let appended = blocks.len();

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

        let is_validator = validator_key.is_some();
        let timing = Timing {
            propose_ms: cfg.consensus.propose_timeout_ms,
            prevote_ms: cfg.consensus.prevote_timeout_ms,
            precommit_ms: cfg.consensus.precommit_timeout_ms,
            delta_ms: cfg.consensus.timeout_delta_ms,
            block_interval_ms: cfg.consensus.block_interval_ms,
            create_empty_blocks: cfg.consensus.create_empty_blocks,
        };
        // M39: seed the address book with our own listen addr + every configured
        // peer, and mark the higher-id peers as already-dialing (the boot
        // connectors below cover them — don't let discovery re-dial). Discovery
        // grows both sets as address-book gossip arrives.
        let mut addrs: HashMap<u64, String> = HashMap::new();
        addrs.insert(my_id, cfg.node.listen.clone());
        let mut dialing: HashSet<u64> = HashSet::new();
        for p in &cfg.peers {
            addrs.entry(p.id).or_insert_with(|| p.addr.clone());
            if p.id > my_id {
                dialing.insert(p.id);
            }
        }
        // M40: build the shared handshake auth context. Clone the signing key
        // (the consensus actor keeps its own owned copy below), snapshot the
        // genesis id→pubkey registry, and carry the `require_peer_auth` policy.
        let validators: HashMap<u64, PubKey> =
            genesis.validators.iter().map(|(id, pk, _)| (*id, *pk)).collect();
        let auth = Arc::new(AuthContext {
            my_id,
            kp: validator_key.clone(),
            validators,
            require: cfg.network.require_peer_auth,
        });

        let actor = Actor {
            node,
            outbound: HashMap::new(),
            kp: validator_key,
            self_tx: cmd_tx.clone(),
            cons: None,
            blog,
            clog,
            appended,
            timing,
            addrs,
            dialing,
            peer_exchange: cfg.network.enable_peer_exchange,
            auth: auth.clone(),
        };
        tokio::spawn(run_actor(actor, cmd_rx));

        // inbound listener
        let listener = TcpListener::bind(listen).await?;
        let actual = listener.local_addr()?;
        info!(
            node = my_id,
            addr = %actual,
            peers = peer_ids.len(),
            height = appended,
            role = if is_validator { "validator" } else { "follower" },
            "listening",
        );
        tokio::spawn(run_listener(listener, auth.clone(), cmd_tx.clone()));

        // outbound connectors (dial higher ids only → one link per pair)
        for p in &cfg.peers {
            if p.id > my_id {
                let addr = p.socket_addr().map_err(cfg_io)?;
                tokio::spawn(run_connector(addr, auth.clone(), cmd_tx.clone()));
            }
        }

        // periodic anti-entropy heartbeat
        {
            let cmd = cmd_tx.clone();
            let announce_ms = cfg.network.announce_interval_ms;
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(announce_ms));
                loop {
                    tick.tick().await;
                    if cmd.send(Cmd::Announce).is_err() {
                        break;
                    }
                }
            });
        }

        // M38: opt-in read-only metrics/health endpoint. Bound only when the
        // `[metrics]` section is present and `enabled = true`; otherwise this is a
        // no-op and the daemon behaves exactly as before.
        if let Some(mc) = cfg.metrics.as_ref().filter(|m| m.enabled) {
            let addr = mc.listen_addr().map_err(cfg_io)?;
            let mlistener = TcpListener::bind(addr).await?;
            info!(node = my_id, addr = %mlistener.local_addr()?, "metrics listening");
            tokio::spawn(run_metrics(mlistener, my_id, cmd_tx.clone()));
        }

        // M33: kick off consensus for the first height after the mesh has had a
        // moment to dial + handshake. A follower ignores this (no key). The
        // decide→next-height and sync-reconcile chains keep it going thereafter.
        if is_validator {
            let cmd = cmd_tx.clone();
            let next = appended as u64 + 1;
            let startup_ms = cfg.network.startup_delay_ms;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(startup_ms)).await;
                let _ = cmd.send(Cmd::StartHeight { height: next });
            });
        }

        Ok(Node { cmd: cmd_tx })
    }
}

/// CLI entry point for `node run`: start the daemon and block until Ctrl-C.
pub async fn run(
    cfg: NodeConfig,
    genesis: Genesis,
    validator_key: Option<Keypair>,
) -> io::Result<()> {
    let _node = Node::start(cfg, genesis, validator_key).await?;
    tokio::signal::ctrl_c().await?;
    info!("shutdown requested — exiting (logs are fsync'd per append)");
    Ok(())
}

/// M37: install a process-global `tracing` subscriber (stderr, `RUST_LOG`-filtered,
/// default `info`). Idempotent — safe to call from either entry point, twice, or
/// after a test has already set a global default (`try_init` error is swallowed).
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip_over_duplex() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        // include an M33 consensus frame (a signed precommit) to exercise the new
        // TAG_CONSENSUS wire path over the async framing.
        let vote = crate::Vote::signed(21, 4, 0, [9u8; 32], crate::VoteType::Precommit, &kp(21));
        let cons = GossipMsg::Consensus(Box::new(crate::round::Msg::Vote(vote)));
        let msgs = vec![
            GossipMsg::Status { height: 7 },
            GossipMsg::GetBlocks { from: 3 },
            cons.clone(),
        ];
        let expect = msgs.clone();
        let writer = tokio::spawn(async move {
            for m in &msgs {
                write_frame(&mut a, m).await.unwrap();
            }
        });
        for want in expect {
            let got = read_frame(&mut b).await.unwrap();
            // re-encoding equality is a kind-agnostic round-trip check.
            assert_eq!(encode_gossip(&want), encode_gossip(&got), "frame round-trip");
        }
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        // A length header larger than MAX_FRAME must error before allocating.
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            let len = (MAX_FRAME as u32) + 1;
            a.write_all(&len.to_be_bytes()).await.unwrap();
            // no body needed; read_frame should reject on the length alone
            let _ = a.flush().await;
        });
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn hello_handshake_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            write_hello(&mut a, 4242).await.unwrap();
        });
        assert_eq!(read_hello(&mut b).await.unwrap(), 4242);
        writer.await.unwrap();
    }

    // --- M40: authenticated handshake (pure core) -------------------------------

    #[test]
    fn auth_transcript_is_deterministic_and_order_sensitive() {
        let n1 = [1u8; 32];
        let n2 = [2u8; 32];
        let a = auth_transcript(21, &n1, 22, &n2);
        assert_eq!(a, auth_transcript(21, &n1, 22, &n2), "same inputs ⇒ identical bytes");
        // Swapping the signer/peer roles must change the bytes: each side signs a
        // *distinct* transcript, so one side's signature can't be replayed as the
        // other's.
        assert_ne!(a, auth_transcript(22, &n2, 21, &n1));
        // The domain tag is a prefix (cross-protocol signature separation).
        assert!(a.starts_with(AUTH_DOMAIN));
        assert_eq!(a.len(), AUTH_DOMAIN.len() + 8 + 32 + 8 + 32);
    }

    #[test]
    fn auth_sign_verify_round_trip() {
        let n1 = [3u8; 32];
        let n2 = [4u8; 32];
        let t = auth_transcript(21, &n1, 22, &n2);
        let sig = kp(21).sign(&t);
        assert!(verify(&kp(21).public(), &t, &sig), "the correct genesis key verifies");
        // A different validator's signature over the same transcript must fail —
        // exactly what stops an impostor from authenticating as validator 21.
        let forged = kp(22).sign(&t);
        assert!(!verify(&kp(21).public(), &t, &forged));
    }

    // --- integration: three in-process nodes over loopback TCP converge --------

    use crate::validator::{Validator, ValidatorSet};
    use crate::consensus::Commit;
    use crate::{Block, Keypair, Review, MICRO};
    use std::collections::BTreeMap;
    use zhixing_engine::{DeltaKParams, DIM};

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
    fn test_genesis() -> Genesis {
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
    fn test_vset() -> ValidatorSet {
        ValidatorSet::new(
            [21u64, 22, 23, 24]
                .iter()
                .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
                .collect(),
        )
    }
    fn test_tx(author: u64, dim: usize, domain: u32) -> SubmissionTx {
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

    fn tmp_dir(tag: &str) -> String {
        let p = std::env::temp_dir().join(format!("zhixing-daemon-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p.to_string_lossy().into_owned()
    }

    fn node_config(id: u64, port_base: u16, ids: &[u64], data_dir: String) -> NodeConfig {
        let addr = |i: u64| format!("127.0.0.1:{}", port_base + (i - 21) as u16);
        NodeConfig {
            node: crate::config::NodeSection { id, listen: addr(id), data_dir },
            peers: ids
                .iter()
                .filter(|&&p| p != id)
                .map(|&p| crate::config::PeerConfig { id: p, addr: addr(p) })
                .collect(),
            genesis: String::new(),
            // Tests hand the signing key to `Node::start` directly, so the config's
            // own `[validator]` section is irrelevant here (it's read only by the
            // `node run` CLI in main.rs).
            validator: None,
            consensus: crate::config::ConsensusConfig::default(),
            network: crate::config::NetworkConfig::default(),
            metrics: None,
        }
    }

    /// Poll every node's status until all report `height >= target` and share an
    /// identical `(height, head)` snapshot, or the deadline passes. Returns the
    /// converged snapshot. With the M33 empty-block heartbeat all validators sit
    /// at the same committed head between heights, so a lockstep snapshot is the
    /// common case.
    async fn await_converged(
        nodes: &[(u64, Node)],
        target: u64,
        within: Duration,
    ) -> Vec<(u64, Hash)> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let mut states: Vec<(u64, Hash)> = Vec::new();
            for (_id, node) in nodes {
                states.push(node.status().await.unwrap_or((0, [0u8; 32])));
            }
            let converged = states.iter().all(|(h, _)| *h >= target)
                && states.windows(2).all(|w| w[0] == w[1]);
            if converged {
                return states;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "nodes did not converge to height {target}: {states:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait until a node's persisted block log holds at least `n` fully-flushed
    /// records, then return the first `n` blocks + certs. Tolerates the rare torn
    /// tail of a log being actively appended by retrying.
    async fn read_prefix(dir: &str, n: usize, within: Duration) -> (Vec<Block>, Vec<Commit>) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let blocks = BlockLog::open(format!("{dir}/blocks.log")).and_then(|l| l.read_all());
            let certs = CertLog::open(format!("{dir}/certs.log")).and_then(|l| l.read_all());
            if let (Ok(mut b), Ok(mut c)) = (blocks, certs) {
                if b.len() >= n && c.len() >= n {
                    b.truncate(n);
                    c.truncate(n);
                    return (b, c);
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "log for {dir} never reached {n} records");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn cleanup(dirs: &BTreeMap<u64, String>) {
        for dir in dirs.values() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    // --- integration: distributed BFT over loopback TCP (M33) -------------------

    #[tokio::test]
    async fn four_validators_converge_over_tcp() {
        // Four validators, no sequencer: each owns one key and votes. They should
        // agree on identical certified heads driven purely by prevote/precommit
        // gossip over real sockets.
        let ids = [21u64, 22, 23, 24];
        let port_base = 19531u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("conv-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &ids, dir);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // let the mesh dial + handshake, then feed txs to one node (they flood)
        tokio::time::sleep(Duration::from_millis(300)).await;
        for t in [test_tx(1, 1, 1), test_tx(2, 2, 2), test_tx(3, 3, 3)] {
            nodes[0].1.submit(t);
        }

        let target = 3u64;
        let states = await_converged(&nodes, target, Duration::from_secs(40)).await;
        let (h0, head0) = states[0];
        assert!(h0 >= target);
        assert!(states.iter().all(|&(h, head)| h == h0 && head == head0));

        // reload node 22's persisted log and re-verify that the first `target`
        // heights each carry a > 2/3 BFT certificate — real finality over the
        // wire, produced by distributed voting, not a sequencer.
        let vset = test_vset();
        let (blocks, certs) = read_prefix(&data_dirs[&22], target as usize, Duration::from_secs(5)).await;
        crate::Chain::replay_verified(genesis.clone(), &blocks, &certs).expect("finality");
        for c in &certs {
            assert!(c.verify(&vset).unwrap() * 3 > vset.total_power() * 2);
        }

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn one_crashed_validator_still_makes_progress() {
        // 3 of 4 live (quorum is 3): consensus advances, exercising round changes
        // whenever the dead node (24) is the elected proposer.
        let ids = [21u64, 22, 23, 24];
        let live = [21u64, 22, 23];
        let port_base = 19551u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &live {
            let dir = tmp_dir(&format!("crash1-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &ids, dir); // peers still list 24
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // no txs needed: the empty-block heartbeat advances heights on its own.
        let target = 2u64;
        let states = await_converged(&nodes, target, Duration::from_secs(60)).await;
        assert!(states.iter().all(|&(h, _)| h >= target));

        let vset = test_vset();
        let (blocks, certs) = read_prefix(&data_dirs[&21], target as usize, Duration::from_secs(5)).await;
        crate::Chain::replay_verified(genesis.clone(), &blocks, &certs).expect("finality with 1 fault");
        for c in &certs {
            // each cert is still a > 2/3 quorum of the FULL set (3 of 4 suffices).
            assert!(c.verify(&vset).unwrap() * 3 > vset.total_power() * 2);
        }

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn two_crashed_validators_stall_safely() {
        // 2 of 4 live: quorum (3) is unreachable, so consensus must NOT advance —
        // safety holds past 1/3 faults as a safe stall, never a forged commit.
        let ids = [21u64, 22, 23, 24];
        let live = [21u64, 22];
        let port_base = 19571u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &live {
            let dir = tmp_dir(&format!("crash2-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &ids, dir);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // give consensus ample time to try (and fail) several rounds
        tokio::time::sleep(Duration::from_secs(8)).await;
        for (_id, node) in &nodes {
            let (h, _) = node.status().await.unwrap();
            assert_eq!(h, 0, "no block may commit without a quorum");
        }

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn late_joiner_syncs_then_participates() {
        // Three validators advance a few heights; the fourth starts late, catches
        // up via anti-entropy sync, then advances in lockstep with the group.
        let ids = [21u64, 22, 23, 24];
        let port_base = 19591u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &[21u64, 22, 23] {
            let dir = tmp_dir(&format!("late-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &ids, dir);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // let the trio commit a few heights on their own
        let head_start = 2u64;
        await_converged(&nodes, head_start, Duration::from_secs(60)).await;

        // now bring up node 24
        let dir = tmp_dir("late-n24");
        data_dirs.insert(24, dir.clone());
        let cfg = node_config(24, port_base, &ids, dir);
        let node24 = Node::start(cfg, genesis.clone(), Some(kp(24))).await.expect("start late node");
        nodes.push((24, node24));

        // all four should reach a common height beyond where the trio started
        let target = head_start + 2;
        let states = await_converged(&nodes, target, Duration::from_secs(60)).await;
        let (h0, head0) = states[0];
        assert!(states.iter().all(|&(h, head)| h == h0 && head == head0));

        // the late joiner recovered real finality, not just an equal head
        let (blocks, certs) = read_prefix(&data_dirs[&24], target as usize, Duration::from_secs(5)).await;
        crate::Chain::replay_verified(genesis.clone(), &blocks, &certs).expect("late joiner finality");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn pure_follower_syncs_certified_chain() {
        // Four validators + one keyless follower (id 25). The follower syncs and
        // persists the certified chain but never votes.
        let vids = [21u64, 22, 23, 24];
        let all = [21u64, 22, 23, 24, 25];
        let port_base = 19611u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &all {
            let dir = tmp_dir(&format!("follow-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &all, dir);
            // id 25 has no key ⇒ pure follower.
            let key = vids.contains(&id).then(|| kp(id));
            let node = Node::start(cfg, genesis.clone(), key).await.expect("start node");
            nodes.push((id, node));
        }

        let target = 3u64;
        let states = await_converged(&nodes, target, Duration::from_secs(60)).await;
        let (h0, head0) = states[0];
        assert!(states.iter().all(|&(h, head)| h == h0 && head == head0));

        // the follower (25) persisted and can re-verify finality it never helped
        // produce.
        let (blocks, certs) = read_prefix(&data_dirs[&25], target as usize, Duration::from_secs(5)).await;
        crate::Chain::replay_verified(genesis.clone(), &blocks, &certs).expect("follower finality");

        cleanup(&data_dirs);
    }

    // --- integration: active slashing on observed equivocation (M34) -------------

    #[tokio::test]
    async fn equivocation_over_tcp_slashes_the_offender() {
        // Genesis has validators 21-24, but only 22/23/24 run honestly (quorum 3,
        // so 3-of-4 still progresses). Validator 21 is Byzantine: a raw TCP peer
        // signs *two* conflicting precommits per height under 21's key and floods
        // them. The honest nodes must observe the double-sign, originate slashing
        // evidence, carry it into a block, and burn/remove validator 21 — all over
        // real sockets, with no coordinator.
        use crate::consensus::{Vote, VoteType};

        let live = [22u64, 23, 24];
        let port_base = 19631u16;
        let genesis = test_genesis(); // validators = 21,22,23,24

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &live {
            let dir = tmp_dir(&format!("equiv-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = node_config(id, port_base, &live, dir);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // Open a Byzantine connection to each honest node, presenting as id 21.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut injectors = Vec::new();
        for &id in &live {
            let addr = format!("127.0.0.1:{}", port_base + (id - 21) as u16);
            let stream = TcpStream::connect(&addr).await.expect("byzantine connect");
            let _ = stream.set_nodelay(true);
            let (mut rd, mut wr) = stream.into_split();
            write_hello(&mut wr, 21).await.expect("hello");
            let _ = read_hello(&mut rd).await;
            // Drain (and discard) everything the honest node sends us.
            tokio::spawn(async move { while read_frame(&mut rd).await.is_ok() {} });
            injectors.push(wr);
        }

        // Flood two conflicting precommits for the live consensus height until an
        // honest node commits a block carrying evidence against validator 21.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(50);
        let mut ev_height: Option<u64> = None;
        while tokio::time::Instant::now() < deadline {
            let h = nodes[0].1.status().await.map(|(h, _)| h).unwrap_or(0);
            let target = h + 1; // the height the honest nodes are voting on now
            let va = Vote::signed(21, target, 0, [1u8; 32], VoteType::Precommit, &kp(21));
            let vb = Vote::signed(21, target, 0, [2u8; 32], VoteType::Precommit, &kp(21));
            let ma = GossipMsg::Consensus(Box::new(Msg::Vote(va)));
            let mb = GossipMsg::Consensus(Box::new(Msg::Vote(vb)));
            for wr in injectors.iter_mut() {
                let _ = write_frame(wr, &ma).await;
                let _ = write_frame(wr, &mb).await;
            }

            // Has any committed block admitted evidence against 21 yet?
            if let Ok(blocks) =
                BlockLog::open(format!("{}/blocks.log", data_dirs[&22])).and_then(|l| l.read_all())
            {
                if let Some(b) = blocks
                    .iter()
                    .find(|b| b.slashing_evidence.iter().any(|e| e.vote_a.validator == 21))
                {
                    ev_height = Some(b.height);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        let ev_height = ev_height.expect("evidence against validator 21 was committed on-chain");

        // Replay the certified chain through the slashing block and confirm the
        // offender is gone from the active set — the double-sign was punished.
        let (blocks, certs) =
            read_prefix(&data_dirs[&22], ev_height as usize, Duration::from_secs(5)).await;
        let chain = crate::Chain::replay_verified(genesis.clone(), &blocks, &certs).expect("finality");
        assert!(
            chain.state.validators.get(21).is_none(),
            "validator 21 must be removed after being slashed for equivocation"
        );

        cleanup(&data_dirs);
    }

    // --- M35: config-driven timing + create_empty_blocks ------------------------

    #[test]
    fn timeout_for_uses_configured_bases_and_delta() {
        // Per-step bases and the linear back-off delta are read straight from the
        // Timing struct (no hard-coded consts), so operator config flows through.
        let t = Timing {
            propose_ms: 200,
            prevote_ms: 300,
            precommit_ms: 400,
            delta_ms: 50,
            block_interval_ms: 1000,
            create_empty_blocks: true,
        };
        // round 0 → base only
        assert_eq!(timeout_for(&t, Step::Propose, 0), Duration::from_millis(200));
        assert_eq!(timeout_for(&t, Step::Prevote, 0), Duration::from_millis(300));
        assert_eq!(timeout_for(&t, Step::Precommit, 0), Duration::from_millis(400));
        // round 2 → base + 2*delta
        assert_eq!(timeout_for(&t, Step::Propose, 2), Duration::from_millis(300));
        assert_eq!(timeout_for(&t, Step::Prevote, 2), Duration::from_millis(400));
        assert_eq!(timeout_for(&t, Step::Precommit, 2), Duration::from_millis(500));
    }

    #[tokio::test]
    async fn create_empty_blocks_false_pauses_then_advances_on_work() {
        // With empty-block heartbeats disabled, an idle chain must hold its height
        // (no blocks produced), then advance exactly on demand when real work is
        // gossiped in — proving the on_start_tick gate over real sockets.
        let ids = [22u64, 23, 24];
        let port_base = 19651u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("ceb-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.consensus.create_empty_blocks = false;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // Let the mesh dial + handshake, then sit idle: no work ⇒ no blocks.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        for (_id, node) in &nodes {
            let (h, _) = node.status().await.expect("status");
            assert_eq!(h, 0, "idle chain must not produce empty heartbeat blocks");
        }

        // Submit one real tx to a single node; it floods to the mesh and unblocks
        // consensus for exactly one non-empty block.
        nodes[0].1.submit(test_tx(1, 1, 1));

        let states = await_converged(&nodes, 1, Duration::from_secs(40)).await;
        let (h0, head0) = states[0];
        assert!(h0 >= 1);
        assert!(states.iter().all(|&(h, head)| h == h0 && head == head0));

        // The committed height-1 block must carry the submitted tx (not empty).
        let (blocks, _certs) = read_prefix(&data_dirs[&22], 1, Duration::from_secs(5)).await;
        assert_eq!(blocks[0].height, 1);
        assert!(
            !blocks[0].txs.is_empty(),
            "the block that broke the idle pause must carry the submitted work"
        );

        cleanup(&data_dirs);
    }

    // ------------------------------------------------------------------------
    // M38: metrics / health endpoint
    // ------------------------------------------------------------------------

    fn sample_metrics() -> Metrics {
        Metrics {
            height: 7,
            head: [0xab; 32],
            peers: 3,
            is_validator: true,
            consensus_active: false,
            mempool: 5,
            pending_stake_ops: 2,
            pending_evidence: 1,
        }
    }

    #[test]
    fn render_prometheus_emits_all_gauges() {
        let out = render_prometheus(&sample_metrics());
        // Every gauge carries its value and a `# TYPE … gauge` declaration.
        for (name, value) in [
            ("zhixing_height", 7),
            ("zhixing_peers_connected", 3),
            ("zhixing_is_validator", 1),
            ("zhixing_consensus_active", 0),
            ("zhixing_mempool_txs", 5),
            ("zhixing_pending_stake_ops", 2),
            ("zhixing_pending_evidence", 1),
        ] {
            assert!(out.contains(&format!("# TYPE {name} gauge")), "missing TYPE for {name}");
            assert!(out.contains(&format!("\n{name} {value}\n")), "missing `{name} {value}`");
        }
    }

    #[test]
    fn render_prometheus_encodes_role_and_head() {
        // Follower with a live consensus round: role 0, consensus 1.
        let m = Metrics { is_validator: false, consensus_active: true, ..sample_metrics() };
        let out = render_prometheus(&m);
        assert!(out.contains("\nzhixing_is_validator 0\n"));
        assert!(out.contains("\nzhixing_consensus_active 1\n"));
        // The full head hex rides the info-gauge label.
        let head = crate::hash::hex(&m.head);
        assert!(out.contains(&format!("zhixing_head_info{{head=\"{head}\"}} 1")));
    }

    #[tokio::test]
    async fn metrics_handle_reports_snapshot() {
        // A single fresh validator: genesis height 0, no peers dialed, has a key.
        let dir = tmp_dir("metrics-handle");
        let cfg = node_config(21, 19671, &[21], dir.clone());
        let node = Node::start(cfg, test_genesis(), Some(kp(21))).await.expect("start node");

        let m = node.metrics().await.expect("metrics snapshot");
        assert_eq!(m.height, 0);
        assert!(m.is_validator);
        assert_eq!(m.peers, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_prometheus_over_tcp() {
        // Enable the endpoint on a fixed loopback port, then scrape it over TCP.
        let dir = tmp_dir("metrics-endpoint");
        let mut cfg = node_config(21, 19691, &[21], dir.clone());
        let metrics_addr = "127.0.0.1:19791";
        cfg.metrics = Some(crate::config::MetricsConfig {
            enabled: true,
            listen: metrics_addr.into(),
        });
        let _node = Node::start(cfg, test_genesis(), Some(kp(21))).await.expect("start node");

        // The listener binds during start(), but give the accept task a beat.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut stream = TcpStream::connect(metrics_addr).await.expect("connect metrics");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("send request");

        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.expect("read response");
        let text = String::from_utf8_lossy(&resp);

        assert!(text.starts_with("HTTP/1.1 200 OK"), "expected 200, got: {text}");
        assert!(text.contains("zhixing_height 0"), "missing height gauge: {text}");
        assert!(text.contains("# TYPE zhixing_height gauge"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- integration: peer discovery / address gossip (M39) ---------------------

    /// Build a node config with an explicit (partial) peer list, so a test can
    /// seed a topology that is *not* a full mesh.
    fn discovery_config(
        id: u64,
        port_base: u16,
        peers: &[u64],
        data_dir: String,
        enable_peer_exchange: bool,
    ) -> NodeConfig {
        let addr = |i: u64| format!("127.0.0.1:{}", port_base + (i - 21) as u16);
        NodeConfig {
            node: crate::config::NodeSection { id, listen: addr(id), data_dir },
            peers: peers
                .iter()
                .map(|&p| crate::config::PeerConfig { id: p, addr: addr(p) })
                .collect(),
            genesis: String::new(),
            validator: None,
            consensus: crate::config::ConsensusConfig::default(),
            network: crate::config::NetworkConfig {
                enable_peer_exchange,
                ..crate::config::NetworkConfig::default()
            },
            metrics: None,
        }
    }

    #[tokio::test]
    async fn discovery_completes_partial_mesh() {
        // Seed a CHAIN topology (not a full mesh): 21 knows only 22; 22 knows
        // 21+23; 23 knows only 22. With peer exchange on (default), 22's address
        // book must propagate 23's listen addr to 21, which then auto-dials it —
        // a link that was never in 21's config. Node 21's peer count reaching 2
        // proves address-book propagation + auto-dial.
        let port_base = 19711u16;
        let genesis = test_genesis();
        let seeds: [(u64, Vec<u64>); 3] = [(21, vec![22]), (22, vec![21, 23]), (23, vec![22])];

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for (id, peers) in seeds {
            let dir = tmp_dir(&format!("disc-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = discovery_config(id, port_base, &peers, dir, true);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // Poll node 21's peer count until it reaches 2 — it dialed 23, which was
        // never in its own config.
        let node21 = &nodes[0].1;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let peers = node21.metrics().await.map(|m| m.peers).unwrap_or(0);
            if peers >= 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "node 21 never discovered a 2nd peer (peers={peers})"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn peer_exchange_disabled_stays_seeded() {
        // Same chain seed, but with `enable_peer_exchange = false`: node 21 must
        // stay pinned to its single configured peer (22). No address-book gossip
        // is sent (not even on the periodic announce), so it never learns 23. This
        // both guards the opt-out toggle and proves the previous test genuinely
        // depends on discovery.
        let port_base = 19731u16;
        let genesis = test_genesis();
        let seeds: [(u64, Vec<u64>); 3] = [(21, vec![22]), (22, vec![21, 23]), (23, vec![22])];

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for (id, peers) in seeds {
            let dir = tmp_dir(&format!("noexch-n{id}"));
            data_dirs.insert(id, dir.clone());
            let cfg = discovery_config(id, port_base, &peers, dir, false);
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // Wait past one announce tick (default 2000 ms) to prove even the heartbeat
        // path doesn't leak an address book when exchange is off.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let peers = nodes[0].1.metrics().await.map(|m| m.peers).unwrap_or(0);
        assert_eq!(peers, 1, "with exchange off, node 21 must stay at its 1 seeded peer");

        cleanup(&data_dirs);
    }

    // --- integration: authenticated handshake / peer auth (M40) ------------------

    #[tokio::test]
    async fn authenticated_mesh_converges() {
        // Three validators with `require_peer_auth = true` and their genesis keys.
        // The mutually-authenticated handshake must succeed end-to-end over real
        // sockets, so consensus still runs and they converge to a shared head
        // (quorum 3-of-4). This proves the authenticated path is fully functional.
        let ids = [22u64, 23, 24];
        let port_base = 19751u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("auth-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.network.require_peer_auth = true;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        // If authentication works, consensus proceeds and heads agree.
        let states = await_converged(&nodes, 2, Duration::from_secs(30)).await;
        assert!(states.windows(2).all(|w| w[0] == w[1]), "authenticated mesh converged");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn impostor_without_key_is_rejected() {
        // One honest validator with peer auth on. A raw TCP peer completes the
        // handshake *shape* but claims validator id 22 while presenting a pubkey
        // that isn't 22's genesis key. The honest node must reject it — its peer
        // count stays 0 (no Register), so an unauthenticated impostor never lands
        // on the vote path.
        let port_base = 19771u16;
        let genesis = test_genesis();
        let dir = tmp_dir("impostor-n21");
        let mut data_dirs = BTreeMap::new();
        data_dirs.insert(21u64, dir.clone());

        let mut cfg = node_config(21, port_base, &[21], dir);
        cfg.network.require_peer_auth = true;
        let node = Node::start(cfg, genesis.clone(), Some(kp(21))).await.expect("start node");

        // Give the listener a moment, then connect as a bogus peer.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let addr = format!("127.0.0.1:{port_base}");
        let stream = TcpStream::connect(&addr).await.expect("connect");
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();

        // HelloInit: claim id 22, but present kp(99) — not 22's genesis key.
        let claimed_id = 22u64;
        let wrong = kp(99);
        let mut init = Vec::with_capacity(72);
        init.extend_from_slice(&claimed_id.to_be_bytes());
        init.extend_from_slice(&wrong.public());
        init.extend_from_slice(&[7u8; 32]); // our nonce
        wr.write_all(&init).await.expect("send hello init");
        wr.flush().await.expect("flush");
        // Read the honest node's HelloInit (confirms it spoke auth, not plain hello).
        let mut hi = [0u8; 72];
        rd.read_exact(&mut hi).await.expect("honest hello init");
        // Send a signature; it fails the pubkey/genesis check regardless.
        let sig = wrong.sign(b"bogus");
        let _ = wr.write_all(&sig).await;
        let _ = wr.flush().await;

        // The honest node must never register this peer.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let peers = node.metrics().await.map(|m| m.peers).unwrap_or(99);
        assert_eq!(peers, 0, "impostor without the genesis key must be rejected");

        cleanup(&data_dirs);
    }
}
