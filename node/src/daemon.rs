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

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

use crate::config::{ConfigError, NodeConfig};
use crate::net::{decode_gossip, encode_gossip, GossipMsg, GossipNode};
use crate::round::{Action, Msg, RoundState, Step};
use crate::store::{BlockLog, CertLog};
use crate::{Genesis, Hash, Keypair, SlashEvidence, SubmissionTx};

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

    /// Append any newly-certified blocks the node gained (single-writer durability).
    fn persist(&mut self) {
        let blocks = self.node.blocks();
        let certs = self.node.certificates();
        while self.appended < blocks.len() {
            if let Err(e) = self.blog.append(&blocks[self.appended]) {
                eprintln!("[node {}] append block failed: {e}", self.node.id);
                return;
            }
            if let Err(e) = self.clog.append(&certs[self.appended]) {
                eprintln!("[node {}] append cert failed: {e}", self.node.id);
                return;
            }
            self.appended += 1;
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
            Cmd::Announce => actor.broadcast_status(),
            Cmd::Query(reply) => {
                let _ = reply.send((actor.node.height(), actor.node.head()));
            }
        }
    }
}

// ----------------------------------------------------------------------------
// connection tasks
// ----------------------------------------------------------------------------

/// Drive one TCP connection: handshake, then split into a reader loop (forwards
/// `Inbound` to the actor) and a writer task (drains a per-peer queue). Returns
/// when the connection ends.
async fn handle_conn(stream: TcpStream, my_id: u64, cmd: mpsc::UnboundedSender<Cmd>) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();

    if write_hello(&mut wr, my_id).await.is_err() {
        return;
    }
    let peer_id = match read_hello(&mut rd).await {
        Ok(id) => id,
        Err(_) => return,
    };

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
    writer.abort();
}

async fn run_listener(listener: TcpListener, my_id: u64, cmd: mpsc::UnboundedSender<Cmd>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(handle_conn(stream, my_id, cmd.clone()));
            }
            Err(e) => eprintln!("[node {my_id}] accept error: {e}"),
        }
    }
}

/// Dial a higher-id peer, reconnecting with capped backoff after any drop.
async fn run_connector(addr: SocketAddr, my_id: u64, cmd: mpsc::UnboundedSender<Cmd>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        if let Ok(stream) = TcpStream::connect(addr).await {
            backoff = Duration::from_millis(500);
            handle_conn(stream, my_id, cmd.clone()).await; // returns on disconnect
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(8));
    }
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
        };
        tokio::spawn(run_actor(actor, cmd_rx));

        // inbound listener
        let listener = TcpListener::bind(listen).await?;
        let actual = listener.local_addr()?;
        eprintln!(
            "[node {my_id}] listening on {actual}  peers={}  height={}  role={}",
            peer_ids.len(),
            appended,
            if is_validator { "validator" } else { "follower" },
        );
        tokio::spawn(run_listener(listener, my_id, cmd_tx.clone()));

        // outbound connectors (dial higher ids only → one link per pair)
        for p in &cfg.peers {
            if p.id > my_id {
                let addr = p.socket_addr().map_err(cfg_io)?;
                tokio::spawn(run_connector(addr, my_id, cmd_tx.clone()));
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
    eprintln!("shutdown requested — exiting (logs are fsync'd per append)");
    Ok(())
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
}
