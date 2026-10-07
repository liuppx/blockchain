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
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::{rustls, TlsAcceptor, TlsConnector};
use tracing::{debug, error, info, warn};

use crate::codec::CertifiedHeader;
use crate::config::{ConfigError, NodeConfig};
use crate::consensus::Commit;
use crate::crypto::verify;
use crate::light::{BatchItem, BatchResponseEnvelope, ProofEntry, ProofKind};
use crate::net::{decode_gossip, encode_gossip, GossipMsg, GossipNode};
use crate::round::{Action, Msg, RoundState, Step};
use crate::store::{BlockLog, CertLog};
use crate::{Account, Block, Genesis, Hash, Keypair, PubKey, SlashEvidence, SubmissionTx};

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

/// M42: RFC 5705/8446 exporter label for the channel-binding value mixed into the
/// auth transcript when `[network] bind_channel` is on. Domain-separated from
/// `AUTH_DOMAIN` so the exported secret can never collide with any signed layout.
const CHANNEL_BINDING_LABEL: &[u8] = b"zhixing-node-channel-binding-v1";

/// M43: fixed DER prefix of an ed25519 private key in PKCS#8 v1 form. The full
/// encoding is this 16-byte header followed by the 32-byte seed — exactly what
/// `ring`'s `Ed25519KeyPair::from_pkcs8_maybe_unchecked` (reached via rustls's
/// `any_eddsa_type`) parses. Lets a node build its TLS credential straight from
/// its genesis seed, so its TLS identity *is* its consensus key.
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// M43: fixed DER prefix of an ed25519 SubjectPublicKeyInfo. A full SPKI is this
/// 12-byte header followed by the 32-byte public key (44 bytes total). With RFC
/// 7250 raw public keys the TLS "certificate" *is* this SPKI, so a genesis-pinned
/// verifier extracts the peer's ed25519 key by validating this prefix and slicing
/// off the trailing 32 bytes.
const ED25519_SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

/// M40: the exact bytes a peer signs to prove it holds the genesis key for
/// `signer_id`. Binding *both* sides' fresh per-session nonces makes a captured
/// `(nonce, signature)` pair non-replayable and stops a relay from splicing two
/// sessions: `AUTH_DOMAIN || signer_id(8 BE) || signer_nonce || peer_id(8 BE) ||
/// peer_nonce`. Each side signs with itself as `signer`; the verifier reconstructs
/// the peer's transcript with the peer as `signer`.
///
/// M42: when `channel_binding` is `Some`, the 32-byte TLS keying-material exporter
/// is appended, tying the signature to *this* TLS channel. Both endpoints of one
/// TLS session derive the identical exporter (RFC 5705/8446), so the transcript
/// stays symmetric; a MITM's two distinct TLS legs derive different exporters, so a
/// relayed signature no longer verifies. `None` ⇒ byte-identical to the M40 layout.
fn auth_transcript(
    signer_id: u64,
    signer_nonce: &[u8; 32],
    peer_id: u64,
    peer_nonce: &[u8; 32],
    channel_binding: Option<&[u8; 32]>,
) -> Vec<u8> {
    let mut t = Vec::with_capacity(AUTH_DOMAIN.len() + 8 + 32 + 8 + 32 + 32);
    t.extend_from_slice(AUTH_DOMAIN);
    t.extend_from_slice(&signer_id.to_be_bytes());
    t.extend_from_slice(signer_nonce);
    t.extend_from_slice(&peer_id.to_be_bytes());
    t.extend_from_slice(peer_nonce);
    if let Some(cb) = channel_binding {
        t.extend_from_slice(cb);
    }
    t
}

/// M40: read-only handshake auth context, shared (`Arc`) across every connection
/// task and built once in [`Node::start`]. `kp` is a *clone* of the validator
/// signing key used only to sign handshake transcripts (consensus keeps its own
/// owned copy in the [`Actor`]); `validators` is the genesis id→pubkey registry;
/// `require` is the `[network] require_peer_auth` toggle.
///
/// M41: `tls` (built from `[network] enable_tls`) is `Some` when every connection
/// must be wrapped in TLS 1.3 before the handshake runs. Encryption-only: the
/// same `TlsAcceptor`/`TlsConnector` (cheap `Arc`-backed clones) serve inbound and
/// outbound links; peer *authentication* remains `require`/`auth_handshake`.
///
/// M42: `bind_channel` (from `[network] bind_channel`) folds each connection's TLS
/// keying-material exporter into the auth transcript, binding the authenticated
/// identity to that TLS channel. It presupposes `tls.is_some()` && `require` — the
/// daemon fails fast at boot otherwise.
struct AuthContext {
    my_id: u64,
    kp: Option<Keypair>,
    validators: HashMap<u64, PubKey>,
    require: bool,
    tls: Option<TlsSetup>,
    bind_channel: bool,
}

/// M41: TLS material for the P2P transport, built once in [`Node::start`]. Both
/// handles are `Arc`-backed inside `tokio-rustls`, so cloning into each connection
/// task is cheap. The acceptor serves inbound links; the connector dials outbound
/// links. With `[network] require_peer_certs` off (M41) it presents an ephemeral
/// self-signed cert and accepts any peer cert ([`AcceptAnyServerCert`]); with it on
/// (M43) both sides present their genesis ed25519 key and require the peer's to be a
/// genesis validator ([`GenesisPinnedVerifier`]).
#[derive(Clone)]
struct TlsSetup {
    acceptor: TlsAcceptor,
    connector: TlsConnector,
}

/// M43: inputs for building the genesis-pinned mTLS [`TlsSetup`] (present only when
/// `[network] require_peer_certs` is on). `seed` is this node's genesis ed25519 seed
/// (its TLS credential is derived from it, RFC 7250 raw public key); `validators` is
/// the set of genesis validator pubkeys a presented peer key is checked against.
struct MtlsMaterial {
    seed: [u8; 32],
    validators: Arc<HashSet<PubKey>>,
}

/// M41: any byte stream a peer connection can run over — a raw [`TcpStream`] (TLS
/// off) or a `tokio_rustls` TLS stream (TLS on). Boxing behind this trait lets
/// [`handle_conn`] and the framing/handshake helpers stay a single, non-generic
/// code path regardless of transport.
trait PeerStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> PeerStream for T {}

/// M40: run the mutually-authenticated handshake and return the authenticated
/// peer id. Both sides send a `HelloInit` (`id(8) || pubkey(32) || nonce(32)`),
/// then each signs the transcript binding both nonces and sends the 64-byte
/// signature. Any I/O failure, an unknown/non-genesis peer id, a pubkey that
/// doesn't match genesis, or a bad signature yields `Err` — the caller then drops
/// the connection. Symmetric (write-then-read for both messages), so two peers
/// dialing each other never deadlock; the payloads (72 B, 64 B) are tiny.
///
/// M42: when `ctx.bind_channel` is set, `binding` (this connection's TLS exporter)
/// is mixed into both the signed and verified transcripts. `Err` if the flag is on
/// but no binding was supplied (no TLS) — the boot-time fail-fast normally prevents
/// this, so it is a defensive guard.
async fn auth_handshake<R, W>(
    rd: &mut R,
    wr: &mut W,
    ctx: &AuthContext,
    binding: Option<&[u8; 32]>,
) -> io::Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let kp = ctx.kp.as_ref().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "peer auth required but this node has no signing key")
    })?;
    if ctx.bind_channel && binding.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "channel binding required but the connection has no TLS exporter",
        ));
    }
    // Only fold the binding in when the policy is on (byte-identical to M40 otherwise).
    let cb = if ctx.bind_channel { binding } else { None };
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
    let sig = kp.sign(&auth_transcript(ctx.my_id, &my_nonce, peer_id, &peer_nonce, cb));
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
    let transcript = auth_transcript(peer_id, &peer_nonce, ctx.my_id, &my_nonce, cb);
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
    /// M53: a transaction submitted via the external ingress RPC. Unlike
    /// `LocalTx` this carries a oneshot ack so the HTTP handler can report accept
    /// (with the tx hash) or reject (with the `ChainError`).
    SubmitTx {
        tx: Box<SubmissionTx>,
        reply: oneshot::Sender<Result<Hash, crate::ChainError>>,
    },
    /// Periodic anti-entropy heartbeat.
    Announce,
    /// Read this node's (height, head) — used by the demo/tests.
    Query(oneshot::Sender<(u64, Hash)>),
    /// M58: read one account snapshot for the read-class RPC. `None` ⇒ unknown id.
    QueryAccount {
        id: u64,
        reply: oneshot::Sender<Option<Account>>,
    },
    /// M65: plain (unverified) read of a reviewer/validator/graph entity by id, the
    /// non-proof sibling of `QueryAccount`. `None` ⇒ no such entity (404). Account has
    /// its own richer plain read (`QueryAccount`).
    QueryEntity {
        kind: ProofKind,
        id: u64,
        reply: oneshot::Sender<Option<EntityView>>,
    },
    /// M59: read one account's inclusion proof + the certified head it verifies
    /// against, for the verifiable read-class RPC. `None` ⇒ unknown id or no
    /// certified head yet (height 0).
    QueryAccountProof {
        id: u64,
        reply: oneshot::Sender<Option<(CertifiedHeader, ProofEntry)>>,
    },
    /// M60: read any `ProofKind`'s inclusion proof + the certified head it verifies
    /// against (reviewer/validator/graph node), for the verifiable read-class RPC.
    /// `None` ⇒ unknown id/index or no certified head yet (height 0).
    QueryInclusion {
        kind: ProofKind,
        id: u64,
        reply: oneshot::Sender<Option<(CertifiedHeader, ProofEntry)>>,
    },
    /// M63: read one bridge lock's self-contained `LockEnvelope` (header + cert +
    /// tracked set + lock + proof), for the verifiable read-class RPC. `None` ⇒
    /// unknown lock id.
    QueryLock {
        lock_id: u64,
        reply: oneshot::Sender<Option<crate::bridge::LockEnvelope>>,
    },
    /// M64: list all bridge locks (id + height + fields) for the plain read-class RPC
    /// directory. Always a (possibly empty) list — an empty chain is a valid `200`.
    QueryLocks {
        reply: oneshot::Sender<crate::light::LockListing>,
    },
    /// M61: serve a heterogeneous proof batch + the certified head it verifies
    /// against, for the verifiable batch read RPC. `None` ⇒ `serve_batch` rejected
    /// (over `MAX_BATCH_ITEMS` or a degenerate Diff range) or no certified head
    /// yet (height 0).
    QueryBatch {
        items: Vec<BatchItem>,
        reply: oneshot::Sender<Option<crate::light::BatchReply>>,
    },
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
    /// M54: configured mempool pending-pool capacity bound (`usize::MAX` ⇒
    /// unbounded). Reported so saturation (`mempool` vs `mempool_capacity`) is
    /// observable.
    pub mempool_capacity: usize,
    /// M57: configured per-account pending-tx bound (`usize::MAX` ⇒ unbounded).
    /// Reported alongside `mempool` so per-account saturation policy is observable.
    pub mempool_per_account_limit: usize,
    /// M55: current entries in the gossip tx dedup set (`seen_tx`), the set under
    /// flood pressure. Reported so saturation (`seen_tx` vs `seen_tx_capacity`) is
    /// observable.
    pub seen_tx: usize,
    /// M55: configured per-set dedup bound (`usize::MAX` ⇒ unbounded). Renders as a
    /// large number when unbounded — the honest "no bound" signal.
    pub seen_tx_capacity: usize,
    /// Pending stake operations awaiting inclusion.
    pub pending_stake_ops: usize,
    /// Pending slashing evidence awaiting inclusion.
    pub pending_evidence: usize,
    /// M52: cumulative peer registrations since boot (monotonic counter).
    pub peer_connects: u64,
    /// M52: cumulative txs submitted to this node's local API (monotonic counter).
    pub local_txs: u64,
    /// M52: cumulative blocks this node finalized via its own consensus round
    /// (monotonic counter; anti-entropy-synced blocks are not counted here).
    pub blocks_committed: u64,
    /// M52: cumulative equivocation events this node observed + submitted
    /// (monotonic counter).
    pub slashing_events: u64,
    /// M54: cumulative gossip txs dropped by the per-peer rate limiter
    /// (monotonic counter; `0` when rate limiting is disabled).
    pub txs_rate_limited: u64,
    /// M57: cumulative admissions rejected by the per-account mempool quota
    /// (monotonic counter; `0` when the quota is disabled).
    pub txs_quota_rejected: u64,
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

    /// M53: submit a transaction through the external-ingress path, awaiting the
    /// actor's admission result: `Ok(hash)` if it entered the mempool (and was
    /// flooded to peers), `Err(ChainError)` if it failed validation. Returns
    /// `None` if the actor has stopped.
    pub async fn submit_tx(&self, tx: SubmissionTx) -> Option<Result<Hash, crate::ChainError>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::SubmitTx { tx: Box::new(tx), reply }).ok()?;
        rx.await.ok()
    }

    /// M58: read one account snapshot via the read-class query path. The outer
    /// `None` means the actor has stopped; the inner `None` means no such account.
    pub async fn account(&self, id: u64) -> Option<Option<Account>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryAccount { id, reply }).ok()?;
        rx.await.ok()
    }

    /// M65: plain (unverified) read of a reviewer/validator/graph entity by id (the
    /// non-proof sibling of `account`). Outer `None` ⇒ actor stopped; inner `None` ⇒
    /// no such entity.
    pub async fn entity(&self, kind: ProofKind, id: u64) -> Option<Option<EntityView>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryEntity { kind, id, reply }).ok()?;
        rx.await.ok()
    }

    /// M59: read one account's inclusion proof + the certified head it verifies
    /// against, via the verifiable read-class query path. Outer `None` ⇒ actor
    /// stopped; inner `None` ⇒ unknown account or no certified head yet.
    pub async fn account_proof(
        &self,
        id: u64,
    ) -> Option<Option<(CertifiedHeader, ProofEntry)>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryAccountProof { id, reply }).ok()?;
        rx.await.ok()
    }

    /// M60: read any `ProofKind`'s inclusion proof + the certified head it verifies
    /// against (reviewer/validator/graph node), generalizing M59's `account_proof`.
    /// Outer `None` ⇒ actor stopped; inner `None` ⇒ unknown id/index or no cert head.
    pub async fn proof(
        &self,
        kind: ProofKind,
        id: u64,
    ) -> Option<Option<(CertifiedHeader, ProofEntry)>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryInclusion { kind, id, reply }).ok()?;
        rx.await.ok()
    }

    /// M63: read one bridge lock's self-contained `LockEnvelope`, via the
    /// verifiable read-class query path. Outer `None` ⇒ actor stopped; inner
    /// `None` ⇒ unknown lock id.
    pub async fn lock_proof(&self, lock_id: u64) -> Option<Option<crate::bridge::LockEnvelope>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryLock { lock_id, reply }).ok()?;
        rx.await.ok()
    }

    /// M64: list every bridge lock on the chain (id + height + fields), via the plain
    /// read-class query path. `None` ⇒ actor stopped; the inner list may be empty.
    pub async fn lock_listing(&self) -> Option<crate::light::LockListing> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryLocks { reply }).ok()?;
        rx.await.ok()
    }

    /// M61: serve a heterogeneous proof batch + the certified head it verifies
    /// against, for the verifiable batch read RPC — the RPC analogue of the gossip
    /// GetBatch path. Outer `None` ⇒ actor stopped; inner `None` ⇒ `serve_batch`
    /// rejected (over cap / degenerate Diff range) or no cert head yet. M62: the
    /// third tuple element is the `[1..=max_h2]` block range Diff slots replay.
    pub async fn batch_proof(
        &self,
        items: Vec<BatchItem>,
    ) -> Option<Option<crate::light::BatchReply>> {
        let (reply, rx) = oneshot::channel();
        self.cmd.send(Cmd::QueryBatch { items, reply }).ok()?;
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
    /// M52: monotonic metrics counters (actor-owned — the actor task is the sole
    /// writer, so plain `+= 1` needs no atomics). Surfaced via `Cmd::Metrics`.
    peer_connects: u64,
    local_txs: u64,
    blocks_committed: u64,
    slashing_events: u64,
    /// M54: per-peer gossip-tx token buckets (DoS hardening). Keyed by peer id;
    /// the actor task is the sole writer. Empty when rate limiting is disabled.
    peer_tx_buckets: HashMap<u64, TokenBucket>,
    /// M54: token refill rate (tx/sec) and bucket capacity, from `[mempool]`.
    /// `tx_rate <= 0.0` ⇒ rate limiting disabled (all gossip txs admitted).
    tx_rate: f64,
    tx_burst: f64,
    /// M54: count of gossip txs dropped by the per-peer rate limiter.
    txs_rate_limited: u64,
}

/// M54: a minimal token bucket for per-peer gossip rate limiting. Refills
/// continuously at `rate` tokens/sec up to `burst`; `allow` consumes one token.
struct TokenBucket {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn allow(&mut self, now: Instant, rate: f64, burst: f64) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Actor {
    fn route(&self, out: Vec<(u64, GossipMsg)>) {
        for (dst, msg) in out {
            if let Some(tx) = self.outbound.get(&dst) {
                let _ = tx.send(msg);
            }
        }
    }

    /// M54: per-peer gossip-tx admission gate. Returns `true` (admit) when rate
    /// limiting is disabled (`tx_rate <= 0.0`); otherwise refills and consumes one
    /// token from `from`'s bucket (seeded full on first sight), returning whether
    /// a token was available.
    fn allow_peer_tx(&mut self, from: u64) -> bool {
        if self.tx_rate <= 0.0 {
            return true;
        }
        let now = Instant::now();
        let burst = self.tx_burst;
        let bucket = self
            .peer_tx_buckets
            .entry(from)
            .or_insert_with(|| TokenBucket { tokens: burst, last: now });
        bucket.allow(now, self.tx_rate, burst)
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
        self.slashing_events += 1; // M52
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
            self.blocks_committed += 1; // M52
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
                actor.peer_connects += 1; // M52
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
                // M54: per-peer rate limiting applies only to gossip-flooded txs
                // (the operator's own LocalTx/SubmitTx ingress is never throttled).
                // Disabled by default (`tx_rate <= 0.0`), so this is a no-op gate
                // unless an operator opts in via `[mempool]`.
                if let GossipMsg::Tx(_) = &msg {
                    if !actor.allow_peer_tx(from) {
                        actor.txs_rate_limited += 1;
                        continue;
                    }
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
                actor.local_txs += 1; // M52
                let out = actor.node.submit_local(*tx);
                actor.route(out);
            }
            Cmd::SubmitTx { tx, reply } => {
                // M53: an RPC submit is a local-API submit — bump the M52 counter
                // unconditionally, then admit-or-reject and ack the caller either
                // way (flooding to peers only on admission).
                actor.local_txs += 1;
                match actor.node.submit_local_checked(*tx) {
                    Ok((h, out)) => {
                        actor.route(out);
                        let _ = reply.send(Ok(h));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
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
            Cmd::QueryAccount { id, reply } => {
                let _ = reply.send(actor.node.chain.state.accounts.get(&id).cloned());
            }
            Cmd::QueryEntity { kind, id, reply } => {
                // M65: plain read — simple state lookup, built inline like QueryAccount.
                let st = &actor.node.chain.state;
                let view = match kind {
                    ProofKind::Reviewer => st
                        .reviewers
                        .get(&id)
                        .map(|&r| EntityView::Reviewer { id, reputation: r }),
                    ProofKind::Validator => st
                        .validators
                        .get(id)
                        .map(|v| EntityView::Validator { id, power: v.power, pubkey: v.pubkey }),
                    ProofKind::GraphNode => st.graph.nodes.get(id as usize).map(|n| {
                        EntityView::GraphNode {
                            node_id: n.node_id,
                            domain: n.domain,
                            embedding: n.embedding,
                        }
                    }),
                    // Account has its own plain read (QueryAccount); never routed here.
                    ProofKind::Account => None,
                };
                let _ = reply.send(view);
            }
            Cmd::QueryAccountProof { id, reply } => {
                let _ = reply.send(actor.node.account_inclusion(id));
            }
            Cmd::QueryInclusion { kind, id, reply } => {
                let _ = reply.send(actor.node.inclusion(kind, id));
            }
            Cmd::QueryLock { lock_id, reply } => {
                let _ = reply.send(actor.node.serve_lock(lock_id));
            }
            Cmd::QueryLocks { reply } => {
                let _ = reply.send(actor.node.lock_listing());
            }
            Cmd::QueryBatch { items, reply } => {
                let _ = reply.send(actor.node.batch(items));
            }
            Cmd::Metrics(reply) => {
                let _ = reply.send(Metrics {
                    height: actor.node.height(),
                    head: actor.node.head(),
                    peers: actor.outbound.len(),
                    is_validator: actor.kp.is_some(),
                    consensus_active: actor.cons.is_some(),
                    mempool: actor.node.mempool.len(),
                    mempool_capacity: actor.node.mempool.capacity(),
                    mempool_per_account_limit: actor.node.mempool.per_account_limit(),
                    seen_tx: actor.node.seen_tx_len(),
                    seen_tx_capacity: actor.node.seen_tx_capacity(),
                    pending_stake_ops: actor.node.pending_stake_ops().len(),
                    pending_evidence: actor.node.pending_evidence().len(),
                    peer_connects: actor.peer_connects,
                    local_txs: actor.local_txs,
                    blocks_committed: actor.blocks_committed,
                    slashing_events: actor.slashing_events,
                    txs_rate_limited: actor.txs_rate_limited,
                    txs_quota_rejected: actor.node.mempool.rejected_quota(),
                });
            }
        }
    }
}

// ----------------------------------------------------------------------------
// connection tasks
// ----------------------------------------------------------------------------

/// Drive one peer connection: handshake, then split into a reader loop (forwards
/// `Inbound` to the actor) and a writer task (drains a per-peer queue). Returns
/// when the connection ends. M40: when `ctx.require` is set the handshake is the
/// mutually-authenticated [`auth_handshake`]; otherwise it is the pre-M40
/// cleartext [`write_hello`]/[`read_hello`] (byte-identical back-compat). M41: the
/// `stream` is already TLS-wrapped by the caller when `[network] enable_tls` is on,
/// so the handshake and all framing run unchanged over the encrypted transport.
/// M42: `binding` carries this connection's TLS keying-material exporter (`Some`
/// only when TLS is on and `[network] bind_channel` is set); it is folded into the
/// authenticated handshake to defeat a MITM relay.
async fn handle_conn(
    stream: Box<dyn PeerStream>,
    binding: Option<[u8; 32]>,
    ctx: Arc<AuthContext>,
    cmd: mpsc::UnboundedSender<Cmd>,
) {
    let my_id = ctx.my_id;
    let (mut rd, mut wr) = tokio::io::split(stream);

    let peer_id = if ctx.require {
        match auth_handshake(&mut rd, &mut wr, &ctx, binding.as_ref()).await {
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
                let ctx = ctx.clone();
                let cmd = cmd.clone();
                // Wrap (TLS accept) inside the spawned task so a slow or hostile
                // TLS handshake never blocks the accept loop.
                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    match server_wrap(&ctx, stream).await {
                        Ok((s, binding)) => handle_conn(s, binding, ctx, cmd).await,
                        Err(e) => warn!(node = my_id, error = %e, "TLS accept failed"),
                    }
                });
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
            let _ = stream.set_nodelay(true);
            // A TLS dial failure is treated like a dead addr: drop and back off.
            if let Ok((s, binding)) = client_wrap(&ctx, stream).await {
                backoff = Duration::from_millis(500);
                handle_conn(s, binding, ctx.clone(), cmd.clone()).await; // returns on disconnect
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(8));
    }
}

// ----------------------------------------------------------------------------
// M41: opt-in TLS 1.3 transport encryption (encryption-only; identity is M40's job)
// ----------------------------------------------------------------------------

/// Wrap an inbound TCP stream for [`handle_conn`]. TLS off ⇒ the raw stream
/// (byte-identical to pre-M41); TLS on ⇒ complete the server-side TLS handshake
/// (presenting our ephemeral self-signed cert) and box the resulting stream. M42:
/// the second tuple element is this channel's TLS exporter — `Some` only when TLS
/// is on and `bind_channel` is set.
async fn server_wrap(
    ctx: &AuthContext,
    tcp: TcpStream,
) -> io::Result<(Box<dyn PeerStream>, Option<[u8; 32]>)> {
    match &ctx.tls {
        Some(t) => {
            let stream = t.acceptor.accept(tcp).await?;
            let binding = if ctx.bind_channel {
                Some(export_channel_binding(stream.get_ref().1)?)
            } else {
                None
            };
            Ok((Box::new(stream), binding))
        }
        None => Ok((Box::new(tcp), None)),
    }
}

/// Wrap an outbound TCP stream for [`handle_conn`]. TLS off ⇒ the raw stream; TLS
/// on ⇒ complete the client-side TLS handshake. The SNI name is a fixed dummy —
/// [`AcceptAnyServerCert`] ignores it (encryption-only, no server authentication).
/// M42: see [`server_wrap`] for the exporter tuple element.
async fn client_wrap(
    ctx: &AuthContext,
    tcp: TcpStream,
) -> io::Result<(Box<dyn PeerStream>, Option<[u8; 32]>)> {
    match &ctx.tls {
        Some(t) => {
            let name = rustls::pki_types::ServerName::try_from("zhixing-node")
                .expect("static SNI is valid");
            let stream = t.connector.connect(name, tcp).await?;
            let binding = if ctx.bind_channel {
                Some(export_channel_binding(stream.get_ref().1)?)
            } else {
                None
            };
            Ok((Box::new(stream), binding))
        }
        None => Ok((Box::new(tcp), None)),
    }
}

/// M42: derive the 32-byte channel-binding value from a completed TLS connection's
/// keying-material exporter (RFC 5705/8446). Called only after the handshake has
/// finished (`server_wrap`/`client_wrap` await it first), so the export never fails
/// for being premature; any error is surfaced as a dropped connection.
fn export_channel_binding<D>(conn: &rustls::ConnectionCommon<D>) -> io::Result<[u8; 32]> {
    let mut b = [0u8; 32];
    conn.export_keying_material(&mut b, CHANNEL_BINDING_LABEL, None)
        .map_err(|e| io::Error::other(format!("TLS channel-binding export failed: {e}")))?;
    Ok(b)
}

/// Build the P2P [`TlsSetup`] once at boot.
///
/// `mtls = None` (M41, `require_peer_certs` off): **encryption-only** — an ephemeral
/// self-signed cert on the server side and an accept-any verifier on the client
/// side. It protects confidentiality/integrity but authenticates no one; peer
/// identity is proven by the M40 `auth_handshake` inside the tunnel.
///
/// `mtls = Some(..)` (M43, `require_peer_certs` on): **genesis-pinned mutual TLS**
/// over TLS 1.3, using RFC 7250 raw public keys. Each side presents its genesis
/// ed25519 key (derived from the seed) as its credential, and a
/// [`GenesisPinnedVerifier`] admits a connection only if the peer's presented key is
/// a genesis validator — both directions. A non-validator cannot complete the
/// handshake at all.
fn build_tls_setup(mtls: Option<MtlsMaterial>) -> io::Result<TlsSetup> {
    // Install the ring crypto provider process-wide. Idempotent: a second call
    // (e.g. an in-process `localnet` with several nodes) returns Err, which we
    // ignore — some provider is now installed either way.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let Some(m) = mtls else {
        return build_encrypt_only_tls_setup();
    };

    // Derive the TLS signing credential from the genesis seed: PKCS#8 v1 =
    // fixed 16-byte header || 32-byte seed. `any_eddsa_type` yields a rustls
    // signer whose `public_key()` SPKI carries exactly this node's genesis pubkey.
    let mut pkcs8 = Vec::with_capacity(ED25519_PKCS8_PREFIX.len() + 32);
    pkcs8.extend_from_slice(&ED25519_PKCS8_PREFIX);
    pkcs8.extend_from_slice(&m.seed);
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8);
    let signing_key = rustls::crypto::ring::sign::any_eddsa_type(&key_der)
        .map_err(|e| io::Error::other(format!("mTLS ed25519 signer failed: {e}")))?;
    let spki = signing_key
        .public_key()
        .ok_or_else(|| io::Error::other("mTLS signer exposed no public key"))?;
    let certified = Arc::new(rustls::sign::CertifiedKey::new(
        vec![rustls::pki_types::CertificateDer::from(spki.as_ref().to_vec())],
        signing_key,
    ));

    let verifier = Arc::new(GenesisPinnedVerifier::new(m.validators));
    let tls13 = &[&rustls::version::TLS13][..];

    // Raw public keys are a TLS 1.3 feature (the raw-key signature verifier is
    // 1.3-only), so pin the version explicitly on both sides.
    let server_config = rustls::ServerConfig::builder_with_protocol_versions(tls13)
        .with_client_cert_verifier(verifier.clone())
        .with_cert_resolver(Arc::new(
            rustls::server::AlwaysResolvesServerRawPublicKeys::new(certified.clone()),
        ));

    let client_config = rustls::ClientConfig::builder_with_protocol_versions(tls13)
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(
            rustls::client::AlwaysResolvesClientRawPublicKeys::new(certified),
        ));

    Ok(TlsSetup {
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        connector: TlsConnector::from(Arc::new(client_config)),
    })
}

/// M41 encryption-only TLS: ephemeral self-signed cert + accept-any client verifier.
/// Split out so [`build_tls_setup`]'s mTLS branch stays legible; behavior here is
/// byte-identical to the pre-M43 path.
fn build_encrypt_only_tls_setup() -> io::Result<TlsSetup> {
    // Fresh self-signed cert + key for this process (SAN "zhixing-node").
    let cert = rcgen::generate_simple_self_signed(vec!["zhixing-node".to_string()])
        .map_err(|e| io::Error::other(format!("self-signed cert generation failed: {e}")))?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| io::Error::other(format!("private key encoding failed: {e}")))?;

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| io::Error::other(format!("TLS server config failed: {e}")))?;

    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
        .with_no_client_auth();

    Ok(TlsSetup {
        acceptor: TlsAcceptor::from(Arc::new(server_config)),
        connector: TlsConnector::from(Arc::new(client_config)),
    })
}

/// A rustls client verifier that accepts **any** server certificate. Intentional:
/// M41 TLS is encryption-only, so there is no PKI and no server identity to check
/// here — an active MITM is out of scope for this slice (channel binding to the
/// M40 handshake is deferred). Peer authentication is `auth_handshake`'s job.
#[derive(Debug)]
struct AcceptAnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Extract the 32-byte ed25519 public key from an RFC 7250 raw-public-key SPKI.
///
/// An ed25519 SPKI is a fixed 44 bytes: the 12-byte [`ED25519_SPKI_PREFIX`] (the
/// `AlgorithmIdentifier` for `id-Ed25519` plus the BIT STRING header) followed by
/// the 32-byte key. Anything of a different length or prefix is not an ed25519 raw
/// public key and is rejected (returns `None`) rather than mis-sliced.
fn spki_to_ed25519(spki: &[u8]) -> Option<PubKey> {
    if spki.len() != ED25519_SPKI_PREFIX.len() + 32 {
        return None;
    }
    if spki[..ED25519_SPKI_PREFIX.len()] != ED25519_SPKI_PREFIX {
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&spki[ED25519_SPKI_PREFIX.len()..]);
    Some(key)
}

/// M43 genesis-pinned mutual-TLS verifier, used on **both** sides of every peer
/// link: as a [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier)
/// (the dialer checking the listener's credential) and as a
/// [`ClientCertVerifier`](rustls::server::danger::ClientCertVerifier) (the listener
/// checking the dialer's). It admits a connection only if the peer presents an
/// RFC 7250 raw ed25519 public key that is a member of the genesis validator set —
/// so a non-validator cannot even complete the TLS handshake. `requires_raw_public_keys`
/// is `true` on both traits, so rustls hands us the SPKI directly as `end_entity`.
#[derive(Debug)]
struct GenesisPinnedVerifier {
    validators: Arc<HashSet<PubKey>>,
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl GenesisPinnedVerifier {
    fn new(validators: Arc<HashSet<PubKey>>) -> Self {
        Self {
            validators,
            algs: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }

    /// Decode the peer's SPKI and require the key to be a genesis validator.
    /// Bad encoding and non-membership map to distinct certificate errors so a
    /// handshake failure is legible; both abort the connection.
    fn check_pinned(&self, spki: &[u8]) -> Result<(), rustls::Error> {
        let key = spki_to_ed25519(spki).ok_or(rustls::Error::InvalidCertificate(
            rustls::CertificateError::BadEncoding,
        ))?;
        if self.validators.contains(&key) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    /// TLS 1.3 raw-public-key signature check, shared by both trait impls: proves
    /// the peer holds the private half of the SPKI it presented.
    fn verify_tls13(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature_with_raw_key(
            message,
            &rustls::pki_types::SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algs,
        )
    }
}

impl rustls::client::danger::ServerCertVerifier for GenesisPinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        self.check_pinned(end_entity.as_ref())?;
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // The mTLS path is TLS 1.3-only, so this is never reached; reject defensively.
        Err(rustls::Error::General(
            "TLS 1.2 not supported on the genesis-pinned mTLS path".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl rustls::server::danger::ClientCertVerifier for GenesisPinnedVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // Raw public keys have no issuer subjects to hint.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        self.check_pinned(end_entity.as_ref())?;
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // TLS 1.3-only path (see the server verifier); reject defensively.
        Err(rustls::Error::General(
            "TLS 1.2 not supported on the genesis-pinned mTLS path".into(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
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
        "zhixing_mempool_capacity",
        "Configured mempool pending-pool capacity bound (saturation = txs/capacity).",
        m.mempool_capacity as u64,
    );
    gauge(
        &mut s,
        "zhixing_mempool_per_account_limit",
        "Configured per-account pending-tx bound (large = unbounded/off).",
        m.mempool_per_account_limit as u64,
    );
    gauge(
        &mut s,
        "zhixing_seen_tx",
        "Entries in the gossip tx dedup set (seen_tx).",
        m.seen_tx as u64,
    );
    gauge(
        &mut s,
        "zhixing_seen_tx_capacity",
        "Configured per-set gossip dedup bound (large = unbounded).",
        m.seen_tx_capacity as u64,
    );
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
    // M52: monotonic counters (the `counter` half of the Prometheus data model).
    let counter = |s: &mut String, name: &str, help: &str, value: u64| {
        s.push_str(&format!("# HELP {name} {help}\n"));
        s.push_str(&format!("# TYPE {name} counter\n"));
        s.push_str(&format!("{name} {value}\n"));
    };
    counter(
        &mut s,
        "zhixing_peer_connects_total",
        "Cumulative peer registrations since boot.",
        m.peer_connects,
    );
    counter(
        &mut s,
        "zhixing_local_txs_total",
        "Cumulative transactions submitted to this node's local API.",
        m.local_txs,
    );
    counter(
        &mut s,
        "zhixing_blocks_committed_total",
        "Cumulative blocks finalized via this node's own consensus round.",
        m.blocks_committed,
    );
    counter(
        &mut s,
        "zhixing_slashing_events_total",
        "Cumulative equivocation events observed and submitted.",
        m.slashing_events,
    );
    counter(
        &mut s,
        "zhixing_txs_rate_limited_total",
        "Cumulative gossip transactions dropped by the per-peer rate limiter.",
        m.txs_rate_limited,
    );
    counter(
        &mut s,
        "zhixing_txs_quota_rejected_total",
        "Cumulative admissions rejected by the per-account mempool quota.",
        m.txs_quota_rejected,
    );
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
// M53: external transaction-ingress RPC (opt-in, hand-rolled HTTP)
// ----------------------------------------------------------------------------

/// Cap on the request-header bytes we buffer before giving up (mirrors the metrics
/// endpoint's 8 KiB header cap).
const MAX_RPC_HEADER: usize = 8192;
/// Cap on the request-body bytes we accept. A tx is bounded (a fixed-`DIM`
/// embedding, ≤32 reviews, a 64-byte signature), so 64 KiB is comfortably above
/// any legitimate `codec::encode_tx` output while bounding a hostile client.
const MAX_RPC_BODY: usize = 65536;

/// Parse a `Content-Length` value out of the raw request-header block
/// (case-insensitive header name). Returns `None` if absent or unparsable.
fn parse_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

/// Build a fixed-shape `HTTP/1.1` response with a plain-text body. `status_line`
/// is e.g. `"200 OK"`; the connection is closed after the response.
fn http_response(status_line: &str, body: &str) -> String {
    // M66: delegate to the Content-Type-aware builder with the historic plaintext type,
    // so every pre-M66 caller's output stays byte-identical.
    http_response_ct(status_line, "text/plain; charset=utf-8", body)
}

/// M66: build a response with an explicit `Content-Type`. Plain reads keep
/// `text/plain; charset=utf-8` (via [`http_response`]); a `?format=json` read passes
/// bare `application/json` (JSON is always UTF-8, so it carries no `charset` param).
/// `Content-Length` is the UTF-8 byte length of `body`, exactly as before.
/// M71: every response carries `Vary: Accept` (RFC 7231 §7.1.4) — the endpoint
/// selects text vs JSON from the request's `Accept` header, so shared caches must
/// key on it (lest a JSON body be replayed to a text client, or vice-versa).
fn http_response_ct(status_line: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Vary: Accept\r\n\
         Connection: close\r\n\
         \r\n\
         {}",
        status_line,
        content_type,
        body.len(),
        body,
    )
}

/// M70: the representations this RPC server can emit, in negotiation-preference
/// order (named JSON wins the q-tie, so it is listed first). Used to build the
/// `406 Not Acceptable` body per RFC 7231 §6.5.6.
const OFFERED_MEDIA_TYPES: [&str; 2] = ["application/json", "text/plain"];

/// M70/M80: RFC 7231 §6.5.6 — the `406` body as a machine-readable JSON object listing
/// the representations we can emit, from the single OFFERED_MEDIA_TYPES source of truth.
/// Always JSON: a 406 only fires on an explicit unsatisfiable `Accept`, so there is no
/// representation preference to honor for the error body.
fn not_acceptable_json() -> String {
    let available = OFFERED_MEDIA_TYPES
        .iter()
        .map(|t| json_str(t))
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"error\":\"not_acceptable\",\"available\":[{available}]}}")
}

/// Accept loop for the ingress RPC endpoint. Mirrors [`run_metrics`]: each
/// connection is handled on its own task; an accept error is logged and the loop
/// continues.
async fn run_rpc(listener: TcpListener, my_id: u64, cmd: mpsc::UnboundedSender<Cmd>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(serve_rpc_conn(stream, cmd.clone()));
            }
            Err(e) => warn!(node = my_id, error = %e, "rpc accept error"),
        }
    }
}

/// Serve one ingress request. Reads the request headers (bounded), then:
/// - a non-`POST` method (e.g. `GET`/`HEAD`) returns `200 OK`/`"ok"` — doubling
///   as a health probe;
/// - a `POST` reads the body (bounded by `Content-Length`, capped at
///   [`MAX_RPC_BODY`]), decodes it as raw `codec::encode_tx` bytes, and submits
///   it through the actor: `200`/hash on admission, `400` on a decode error,
///   `422`/reason on a validation reject.
///
/// All client-side errors are swallowed (early return) — a broken client must
/// never affect the node.
/// M58: a parsed read-class GET route. `Health` is the catch-all (`/` and any
/// unrecognized path) so the endpoint stays a liveness probe exactly as before M58;
/// only a malformed `/account/<non-numeric>` resolves to `NotFound` (→ 404).
enum GetRoute {
    Health,
    Height,
    Head,
    Account(u64),
    /// M59: `GET /account/{id}/proof` — a verifiable read (header + cert + proof).
    AccountProof(u64),
    /// M60: `GET /{reviewer,validator,graph}/{id}/proof` — verifiable reads for the
    /// other `ProofKind`s (graph node addressed by insertion index).
    Proof(ProofKind, u64),
    /// M65: `GET /{reviewer,validator,graph}/{id}` — plain (unverified) read of the
    /// entity's fields, the non-proof sibling of `Account(u64)` (M58).
    Plain(ProofKind, u64),
    /// M63: `GET /bridge/lock/{id}/proof` — the self-contained bridge-lock
    /// `LockEnvelope` for `lock_id` (header + cert + tracked set + lock + proof).
    BridgeLock(u64),
    /// M64: `GET /bridge/locks` — the plain (unverified) directory of every bridge
    /// lock on the chain (id + height + fields), so a client can discover ids.
    BridgeLocks,
    NotFound,
}

/// M58: map a request path to a read-class route. Pure (no I/O) so it is unit-tested
/// directly; `serve_rpc_conn` is the thin socket shell around it.
/// M66: split an origin-form request target into `(path, query)` at the first `?`.
/// No `?` ⇒ `("<path>", "")`. A request-line target never carries a `#fragment`
/// (RFC 9112 — fragments are client-side only), so we deliberately do not split on `#`.
/// Keeping the path byte-identical for query-less targets is what makes the pre-M66
/// plaintext routing and responses unchanged.
fn split_query(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    }
}

/// M66: the chosen response rendering. `Text` (the default) preserves every pre-M66
/// body byte-for-byte; `Json` selects the hand-rolled minimal JSON renderers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RespFormat {
    Text,
    Json,
}

/// M66/M68: parse `?format=…` out of a raw query string. `format=json` (exact, case
/// sensitive) ⇒ `Some(Json)`; any other present `format=` value — empty, `format=text`,
/// an unknown value — ⇒ `Some(Text)`. The first `format=` pair wins; later duplicates
/// are ignored. `None` when the query carries no `format=` pair at all, so the caller
/// can fall back to `Accept`-header negotiation (M68).
fn response_format(query: &str) -> Option<RespFormat> {
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("format=") {
            return Some(if v == "json" { RespFormat::Json } else { RespFormat::Text });
        }
    }
    None
}

/// M72: parse an optional `usize` query parameter (`<key>=<n>`) from a raw query
/// string. The first pair whose key matches wins (mirrors `response_format`); a
/// missing key or an unparseable value ⇒ `None`. Pure, so it is unit-tested directly.
fn usize_param(query: &str, key: &str) -> Option<usize> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse::<usize>().ok())
}

/// M81: default page size when a request omits `?limit` (supersedes the M72/M79
/// "absent limit ⇒ whole listing" RPC behavior — a read can no longer return an
/// unbounded body by omitting the param).
const DEFAULT_PAGE_LIMIT: usize = 50;
/// M81: hard upper bound on a requested `?limit` (defensive cap on response size).
const MAX_PAGE_LIMIT: usize = 500;

/// M72: apply an `offset`/`limit` window to a slice, clamped to bounds with
/// saturating arithmetic (huge params can't overflow). `offset` past the end ⇒
/// empty; `limit == None` ⇒ through the end; `limit == Some(0)` ⇒ empty. Pure and
/// generic so future list endpoints reuse it. Unit-tested directly.
fn paginate<T>(items: &[T], offset: usize, limit: Option<usize>) -> &[T] {
    let start = offset.min(items.len());
    let end = match limit {
        Some(l) => start.saturating_add(l).min(items.len()),
        None => items.len(),
    };
    &items[start..end]
}

/// M81: resolve the effective page size from an optional requested `?limit`:
/// absent ⇒ `DEFAULT_PAGE_LIMIT`; present ⇒ clamped down to `MAX_PAGE_LIMIT`.
/// `Some(0)` stays `0` (an explicit empty page, as `paginate` already honors).
/// Pure, so it is unit-tested directly.
fn effective_limit(requested: Option<usize>) -> usize {
    match requested {
        Some(l) => l.min(MAX_PAGE_LIMIT),
        None => DEFAULT_PAGE_LIMIT,
    }
}

/// M69: outcome of `Accept`-header negotiation (RFC 7231 §5.3 q-values).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Negotiation {
    /// No `Accept` header at all → the caller uses its default (`Text`).
    Absent,
    /// A representation is acceptable.
    Use(RespFormat),
    /// `Accept` present, but every offered type has `q=0` / is unmatched → `406`.
    NotAcceptable,
}

/// M69: parse an RFC 7231 q-value into fixed-point milli-units (`0..=1000`), so q
/// comparisons are integer and never trip `clippy::float_cmp`. A missing or malformed
/// value defaults to `1000` (q=1.0, the RFC default weight); out-of-range values clamp.
fn parse_qmilli(s: &str) -> u16 {
    match s.trim().parse::<f32>() {
        Ok(f) if f.is_finite() => (f.clamp(0.0, 1.0) * 1000.0).round() as u16,
        _ => 1000,
    }
}

/// M69: best `(q_milli, specificity)` for one concrete media type against an `Accept`
/// value. specificity: `3` = exact `type/subtype`, `2` = `type/*`, `1` = `*/*`, `0` =
/// no match. The most specific matching range wins (RFC 7231); ties on specificity
/// break to the higher q.
fn media_match(accept_value: &str, ty: &str, sub: &str) -> (u16, u8) {
    let (mut best_q, mut best_spec) = (0u16, 0u8);
    for range in accept_value.split(',') {
        let mut parts = range.split(';');
        let media = parts.next().unwrap_or("").trim();
        let spec = match media.split_once('/') {
            Some((t, s)) => {
                let (t, s) = (t.trim(), s.trim());
                if t.eq_ignore_ascii_case(ty) && s.eq_ignore_ascii_case(sub) {
                    3
                } else if t.eq_ignore_ascii_case(ty) && s == "*" {
                    2
                } else if t == "*" && s == "*" {
                    1
                } else {
                    0
                }
            }
            None => 0,
        };
        if spec == 0 {
            continue;
        }
        let mut q = 1000u16;
        for p in parts {
            let p = p.trim();
            if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
                q = parse_qmilli(v);
            }
        }
        if spec > best_spec || (spec == best_spec && q > best_q) {
            best_spec = spec;
            best_q = q;
        }
    }
    (best_q, best_spec)
}

/// M68/M69: content negotiation from the request `Accept:` header (case-insensitive
/// name, modeled on `parse_content_length`). M69 honors RFC 7231 §5.3 q-values when
/// choosing between the two representations we can emit (`application/json` and
/// `text/plain`): the higher-q type wins; on an exact q tie, json is preferred only
/// when it was *named* (specificity ≥ 2) so `application/json, text/plain` ⇒ Json while
/// `*/*` ⇒ Text (browsers keep plaintext). If both types resolve to `q=0` (explicit
/// `q=0`, or neither matched, e.g. `application/xml`) the request is `NotAcceptable`
/// (⇒ `406`); a missing `Accept` header is `Absent` (⇒ the caller's default).
fn accept_format(head: &str) -> Negotiation {
    let mut value = None;
    for line in head.split("\r\n") {
        let Some((name, v)) = line.split_once(':') else { continue };
        if name.trim().eq_ignore_ascii_case("accept") {
            value = Some(v);
            break;
        }
    }
    let Some(value) = value else {
        return Negotiation::Absent;
    };
    let (q_json, spec_json) = media_match(value, "application", "json");
    let (q_text, _spec_text) = media_match(value, "text", "plain");
    if q_json == 0 && q_text == 0 {
        return Negotiation::NotAcceptable;
    }
    let use_json = q_json > q_text || (q_json == q_text && spec_json >= 2);
    Negotiation::Use(if use_json { RespFormat::Json } else { RespFormat::Text })
}

/// M68/M69: resolve the response rendering. An explicit `?format=` query wins (the most
/// specific, deliberate signal — and never 406s, as it is our own param); else negotiate
/// the `Accept` header with q-values — no `Accept` ⇒ `Text` (so a request with neither
/// keeps every response byte-identical to pre-M68); an unsatisfiable `Accept` ⇒ `None`,
/// on which the caller emits `406 Not Acceptable`.
fn resolve_format(query: &str, head: &str) -> Option<RespFormat> {
    if let Some(f) = response_format(query) {
        return Some(f);
    }
    match accept_format(head) {
        Negotiation::Absent => Some(RespFormat::Text),
        Negotiation::Use(f) => Some(f),
        Negotiation::NotAcceptable => None,
    }
}

fn route_get(path: &str) -> GetRoute {
    match path {
        "/height" => GetRoute::Height,
        "/head" => GetRoute::Head,
        // M64: the plain bridge-lock directory. Exact-match here, so it never collides
        // with the M63 `/bridge/lock/` prefix below (`…lock` + `s`, not `…lock` + `/`).
        "/bridge/locks" => GetRoute::BridgeLocks,
        p => {
            if let Some(rest) = p.strip_prefix("/account/") {
                // M59: `{id}/proof` is the verifiable read; a bare `{id}` is the M58
                // plain read. An empty or non-numeric id in either shape ⇒ 404.
                match rest.strip_suffix("/proof") {
                    Some(idp) => idp
                        .parse::<u64>()
                        .map(GetRoute::AccountProof)
                        .unwrap_or(GetRoute::NotFound),
                    None => rest.parse::<u64>().map(GetRoute::Account).unwrap_or(GetRoute::NotFound),
                }
            } else if let Some(rest) = p.strip_prefix("/reviewer/") {
                entity_route(rest, ProofKind::Reviewer)
            } else if let Some(rest) = p.strip_prefix("/validator/") {
                entity_route(rest, ProofKind::Validator)
            } else if let Some(rest) = p.strip_prefix("/graph/") {
                entity_route(rest, ProofKind::GraphNode)
            } else if let Some(rest) = p.strip_prefix("/bridge/lock/") {
                // M63: `{id}/proof` is the only form — a bridge lock has no plain
                // read, so a bare id (no `/proof`) or a non-numeric id ⇒ 404.
                match rest.strip_suffix("/proof") {
                    Some(idp) => idp
                        .parse::<u64>()
                        .map(GetRoute::BridgeLock)
                        .unwrap_or(GetRoute::NotFound),
                    None => GetRoute::NotFound,
                }
            } else {
                GetRoute::Health
            }
        }
    }
}

/// M60/M65: parse `/<entity>/{id}[/proof]` for reviewer/validator/graph. `{id}/proof`
/// is the M60 verifiable read; a bare `{id}` is the M65 plain (unverified) read, the
/// non-proof sibling of `/account/{id}`. A non-numeric or empty id ⇒ 404.
fn entity_route(rest: &str, kind: ProofKind) -> GetRoute {
    match rest.strip_suffix("/proof") {
        Some(idp) => idp
            .parse::<u64>()
            .map(|id| GetRoute::Proof(kind, id))
            .unwrap_or(GetRoute::NotFound),
        None => rest
            .parse::<u64>()
            .map(|id| GetRoute::Plain(kind, id))
            .unwrap_or(GetRoute::NotFound),
    }
}

/// M60: a short human label for a `ProofKind`, used in the `404` body of a verifiable
/// read (`"<kind> {id} not found"`).
fn proof_kind_label(k: ProofKind) -> &'static str {
    match k {
        ProofKind::Account => "account",
        ProofKind::Reviewer => "reviewer",
        ProofKind::Validator => "validator",
        ProofKind::GraphNode => "graph node",
    }
}

/// M58: render an account as a grep-friendly `key=value` plain-text line, matching the
/// endpoint's existing plain-text bodies (the ingress RPC replies with a bare hash).
fn format_account(id: u64, a: &Account) -> String {
    format!(
        "id={id} balance={} staked_total={} earned_total={} slashed_total={} \
         submissions={} accepted={} pubkey={}",
        a.balance,
        a.staked_total,
        a.earned_total,
        a.slashed_total,
        a.submissions,
        a.accepted,
        crate::hash::hex(&a.pubkey),
    )
}

/// M65: a plain (unverified) snapshot of a reviewer/validator/graph entity for the
/// read-class RPC — the non-proof siblings of `GET /account/{id}`. Account keeps its
/// own richer `Account` snapshot; this carries the per-kind fields.
pub enum EntityView {
    Reviewer { id: u64, reputation: f32 },
    Validator { id: u64, power: u64, pubkey: crate::crypto::PubKey },
    GraphNode { node_id: u64, domain: u32, embedding: crate::Embedding },
}

/// M65: render a plain entity view as a grep-friendly `key=value` line (same shape as
/// `format_account`). The graph embedding is emitted as comma-joined f32 for a
/// complete read. Pure (no I/O) for direct unit testing.
fn format_entity(v: &EntityView) -> String {
    match v {
        EntityView::Reviewer { id, reputation } => {
            format!("kind=reviewer id={id} reputation={reputation}")
        }
        EntityView::Validator { id, power, pubkey } => {
            format!("kind=validator id={id} power={power} pubkey={}", crate::hash::hex(pubkey))
        }
        EntityView::GraphNode { node_id, domain, embedding } => {
            let emb = embedding.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",");
            format!("kind=graph node_id={node_id} domain={domain} dim={} embedding={emb}", embedding.len())
        }
    }
}
/// M59: render a verifiable account read as two grep-friendly hex lines — the
/// certified head (`BlockHeader` + its finality `Commit`) and the account's typed
/// inclusion `ProofEntry`. A client hex-decodes both, runs `decode_certified_header`
/// / `decode_proof_entry`, then `ValidatorTracker::verify_proof_against_header`
/// against its own independently-tracked validator set — so the read is provable
/// rather than trusted. Pure (no I/O) for direct unit testing.
fn format_account_proof(ch: &CertifiedHeader, entry: &ProofEntry) -> String {
    format!(
        "certified_header={}\nproof_entry={}",
        crate::hash::hex(&crate::codec::encode_certified_header(ch)),
        crate::hash::hex(&crate::codec::encode_proof_entry(entry)),
    )
}

/// M63: render a verifiable bridge-lock read as one grep-friendly hex line — the
/// self-contained `LockEnvelope` (header + cert + tracked set + lock + proof). A
/// client hex-decodes it, runs `decode_lock_envelope`, then follows the source and
/// `BridgeEndpoint::verify_lock` — no separate certified head is needed because the
/// envelope bundles its own. Pure (no I/O) for direct unit testing.
fn format_lock(env: &crate::bridge::LockEnvelope) -> String {
    format!(
        "lock_envelope={}",
        crate::hash::hex(&crate::net::encode_lock_envelope(env)),
    )
}

/// M64: render the bridge-lock directory as grep-friendly `key=value` lines, one
/// per lock (empty string when the chain has no locks). Plain/unverified, like
/// `format_account` — a client verifies any single lock via the M63 proof route.
fn format_lock_listing(locks: &[(u64, u64, crate::BridgeLock)]) -> String {
    locks
        .iter()
        .map(|(id, h, l)| {
            format!(
                "lock_id={id} height={h} account={} amount={} dest_chain={} dest_account={} nonce={}",
                l.account,
                l.amount,
                crate::hash::hex(&l.dest_chain),
                l.dest_account,
                l.nonce,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// M79: wrap a rendered text page in the pagination envelope. Always a `total={n}`
/// header line; a `next={off}` line only when another page remains (its presence is the
/// grep-friendly "more pages" signal); then the item lines (omitted when the page is
/// empty). Generic over the inner renderer so future list reads reuse it.
fn format_page(items: &str, total: usize, next: Option<usize>) -> String {
    let mut s = format!("total={total}");
    if let Some(n) = next {
        s.push_str(&format!("\nnext={n}"));
    }
    if !items.is_empty() {
        s.push('\n');
        s.push_str(items);
    }
    s
}

/// M61: render a verifiable batch read as two grep-friendly hex lines — the
/// certified head and the encoded `BatchResponseEnvelope`. A client hex-decodes
/// both, runs `decode_certified_header` / `decode_batch_envelope`, then
/// `ValidatorTracker::verify_batch` against its own tracked set. Pure (no I/O).
fn format_batch(
    ch: &CertifiedHeader,
    env: &BatchResponseEnvelope,
    range: &[(Block, Commit)],
) -> String {
    format!(
        "certified_header={}\nbatch_envelope={}\nrange_blocks={}",
        crate::hash::hex(&crate::codec::encode_certified_header(ch)),
        crate::hash::hex(&crate::net::encode_batch_envelope(env)),
        crate::hash::hex(&crate::net::encode_blocks(range)),
    )
}

// ---- M66: hand-rolled minimal JSON renderers (no `serde_json`). ----------------
// These mirror the `format_*` plain-text renderers above but emit `application/json`
// bodies for `?format=json` reads. All are pure (no I/O) for direct unit testing.
// u64 fields are emitted as JSON *strings* — values routinely exceed 2^53 and would
// lose precision as JSON numbers in a JavaScript `JSON.parse` client; f32 fields
// (reputation, embedding) stay JSON numbers (`null` when non-finite).

/// Quote and escape a string as a JSON string literal, per RFC 8259: escape `"` and
/// `\`, and all control characters U+0000–U+001F (`\n`/`\r`/`\t` by name, the rest as
/// `\u00XX`). Today's inputs are hex strings and `{error}` messages, but a complete
/// escaper keeps the renderers correct for any text that later flows through.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A u64 as a JSON string (lossless for any consumer).
fn json_u64(n: u64) -> String {
    json_str(&n.to_string())
}

/// An f32 as a JSON number token, or `null` when non-finite (`inf`/`NaN` are not valid
/// JSON, so a non-finite reputation/embedding component must never leak into a body).
fn json_f32(x: f32) -> String {
    if x.is_finite() {
        x.to_string()
    } else {
        "null".to_string()
    }
}

/// `GET /height?format=json` → `{"height":"<n>"}`.
fn json_height(h: u64) -> String {
    format!("{{\"height\":{}}}", json_u64(h))
}

/// `GET /head?format=json` → `{"head":"<hex>"}`.
fn json_head(head: &[u8; 32]) -> String {
    format!("{{\"head\":{}}}", json_str(&crate::hash::hex(head)))
}

/// `GET /account/{id}?format=json` — the JSON sibling of [`format_account`].
fn json_account(id: u64, a: &Account) -> String {
    format!(
        "{{\"id\":{},\"balance\":{},\"staked_total\":{},\"earned_total\":{},\
         \"slashed_total\":{},\"submissions\":{},\"accepted\":{},\"pubkey\":{}}}",
        json_u64(id),
        json_u64(a.balance),
        json_u64(a.staked_total),
        json_u64(a.earned_total),
        json_u64(a.slashed_total),
        json_u64(a.submissions),
        json_u64(a.accepted),
        json_str(&crate::hash::hex(&a.pubkey)),
    )
}

/// `GET /{reviewer,validator,graph}/{id}?format=json` — the JSON sibling of
/// [`format_entity`]. Each object carries a `"kind"` discriminator so a client can
/// disambiguate the three shapes.
fn json_entity(v: &EntityView) -> String {
    match v {
        EntityView::Reviewer { id, reputation } => format!(
            "{{\"kind\":\"reviewer\",\"id\":{},\"reputation\":{}}}",
            json_u64(*id),
            json_f32(*reputation),
        ),
        EntityView::Validator { id, power, pubkey } => format!(
            "{{\"kind\":\"validator\",\"id\":{},\"power\":{},\"pubkey\":{}}}",
            json_u64(*id),
            json_u64(*power),
            json_str(&crate::hash::hex(pubkey)),
        ),
        EntityView::GraphNode { node_id, domain, embedding } => {
            let emb = embedding.iter().map(|f| json_f32(*f)).collect::<Vec<_>>().join(",");
            format!(
                "{{\"kind\":\"graph\",\"node_id\":{},\"domain\":{},\"dim\":{},\"embedding\":[{}]}}",
                json_u64(*node_id),
                json_u64(*domain as u64),
                json_u64(embedding.len() as u64),
                emb,
            )
        }
    }
}

/// `GET /bridge/locks?format=json` — the JSON sibling of [`format_lock_listing`]. A
/// JSON **array** of objects (`[]` when the chain has no locks — the one intentional
/// divergence from the text renderer's empty string).
fn json_lock_listing(locks: &[(u64, u64, crate::BridgeLock)]) -> String {
    let items = locks
        .iter()
        .map(|(id, h, l)| {
            format!(
                "{{\"lock_id\":{},\"height\":{},\"account\":{},\"amount\":{},\
                 \"dest_chain\":{},\"dest_account\":{},\"nonce\":{}}}",
                json_u64(*id),
                json_u64(*h),
                json_u64(l.account),
                json_u64(l.amount),
                json_str(&crate::hash::hex(&l.dest_chain)),
                json_u64(l.dest_account),
                json_u64(l.nonce),
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{items}]")
}

/// M79: wrap a rendered JSON array page as `{"total":"N","next":"M"|null,"items":[…]}`.
/// `total`/`next` use the lossless quoted-u64 convention; `next` is bare `null` at the
/// end of the listing (the one divergence from the text form, which omits the line).
fn json_page(items: &str, total: usize, next: Option<usize>) -> String {
    let next = match next {
        Some(n) => json_u64(n as u64),
        None => "null".to_string(),
    };
    format!("{{\"total\":{},\"next\":{},\"items\":{}}}", json_u64(total as u64), next, items)
}

// ---- M67/M74/M75/M76/M77/M78: JSON renderers for the proof / batch / lock reads. -----
// M67 serialized every proof envelope to one opaque `hex(encode_*)` blob dropped into
// a named JSON field — self-verifying, but useless to a JSON client that only wants a
// height or a state root without pulling in the binary codec. M74 decoded the
// `certified_header` sub-envelope (shared by the proof and batch reads); M75 decoded
// the `proof_entry` field (typed leaf + merkle path), so the account/entity proof read
// is fully structured; M76 decodes the `batch_envelope` field (all four
// `BatchResponseItem` variants: Inclusion / Knn / Range / Diff); M77 decodes the
// `lock_envelope` field (bridge-lock proof read); M78 decodes the `range_blocks` field
// (full `Block` bodies + commits). With M78 **no opaque-hex field remains** anywhere on
// the structured-JSON read surface. All pure (no I/O).

/// M74: a `ValidatorUpdate` as `{"id","pubkey","power"}`.
fn json_validator_update(u: &crate::validator::ValidatorUpdate) -> String {
    format!(
        "{{\"id\":{},\"pubkey\":{},\"power\":{}}}",
        json_u64(u.id),
        json_str(&crate::hash::hex(&u.pubkey)),
        json_u64(u.power),
    )
}

/// M74: a finality `Vote`. `vote_type` is a string discriminator
/// (`"prevote"`/`"precommit"`), mirroring `json_entity`'s `"kind"`.
fn json_vote(v: &crate::consensus::Vote) -> String {
    let vote_type = match v.vote_type {
        crate::consensus::VoteType::Prevote => "prevote",
        crate::consensus::VoteType::Precommit => "precommit",
    };
    format!(
        "{{\"validator\":{},\"height\":{},\"round\":{},\"block_hash\":{},\
         \"vote_type\":{},\"signature\":{}}}",
        json_u64(v.validator),
        json_u64(v.height),
        json_u64(v.round as u64),
        json_str(&crate::hash::hex(&v.block_hash)),
        json_str(vote_type),
        json_str(&crate::hash::hex(&v.signature)),
    )
}

/// M74: a finality `Commit` — `{"height","round","block_hash","precommits":[…]}`.
fn json_commit(c: &Commit) -> String {
    let precommits = c.precommits.iter().map(json_vote).collect::<Vec<_>>().join(",");
    format!(
        "{{\"height\":{},\"round\":{},\"block_hash\":{},\"precommits\":[{}]}}",
        json_u64(c.height),
        json_u64(c.round as u64),
        json_str(&crate::hash::hex(&c.block_hash)),
        precommits,
    )
}

/// M74: a `BlockHeader` — all 15 fields, hashes as hex strings, the
/// `validator_updates` vec as a JSON array.
fn json_block_header(h: &crate::codec::BlockHeader) -> String {
    let vus = h
        .validator_updates
        .iter()
        .map(json_validator_update)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"height\":{},\"prev_hash\":{},\"timestamp_days\":{},\
         \"next_validators_root\":{},\"state_root\":{},\"accounts_root\":{},\
         \"graph_root\":{},\"bridge_root\":{},\"validator_updates\":[{}],\
         \"txs_commitment\":{},\"stake_ops_commitment\":{},\"evidence_commitment\":{},\
         \"bridge_locks_commitment\":{},\"bridge_headers_commitment\":{},\
         \"bridge_redeems_commitment\":{}}}",
        json_u64(h.height),
        json_str(&crate::hash::hex(&h.prev_hash)),
        json_f32(h.timestamp_days),
        json_str(&crate::hash::hex(&h.next_validators_root)),
        json_str(&crate::hash::hex(&h.state_root)),
        json_str(&crate::hash::hex(&h.accounts_root)),
        json_str(&crate::hash::hex(&h.graph_root)),
        json_str(&crate::hash::hex(&h.bridge_root)),
        vus,
        json_str(&crate::hash::hex(&h.txs_commitment)),
        json_str(&crate::hash::hex(&h.stake_ops_commitment)),
        json_str(&crate::hash::hex(&h.evidence_commitment)),
        json_str(&crate::hash::hex(&h.bridge_locks_commitment)),
        json_str(&crate::hash::hex(&h.bridge_headers_commitment)),
        json_str(&crate::hash::hex(&h.bridge_redeems_commitment)),
    )
}

/// M74: the shared `CertifiedHeader` as `{"header":{…},"cert":{…}}` — replaces the
/// M67 opaque-hex blob in both the proof and batch JSON reads.
fn json_certified_header(ch: &CertifiedHeader) -> String {
    format!(
        "{{\"header\":{},\"cert\":{}}}",
        json_block_header(&ch.header),
        json_commit(&ch.cert),
    )
}

/// M75: a `merkle::Proof` as `{"steps":[{"side":"left|right","hash":"<hex>"}, …]}`.
/// Each step is one sibling hash tagged by the side it hashes in on.
fn json_merkle_proof(p: &crate::merkle::Proof) -> String {
    let steps = p
        .steps
        .iter()
        .map(|s| match s {
            crate::merkle::Step::Left(h) => {
                format!("{{\"side\":\"left\",\"hash\":{}}}", json_str(&crate::hash::hex(h)))
            }
            crate::merkle::Step::Right(h) => {
                format!("{{\"side\":\"right\",\"hash\":{}}}", json_str(&crate::hash::hex(h)))
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"steps\":[{}]}}", steps)
}

/// M75: a `ProofEntry` as a `{"kind":…, <leaf fields>, "proof":{…}}` object. The
/// `kind` discriminator + leaf shapes mirror `json_entity`; the `Account` leaf reuses
/// `json_account`; every variant carries its `json_merkle_proof` inclusion path.
fn json_proof_entry(e: &ProofEntry) -> String {
    match e {
        ProofEntry::Account { id, account, proof } => format!(
            "{{\"kind\":\"account\",\"account\":{},\"proof\":{}}}",
            json_account(*id, account),
            json_merkle_proof(proof),
        ),
        ProofEntry::Reviewer { id, reputation, proof } => format!(
            "{{\"kind\":\"reviewer\",\"id\":{},\"reputation\":{},\"proof\":{}}}",
            json_u64(*id),
            json_f32(*reputation),
            json_merkle_proof(proof),
        ),
        ProofEntry::Validator { id, validator, proof } => format!(
            "{{\"kind\":\"validator\",\"id\":{},\"power\":{},\"pubkey\":{},\"proof\":{}}}",
            json_u64(*id),
            json_u64(validator.power),
            json_str(&crate::hash::hex(&validator.pubkey)),
            json_merkle_proof(proof),
        ),
        ProofEntry::GraphNode { node_id, graph_node, proof } => {
            let emb = graph_node
                .embedding
                .iter()
                .map(|f| json_f32(*f))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"kind\":\"graph\",\"node_id\":{},\"domain\":{},\"dim\":{},\
                 \"embedding\":[{}],\"proof\":{}}}",
                json_u64(*node_id),
                json_u64(graph_node.domain as u64),
                json_u64(graph_node.embedding.len() as u64),
                emb,
                json_merkle_proof(proof),
            )
        }
    }
}

/// M67/M74/M75: `{"certified_header":{…},"proof_entry":{…}}` — the JSON twin of
/// [`format_account_proof`], shared by the account- and entity-proof reads. Both the
/// `certified_header` (M74) and `proof_entry` (M75) are now fully structured.
fn json_account_proof(ch: &CertifiedHeader, entry: &ProofEntry) -> String {
    format!(
        "{{\"certified_header\":{},\"proof_entry\":{}}}",
        json_certified_header(ch),
        json_proof_entry(entry),
    )
}

/// M76: an `Embedding` / `[f32]` slice as a JSON number array (each via `json_f32`, so
/// a non-finite component renders as `null`).
fn json_embedding(emb: &[f32]) -> String {
    format!(
        "[{}]",
        emb.iter().map(|f| json_f32(*f)).collect::<Vec<_>>().join(","),
    )
}

/// M76: a `GraphNode` as `{"node_id","domain","dim","embedding":[…]}` (nested form, used
/// inside the batch graph-leaf renderer; distinct from `json_proof_entry`'s flattened
/// `"kind":"graph"` leaf).
fn json_graph_node(gn: &crate::engine::GraphNode) -> String {
    format!(
        "{{\"node_id\":{},\"domain\":{},\"dim\":{},\"embedding\":{}}}",
        json_u64(gn.node_id),
        json_u64(gn.domain as u64),
        json_u64(gn.embedding.len() as u64),
        json_embedding(&gn.embedding),
    )
}

/// M76: one cert-signed graph leaf `{"node_id","graph_node":{…},"proof":{…}}` — shared by
/// the kNN / range neighbour tuples (`(node_id, GraphNode, Proof)`) and `DiffClaim`'s
/// `GraphLeafAtHeight`.
fn json_graph_leaf(node_id: u64, gn: &crate::engine::GraphNode, proof: &crate::merkle::Proof) -> String {
    format!(
        "{{\"node_id\":{},\"graph_node\":{},\"proof\":{}}}",
        json_u64(node_id),
        json_graph_node(gn),
        json_merkle_proof(proof),
    )
}

/// M76: the M26 kNN claim as `{"query":[…],"k":…,"neighbours":[graph_leaf…]}`.
fn json_knn_claim(c: &crate::light::KnnClaim) -> String {
    let neighbours = c
        .neighbours
        .iter()
        .map(|(id, gn, proof)| json_graph_leaf(*id, gn, proof))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"query\":{},\"k\":{},\"neighbours\":[{}]}}",
        json_embedding(&c.query),
        json_u64(c.k as u64),
        neighbours,
    )
}

/// M76: the M27 range claim as `{"query":[…],"min_sim":…,"nodes":[graph_leaf…]}`.
fn json_range_claim(c: &crate::light::RangeClaim) -> String {
    let nodes = c
        .nodes
        .iter()
        .map(|(id, gn, proof)| json_graph_leaf(*id, gn, proof))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"query\":{},\"min_sim\":{},\"nodes\":[{}]}}",
        json_embedding(&c.query),
        json_f32(c.min_sim),
        nodes,
    )
}

/// M76: the M28 diff body as `{"added":[graph_leaf…],"dropped":[graph_leaf…]}`.
fn json_diff_claim(d: &crate::DiffClaim) -> String {
    let leaves = |xs: &[crate::GraphLeafAtHeight]| {
        xs.iter()
            .map(|l| json_graph_leaf(l.node_id, &l.graph_node, &l.proof))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "{{\"added\":[{}],\"dropped\":[{}]}}",
        leaves(&d.added),
        leaves(&d.dropped),
    )
}

/// M76: a single `Validator` as `{"id","pubkey","power"}`.
fn json_validator(v: &crate::validator::Validator) -> String {
    format!(
        "{{\"id\":{},\"pubkey\":{},\"power\":{}}}",
        json_u64(v.id),
        json_str(&crate::hash::hex(&v.pubkey)),
        json_u64(v.power),
    )
}

/// M76: a `ValidatorSet` as a JSON array of validators.
fn json_validator_set(s: &crate::validator::ValidatorSet) -> String {
    format!(
        "[{}]",
        s.validators().iter().map(json_validator).collect::<Vec<_>>().join(","),
    )
}

/// M76: the M28 temporal-diff envelope — both certified headers + their certs + the diff
/// body + both tracked validator sets. Reuses `json_block_header`/`json_commit` (M74).
fn json_diff_envelope(e: &crate::light::DiffEnvelope) -> String {
    format!(
        "{{\"header_prev\":{},\"cert_prev\":{},\"header_new\":{},\"cert_new\":{},\
         \"diff\":{},\"tracked_set_h1\":{},\"tracked_set_h2\":{}}}",
        json_block_header(&e.header_prev),
        json_commit(&e.cert_prev),
        json_block_header(&e.header_new),
        json_commit(&e.cert_new),
        json_diff_claim(&e.diff),
        json_validator_set(&e.tracked_set_h1),
        json_validator_set(&e.tracked_set_h2),
    )
}

/// M76: one `BatchResponseItem` slot, kind-tagged; the inner body is bare `null` on the
/// empty/unknown `Option`, else the structured entry/claim/envelope.
fn json_batch_item(it: &crate::light::BatchResponseItem) -> String {
    use crate::light::BatchResponseItem as I;
    match it {
        I::Inclusion(Some(e)) => format!("{{\"kind\":\"inclusion\",\"entry\":{}}}", json_proof_entry(e)),
        I::Inclusion(None) => "{\"kind\":\"inclusion\",\"entry\":null}".to_string(),
        I::Knn(Some(c)) => format!("{{\"kind\":\"knn\",\"claim\":{}}}", json_knn_claim(c)),
        I::Knn(None) => "{\"kind\":\"knn\",\"claim\":null}".to_string(),
        I::Range(Some(c)) => format!("{{\"kind\":\"range\",\"claim\":{}}}", json_range_claim(c)),
        I::Range(None) => "{\"kind\":\"range\",\"claim\":null}".to_string(),
        I::Diff(e) => format!("{{\"kind\":\"diff\",\"envelope\":{}}}", json_diff_envelope(e)),
    }
}

/// M76: the full `BatchResponseEnvelope` as `{"items":[…]}` — replaces the M67 opaque
/// hex in `json_batch`.
fn json_batch_envelope(env: &BatchResponseEnvelope) -> String {
    format!(
        "{{\"items\":[{}]}}",
        env.items.iter().map(json_batch_item).collect::<Vec<_>>().join(","),
    )
}

/// M77: a `BridgeLock` as all six fields (incl. `signature` — this is a proof, so full
/// fidelity). Distinct from [`json_lock_listing`]'s flattened directory shape (which
/// interleaves `lock_id`/`height` and omits the signature).
fn json_bridge_lock(l: &crate::BridgeLock) -> String {
    format!(
        "{{\"account\":{},\"amount\":{},\"dest_chain\":{},\"dest_account\":{},\
         \"nonce\":{},\"signature\":{}}}",
        json_u64(l.account),
        json_u64(l.amount),
        json_str(&crate::hash::hex(&l.dest_chain)),
        json_u64(l.dest_account),
        json_u64(l.nonce),
        json_str(&crate::hash::hex(&l.signature)),
    )
}

/// M77: the self-contained bridge-lock proof envelope — certified source header + cert +
/// tracked validator set + the lock + its merkle inclusion path. Reuses the M74/M75/M76
/// header/commit/validator-set/merkle renderers; only `lock` needs a new helper.
fn json_lock_envelope(env: &crate::bridge::LockEnvelope) -> String {
    format!(
        "{{\"source_header\":{},\"source_cert\":{},\"source_tracked_set\":{},\
         \"lock_id\":{},\"lock\":{},\"proof\":{}}}",
        json_block_header(&env.source_header),
        json_commit(&env.source_cert),
        json_validator_set(&env.source_tracked_set),
        json_u64(env.lock_id),
        json_bridge_lock(&env.lock),
        json_merkle_proof(&env.proof),
    )
}

/// M67/M77: `{"lock_envelope":{…}}` — the JSON twin of [`format_lock`]. The envelope is
/// now a structured object (M77); the text sibling still carries it as opaque hex.
fn json_lock(env: &crate::bridge::LockEnvelope) -> String {
    format!("{{\"lock_envelope\":{}}}", json_lock_envelope(env))
}

/// M78: a `Review` as `{"reviewer","score"}`.
fn json_review(r: &crate::Review) -> String {
    format!("{{\"reviewer\":{},\"score\":{}}}", json_u64(r.reviewer), json_f32(r.score))
}

/// M78: a `SubmissionTx` — the unit of work, with its embedding and reviews.
fn json_submission_tx(tx: &crate::SubmissionTx) -> String {
    let reviews = tx.reviews.iter().map(json_review).collect::<Vec<_>>().join(",");
    format!(
        "{{\"author\":{},\"embedding\":{},\"domain\":{},\"stake\":{},\"reviews\":[{}],\
         \"repl_success\":{},\"repl_total\":{},\"timestamp_days\":{},\"signature\":{}}}",
        json_u64(tx.author),
        json_embedding(&tx.embedding),
        json_u64(tx.domain as u64),
        json_u64(tx.stake),
        reviews,
        json_u64(tx.repl_success as u64),
        json_u64(tx.repl_total as u64),
        json_f32(tx.timestamp_days),
        json_str(&crate::hash::hex(&tx.signature)),
    )
}

/// M78: a `StakeOp` — a bond/unbond staking op (enum → string discriminator).
fn json_stake_op(op: &crate::StakeOp) -> String {
    let kind = match op.kind {
        crate::BondKind::Bond => "bond",
        crate::BondKind::Unbond => "unbond",
    };
    format!(
        "{{\"account\":{},\"kind\":{},\"amount\":{},\"signature\":{}}}",
        json_u64(op.account),
        json_str(kind),
        json_u64(op.amount),
        json_str(&crate::hash::hex(&op.signature)),
    )
}

/// M78: a `SlashEvidence` — a pair of conflicting precommit votes (reuses `json_vote`).
fn json_slash_evidence(e: &crate::SlashEvidence) -> String {
    format!("{{\"vote_a\":{},\"vote_b\":{}}}", json_vote(&e.vote_a), json_vote(&e.vote_b))
}

/// M78: a `BridgeHeader` — a followed source header + cert + next set.
fn json_bridge_header(h: &crate::BridgeHeader) -> String {
    format!(
        "{{\"source_chain\":{},\"header\":{},\"cert\":{},\"next_set\":{}}}",
        json_str(&crate::hash::hex(&h.source_chain)),
        json_block_header(&h.header),
        json_commit(&h.cert),
        json_validator_set(&h.next_set),
    )
}

/// M78: a `BridgeRedeem` — a cross-chain lock redemption (reuses `json_bridge_lock`).
fn json_bridge_redeem(r: &crate::BridgeRedeem) -> String {
    format!(
        "{{\"source_chain\":{},\"source_header\":{},\"source_cert\":{},\"lock_id\":{},\
         \"lock\":{},\"proof\":{}}}",
        json_str(&crate::hash::hex(&r.source_chain)),
        json_block_header(&r.source_header),
        json_commit(&r.source_cert),
        json_u64(r.lock_id),
        json_bridge_lock(&r.lock),
        json_merkle_proof(&r.proof),
    )
}

/// M78: a full `Block` — the 8 header-scalar fields + the 7 body vectors, each via its
/// leaf renderer. (`Block` holds the real body vecs, not `BlockHeader`'s `*_commitment`
/// digests, so it needs its own renderer.)
fn json_block(b: &crate::Block) -> String {
    let join = |parts: Vec<String>| parts.join(",");
    let txs = join(b.txs.iter().map(json_submission_tx).collect());
    let vus = join(b.validator_updates.iter().map(json_validator_update).collect());
    let sops = join(b.stake_ops.iter().map(json_stake_op).collect());
    let ev = join(b.slashing_evidence.iter().map(json_slash_evidence).collect());
    let locks = join(b.bridge_locks.iter().map(json_bridge_lock).collect());
    let bhdrs = join(b.bridge_headers.iter().map(json_bridge_header).collect());
    let brdms = join(b.bridge_redeems.iter().map(json_bridge_redeem).collect());
    format!(
        "{{\"height\":{},\"prev_hash\":{},\"timestamp_days\":{},\"next_validators_root\":{},\
         \"state_root\":{},\"accounts_root\":{},\"graph_root\":{},\"bridge_root\":{},\
         \"txs\":[{}],\"validator_updates\":[{}],\"stake_ops\":[{}],\"slashing_evidence\":[{}],\
         \"bridge_locks\":[{}],\"bridge_headers\":[{}],\"bridge_redeems\":[{}]}}",
        json_u64(b.height),
        json_str(&crate::hash::hex(&b.prev_hash)),
        json_f32(b.timestamp_days),
        json_str(&crate::hash::hex(&b.next_validators_root)),
        json_str(&crate::hash::hex(&b.state_root)),
        json_str(&crate::hash::hex(&b.accounts_root)),
        json_str(&crate::hash::hex(&b.graph_root)),
        json_str(&crate::hash::hex(&b.bridge_root)),
        txs, vus, sops, ev, locks, bhdrs, brdms,
    )
}

/// M78: the batch read's block range as `[{"block":{…},"commit":{…}}, …]` (empty → `[]`).
fn json_range_blocks(range: &[(Block, Commit)]) -> String {
    let items = range
        .iter()
        .map(|(b, c)| format!("{{\"block\":{},\"commit\":{}}}", json_block(b), json_commit(c)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{items}]")
}

/// M67/M74/M76/M78: `{"certified_header":{…},"batch_envelope":{…},"range_blocks":[…]}`
/// — the JSON twin of [`format_batch`]. The `certified_header` (M74), `batch_envelope`
/// (M76) and `range_blocks` (M78) are all structured; no opaque-hex field remains.
fn json_batch(ch: &CertifiedHeader, env: &BatchResponseEnvelope, range: &[(Block, Commit)]) -> String {
    format!(
        "{{\"certified_header\":{},\"batch_envelope\":{},\"range_blocks\":{}}}",
        json_certified_header(ch),
        json_batch_envelope(env),
        json_range_blocks(range),
    )
}

/// M66: a `200 OK` body whose rendering follows the requested [`RespFormat`] — plain
/// text (the default) or `application/json`.
fn ok_body(fmt: RespFormat, text: &str, json: &str) -> String {
    match fmt {
        RespFormat::Text => http_response("200 OK", text),
        RespFormat::Json => http_response_ct("200 OK", "application/json", json),
    }
}

/// M67: a status-agnostic *error* body for a semantic failure (data miss, decode
/// failure, admission reject), rendered as plain text or `{"error":"<msg>"}` per
/// [`RespFormat`]. Pure transport/framing errors (`411`/`413`/truncated body/`431`/
/// `503`) and routing misses (`GetRoute::NotFound`) stay plain text — format is only
/// threaded into responses that carry a semantic body.
fn error_body(fmt: RespFormat, status_line: &str, msg: &str) -> String {
    match fmt {
        RespFormat::Text => http_response(status_line, msg),
        RespFormat::Json => {
            http_response_ct(status_line, "application/json", &format!("{{\"error\":{}}}", json_str(msg)))
        }
    }
}

/// M66: a `404 Not Found` body for a *data* miss (the entity id does not exist), as
/// plain text or `{"error":"<msg>"}` per [`RespFormat`].
fn not_found_body(fmt: RespFormat, msg: &str) -> String {
    error_body(fmt, "404 Not Found", msg)
}

async fn serve_rpc_conn(mut stream: TcpStream, cmd: mpsc::UnboundedSender<Cmd>) {
    // Accumulate bytes until we see the end-of-headers marker, keeping any body
    // bytes that arrived in the same read.
    let mut acc: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    let header_end = loop {
        match stream.read(&mut buf).await {
            Ok(0) => return, // client closed before a complete request
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                if let Some(pos) = acc.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
                if acc.len() >= MAX_RPC_HEADER {
                    let _ = stream.write_all(http_response("431 Request Header Fields Too Large", "header too large").as_bytes()).await;
                    return;
                }
            }
            Err(_) => return,
        }
    };

    // The request line + headers are ASCII; a non-UTF-8 header block is malformed.
    let Ok(head) = std::str::from_utf8(&acc[..header_end]) else {
        let _ = stream.write_all(http_response("400 Bad Request", "malformed headers").as_bytes()).await;
        return;
    };
    let method = head.split_whitespace().next().unwrap_or("");
    let target = head.split_whitespace().nth(1).unwrap_or("");
    // M66: split any `?query` off the request target before routing, then read the
    // optional `?format=json`. A query-less target leaves `path` byte-identical, so
    // pre-M66 routing and plaintext responses are unchanged. M68: fall back to the
    // `Accept` header when no `?format=` is present (query still wins). M69: honor
    // RFC 7231 §5.3 q-values and answer `406` when the client accepts neither type.
    let (path, query) = split_query(target);
    let fmt = match resolve_format(query, head) {
        Some(f) => f,
        None => {
            // M69/M70/M80: Accept present but no offered representation acceptable — list
            // what we *can* emit (RFC 7231 §6.5.6) as a machine-readable JSON object.
            let _ = stream
                .write_all(
                    http_response_ct("406 Not Acceptable", "application/json", &not_acceptable_json())
                        .as_bytes(),
                )
                .await;
            let _ = stream.flush().await;
            return;
        }
    };

    // M58: read-class GET routes. Each read routes through the single-owner actor via
    // the same local-oneshot pattern as the POST path below; a send failure (actor
    // stopped) reports 503. Unrecognized GET paths stay a 200 health probe.
    if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
        let resp = match route_get(path) {
            GetRoute::Height => {
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::Query(reply)).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok((height, _)) => ok_body(fmt, &height.to_string(), &json_height(height)),
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::Head => {
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::Query(reply)).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok((_, head_hash)) => {
                            ok_body(fmt, &crate::hash::hex(&head_hash), &json_head(&head_hash))
                        }
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::Account(id) => {
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryAccount { id, reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(Some(a)) => ok_body(fmt, &format_account(id, &a), &json_account(id, &a)),
                        Ok(None) => not_found_body(fmt, &format!("account {id} not found")),
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::Plain(kind, id) => {
                // M65: plain (unverified) reviewer/validator/graph read — the non-proof
                // sibling of `GET /account/{id}`. 404 when no such entity.
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryEntity { kind, id, reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(Some(v)) => ok_body(fmt, &format_entity(&v), &json_entity(&v)),
                        Ok(None) => {
                            not_found_body(fmt, &format!("{} {id} not found", proof_kind_label(kind)))
                        }
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::AccountProof(id) => {
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryAccountProof { id, reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(Some((ch, entry))) => ok_body(
                            fmt,
                            &format_account_proof(&ch, &entry),
                            &json_account_proof(&ch, &entry),
                        ),
                        Ok(None) => not_found_body(fmt, &format!("account {id} not found")),
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::Proof(kind, id) => {
                // M60: reviewer/validator/graph verifiable read — reuses the
                // kind-agnostic `format_account_proof` (it hex-encodes the pair).
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryInclusion { kind, id, reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(Some((ch, entry))) => ok_body(
                            fmt,
                            &format_account_proof(&ch, &entry),
                            &json_account_proof(&ch, &entry),
                        ),
                        Ok(None) => not_found_body(
                            fmt,
                            &format!("{} {id} not found", proof_kind_label(kind)),
                        ),
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::BridgeLock(id) => {
                // M63: bridge-lock verifiable read — `serve_lock` returns a
                // self-contained `LockEnvelope`, so the body is a single hex line.
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryLock { lock_id: id, reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(Some(env)) => ok_body(fmt, &format_lock(&env), &json_lock(&env)),
                        Ok(None) => {
                            not_found_body(fmt, &format!("bridge lock {id} not found"))
                        }
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::BridgeLocks => {
                // M64: plain bridge-lock directory — always a (possibly empty) `200`.
                let (reply, rx) = oneshot::channel();
                if cmd.send(Cmd::QueryLocks { reply }).is_err() {
                    http_response("503 Service Unavailable", "node stopped")
                } else {
                    match rx.await {
                        Ok(listing) => {
                            // M72: optional `?offset=`/`?limit=` window. M79: wrap the
                            // page in the `total`/`next` envelope — `total` is the full
                            // unwindowed count, `next` the offset to request for the
                            // following page (none once the window reaches the end).
                            // M81: an absent `?limit` now falls back to
                            // `DEFAULT_PAGE_LIMIT` and any requested limit is clamped to
                            // `MAX_PAGE_LIMIT` (no read can return an unbounded body); the
                            // `next` envelope still signals the remaining pages.
                            let offset = usize_param(query, "offset").unwrap_or(0);
                            let limit = Some(effective_limit(usize_param(query, "limit")));
                            let total = listing.len();
                            let page = paginate(&listing, offset, limit);
                            let start = offset.min(total);
                            let next = if start + page.len() < total {
                                Some(start + page.len())
                            } else {
                                None
                            };
                            ok_body(
                                fmt,
                                &format_page(&format_lock_listing(page), total, next),
                                &json_page(&json_lock_listing(page), total, next),
                            )
                        }
                        Err(_) => http_response("503 Service Unavailable", "node stopped"),
                    }
                }
            }
            GetRoute::Health => http_response("200 OK", "ok"),
            GetRoute::NotFound => http_response("404 Not Found", "not found"),
        };
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // Anything that isn't a POST is treated as a health probe.
    if !method.eq_ignore_ascii_case("POST") {
        let _ = stream.write_all(http_response("200 OK", "ok").as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // M61: `POST /batch` is a verifiable batch read — an encoded `Vec<BatchItem>`
    // body in, the certified head + `BatchResponseEnvelope` out. Any other POST
    // path stays the M53 tx-submission path verbatim.
    if path == "/batch" {
        let Some(len) = parse_content_length(head) else {
            let _ = stream.write_all(http_response("411 Length Required", "missing content-length").as_bytes()).await;
            return;
        };
        if len > MAX_RPC_BODY {
            let _ = stream.write_all(http_response("413 Payload Too Large", "batch too large").as_bytes()).await;
            return;
        }
        let mut body: Vec<u8> = acc[header_end..].to_vec();
        while body.len() < len {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(_) => return,
            }
        }
        if body.len() < len {
            let _ = stream.write_all(http_response("400 Bad Request", "truncated body").as_bytes()).await;
            return;
        }
        body.truncate(len);

        let items = match crate::net::decode_batch_request(&body) {
            Ok(items) => items,
            Err(e) => {
                let _ = stream.write_all(error_body(fmt, "400 Bad Request", &e.to_string()).as_bytes()).await;
                let _ = stream.flush().await;
                return;
            }
        };

        let (reply, rx) = oneshot::channel();
        if cmd.send(Cmd::QueryBatch { items, reply }).is_err() {
            let _ = stream.write_all(http_response("503 Service Unavailable", "node stopped").as_bytes()).await;
            return;
        }
        let resp = match rx.await {
            Ok(Some((ch, env, range))) => ok_body(
                fmt,
                &format_batch(&ch, &env, &range),
                &json_batch(&ch, &env, &range),
            ),
            Ok(None) => error_body(fmt, "422 Unprocessable Entity", "batch rejected"),
            Err(_) => http_response("503 Service Unavailable", "node stopped"),
        };
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
        return;
    }

    // Determine how many body bytes to expect, capped.
    let Some(len) = parse_content_length(head) else {
        let _ = stream.write_all(http_response("411 Length Required", "missing content-length").as_bytes()).await;
        return;
    };
    if len > MAX_RPC_BODY {
        let _ = stream.write_all(http_response("413 Payload Too Large", "tx too large").as_bytes()).await;
        return;
    }

    // Body bytes already read past the header terminator, plus whatever remains.
    let mut body: Vec<u8> = acc[header_end..].to_vec();
    while body.len() < len {
        match stream.read(&mut buf).await {
            Ok(0) => break, // client closed early
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    }
    if body.len() < len {
        let _ = stream.write_all(http_response("400 Bad Request", "truncated body").as_bytes()).await;
        return;
    }
    body.truncate(len);

    // Decode raw codec bytes into a SubmissionTx.
    let tx = match crate::codec::decode_tx(&body) {
        Ok(tx) => tx,
        Err(e) => {
            let _ = stream.write_all(error_body(fmt, "400 Bad Request", &e.to_string()).as_bytes()).await;
            let _ = stream.flush().await;
            return;
        }
    };

    // Submit through the actor and report the admission result.
    let (reply, rx) = oneshot::channel();
    if cmd.send(Cmd::SubmitTx { tx: Box::new(tx), reply }).is_err() {
        let _ = stream.write_all(http_response("503 Service Unavailable", "node stopped").as_bytes()).await;
        return;
    }
    let resp = match rx.await {
        Ok(Ok(h)) => {
            let hex = crate::hash::hex(&h);
            ok_body(fmt, &hex, &format!("{{\"hash\":{}}}", json_str(&hex)))
        }
        Ok(Err(e)) => error_body(fmt, "422 Unprocessable Entity", &e.to_string()),
        Err(_) => http_response("503 Service Unavailable", "node stopped"),
    };
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

fn cfg_io(e: ConfigError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}

/// M51: the address a node gossips for itself in M39 peer exchange. When
/// `advertise` is set it's a public/dialable address (NAT / port-mapped
/// deployments); empty ⇒ the bind `listen`, exactly the M39 behavior
/// (byte-identical back-compat). Only the *advertised* address changes here —
/// the listener still binds `listen`.
fn self_advertise_addr(listen: &str, advertise: &str) -> String {
    if advertise.is_empty() {
        listen.to_string()
    } else {
        advertise.to_string()
    }
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

        // M42: channel binding folds the TLS exporter into the auth transcript, so
        // it is meaningless without both a TLS channel to bind to and an auth
        // handshake to bind it into. Refuse to start on a nonsensical combination
        // rather than silently produce transcripts no peer can match.
        if cfg.network.bind_channel && (!cfg.network.enable_tls || !cfg.network.require_peer_auth) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "node {my_id} sets bind_channel but requires enable_tls + require_peer_auth"
                ),
            ));
        }

        // M43: genesis-pinned mTLS presents this node's genesis key as its TLS
        // credential, so it needs a TLS channel to authenticate and a validator
        // signing key to present. Refuse the nonsensical combinations up front.
        if cfg.network.require_peer_certs && !cfg.network.enable_tls {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("node {my_id} sets require_peer_certs but not enable_tls"),
            ));
        }
        if cfg.network.require_peer_certs && validator_key.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "node {my_id} sets require_peer_certs but has no validator signing key to present"
                ),
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

        let mut node = GossipNode::new(
            my_id,
            genesis.clone(),
            cfg.mempool.max_block_txs,
            peer_ids.iter().copied(),
        );
        // M54: bound the pending pool (DoS hardening). Default capacity is ample
        // (localnet never reaches it ⇒ head unchanged); operators tune `[mempool]`.
        node.set_mempool_capacity(cfg.mempool.capacity);
        // M55: bound the gossip dedup sets (FIFO eviction). `seen_cache == 0` ⇒
        // unbounded (the default) ⇒ setter not called ⇒ behavior byte-identical.
        if cfg.mempool.seen_cache > 0 {
            node.set_seen_capacity(cfg.mempool.seen_cache);
        }
        // M57: bound per-account pending occupancy (DoS hardening). `per_account_limit
        // == 0` ⇒ unbounded (the default) ⇒ setter not called ⇒ behavior unchanged.
        if cfg.mempool.per_account_limit > 0 {
            node.set_mempool_per_account_limit(cfg.mempool.per_account_limit);
        }
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
        addrs.insert(my_id, self_advertise_addr(&cfg.node.listen, &cfg.network.advertise_addr));
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
        // M41: build the TLS setup once when `[network] enable_tls` is on so every
        // link (listener + boot/discovered connectors) shares the same acceptor +
        // connector. M43: when `require_peer_certs` is on too, build genesis-pinned
        // mTLS — the credential is this node's genesis seed and the verifier admits
        // only genesis validators (guarded by the fail-fasts above).
        let validators: HashMap<u64, PubKey> =
            genesis.validators.iter().map(|(id, pk, _)| (*id, *pk)).collect();
        let tls = if cfg.network.enable_tls {
            let mtls = if cfg.network.require_peer_certs {
                Some(MtlsMaterial {
                    seed: validator_key
                        .as_ref()
                        .expect("guarded by require_peer_certs fail-fast")
                        .secret_seed(),
                    validators: Arc::new(
                        genesis.validators.iter().map(|(_, pk, _)| *pk).collect(),
                    ),
                })
            } else {
                None
            };
            Some(build_tls_setup(mtls)?)
        } else {
            None
        };
        let auth = Arc::new(AuthContext {
            my_id,
            kp: validator_key.clone(),
            validators,
            require: cfg.network.require_peer_auth,
            tls,
            bind_channel: cfg.network.bind_channel,
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
            peer_connects: 0,
            local_txs: 0,
            blocks_committed: 0,
            slashing_events: 0,
            peer_tx_buckets: HashMap::new(),
            tx_rate: cfg.mempool.per_peer_tx_per_sec,
            tx_burst: cfg.mempool.per_peer_tx_burst,
            txs_rate_limited: 0,
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

        // M53: optionally bind the external transaction-ingress RPC. Opt-in like
        // metrics: only when `[rpc]` is present and `enabled = true`. Default off
        // ⇒ no write path is exposed and the daemon behaves exactly as before.
        if let Some(rc) = cfg.rpc.as_ref().filter(|r| r.enabled) {
            let addr = rc.listen_addr().map_err(cfg_io)?;
            let rlistener = TcpListener::bind(addr).await?;
            info!(node = my_id, addr = %rlistener.local_addr()?, "rpc listening");
            tokio::spawn(run_rpc(rlistener, my_id, cmd_tx.clone()));
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
///
/// M44: this is the config-free default — `init_tracing_with(None)` — kept so
/// callers with no `[logging]` config (e.g. `localnet`) stay byte-identical.
pub fn init_tracing() {
    init_tracing_with(None);
}

/// M44: install the subscriber from an optional `[logging]` config. `None` (or a
/// defaulted section) reproduces the M37 behavior verbatim: `info` fallback
/// filter, text formatter, stderr. `level` replaces only the *default* filter —
/// `RUST_LOG` still wins when set. `format = "json"` switches to the JSON
/// formatter. Idempotent for the same reasons as [`init_tracing`].
///
/// M45: `file` non-empty switches the writer to a rolling log file (see
/// [`build_file_appender`]); `file` empty keeps the M37/M44 stderr writer, so an
/// absent/defaulted section is still byte-identical.
///
/// M46: with a file target, `stderr = true` tees to **both** the file and stderr
/// (via a layered `Registry`, see [`init_tee`]). The two single-sink arms below are
/// unchanged, so an absent `stderr` knob stays byte-identical to M45/M44/M37.
///
/// M47: within the tee, `stderr_level`/`file_level` give each sink its own
/// `EnvFilter` (see [`init_tee_leveled`]); both empty (or `RUST_LOG` set) keeps the
/// M46 shared-filter tee, so an absent per-sink knob stays byte-identical to M46.
///
/// M48: `levels`/`stderr_levels`/`file_levels` are array forms of the three scalar
/// filter knobs — a non-empty array (joined by `,`, see [`resolve_directive`]) wins
/// over its scalar; all empty ⇒ the scalars are used verbatim, so an absent array
/// stays byte-identical to M47.
///
/// M49: within the tee, `stderr_format`/`file_format` give each sink its own
/// formatter (text/json, see [`init_tee_leveled`]); both empty ⇒ both sinks inherit
/// `format` (the M48/M46 tee), so an absent per-sink format stays byte-identical.
pub fn init_tracing_with(logging: Option<&crate::config::LoggingConfig>) {
    use tracing_subscriber::{fmt, EnvFilter};
    let lc = logging.cloned().unwrap_or_default(); // level "info", format "text", stderr
    // M48: resolve each scalar↔array pair into one directive string (array wins).
    let base = resolve_directive(&lc.level, &lc.levels);
    let se = resolve_directive(&lc.stderr_level, &lc.stderr_levels);
    let fe = resolve_directive(&lc.file_level, &lc.file_levels);
    // `RUST_LOG` is the global override. When set & valid it wins everywhere (incl.
    // both tee sinks); when unset, the per-sink levels (M47) can take effect.
    let rust_log = EnvFilter::try_from_default_env();
    let filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => EnvFilter::new(&base),
    };
    // `format` (text/json) and the writer (stderr/file) form a 2×2. Branch on the
    // writer first because `.with_writer(...)` changes the builder's type, then on
    // the format inside each arm. `filter` is moved into exactly one arm.
    let json = lc.format == "json";
    if lc.file.is_empty() {
        // Exactly the M37/M44 path — stderr, byte-identical when `[logging]` absent.
        let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
        if json {
            let _ = builder.json().try_init();
        } else {
            let _ = builder.try_init();
        }
    } else if !lc.stderr {
        // M45 path — file only, byte-identical to an M45 file target without a tee.
        let builder = fmt().with_env_filter(filter).with_writer(build_file_appender(&lc.file, &lc.rotation, lc.max_files));
        if json {
            let _ = builder.json().try_init();
        } else {
            let _ = builder.try_init();
        }
    } else {
        // Tee: file + stderr simultaneously.
        let per_sink_level = rust_log.is_err() && (!se.is_empty() || !fe.is_empty());
        // M49: per-sink formatter override. Independent of RUST_LOG (which governs
        // filtering, not formatting); empty ⇒ inherit the base `format`.
        let sjson = if lc.stderr_format.is_empty() { json } else { lc.stderr_format == "json" };
        let fjson = if lc.file_format.is_empty() { json } else { lc.file_format == "json" };
        let per_sink_fmt = !lc.stderr_format.is_empty() || !lc.file_format.is_empty();
        if !per_sink_level && !per_sink_fmt {
            // M46 path — one shared filter + one shared format for both sinks
            // (byte-identical to M46).
            init_tee(filter, json, &lc.file, &lc.rotation, lc.max_files);
        } else {
            // M47/M49 path — each sink gets its own EnvFilter and/or formatter. The
            // per-sink filter builder honors RUST_LOG (global) first, else the per-sink
            // directive, else the resolved base `level`. When `per_sink_level` is set,
            // RUST_LOG is unset (the gate), so this reduces to the M47 `EnvFilter::new`.
            let mk = |dir: &str| match EnvFilter::try_from_default_env() {
                Ok(f) => f,
                Err(_) => EnvFilter::new(if dir.is_empty() { &base } else { dir }),
            };
            let (sdir, fdir) = if per_sink_level { (se.as_str(), fe.as_str()) } else { ("", "") };
            init_tee_leveled(mk(sdir), mk(fdir), sjson, fjson, &lc.file, &lc.rotation, lc.max_files);
        }
    }
}

/// M48: compose one `EnvFilter` directive string from a scalar knob and its array
/// counterpart. A non-empty `array` (its non-empty, trimmed entries joined by `,`)
/// wins; otherwise the scalar `s` is returned verbatim — so an absent/empty array is
/// byte-identical to M47. Pure and tracing-free for easy testing.
fn resolve_directive(s: &str, array: &[String]) -> String {
    let joined: Vec<&str> = array
        .iter()
        .map(|d| d.trim())
        .filter(|d| !d.is_empty())
        .collect();
    if joined.is_empty() {
        s.to_string()
    } else {
        joined.join(",")
    }
}

/// M46: install a subscriber that writes to **both** stderr and a rolling file at
/// once. Two writers can't share the all-in-one `fmt()` subscriber, so the tee uses
/// a layered `Registry` with one `fmt::Layer` per writer under a single shared
/// `EnvFilter`. This path runs only for `file != "" && stderr == true`; the
/// single-sink cases keep using `fmt()` untouched (byte-identical). Idempotent via
/// `try_init`; the blocking `RollingFileAppender` needs no `WorkerGuard`.
fn init_tee(
    filter: tracing_subscriber::EnvFilter,
    json: bool,
    file: &str,
    rotation: &str,
    max_files: usize,
) {
    use tracing_subscriber::{fmt, prelude::*};
    let appender = build_file_appender(file, rotation, max_files);
    if json {
        let _ = tracing_subscriber::registry()
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .with(fmt::layer().json().with_writer(appender))
            .with(filter)
            .try_init();
    } else {
        let _ = tracing_subscriber::registry()
            .with(fmt::layer().with_writer(std::io::stderr))
            .with(fmt::layer().with_writer(appender))
            .with(filter)
            .try_init();
    }
}

/// M47: like [`init_tee`], but each sink carries its **own** `EnvFilter` instead of
/// one shared filter on the registry — so stderr and the file can log at different
/// levels. `EnvFilter` implements `Filter`, so `layer.with_filter(env)` attaches it
/// per layer. Runs only when `file != "" && stderr == true` and at least one per-sink
/// level (RUST_LOG unset) or per-sink format is set; otherwise [`init_tee`] (shared
/// filter + shared format) is used. Idempotent via `try_init`.
///
/// M49: each sink also carries its **own** formatter — `stderr_json`/`file_json`
/// select text vs JSON independently. `.json()` returns a differently-typed layer, so
/// the 2×2 format combos are enumerated (no boxing, matching [`init_tee`]'s style).
fn init_tee_leveled(
    stderr_filter: tracing_subscriber::EnvFilter,
    file_filter: tracing_subscriber::EnvFilter,
    stderr_json: bool,
    file_json: bool,
    file: &str,
    rotation: &str,
    max_files: usize,
) {
    use tracing_subscriber::{fmt, prelude::*, Layer};
    let appender = build_file_appender(file, rotation, max_files);
    match (stderr_json, file_json) {
        (false, false) => {
            let _ = tracing_subscriber::registry()
                .with(fmt::layer().with_writer(std::io::stderr).with_filter(stderr_filter))
                .with(fmt::layer().with_writer(appender).with_filter(file_filter))
                .try_init();
        }
        (false, true) => {
            let _ = tracing_subscriber::registry()
                .with(fmt::layer().with_writer(std::io::stderr).with_filter(stderr_filter))
                .with(fmt::layer().json().with_writer(appender).with_filter(file_filter))
                .try_init();
        }
        (true, false) => {
            let _ = tracing_subscriber::registry()
                .with(fmt::layer().json().with_writer(std::io::stderr).with_filter(stderr_filter))
                .with(fmt::layer().with_writer(appender).with_filter(file_filter))
                .try_init();
        }
        (true, true) => {
            let _ = tracing_subscriber::registry()
                .with(fmt::layer().json().with_writer(std::io::stderr).with_filter(stderr_filter))
                .with(fmt::layer().json().with_writer(appender).with_filter(file_filter))
                .try_init();
        }
    }
}

/// M45: build a rolling-file writer from a `[logging] file` path + `rotation` knob.
/// The path is split into a parent directory (created best-effort) and a file-name
/// prefix; `RollingFileAppender` implements `MakeWriter` directly, so it feeds
/// straight into `fmt().with_writer(...)` in blocking mode — no `WorkerGuard`.
///
/// M50: `max_files` caps retained rotated files (oldest pruned). `0` ⇒ the M45
/// unbounded `RollingFileAppender::new` path, byte-identical; `> 0` ⇒ the builder
/// with `max_log_files`, falling back to `::new` on any builder error.
fn build_file_appender(
    file: &str,
    rotation: &str,
    max_files: usize,
) -> tracing_appender::rolling::RollingFileAppender {
    use std::path::Path;
    use tracing_appender::rolling::RollingFileAppender;
    let path = Path::new(file);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("node.log"));
    // Best-effort — a create/open failure just yields no file output (init already
    // swallows subscriber errors); a bad `rotation` is rejected earlier at load.
    let _ = std::fs::create_dir_all(dir);
    let rot = parse_rotation(rotation);
    if max_files == 0 {
        // M45 path — unbounded retention, kept byte-identical for the default.
        RollingFileAppender::new(rot, dir, prefix)
    } else {
        // M50 path — cap retained rotated files; fall back to the unbounded `::new`
        // on any builder error rather than losing file output (init is best-effort).
        let prefix = prefix.to_string_lossy().into_owned();
        RollingFileAppender::builder()
            .rotation(rot.clone())
            .filename_prefix(prefix.clone())
            .max_log_files(max_files)
            .build(dir)
            .unwrap_or_else(|_| RollingFileAppender::new(rot, dir, &prefix))
    }
}

/// M45: map a validated `rotation` string to a `Rotation`. The config layer rejects
/// unknown values at load time; `DAILY` is a defensive default here.
fn parse_rotation(rotation: &str) -> tracing_appender::rolling::Rotation {
    use tracing_appender::rolling::Rotation;
    match rotation {
        "hourly" => Rotation::HOURLY,
        "minutely" => Rotation::MINUTELY,
        "never" => Rotation::NEVER,
        _ => Rotation::DAILY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_advertise_addr_prefers_override() {
        // M51: empty advertise ⇒ the bind `listen`, verbatim (byte-identical M39
        // seed). A non-empty advertise wins (the public/dialable NAT address).
        assert_eq!(self_advertise_addr("0.0.0.0:9021", ""), "0.0.0.0:9021");
        assert_eq!(
            self_advertise_addr("0.0.0.0:9021", "203.0.113.7:9021"),
            "203.0.113.7:9021"
        );
    }

    #[test]
    fn init_tracing_is_idempotent() {
        // M44: the split init path must be safe to call from either entry point,
        // more than once, and with either format — the global-subscriber
        // `try_init` error is swallowed, so none of these panic. (Output capture
        // against a process-global subscriber isn't asserted; the byte-identical
        // default is covered structurally by `unwrap_or_default` + the config
        // `logging_defaults_off_and_parses` test.)
        init_tracing();
        init_tracing_with(None);
        init_tracing_with(Some(&crate::config::LoggingConfig {
            level: "warn".into(),
            format: "json".into(),
            ..Default::default()
        }));
    }

    #[test]
    fn parse_rotation_and_file_init() {
        // M45: the rotation string maps to the right `Rotation` (and an unknown
        // value defaults to DAILY defensively — the config layer rejects it first).
        // Compare via Debug so the test does not depend on `Rotation: PartialEq`.
        use tracing_appender::rolling::Rotation;
        let dbg = |r: &str| format!("{:?}", parse_rotation(r));
        assert_eq!(dbg("hourly"), format!("{:?}", Rotation::HOURLY));
        assert_eq!(dbg("minutely"), format!("{:?}", Rotation::MINUTELY));
        assert_eq!(dbg("never"), format!("{:?}", Rotation::NEVER));
        assert_eq!(dbg("daily"), format!("{:?}", Rotation::DAILY));
        assert_eq!(dbg("weekly"), format!("{:?}", Rotation::DAILY)); // defensive default

        // And the file branch of `init_tracing_with` builds a rolling appender
        // (creating the parent dir) and installs without panicking — extends the
        // M44 idempotency coverage to the non-stderr writer path.
        let dir = std::env::temp_dir().join(format!("zhixing-m45-log-{}", std::process::id()));
        let file = dir.join("node.log");
        init_tracing_with(Some(&crate::config::LoggingConfig {
            file: file.to_str().unwrap().to_string(),
            rotation: "never".into(),
            ..Default::default()
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tee_init_does_not_panic() {
        // M46: the tee branch (`file` set + `stderr = true`) builds a layered
        // Registry with a stderr layer and a rolling-file layer, creates the parent
        // dir, and installs without panicking — extends the M45 file-branch coverage
        // to the two-writer path. (Global-subscriber `try_init` is swallowed, so this
        // is safe to run alongside the other init tests.)
        let dir = std::env::temp_dir().join(format!("zhixing-m46-tee-{}", std::process::id()));
        let file = dir.join("node.log");
        init_tracing_with(Some(&crate::config::LoggingConfig {
            file: file.to_str().unwrap().to_string(),
            rotation: "never".into(),
            stderr: true,
            ..Default::default()
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tee_per_sink_levels_init_does_not_panic() {
        // M47: with RUST_LOG unset and per-sink levels set, the tee builds a layered
        // Registry where each sink carries its own EnvFilter (stderr=info, file=debug),
        // creates the parent dir, and installs without panicking. (Global-subscriber
        // `try_init` is swallowed, so this is safe alongside the other init tests.)
        let dir =
            std::env::temp_dir().join(format!("zhixing-m47-tee-lvl-{}", std::process::id()));
        let file = dir.join("node.log");
        init_tracing_with(Some(&crate::config::LoggingConfig {
            file: file.to_str().unwrap().to_string(),
            rotation: "never".into(),
            stderr: true,
            stderr_level: "info".into(),
            file_level: "debug".into(),
            ..Default::default()
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_appender_max_files_builds() {
        // M50: with `max_files > 0`, `build_file_appender` takes the builder path
        // (bounded retention via `max_log_files`), creates the parent dir, and yields
        // a working appender; and the file arm of `init_tracing_with` installs without
        // panicking. `max_files == 0` stays on the M45 `::new` path (covered elsewhere).
        let dir =
            std::env::temp_dir().join(format!("zhixing-m50-retain-{}", std::process::id()));
        let file = dir.join("node.log");
        let path = file.to_str().unwrap().to_string();
        // The builder path builds without panicking (a real bounded appender).
        let _appender = build_file_appender(&path, "minutely", 3);
        assert!(dir.exists(), "parent dir is created best-effort");
        // And it threads through the subscriber init just like the M45 file arm.
        init_tracing_with(Some(&crate::config::LoggingConfig {
            file: path,
            rotation: "minutely".into(),
            max_files: 3,
            ..Default::default()
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tee_per_sink_formats_init_does_not_panic() {
        // M49: with per-sink formats set (stderr=text, file=json), the tee builds a
        // layered Registry where each sink carries its own formatter, creates the
        // parent dir, and installs without panicking — exercises the 2×2 format path.
        // (Global-subscriber `try_init` is swallowed, so this is safe alongside the
        // other init tests.)
        let dir =
            std::env::temp_dir().join(format!("zhixing-m49-tee-fmt-{}", std::process::id()));
        let file = dir.join("node.log");
        init_tracing_with(Some(&crate::config::LoggingConfig {
            file: file.to_str().unwrap().to_string(),
            rotation: "never".into(),
            stderr: true,
            stderr_format: "text".into(),
            file_format: "json".into(),
            ..Default::default()
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_directive_composes_arrays() {
        // M48: an empty array ⇒ the scalar verbatim (byte-identical to M47).
        assert_eq!(resolve_directive("info", &[]), "info");
        assert_eq!(resolve_directive("", &[]), "");
        // a non-empty array wins over the scalar and is joined by ','.
        assert_eq!(
            resolve_directive(
                "info",
                &["debug".to_string(), "tokio=warn".to_string()],
            ),
            "debug,tokio=warn",
        );
        // even when the scalar is empty, a non-empty array composes.
        assert_eq!(
            resolve_directive("", &["zhixing_node::daemon=debug".to_string()]),
            "zhixing_node::daemon=debug",
        );
        // empty/whitespace entries are dropped so a stray "" can't poison the join.
        assert_eq!(
            resolve_directive(
                "info",
                &["".to_string(), "  ".to_string(), "warn".to_string()],
            ),
            "warn",
        );
        // an array of only-blank entries falls back to the scalar.
        assert_eq!(resolve_directive("info", &["".to_string(), "  ".to_string()]), "info");
    }

    #[test]
    fn directive_array_init_does_not_panic() {
        // M48: with RUST_LOG unset and a `levels` array set (no file), the single-sink
        // stderr path composes the array into one EnvFilter and installs without
        // panicking. (Global-subscriber `try_init` is swallowed, so this is safe
        // alongside the other init tests.)
        init_tracing_with(Some(&crate::config::LoggingConfig {
            levels: vec!["info".to_string(), "tokio=warn".to_string()],
            ..Default::default()
        }));
    }

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
        let a = auth_transcript(21, &n1, 22, &n2, None);
        assert_eq!(a, auth_transcript(21, &n1, 22, &n2, None), "same inputs ⇒ identical bytes");
        // Swapping the signer/peer roles must change the bytes: each side signs a
        // *distinct* transcript, so one side's signature can't be replayed as the
        // other's.
        assert_ne!(a, auth_transcript(22, &n2, 21, &n1, None));
        // The domain tag is a prefix (cross-protocol signature separation).
        assert!(a.starts_with(AUTH_DOMAIN));
        assert_eq!(a.len(), AUTH_DOMAIN.len() + 8 + 32 + 8 + 32);
    }

    #[test]
    fn auth_transcript_binding_enters_signed_material() {
        // M42: the channel binding is `None` by default and byte-identical to the
        // pre-M42 layout; `Some(b)` appends the 32-byte exporter to the signed bytes.
        let n1 = [5u8; 32];
        let n2 = [6u8; 32];
        let base = auth_transcript(21, &n1, 22, &n2, None);
        assert_eq!(base.len(), AUTH_DOMAIN.len() + 8 + 32 + 8 + 32);

        let bind_a = [7u8; 32];
        let bind_b = [8u8; 32];
        let bound_a = auth_transcript(21, &n1, 22, &n2, Some(&bind_a));
        // The binding is appended (same 32-byte suffix growth) and the prefix is
        // exactly the unbound transcript — so an unbound peer signs different bytes.
        assert_eq!(bound_a.len(), base.len() + 32);
        assert!(bound_a.starts_with(&base));
        assert_ne!(bound_a, base);
        // Different TLS channels ⇒ different exporters ⇒ different signed material.
        let bound_b = auth_transcript(21, &n1, 22, &n2, Some(&bind_b));
        assert_ne!(bound_a, bound_b);
    }

    #[test]
    fn channel_binding_mismatch_rejects_handshake() {
        // M42 (the MITM-relay defense at the crypto layer): a signature made over
        // one channel's binding must NOT verify against a transcript reconstructed
        // with a different channel's binding — which is exactly what a MITM relay
        // produces (its two TLS legs export different keying material). The same
        // binding on both sides still verifies (honest direct peers).
        let n1 = [9u8; 32];
        let n2 = [10u8; 32];
        let leg_a = [11u8; 32]; // real peer ↔ attacker leg
        let leg_b = [12u8; 32]; // attacker ↔ real peer leg

        let signed = kp(21).sign(&auth_transcript(21, &n1, 22, &n2, Some(&leg_a)));
        // Verifier on the other honest end reconstructs with ITS leg's binding:
        let relayed = auth_transcript(21, &n1, 22, &n2, Some(&leg_b));
        assert!(!verify(&kp(21).public(), &relayed, &signed), "relayed sig must fail");
        // Same channel binding on both ends ⇒ verifies.
        let direct = auth_transcript(21, &n1, 22, &n2, Some(&leg_a));
        assert!(verify(&kp(21).public(), &direct, &signed));
    }

    #[test]
    fn auth_sign_verify_round_trip() {
        let n1 = [3u8; 32];
        let n2 = [4u8; 32];
        let t = auth_transcript(21, &n1, 22, &n2, None);
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
            rpc: None,
            logging: None,
            mempool: crate::config::MempoolConfig::default(),
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
            mempool_capacity: 4096,
            mempool_per_account_limit: 32,
            seen_tx: 9,
            seen_tx_capacity: 1024,
            pending_stake_ops: 2,
            pending_evidence: 1,
            peer_connects: 11,
            local_txs: 13,
            blocks_committed: 17,
            slashing_events: 19,
            txs_rate_limited: 23,
            txs_quota_rejected: 29,
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
            ("zhixing_mempool_capacity", 4096),
            ("zhixing_mempool_per_account_limit", 32),
            ("zhixing_seen_tx", 9),
            ("zhixing_seen_tx_capacity", 1024),
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

    #[test]
    fn render_prometheus_emits_counters() {
        let out = render_prometheus(&sample_metrics());
        // M52: each monotonic counter carries its value and a `# TYPE … counter`.
        for (name, value) in [
            ("zhixing_peer_connects_total", 11),
            ("zhixing_local_txs_total", 13),
            ("zhixing_blocks_committed_total", 17),
            ("zhixing_slashing_events_total", 19),
            ("zhixing_txs_rate_limited_total", 23),
            ("zhixing_txs_quota_rejected_total", 29),
        ] {
            assert!(out.contains(&format!("# TYPE {name} counter")), "missing TYPE for {name}");
            assert!(out.contains(&format!("\n{name} {value}\n")), "missing `{name} {value}`");
        }
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
    async fn metrics_counters_advance() {
        // M52: a lone validator (single-validator genesis ⇒ quorum 1) self-commits
        // empty blocks on the M33 heartbeat, so `blocks_committed` climbs on its
        // own; a submitted tx bumps `local_txs`. Exercises the actor-owned counter
        // wiring end-to-end. (The active set comes from genesis, not the peer list,
        // so we must shrink the genesis validator set — not just the seed peers.)
        let dir = tmp_dir("metrics-counters");
        let cfg = node_config(21, 19681, &[21], dir.clone());
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        node.submit(test_tx(21, 0, 1));
        // Let the heartbeat drive a couple of block intervals (1000ms each).
        tokio::time::sleep(Duration::from_millis(2500)).await;

        let m = node.metrics().await.expect("metrics snapshot");
        assert!(m.blocks_committed >= 1, "expected >=1 committed block, got {}", m.blocks_committed);
        assert!(m.local_txs >= 1, "expected >=1 local tx, got {}", m.local_txs);

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

    #[test]
    fn parse_content_length_parses_and_caps() {
        // M53: case-insensitive header lookup; absent/garbage ⇒ None.
        let h = "POST /submit_tx HTTP/1.1\r\nHost: x\r\nContent-Length: 128\r\n\r\n";
        assert_eq!(parse_content_length(h), Some(128));
        let lower = "POST / HTTP/1.1\r\ncontent-length: 7\r\n\r\n";
        assert_eq!(parse_content_length(lower), Some(7));
        let none = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(parse_content_length(none), None);
        let garbage = "POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n";
        assert_eq!(parse_content_length(garbage), None);
    }

    #[tokio::test]
    async fn submit_tx_accepts_valid_and_rejects_invalid() {
        // M53: the Node::submit_tx handle round-trips through the actor and
        // surfaces the admission result. A single-validator genesis (quorum 1)
        // keeps the node live; we assert on the mempool, not on a committed block.
        let dir = tmp_dir("submit-tx-handle");
        let cfg = node_config(21, 19701, &[21], dir.clone());
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let good = test_tx(1, 0, 1);
        let h = good.hash();
        let accepted = node.submit_tx(good).await.expect("actor alive");
        assert_eq!(accepted.expect("valid tx admitted"), h);

        let m = node.metrics().await.expect("metrics");
        assert!(m.mempool >= 1, "valid tx landed in the mempool, got {}", m.mempool);

        // An unknown-author tx fails validation → Err surfaced to the caller.
        let bad = test_tx(99, 1, 2);
        let rejected = node.submit_tx(bad).await.expect("actor alive");
        assert!(rejected.is_err(), "invalid tx is rejected");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_endpoint_accepts_tx_over_tcp() {
        // M53: enable the ingress RPC, POST a codec-encoded tx over TCP, and
        // assert 200 + hash in the body; then confirm it entered the mempool.
        // A malformed body gets a 400.
        let dir = tmp_dir("rpc-endpoint");
        let mut cfg = node_config(21, 19721, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:19821";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Encode a valid tx and POST it.
        let tx = test_tx(1, 0, 1);
        let h = tx.hash();
        let body = crate::codec::encode_tx(&tx);
        let req = {
            let mut r = format!(
                "POST /submit_tx HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend_from_slice(&body);
            r
        };
        let mut stream = TcpStream::connect(rpc_addr).await.expect("connect rpc");
        stream.write_all(&req).await.expect("send request");
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.expect("read response");
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "expected 200, got: {text}");
        assert!(text.contains(&crate::hash::hex(&h)), "expected hash in body: {text}");

        let m = node.metrics().await.expect("metrics");
        assert!(m.mempool >= 1, "tx entered the mempool, got {}", m.mempool);

        // A malformed body (not valid codec) gets a 400.
        let bad_body = b"not a tx";
        let bad_req = {
            let mut r = format!(
                "POST /submit_tx HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bad_body.len()
            )
            .into_bytes();
            r.extend_from_slice(bad_body);
            r
        };
        let mut s2 = TcpStream::connect(rpc_addr).await.expect("connect rpc 2");
        s2.write_all(&bad_req).await.expect("send bad request");
        let mut resp2 = Vec::new();
        s2.read_to_end(&mut resp2).await.expect("read response 2");
        let text2 = String::from_utf8_lossy(&resp2);
        assert!(text2.starts_with("HTTP/1.1 400"), "expected 400, got: {text2}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_get_parses_paths() {
        // M58: the read-class router. Unknown non-/account paths stay Health (200) so
        // the endpoint remains a liveness probe; only a bad account id is NotFound.
        assert!(matches!(route_get("/height"), GetRoute::Height));
        assert!(matches!(route_get("/head"), GetRoute::Head));
        assert!(matches!(route_get("/account/7"), GetRoute::Account(7)));
        assert!(matches!(route_get("/"), GetRoute::Health));
        assert!(matches!(route_get("/whatever"), GetRoute::Health));
        assert!(matches!(route_get("/account/notanum"), GetRoute::NotFound));
    }

    #[test]
    fn format_account_renders_fields() {
        // M58: the plain-text account renderer — a stable key=value line incl. hex pubkey.
        let a = Account {
            pubkey: [0xab; 32],
            balance: 12,
            staked_total: 3,
            earned_total: 4,
            slashed_total: 5,
            submissions: 6,
            accepted: 7,
        };
        assert_eq!(
            format_account(1, &a),
            format!(
                "id=1 balance=12 staked_total=3 earned_total=4 slashed_total=5 \
                 submissions=6 accepted=7 pubkey={}",
                crate::hash::hex(&[0xab; 32])
            )
        );
    }

    #[tokio::test]
    async fn rpc_get_returns_reads() {
        // M58: enable the ingress RPC and exercise the read-class GET routes over real
        // TCP: /height, /head, /account/{id} (known + unknown), and the "/" health
        // probe (preserved from the pre-M58 method-only behavior).
        let dir = tmp_dir("rpc-get");
        let mut cfg = node_config(21, 19761, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:19841";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let _node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        tokio::time::sleep(Duration::from_millis(200)).await;

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // /height → 200 + a bare integer body.
        let h = get(rpc_addr, "/height").await;
        assert!(h.starts_with("HTTP/1.1 200 OK"), "height status: {h}");
        let body = body_of(&h);
        assert!(!body.is_empty() && body.chars().all(|c| c.is_ascii_digit()), "height body: {body:?}");

        // /head → 200 + a 64-char hex head hash.
        let hd = get(rpc_addr, "/head").await;
        assert!(hd.starts_with("HTTP/1.1 200 OK"), "head status: {hd}");
        let body = body_of(&hd);
        assert_eq!(body.len(), 64, "head hex len: {body:?}");
        assert!(body.chars().all(|c| c.is_ascii_hexdigit()), "head hex: {body:?}");

        // /account/1 → 200 + account fields (genesis account 1 exists).
        let a = get(rpc_addr, "/account/1").await;
        assert!(a.starts_with("HTTP/1.1 200 OK"), "account status: {a}");
        assert!(body_of(&a).contains("balance="), "account body: {a}");

        // /account/<unknown> → 404.
        let miss = get(rpc_addr, "/account/999999").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "unknown account: {miss}");

        // "/" → 200 ok (health probe preserved).
        let root = get(rpc_addr, "/").await;
        assert!(root.starts_with("HTTP/1.1 200 OK"), "root status: {root}");
        assert!(body_of(&root).contains("ok"), "root body: {root}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_get_parses_account_proof() {
        // M59: `/account/{id}/proof` is the verifiable read; the bare `{id}` stays the
        // M58 plain read; a bad/empty id in either shape is NotFound (404). The M58
        // routes are unaffected.
        assert!(matches!(route_get("/account/7/proof"), GetRoute::AccountProof(7)));
        assert!(matches!(route_get("/account/7"), GetRoute::Account(7)));
        assert!(matches!(route_get("/account/notanum/proof"), GetRoute::NotFound));
        assert!(matches!(route_get("/account//proof"), GetRoute::NotFound));
        assert!(matches!(route_get("/height"), GetRoute::Height));
        assert!(matches!(route_get("/"), GetRoute::Health));
    }

    #[tokio::test]
    async fn account_inclusion_verifies_end_to_end() {
        // M59: the verifiable read producer bundles the account's inclusion proof with
        // the certified head. A client hex/codec-decodes both and checks them with the
        // existing SPV verifier against its OWN tracked validator set — so the read is
        // provable, not trusted. Here we round-trip through the wire codec and verify.
        let dir = tmp_dir("acct-proof-verify");
        let cfg = node_config(21, 19781, &[21], dir.clone());
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ node self-commits
        let node = Node::start(cfg, genesis.clone(), Some(kp(21))).await.expect("start node");

        // Wait for at least one certified block (empty-block heartbeat gives a head cert).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Genesis account 1 → Some((ch, entry)); round-trip through the wire codec, then
        // verify against the independently-tracked genesis validator set.
        let (ch, entry) = node
            .account_proof(1)
            .await
            .expect("actor up")
            .expect("account 1 exists");
        let ch = crate::codec::decode_certified_header(&crate::codec::encode_certified_header(&ch))
            .expect("certified header round trip");
        let entry = crate::codec::decode_proof_entry(&crate::codec::encode_proof_entry(&entry))
            .expect("proof entry round trip");
        let tracked = crate::light::ValidatorTracker::from_genesis(&genesis)
            .validators()
            .clone();
        crate::light::ValidatorTracker::verify_proof_against_header(
            &ch.header, &ch.cert, &tracked, &entry,
        )
        .expect("account proof must verify against the tracked genesis set");

        // Unknown account → inner None (no such id in state).
        assert!(node.account_proof(999999).await.expect("actor up").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_account_proof_over_tcp() {
        // M59: the verifiable read route over real TCP — a known account returns the two
        // labeled hex lines, an unknown account is 404, and the M58 plain read still works.
        let dir = tmp_dir("rpc-acct-proof");
        let mut cfg = node_config(21, 19801, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:19811";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Wait for a certified head so the proof route has something to verify against.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // /account/1/proof → 200 + two labeled hex lines.
        let p = get(rpc_addr, "/account/1/proof").await;
        assert!(p.starts_with("HTTP/1.1 200 OK"), "proof status: {p}");
        let body = body_of(&p);
        assert!(body.contains("certified_header="), "proof body: {body:?}");
        assert!(body.contains("proof_entry="), "proof body: {body:?}");

        // /account/<unknown>/proof → 404.
        let miss = get(rpc_addr, "/account/999999/proof").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "unknown proof: {miss}");

        // M58 plain read unaffected.
        let a = get(rpc_addr, "/account/1").await;
        assert!(a.starts_with("HTTP/1.1 200 OK"), "account status: {a}");
        assert!(body_of(&a).contains("balance="), "account body: {a}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_proof_formats_and_verifies() {
        // M63: the `/bridge/lock/{id}/proof` body is the single hex line
        // `lock_envelope=`. Build a lock-bearing chain via the driver (a running
        // daemon can't stage a lock — the mempool never carries one), serve it, run
        // it through the actual `format_lock` formatter, then hex-decode and verify
        // the envelope at a destination `BridgeEndpoint` — exactly what a stateless
        // HTTP client would do. Mirrors net.rs `serve_lock_and_lock_envelope_round_trip`.
        use crate::bridge::BridgeEndpoint;
        use crate::driver::ChainDriver;
        use crate::{DeltaKParams, Genesis, MICRO};
        use std::collections::BTreeMap;
        fn seed_for(id: u64) -> [u8; 32] {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            s
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
        // Destination chain B (distinct genesis hash via a different timestamp).
        let mut gb = ga.clone();
        gb.timestamp_days = 1.0;
        let b_genesis_hash = crate::ChainState::genesis(gb.clone()).1;

        // Chain A carries one bridge lock destined for B.
        let seeds: BTreeMap<u64, [u8; 32]> = [(21u64, seed_for(21))].into_iter().collect();
        let mut d = ChainDriver::new(ga.clone(), seeds, 16);
        let lock = crate::BridgeLock {
            account: 1,
            amount: 4 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 7,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        d.stage_bridge_lock(lock);
        d.produce_until_drained(1.0, 16).expect("produce");
        let mut node = crate::net::GossipNode::new(1, ga.clone(), 16, [2u64]);
        node.load_certified(d.blocks(), d.certificates());
        let env = node.serve_lock(0).expect("serve_lock");

        // Format exactly as the RPC handler would, then recover the envelope.
        let body = format_lock(&env);
        let hex = body.strip_prefix("lock_envelope=").expect("lock_envelope= prefix");
        fn unhex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
                .collect()
        }
        let env2 = crate::net::decode_lock_envelope(&unhex(hex)).expect("decode envelope");

        // A destination endpoint verifies the self-contained envelope — no extra fetch.
        let mut endpoint = BridgeEndpoint::new(&gb, &ga);
        endpoint
            .follow_source(&env2.source_header, &env2.source_cert, &env2.source_tracked_set)
            .expect("follow");
        let verified = endpoint.verify_lock(&env2).expect("verify");
        assert_eq!(verified.dest_account, 7);
        assert_eq!(verified.amount, 4 * MICRO);
    }

    #[tokio::test]
    async fn rpc_lock_proof_over_tcp() {
        // M63: the bridge-lock route over real TCP. A live daemon chain carries no
        // lock (the mempool never stages one), so `serve_lock` returns None ⇒ 404 —
        // this exercises routing + gating + the 404 path end to end. The verifiable
        // 200 body is covered in-process by `lock_proof_formats_and_verifies`.
        let dir = tmp_dir("rpc-lock-proof");
        let mut cfg = node_config(21, 20071, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20081";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Wait for a certified head (the route is reachable regardless of locks).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }

        // No lock on this chain → 404.
        let miss = get(rpc_addr, "/bridge/lock/0/proof").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "unknown lock: {miss}");
        // Bare id (no `/proof`) → 404.
        let bare = get(rpc_addr, "/bridge/lock/0").await;
        assert!(bare.starts_with("HTTP/1.1 404"), "bare lock route: {bare}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_bridge_locks_over_tcp() {
        // M64: the bridge-lock directory over real TCP. A live daemon chain carries no
        // lock (the mempool never stages one), so the plain listing is empty — but it
        // is still a `200` (an empty directory is a valid answer, unlike the M63 proof
        // route's `404`). This exercises routing + gating + the empty-200 path end to
        // end; the two-lock listing is covered in-process by `lock_listing_lists_and_formats`.
        let dir = tmp_dir("rpc-bridge-locks");
        let mut cfg = node_config(21, 20091, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20101";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Wait for a certified head (the route is reachable regardless of locks).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
        }

        // Plain directory on an empty chain → 200. M79: the body is now the pagination
        // envelope, so an empty chain renders `total=0` (no `next=` line, no items).
        let resp = get(rpc_addr, "/bridge/locks").await;
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "bridge locks status: {resp}");
        assert_eq!(body_of(&resp), "total=0", "empty chain → total=0 envelope");

        // M72: pagination params don't break framing on the (empty) live chain — an
        // empty window is still a 200 (window correctness over data: `paginate_windows`).
        // M79: the empty window renders the `total=0` envelope (text) / `items:[]` (JSON).
        let paged = get(rpc_addr, "/bridge/locks?limit=1&offset=0").await;
        assert!(paged.starts_with("HTTP/1.1 200 OK"), "paged status: {paged}");
        assert!(paged.contains("Content-Type: text/plain; charset=utf-8\r\n"), "paged ct: {paged}");
        assert_eq!(body_of(&paged), "total=0", "empty window → total=0 envelope");

        let paged_json = get(rpc_addr, "/bridge/locks?format=json&limit=0").await;
        assert!(paged_json.starts_with("HTTP/1.1 200 OK"), "paged json status: {paged_json}");
        assert!(paged_json.contains("Content-Type: application/json\r\n"), "paged json ct: {paged_json}");
        assert_eq!(
            body_of(&paged_json),
            "{\"total\":\"0\",\"next\":null,\"items\":[]}",
            "empty window → total=0 JSON envelope"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_plain_entities_over_tcp() {
        // M65: plain reviewer/validator/graph reads over real TCP. Unlike bridge locks,
        // `test_genesis` carries real reviewers (10,11,12), validators (21-24), and a
        // seed graph node (idx 0), so a live daemon serves 200-with-data. Exercises
        // routing + gating + the 200/404 paths end to end.
        let dir = tmp_dir("rpc-plain-entities");
        let mut cfg = node_config(21, 20131, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20141";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
        }

        // Reviewer 10 (reputation 1.0).
        let rev = get(rpc_addr, "/reviewer/10").await;
        assert!(rev.starts_with("HTTP/1.1 200 OK"), "reviewer status: {rev}");
        assert!(body_of(&rev).contains("kind=reviewer id=10"), "reviewer body: {rev}");
        assert!(body_of(&rev).contains("reputation=1"), "reviewer body: {rev}");

        // Validator 21 (power 1, the only one in this single-validator genesis).
        let val = get(rpc_addr, "/validator/21").await;
        assert!(val.starts_with("HTTP/1.1 200 OK"), "validator status: {val}");
        assert!(body_of(&val).contains("kind=validator id=21 power=1"), "validator body: {val}");

        // Graph node 0 (the seed node, domain 0).
        let gr = get(rpc_addr, "/graph/0").await;
        assert!(gr.starts_with("HTTP/1.1 200 OK"), "graph status: {gr}");
        assert!(body_of(&gr).contains("kind=graph node_id=0 domain=0"), "graph body: {gr}");

        // Unknown reviewer id → 404.
        let miss = get(rpc_addr, "/reviewer/999").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "unknown reviewer: {miss}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn route_get_parses_proof_kinds() {
        // M60: the three sibling verifiable reads parse to `Proof(kind, id)`; graph is
        // addressed by insertion index. A non-numeric or empty id is NotFound (404);
        // a bare id is the M65 plain read (below). M59's account routes and the M58
        // health fallback are unaffected.
        assert!(matches!(route_get("/reviewer/10/proof"), GetRoute::Proof(ProofKind::Reviewer, 10)));
        assert!(matches!(route_get("/validator/21/proof"), GetRoute::Proof(ProofKind::Validator, 21)));
        assert!(matches!(route_get("/graph/0/proof"), GetRoute::Proof(ProofKind::GraphNode, 0)));
        assert!(matches!(route_get("/reviewer/x/proof"), GetRoute::NotFound));
        // M65: a bare id is now the plain read (was NotFound through M64).
        assert!(matches!(route_get("/validator/21"), GetRoute::Plain(ProofKind::Validator, 21)));
        assert!(matches!(route_get("/graph//proof"), GetRoute::NotFound));
        // M59 account routes still resolve to their own variants.
        assert!(matches!(route_get("/account/7/proof"), GetRoute::AccountProof(7)));
        assert!(matches!(route_get("/account/7"), GetRoute::Account(7)));
        assert!(matches!(route_get("/"), GetRoute::Health));
    }

    #[test]
    fn route_get_parses_plain_entities() {
        // M65: a bare `/{reviewer,validator,graph}/{id}` is the plain (unverified) read,
        // the non-proof sibling of `/account/{id}`. The M60 `/proof` form still resolves
        // to `Proof`; a non-numeric or empty id is still NotFound (404).
        assert!(matches!(route_get("/reviewer/10"), GetRoute::Plain(ProofKind::Reviewer, 10)));
        assert!(matches!(route_get("/validator/21"), GetRoute::Plain(ProofKind::Validator, 21)));
        assert!(matches!(route_get("/graph/0"), GetRoute::Plain(ProofKind::GraphNode, 0)));
        // No M60 regression.
        assert!(matches!(route_get("/reviewer/10/proof"), GetRoute::Proof(ProofKind::Reviewer, 10)));
        // Bad ids.
        assert!(matches!(route_get("/reviewer/x"), GetRoute::NotFound));
        assert!(matches!(route_get("/graph/"), GetRoute::NotFound));
    }

    #[test]
    fn format_entity_renders() {
        // M65: each entity kind renders to a grep-friendly `key=value` line.
        let r = format_entity(&EntityView::Reviewer { id: 10, reputation: 1.0 });
        assert_eq!(r, "kind=reviewer id=10 reputation=1");
        let v = format_entity(&EntityView::Validator { id: 21, power: 5, pubkey: [0xAB; 32] });
        assert!(v.starts_with("kind=validator id=21 power=5 pubkey="), "{v}");
        assert!(v.contains(&crate::hash::hex(&[0xABu8; 32])), "{v}");
        let mut emb = [0.0f32; crate::DIM];
        emb[0] = 1.0;
        let g = format_entity(&EntityView::GraphNode { node_id: 3, domain: 7, embedding: emb });
        assert!(g.starts_with("kind=graph node_id=3 domain=7 "), "{g}");
        assert!(g.contains(&format!("dim={}", crate::DIM)), "{g}");
        assert!(g.contains("embedding=1,0"), "{g}");
    }

    #[test]
    fn split_query_splits() {
        // M66: the query is split off at the first `?`; a query-less target leaves the
        // path byte-identical (so pre-M66 routing/responses are unchanged).
        assert_eq!(split_query("/account/10?format=json"), ("/account/10", "format=json"));
        assert_eq!(split_query("/head"), ("/head", ""));
        assert_eq!(split_query("/bridge/locks?a=1&format=json"), ("/bridge/locks", "a=1&format=json"));
        assert_eq!(split_query("/height?"), ("/height", ""));
    }

    #[test]
    fn response_format_parses() {
        // M66/M68: only an exact, case-sensitive `format=json` selects JSON; any other
        // present `format=` value stays Text; absence/unknown key ⇒ None (so M68 can
        // fall back to the Accept header).
        assert_eq!(response_format("format=json"), Some(RespFormat::Json));
        assert_eq!(response_format(""), None);
        assert_eq!(response_format("format=text"), Some(RespFormat::Text));
        assert_eq!(response_format("format=JSON"), Some(RespFormat::Text));
        assert_eq!(response_format("foo=bar"), None);
        assert_eq!(response_format("format="), Some(RespFormat::Text));
        // Not the first pair.
        assert_eq!(response_format("a=1&format=json"), Some(RespFormat::Json));
        // First `format=` wins — a later duplicate does not override it.
        assert_eq!(response_format("format=text&format=json"), Some(RespFormat::Text));
    }

    #[test]
    fn usize_param_parses() {
        // M72: first matching key wins; missing key or unparseable value ⇒ None.
        assert_eq!(usize_param("limit=5", "limit"), Some(5));
        assert_eq!(usize_param("offset=10&limit=5", "limit"), Some(5));
        assert_eq!(usize_param("offset=10&limit=5", "offset"), Some(10));
        assert_eq!(usize_param("", "limit"), None);
        assert_eq!(usize_param("foo=bar", "limit"), None);
        assert_eq!(usize_param("limit=abc", "limit"), None);
        assert_eq!(usize_param("limit=", "limit"), None);
        // First pair with the matching key wins (a later duplicate does not override).
        assert_eq!(usize_param("limit=3&limit=9", "limit"), Some(3));
    }

    #[test]
    fn paginate_windows() {
        // M72: offset/limit window clamps to bounds with saturating arithmetic.
        let items = [0, 1, 2, 3, 4];
        assert_eq!(paginate(&items, 0, None), &items[..]); // full
        assert_eq!(paginate(&items, 0, Some(0)), &[] as &[i32]); // limit 0 ⇒ empty
        assert_eq!(paginate(&items, 2, Some(2)), &[2, 3]); // mid window
        assert_eq!(paginate(&items, 2, None), &[2, 3, 4]); // tail
        assert_eq!(paginate(&items, 10, Some(3)), &[] as &[i32]); // offset past end
        assert_eq!(paginate(&items, 0, Some(99)), &items[..]); // limit clamps to len
        assert_eq!(paginate(&items, usize::MAX, Some(usize::MAX)), &[] as &[i32]); // no overflow
    }

    #[test]
    fn effective_limit_caps_and_defaults() {
        // M81: absent `?limit` falls back to the default; a requested limit is clamped
        // to the max; an explicit `0` stays an empty page (as `paginate` honors).
        assert_eq!(effective_limit(None), DEFAULT_PAGE_LIMIT); // absent ⇒ default
        assert_eq!(effective_limit(Some(2)), 2); // under the cap ⇒ honored
        assert_eq!(effective_limit(Some(0)), 0); // explicit empty page preserved
        assert_eq!(effective_limit(Some(MAX_PAGE_LIMIT)), MAX_PAGE_LIMIT); // at the cap
        assert_eq!(effective_limit(Some(MAX_PAGE_LIMIT + 1)), MAX_PAGE_LIMIT); // clamped
        assert_eq!(effective_limit(Some(usize::MAX)), MAX_PAGE_LIMIT); // huge ⇒ cap
    }

    #[test]
    fn lock_page_envelopes() {
        // M79: `format_page`/`json_page` wrap an already-rendered inner page in the
        // `total`/`next` pagination envelope. `next` present ⇒ more pages remain; at the
        // end of the listing text omits the `next=` line while JSON carries bare `null`.
        // First/middle page (more remaining): `next = Some(2)` of `total = 3`.
        assert_eq!(
            json_page("[{\"x\":1}]", 3, Some(2)),
            "{\"total\":\"3\",\"next\":\"2\",\"items\":[{\"x\":1}]}"
        );
        assert_eq!(format_page("lock_id=0 …", 3, Some(2)), "total=3\nnext=2\nlock_id=0 …");

        // Last page (window reaches the end): `next = None`.
        assert_eq!(
            json_page("[{\"x\":9}]", 3, None),
            "{\"total\":\"3\",\"next\":null,\"items\":[{\"x\":9}]}"
        );
        assert_eq!(format_page("lock_id=2 …", 3, None), "total=3\nlock_id=2 …");

        // Empty page: JSON keeps the `items:[]` array; text is exactly `total=0`
        // (no `next=` line, no trailing newline).
        assert_eq!(json_page("[]", 0, None), "{\"total\":\"0\",\"next\":null,\"items\":[]}");
        assert_eq!(format_page("", 0, None), "total=0");
    }

    #[test]
    fn accept_format_parses() {
        // M68/M69: Accept-header negotiation. Every M68 behavior is preserved under the
        // q-value algorithm: `application/json` named ⇒ Use(Json); any other explicit,
        // satisfiable Accept ⇒ Use(Text); absent ⇒ Absent.
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\nAccept: application/json\r\n\r\n"),
            Negotiation::Use(RespFormat::Json)
        );
        // Case-insensitive header name.
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\naccept: text/plain\r\n\r\n"),
            Negotiation::Use(RespFormat::Text)
        );
        // `*/*` matches both types at equal q; the tie breaks to Text (json not named).
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\nAccept: */*\r\n\r\n"),
            Negotiation::Use(RespFormat::Text)
        );
        // `text/html, */*` likewise keeps plaintext.
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\nAccept: text/html, */*\r\n\r\n"),
            Negotiation::Use(RespFormat::Text)
        );
        // json named alongside text at equal q ⇒ json wins (specificity tie-break).
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\nAccept: application/json, text/plain\r\n\r\n"),
            Negotiation::Use(RespFormat::Json)
        );
        // No Accept header at all.
        assert_eq!(
            accept_format("GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            Negotiation::Absent
        );
    }

    #[test]
    fn parse_qmilli_parses() {
        // M69: RFC q-values parse into fixed-point milli-units (0..=1000); missing or
        // malformed ⇒ the RFC default weight 1000; out-of-range values clamp.
        assert_eq!(parse_qmilli("1"), 1000);
        assert_eq!(parse_qmilli("1.0"), 1000);
        assert_eq!(parse_qmilli("0.9"), 900);
        assert_eq!(parse_qmilli("0.333"), 333);
        assert_eq!(parse_qmilli("0"), 0);
        assert_eq!(parse_qmilli(""), 1000);
        assert_eq!(parse_qmilli("abc"), 1000);
        assert_eq!(parse_qmilli("2.0"), 1000);
        assert_eq!(parse_qmilli("-1"), 0);
    }

    #[test]
    fn accept_format_qvalues() {
        // M69: q-values weight the choice between the two types we can emit.
        assert_eq!(
            accept_format("Accept: text/plain;q=0.9, application/json;q=0.1\r\n"),
            Negotiation::Use(RespFormat::Text)
        );
        assert_eq!(
            accept_format("Accept: application/json;q=0.9, text/plain;q=0.1\r\n"),
            Negotiation::Use(RespFormat::Json)
        );
        // q=0 explicitly rejects a type; both rejected / unmatched ⇒ 406.
        assert_eq!(
            accept_format("Accept: application/json;q=0\r\n"),
            Negotiation::NotAcceptable
        );
        assert_eq!(
            accept_format("Accept: application/xml\r\n"),
            Negotiation::NotAcceptable
        );
        assert_eq!(
            accept_format("Accept: */*;q=0\r\n"),
            Negotiation::NotAcceptable
        );
        // More specific reference wins: `application/*;q=0.9` lifts json above a
        // lower-q text, so json is chosen despite its own range being unlisted.
        assert_eq!(
            accept_format("Accept: text/plain;q=0.2, application/*;q=0.9\r\n"),
            Negotiation::Use(RespFormat::Json)
        );
    }

    #[test]
    fn resolve_format_precedence() {
        // M68/M69: explicit `?format=` query wins over the Accept header and never 406s.
        assert_eq!(
            resolve_format("format=text", "Accept: application/json\r\n"),
            Some(RespFormat::Text)
        );
        assert_eq!(resolve_format("format=json", ""), Some(RespFormat::Json));
        // Query overrides a would-be-406 Accept.
        assert_eq!(
            resolve_format("format=json", "Accept: application/xml\r\n"),
            Some(RespFormat::Json)
        );
        // No query param ⇒ the Accept header decides.
        assert_eq!(
            resolve_format("", "Accept: application/json\r\n"),
            Some(RespFormat::Json)
        );
        // Neither ⇒ Text default (every pre-M68 response stays byte-identical).
        assert_eq!(resolve_format("", ""), Some(RespFormat::Text));
        assert_eq!(resolve_format("a=1", "Host: x\r\n"), Some(RespFormat::Text));
        // Accept present but unsatisfiable ⇒ None (caller emits 406).
        assert_eq!(resolve_format("", "Accept: application/xml\r\n"), None);
    }

    #[test]
    fn not_acceptable_json_lists_representations() {
        // M70/M80: the 406 body is a machine-readable JSON object enumerating every
        // representation we can emit (RFC 7231 §6.5.6), from the single OFFERED_MEDIA_TYPES
        // source of truth.
        let body = not_acceptable_json();
        assert_eq!(body, "{\"error\":\"not_acceptable\",\"available\":[\"application/json\",\"text/plain\"]}");
        assert!(body.contains("application/json"));
        assert!(body.contains("text/plain"));
        // JSON is listed first (it wins the negotiation q-tie when named).
        assert!(body.find("application/json") < body.find("text/plain"));
    }

    #[test]
    fn http_response_sets_vary_accept() {
        // M71: every RPC response advertises `Vary: Accept` (RFC 7231 §7.1.4) so
        // shared caches key on the Accept header, through the single builder.
        let text = http_response("200 OK", "x");
        assert!(text.contains("\r\nVary: Accept\r\n"), "text: {text}");
        assert!(text.starts_with("HTTP/1.1 200 OK"), "text status: {text}");
        assert!(text.contains("Content-Type: text/plain; charset=utf-8\r\n"), "text ct: {text}");
        let json = http_response_ct("200 OK", "application/json", "{}");
        assert!(json.contains("\r\nVary: Accept\r\n"), "json: {json}");
        assert!(json.starts_with("HTTP/1.1 200 OK"), "json status: {json}");
        assert!(json.contains("Content-Type: application/json\r\n"), "json ct: {json}");
    }

    #[test]
    fn json_escapes_control_chars() {
        // M66: `json_str` escapes `"`, `\`, and all control chars (RFC 8259).
        assert_eq!(json_str("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_str("l1\nl2\tx"), "\"l1\\nl2\\tx\"");
        assert_eq!(json_str("\u{0001}"), "\"\\u0001\"");
        // `json_f32` maps non-finite values to `null`; a finite value is a bare token.
        assert_eq!(json_f32(f32::NAN), "null");
        assert_eq!(json_f32(f32::INFINITY), "null");
        assert_eq!(json_f32(1.0), "1");
    }

    #[test]
    fn json_account_renders() {
        // M66: u64 fields are JSON *strings* (lossless), pubkey is a hex string. A
        // `u64::MAX` value round-trips exactly as a string, not a lossy JSON number.
        let a = Account { balance: u64::MAX, pubkey: [0xAB; 32], ..Default::default() };
        let j = json_account(7, &a);
        assert!(j.starts_with("{\"id\":\"7\","), "{j}");
        assert!(j.contains("\"balance\":\"18446744073709551615\""), "{j}");
        assert!(j.contains(&format!("\"pubkey\":\"{}\"", crate::hash::hex(&[0xABu8; 32]))), "{j}");
    }

    #[test]
    fn json_entity_renders() {
        // M66: each JSON entity object carries a `"kind"` discriminator; u64 fields are
        // strings, the graph embedding is a JSON array of numbers.
        let r = json_entity(&EntityView::Reviewer { id: 10, reputation: 1.0 });
        assert_eq!(r, "{\"kind\":\"reviewer\",\"id\":\"10\",\"reputation\":1}");
        let v = json_entity(&EntityView::Validator { id: 21, power: 5, pubkey: [0xAB; 32] });
        assert!(v.starts_with("{\"kind\":\"validator\",\"id\":\"21\",\"power\":\"5\","), "{v}");
        let mut emb = [0.0f32; crate::DIM];
        emb[0] = 1.0;
        let g = json_entity(&EntityView::GraphNode { node_id: 3, domain: 7, embedding: emb });
        assert!(g.starts_with("{\"kind\":\"graph\",\"node_id\":\"3\",\"domain\":\"7\","), "{g}");
        assert!(g.contains(&format!("\"dim\":\"{}\"", crate::DIM)), "{g}");
        assert!(g.contains("\"embedding\":[1,0"), "{g}");
    }

    #[test]
    fn json_lock_listing_renders() {
        // M66: the empty chain renders `[]` (the one intentional divergence from the
        // text renderer's empty string); a single lock is a one-element array.
        assert_eq!(json_lock_listing(&[]), "[]");
        let lock = crate::BridgeLock {
            account: 3,
            amount: u64::MAX,
            dest_chain: [0xCD; 32],
            dest_account: 9,
            nonce: 2,
            signature: [0u8; 64],
        };
        let j = json_lock_listing(&[(1, 5, lock)]);
        assert!(j.starts_with("[{\"lock_id\":\"1\",\"height\":\"5\","), "{j}");
        assert!(j.contains("\"amount\":\"18446744073709551615\""), "{j}");
        assert!(j.contains("\"nonce\":\"2\"}]"), "{j}");
    }

    #[test]
    fn error_body_renders() {
        // M67: `error_body` is status-agnostic — Text is the bare message with a
        // text/plain type; Json is {"error":"<msg>"} with application/json, the message
        // escaped. A status other than 404 carries through, and `not_found_body`
        // delegates to it with a 404 status line.
        let t = error_body(RespFormat::Text, "422 Unprocessable Entity", "nope");
        assert!(t.starts_with("HTTP/1.1 422 Unprocessable Entity"), "{t}");
        assert!(t.contains("Content-Type: text/plain; charset=utf-8\r\n"), "{t}");
        assert!(t.ends_with("\r\n\r\nnope"), "{t}");

        let j = error_body(RespFormat::Json, "422 Unprocessable Entity", "no\"pe");
        assert!(j.starts_with("HTTP/1.1 422 Unprocessable Entity"), "{j}");
        assert!(j.contains("Content-Type: application/json\r\n"), "{j}");
        assert!(j.ends_with("\r\n\r\n{\"error\":\"no\\\"pe\"}"), "{j}");

        let nf = not_found_body(RespFormat::Json, "gone");
        assert!(nf.starts_with("HTTP/1.1 404 Not Found"), "{nf}");
        assert!(nf.ends_with("\r\n\r\n{\"error\":\"gone\"}"), "{nf}");
    }

    #[test]
    fn json_proof_renderers_render() {
        // M67: the hex-wrapping JSON renderers (`json_account_proof` / `json_lock` /
        // `json_batch`) carry the SAME opaque hex as their `format_*` text siblings,
        // just relocated into named fields. Build a lock-bearing certified chain via
        // the driver (as `lock_proof_formats_and_verifies` does), wrap it in a pure
        // `GossipNode`, then assert each JSON body embeds its sibling's exact hex.
        use crate::driver::ChainDriver;
        use crate::light::{BatchItem, ProofKind};
        use crate::{DeltaKParams, Genesis, MICRO};
        use std::collections::BTreeMap;
        fn seed_for(id: u64) -> [u8; 32] {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            s
        }
        let ga = Genesis {
            accounts: vec![(1, 50 * MICRO, kp(1).public())],
            reviewers: vec![(10, 1.0)],
            seed_nodes: vec![(unit(0), 0)],
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
        let seeds: BTreeMap<u64, [u8; 32]> = [(21u64, seed_for(21))].into_iter().collect();
        let mut d = ChainDriver::new(ga.clone(), seeds, 16);
        let lock = crate::BridgeLock {
            account: 1,
            amount: 4 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 7,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        d.stage_bridge_lock(lock);
        d.produce_until_drained(1.0, 16).expect("produce");
        let mut node = crate::net::GossipNode::new(1, ga.clone(), 16, [2u64]);
        node.load_certified(d.blocks(), d.certificates());

        // Account proof (M74/M75): both the shared `certified_header` and the
        // `proof_entry` are now structured objects (`json_certified_header` /
        // `json_proof_entry`); the text sibling still carries both as hex.
        let (ch, entry) = node.account_inclusion(1).expect("account inclusion");
        let ch_hex = crate::hash::hex(&crate::codec::encode_certified_header(&ch));
        let entry_hex = crate::hash::hex(&crate::codec::encode_proof_entry(&entry));
        let text = format_account_proof(&ch, &entry);
        assert!(text.contains(&ch_hex) && text.contains(&entry_hex), "{text}");
        assert_eq!(
            json_account_proof(&ch, &entry),
            format!(
                "{{\"certified_header\":{},\"proof_entry\":{}}}",
                json_certified_header(&ch),
                json_proof_entry(&entry),
            ),
        );

        // Lock envelope (M77): `lock_envelope` is now a structured object wrapping
        // `json_lock_envelope` (header/commit/validator-set/lock/merkle); the text
        // sibling `format_lock` still carries it as hex.
        let env = node.serve_lock(0).expect("serve_lock");
        assert_eq!(json_lock(&env), format!("{{\"lock_envelope\":{}}}", json_lock_envelope(&env)));

        // Batch (M74/M76/M78): structured `certified_header` + `batch_envelope`;
        // `range_blocks` is now a structured array (here empty — Inclusion-only request
        // carries no Diff range).
        let (bch, benv, range) = node
            .batch(vec![BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 10 }])
            .expect("batch");
        let jb = json_batch(&bch, &benv, &range);
        assert!(jb.starts_with("{\"certified_header\":{\"header\":{"), "{jb}");
        assert!(jb.contains("\"batch_envelope\":{\"items\":["), "{jb}");
        assert!(jb.contains("\"range_blocks\":[]"), "{jb}");
        assert!(jb.ends_with("]}"), "{jb}");
    }

    #[test]
    fn route_get_parses_bridge_lock() {
        // M63: `/bridge/lock/{id}/proof` is the only form — a bridge lock has no
        // plain-read route, so a bare id or a non-numeric id is NotFound (404).
        assert!(matches!(route_get("/bridge/lock/0/proof"), GetRoute::BridgeLock(0)));
        assert!(matches!(route_get("/bridge/lock/42/proof"), GetRoute::BridgeLock(42)));
        assert!(matches!(route_get("/bridge/lock/0"), GetRoute::NotFound));
        assert!(matches!(route_get("/bridge/lock/abc/proof"), GetRoute::NotFound));
        assert!(matches!(route_get("/bridge/lock//proof"), GetRoute::NotFound));
    }

    #[test]
    fn route_get_parses_bridge_locks() {
        // M64: `/bridge/locks` is a plain directory read — an exact match, distinct
        // from the M63 `/bridge/lock/{id}/proof` prefix route (char after `…lock` is
        // `s`, not `/`). A trailing slash is not the directory, so it falls through to
        // the M58 health liveness fallback like any unknown path.
        assert!(matches!(route_get("/bridge/locks"), GetRoute::BridgeLocks));
        assert!(matches!(route_get("/bridge/locks/"), GetRoute::Health));
        // M63 single-lock route still resolves to its own variant (no regression).
        assert!(matches!(route_get("/bridge/lock/0/proof"), GetRoute::BridgeLock(0)));
        assert!(matches!(route_get("/bridge/lock/0"), GetRoute::NotFound));
    }

    #[test]
    fn lock_listing_lists_and_formats() {
        // M64: a chain carrying two bridge locks enumerates both (id order, with the
        // right heights/fields), and the formatter emits one `key=value` line each; an
        // empty chain yields an empty listing and an empty body. Mirrors the net.rs
        // `sample_lock_envelope` driver setup, but staging two locks.
        use crate::driver::ChainDriver;
        use crate::{DeltaKParams, Genesis, MICRO};
        use std::collections::BTreeMap;
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
        let lock0 = crate::BridgeLock {
            account: 1,
            amount: 3 * MICRO,
            dest_chain: [0xAB; 32],
            dest_account: 9,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let lock1 = crate::BridgeLock {
            account: 1,
            amount: 5 * MICRO,
            dest_chain: [0xCD; 32],
            dest_account: 7,
            nonce: 1,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        d.stage_bridge_lock(lock0.clone());
        d.stage_bridge_lock(lock1.clone());
        d.produce_until_drained(1.0, 16).expect("produce");
        let mut node = crate::net::GossipNode::new(1, ga.clone(), 16, [2u64]);
        node.load_certified(d.blocks(), d.certificates());

        let listing = node.lock_listing();
        assert_eq!(listing.len(), 2, "two locks enumerated");
        assert_eq!(listing[0].0, 0, "first id is 0");
        assert_eq!(listing[1].0, 1, "second id is 1");
        assert_eq!(listing[0].2.amount, 3 * MICRO, "lock 0 fields preserved");
        assert_eq!(listing[1].2.dest_account, 7, "lock 1 fields preserved");
        assert!(listing[0].1 >= 1 && listing[1].1 >= 1, "heights are real blocks");

        let body = format_lock_listing(&listing);
        assert!(body.contains("lock_id=0"), "body lists id 0: {body}");
        assert!(body.contains("lock_id=1"), "body lists id 1: {body}");
        assert_eq!(body.lines().count(), 2, "one line per lock");

        // Empty chain → empty listing, empty body.
        let empty = crate::net::GossipNode::new(1, ga, 16, [2u64]);
        assert!(empty.lock_listing().is_empty(), "no locks → empty listing");
        assert_eq!(format_lock_listing(&[]), "", "empty listing → empty body");
    }

    #[tokio::test]
    async fn inclusion_verifies_all_kinds_end_to_end() {
        // M60: the generalized verifiable read producer bundles each `ProofKind`'s
        // inclusion proof with the certified head. For reviewer / validator / graph node,
        // a client round-trips the pair through the wire codec and checks it with the
        // existing SPV verifier against its OWN tracked genesis set — validator proofs
        // route to `next_validators_root`, the others to `accounts_root`. test_genesis
        // already carries reviewers (10,11,12), a seed graph node (idx 0), and validators.
        let dir = tmp_dir("inclusion-all-kinds");
        let cfg = node_config(21, 20011, &[21], dir.clone());
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ node self-commits
        let node = Node::start(cfg, genesis.clone(), Some(kp(21))).await.expect("start node");

        // Wait for a certified head so each proof has a signed header to verify against.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let tracked = crate::light::ValidatorTracker::from_genesis(&genesis)
            .validators()
            .clone();
        for (kind, id) in [
            (ProofKind::Reviewer, 10u64),
            (ProofKind::Validator, 21),
            (ProofKind::GraphNode, 0),
        ] {
            let (ch, entry) = node
                .proof(kind, id)
                .await
                .expect("actor up")
                .unwrap_or_else(|| panic!("{kind:?} {id} should exist in genesis"));
            // Round-trip through the wire codec, exactly as a remote client would.
            let ch = crate::codec::decode_certified_header(
                &crate::codec::encode_certified_header(&ch),
            )
            .expect("certified header round trip");
            let entry = crate::codec::decode_proof_entry(&crate::codec::encode_proof_entry(&entry))
                .expect("proof entry round trip");
            crate::light::ValidatorTracker::verify_proof_against_header(
                &ch.header, &ch.cert, &tracked, &entry,
            )
            .unwrap_or_else(|e| panic!("{kind:?} {id} proof must verify: {e:?}"));
        }

        // Unknown id / index for each kind → inner None.
        assert!(node.proof(ProofKind::Reviewer, 99).await.expect("actor up").is_none());
        assert!(node.proof(ProofKind::Validator, 99).await.expect("actor up").is_none());
        assert!(node.proof(ProofKind::GraphNode, 9999).await.expect("actor up").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_other_proofs_over_tcp() {
        // M60: the reviewer / validator / graph verifiable reads over real TCP — each
        // known id returns the two labeled hex lines, an unknown id is 404, and a bare
        // `{id}` without `/proof` (these have no plain-read form) is also 404.
        let dir = tmp_dir("rpc-other-proofs");
        let mut cfg = node_config(21, 20031, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20051";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Wait for a certified head so the proof routes have something to verify against.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // Each known entity → 200 + two labeled hex lines.
        for path in ["/validator/21/proof", "/reviewer/10/proof", "/graph/0/proof"] {
            let p = get(rpc_addr, path).await;
            assert!(p.starts_with("HTTP/1.1 200 OK"), "{path} status: {p}");
            let body = body_of(&p);
            assert!(body.contains("certified_header="), "{path} body: {body:?}");
            assert!(body.contains("proof_entry="), "{path} body: {body:?}");
        }

        // Unknown id → 404.
        let miss = get(rpc_addr, "/validator/999/proof").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "unknown proof: {miss}");
        // M65: a bare `{id}` (no `/proof`) is now the plain (unverified) read → 200 with
        // a `key=value` body, the non-proof sibling of `/account/{id}`.
        let bare = get(rpc_addr, "/validator/21").await;
        assert!(bare.starts_with("HTTP/1.1 200 OK"), "bare id: {bare}");
        assert!(body_of(&bare).contains("kind=validator id=21"), "bare id body: {bare}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_json_reads_over_tcp() {
        // M66: `?format=json` on a structured plain read returns an `application/json`
        // body; a data miss returns `{"error":…}`; and a query-less read stays
        // byte-identical to the pre-M66 `text/plain` response.
        let dir = tmp_dir("rpc-json");
        let mut cfg = node_config(21, 20151, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20161";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ self-commit
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // Each JSON read is 200 + application/json + the expected JSON shape.
        let h = get(rpc_addr, "/height?format=json").await;
        assert!(h.starts_with("HTTP/1.1 200 OK"), "height: {h}");
        assert!(h.contains("Content-Type: application/json\r\n"), "height ct: {h}");
        // M71: representation-selected responses advertise `Vary: Accept` for caches.
        assert!(h.contains("Vary: Accept\r\n"), "height vary: {h}");
        assert!(body_of(&h).starts_with("{\"height\":\""), "height body: {h}");

        let a = get(rpc_addr, "/account/1?format=json").await;
        assert!(a.starts_with("HTTP/1.1 200 OK"), "account: {a}");
        assert!(a.contains("Content-Type: application/json\r\n"), "account ct: {a}");
        assert!(body_of(&a).contains("\"balance\":\""), "account body: {a}");

        let r = get(rpc_addr, "/reviewer/10?format=json").await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "reviewer: {r}");
        assert!(body_of(&r).contains("\"kind\":\"reviewer\""), "reviewer body: {r}");

        let locks = get(rpc_addr, "/bridge/locks?format=json").await;
        assert!(locks.starts_with("HTTP/1.1 200 OK"), "locks: {locks}");
        assert!(locks.contains("Content-Type: application/json\r\n"), "locks ct: {locks}");
        // M79: JSON locks are now the pagination envelope `{"total":…,"next":…,"items":[…]}`.
        assert!(body_of(&locks).starts_with("{\"total\":"), "locks body: {locks}");
        assert!(body_of(&locks).contains("\"items\":["), "locks body: {locks}");

        // A data miss under JSON → 404 + application/json + {"error":…}.
        let miss = get(rpc_addr, "/reviewer/999?format=json").await;
        assert!(miss.starts_with("HTTP/1.1 404"), "miss: {miss}");
        assert!(miss.contains("Content-Type: application/json\r\n"), "miss ct: {miss}");
        assert!(body_of(&miss).starts_with("{\"error\":"), "miss body: {miss}");

        // Head invariant at the response layer: a query-less read is byte-identical to
        // the pre-M66 plaintext response (text/plain, bare decimal height).
        let plain = get(rpc_addr, "/height").await;
        assert!(plain.contains("Content-Type: text/plain; charset=utf-8\r\n"), "plain ct: {plain}");
        // M71: the text representation also carries `Vary: Accept`.
        assert!(plain.contains("Vary: Accept\r\n"), "plain vary: {plain}");
        assert!(body_of(&plain).chars().all(|c| c.is_ascii_digit()), "plain body: {plain}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_accept_header_over_tcp() {
        // M68: standard HTTP content negotiation. `Accept: application/json` selects
        // JSON with no query param; an explicit `?format=` still wins over Accept; and a
        // request with neither stays byte-identical to the pre-M68 plaintext response.
        let dir = tmp_dir("rpc-accept");
        let mut cfg = node_config(21, 20191, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20201";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ self-commit
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str, accept: Option<&str>) -> String {
            let accept_line = accept.map(|a| format!("Accept: {a}\r\n")).unwrap_or_default();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\n{accept_line}Connection: close\r\n\r\n"
            );
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // Accept: application/json with no query param ⇒ JSON.
        let h = get(rpc_addr, "/height", Some("application/json")).await;
        assert!(h.starts_with("HTTP/1.1 200 OK"), "height: {h}");
        assert!(h.contains("Content-Type: application/json\r\n"), "height ct: {h}");
        assert!(body_of(&h).starts_with("{\"height\":\""), "height body: {h}");

        let a = get(rpc_addr, "/account/1", Some("application/json")).await;
        assert!(a.starts_with("HTTP/1.1 200 OK"), "account: {a}");
        assert!(a.contains("Content-Type: application/json\r\n"), "account ct: {a}");
        assert!(body_of(&a).starts_with("{\""), "account body: {a}");

        // An explicit `?format=text` overrides `Accept: application/json`.
        let q = get(rpc_addr, "/height?format=text", Some("application/json")).await;
        assert!(q.contains("Content-Type: text/plain; charset=utf-8\r\n"), "query-wins ct: {q}");
        assert!(body_of(&q).chars().all(|c| c.is_ascii_digit()), "query-wins body: {q}");

        // A browser-style `Accept: text/html,*/*` keeps the plaintext representation.
        let b = get(rpc_addr, "/height", Some("text/html,*/*")).await;
        assert!(b.contains("Content-Type: text/plain; charset=utf-8\r\n"), "browser ct: {b}");
        assert!(body_of(&b).chars().all(|c| c.is_ascii_digit()), "browser body: {b}");

        // Head invariant: no Accept header and no query ⇒ byte-identical plaintext.
        let plain = get(rpc_addr, "/height", None).await;
        assert!(plain.contains("Content-Type: text/plain; charset=utf-8\r\n"), "plain ct: {plain}");
        assert!(body_of(&plain).chars().all(|c| c.is_ascii_digit()), "plain body: {plain}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_406_over_tcp() {
        // M69: RFC 7231 §5.3 q-values + `406 Not Acceptable`. An Accept that rules out
        // both types we can emit answers 406; q-values weight the choice between them;
        // an explicit `?format=` overrides a would-be-406; and no Accept stays plaintext.
        let dir = tmp_dir("rpc-406");
        let mut cfg = node_config(21, 20211, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20221";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ self-commit
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str, accept: Option<&str>) -> String {
            let accept_line = accept.map(|a| format!("Accept: {a}\r\n")).unwrap_or_default();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\n{accept_line}Connection: close\r\n\r\n"
            );
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // An Accept we cannot satisfy ⇒ 406, with a machine-readable JSON error body that
        // lists the representations we *can* emit (M70/M80, RFC 7231 §6.5.6).
        let x = get(rpc_addr, "/height", Some("application/xml")).await;
        assert!(x.starts_with("HTTP/1.1 406 Not Acceptable"), "xml: {x}");
        assert!(x.contains("Content-Type: application/json\r\n"), "xml ct: {x}");
        // M71: the negotiated-error path varies by `Accept` too.
        assert!(x.contains("Vary: Accept\r\n"), "xml vary: {x}");
        assert!(body_of(&x).contains("\"error\":\"not_acceptable\""), "xml body is JSON error: {x}");
        assert!(body_of(&x).contains("application/json"), "xml body lists json: {x}");
        assert!(body_of(&x).contains("text/plain"), "xml body lists text: {x}");

        // An explicit q=0 rejects our JSON type ⇒ 406 (no text alternative offered).
        let z = get(rpc_addr, "/height", Some("application/json;q=0")).await;
        assert!(z.starts_with("HTTP/1.1 406 Not Acceptable"), "q0: {z}");
        assert!(z.contains("Content-Type: application/json\r\n"), "q0 ct: {z}");
        assert!(body_of(&z).contains("\"error\":\"not_acceptable\""), "q0 body is JSON error: {z}");
        assert!(body_of(&z).contains("application/json"), "q0 body lists json: {z}");
        assert!(body_of(&z).contains("text/plain"), "q0 body lists text: {z}");

        // Weighted preference: JSON outranks text ⇒ 200 + application/json.
        let w = get(rpc_addr, "/height", Some("text/plain;q=0.3, application/json;q=0.9")).await;
        assert!(w.starts_with("HTTP/1.1 200 OK"), "weighted: {w}");
        assert!(w.contains("Content-Type: application/json\r\n"), "weighted ct: {w}");

        // An explicit `?format=json` overrides a would-be-406 Accept.
        let q = get(rpc_addr, "/height?format=json", Some("application/xml")).await;
        assert!(q.starts_with("HTTP/1.1 200 OK"), "query-wins: {q}");
        assert!(q.contains("Content-Type: application/json\r\n"), "query-wins ct: {q}");

        // Head invariant: no Accept header ⇒ byte-identical plaintext (never 406).
        let plain = get(rpc_addr, "/height", None).await;
        assert!(plain.starts_with("HTTP/1.1 200 OK"), "plain: {plain}");
        assert!(plain.contains("Content-Type: text/plain; charset=utf-8\r\n"), "plain ct: {plain}");
        assert!(body_of(&plain).chars().all(|c| c.is_ascii_digit()), "plain body: {plain}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_json_proofs_over_tcp() {
        // M67: `?format=json` now extends to the proof / submit reads that were
        // text-only through M66. Each returns an `application/json` body wrapping the
        // same opaque hex; submit success is {"hash":…}; a decode failure or data miss
        // is {"error":…}; and a query-less proof read stays byte-identical to today.
        let dir = tmp_dir("rpc-json-proofs");
        let mut cfg = node_config(21, 20171, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20181";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ self-commit
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn get(addr: &str, path: &str) -> String {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(req.as_bytes()).await.expect("send get");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        async fn post(addr: &str, path: &str, body: &[u8]) -> String {
            let mut req = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            req.extend_from_slice(body);
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(&req).await.expect("send post");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // Account proof under JSON → 200 + application/json + {"certified_header":…}.
        let ap = get(rpc_addr, "/account/1/proof?format=json").await;
        assert!(ap.starts_with("HTTP/1.1 200 OK"), "account proof: {ap}");
        assert!(ap.contains("Content-Type: application/json\r\n"), "account proof ct: {ap}");
        assert!(body_of(&ap).starts_with("{\"certified_header\":{\"header\":{"), "account proof body: {ap}");

        // Entity proof reuses the same renderer → carries the proof_entry field.
        let rp = get(rpc_addr, "/reviewer/10/proof?format=json").await;
        assert!(rp.starts_with("HTTP/1.1 200 OK"), "reviewer proof: {rp}");
        assert!(
            body_of(&rp).contains("\"proof_entry\":{\"kind\":"),
            "reviewer proof body: {rp}"
        );

        // The daemon drives no bridge locks → data miss → 404 + {"error":…}.
        let lk = get(rpc_addr, "/bridge/lock/1/proof?format=json").await;
        assert!(lk.starts_with("HTTP/1.1 404"), "bridge lock: {lk}");
        assert!(lk.contains("Content-Type: application/json\r\n"), "bridge lock ct: {lk}");
        assert!(body_of(&lk).starts_with("{\"error\":"), "bridge lock body: {lk}");

        // POST /submit_tx?format=json with a valid tx → {"hash":…}.
        let tx = test_tx(1, 0, 1);
        let hx = crate::hash::hex(&tx.hash());
        let ok = post(rpc_addr, "/submit_tx?format=json", &crate::codec::encode_tx(&tx)).await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "submit json: {ok}");
        assert!(ok.contains("Content-Type: application/json\r\n"), "submit json ct: {ok}");
        assert_eq!(body_of(&ok), format!("{{\"hash\":\"{hx}\"}}"), "submit json body: {ok}");

        // A malformed submit body under JSON → 400 + {"error":…}.
        let bad = post(rpc_addr, "/submit_tx?format=json", b"not a tx").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "submit bad: {bad}");
        assert!(body_of(&bad).starts_with("{\"error\":"), "submit bad body: {bad}");

        // POST /batch?format=json with an encoded request → {"certified_header":…}.
        let items = vec![BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 10 }];
        let batch = post(rpc_addr, "/batch?format=json", &crate::net::encode_batch_request(&items)).await;
        assert!(batch.starts_with("HTTP/1.1 200 OK"), "batch json: {batch}");
        assert!(batch.contains("Content-Type: application/json\r\n"), "batch json ct: {batch}");
        assert!(body_of(&batch).starts_with("{\"certified_header\":{\"header\":{"), "batch json body: {batch}");

        // Head invariant at the response layer: a query-less proof read is byte-identical
        // to the pre-M67 plaintext (text/plain, labeled `certified_header=` hex line).
        let plain = get(rpc_addr, "/account/1/proof").await;
        assert!(plain.contains("Content-Type: text/plain; charset=utf-8\r\n"), "plain ct: {plain}");
        assert!(body_of(&plain).starts_with("certified_header="), "plain body: {plain}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn batch_serves_and_verifies_end_to_end() {
        // M61: the batch read producer bundles a heterogeneous proof batch with the
        // certified head. A client round-trips the certified head + envelope through
        // the wire codec and checks the whole batch with the existing `verify_batch`
        // against its OWN tracked genesis set — inclusion (reviewer / validator) +
        // kNN + range in one shot; validator slots route to `next_validators_root`,
        // the rest to `accounts_root`. No Diff item ⇒ empty `blocks_in_range`.
        let dir = tmp_dir("batch-verify");
        let cfg = node_config(21, 20012, &[21], dir.clone());
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)]; // quorum 1 ⇒ node self-commits
        let node = Node::start(cfg, genesis.clone(), Some(kp(21))).await.expect("start node");

        // Wait for a certified head so the batch has a signed header to verify against.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let items = vec![
            BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 10 },
            BatchItem::Inclusion { kind: ProofKind::Validator, id: 21 },
            BatchItem::Knn { query: unit(0), k: 1 },
            BatchItem::Range { query: unit(0), min_sim: 0.0 },
        ];
        let (ch, env, range) = node
            .batch_proof(items.clone())
            .await
            .expect("actor up")
            .expect("batch served");
        assert_eq!(env.items.len(), items.len(), "one response slot per request item");
        // M62: no Diff item ⇒ the shipped range is empty.
        assert!(range.is_empty(), "a Diff-free batch ships no range blocks");

        // Round-trip both halves through the wire codec, exactly as a remote client would.
        let ch = crate::codec::decode_certified_header(&crate::codec::encode_certified_header(&ch))
            .expect("certified header round trip");
        let env = crate::net::decode_batch_envelope(&crate::net::encode_batch_envelope(&env))
            .expect("batch envelope round trip");

        let tracker = crate::light::ValidatorTracker::from_genesis(&genesis);
        let tracked = tracker.validators().clone();
        tracker
            .verify_batch(&genesis, &ch.header, &ch.cert, &tracked, &[], &items, &env)
            .expect("batch must verify against the tracked genesis set");

        // An unknown inclusion id yields an inner None slot and still verifies (no-op).
        let miss_items = vec![BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 99 }];
        let (ch2, env2, _range2) = node
            .batch_proof(miss_items.clone())
            .await
            .expect("actor up")
            .expect("batch served");
        assert!(matches!(env2.items[0], crate::light::BatchResponseItem::Inclusion(None)), "unknown id ⇒ None slot");
        tracker
            .verify_batch(&genesis, &ch2.header, &ch2.cert, &tracked, &[], &miss_items, &env2)
            .expect("a None slot is a verified no-op");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_batch_over_tcp() {
        // M61: the verifiable batch read over real TCP — a POST /batch with an encoded
        // `Vec<BatchItem>` body returns the two labeled hex lines; a garbage body is
        // 400; and a non-`/batch` POST still routes to the M53 tx-submission path.
        let dir = tmp_dir("rpc-batch");
        let mut cfg = node_config(21, 20032, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20052";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Wait for a certified head so the batch route has something to verify against.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if node.status().await.map(|(h, _)| h >= 1).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never produced a block");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn post(addr: &str, path: &str, body: &[u8]) -> String {
            let mut req = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            req.extend_from_slice(body);
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(&req).await.expect("send post");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }

        // POST /batch with a valid encoded request → 200 + two labeled hex lines.
        let items = vec![
            BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 10 },
            BatchItem::Knn { query: unit(0), k: 1 },
        ];
        let ok = post(rpc_addr, "/batch", &crate::net::encode_batch_request(&items)).await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "batch status: {ok}");
        let body = body_of(&ok);
        assert!(body.contains("certified_header="), "batch body: {body:?}");
        assert!(body.contains("batch_envelope="), "batch body: {body:?}");

        // Garbage body → 400 (undecodable as a batch request).
        let bad = post(rpc_addr, "/batch", b"not a batch").await;
        assert!(bad.starts_with("HTTP/1.1 400"), "garbage batch: {bad}");

        // A non-`/batch` POST still routes to the M53 submit path (undecodable tx → 400,
        // proving it took the tx branch rather than the batch branch).
        let submit = post(rpc_addr, "/submit_tx", b"not a tx").await;
        assert!(submit.starts_with("HTTP/1.1 400"), "submit path status: {submit}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rpc_batch_diff_over_tcp() {
        // M62: a POST /batch carrying a Diff item returns a third `range_blocks=`
        // hex line holding the `[1..=h2]` block range the Diff verifier replays —
        // so a stateless client can verify the Diff slot without pre-syncing.
        let dir = tmp_dir("rpc-batch-diff");
        let mut cfg = node_config(21, 20112, &[21], dir.clone());
        let rpc_addr = "127.0.0.1:20113";
        cfg.rpc = Some(crate::config::RpcConfig { enabled: true, listen: rpc_addr.into() });
        let mut genesis = test_genesis();
        genesis.validators = vec![(21, kp(21).public(), 1)];
        let node = Node::start(cfg, genesis, Some(kp(21))).await.expect("start node");

        // Diff{1,2} needs height ≥ 2.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if node.status().await.map(|(h, _)| h >= 2).unwrap_or(false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "node never reached height 2");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        async fn post(addr: &str, path: &str, body: &[u8]) -> String {
            let mut req = format!(
                "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            req.extend_from_slice(body);
            let mut s = TcpStream::connect(addr).await.expect("connect rpc");
            s.write_all(&req).await.expect("send post");
            let mut resp = Vec::new();
            s.read_to_end(&mut resp).await.expect("read response");
            String::from_utf8_lossy(&resp).into_owned()
        }
        fn body_of(resp: &str) -> &str {
            resp.rsplit("\r\n\r\n").next().unwrap_or("")
        }
        fn unhex(s: &str) -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
                .collect()
        }

        let items = vec![
            BatchItem::Diff { h1: 1, h2: 2 },
            BatchItem::Inclusion { kind: ProofKind::Reviewer, id: 10 },
        ];
        let ok = post(rpc_addr, "/batch", &crate::net::encode_batch_request(&items)).await;
        assert!(ok.starts_with("HTTP/1.1 200 OK"), "batch status: {ok}");
        let body = body_of(&ok);
        assert!(body.contains("certified_header="), "batch body: {body:?}");
        assert!(body.contains("batch_envelope="), "batch body: {body:?}");

        // The new third line carries the `[1..=2]` block range; decode it and confirm
        // it holds exactly two certified blocks.
        let range_hex = body
            .lines()
            .find_map(|l| l.strip_prefix("range_blocks="))
            .expect("range_blocks line present");
        let range = crate::net::decode_blocks(&unhex(range_hex)).expect("decode range blocks");
        assert_eq!(range.len(), 2, "Diff{{1,2}} ships the full [1..=2] range over RPC");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_bucket_refills_and_throttles() {
        // M54: a burst of `burst` tokens is allowed, then requests are denied until
        // time passes; refill is `rate`/sec and capped at `burst`.
        let t0 = Instant::now();
        let (rate, burst) = (1.0, 2.0);
        let mut b = TokenBucket { tokens: burst, last: t0 };
        // Two tokens available, both consumed; the third is denied (no time passes).
        assert!(b.allow(t0, rate, burst));
        assert!(b.allow(t0, rate, burst));
        assert!(!b.allow(t0, rate, burst));
        // After 1s exactly one token refills.
        let t1 = t0 + Duration::from_secs(1);
        assert!(b.allow(t1, rate, burst));
        assert!(!b.allow(t1, rate, burst));
        // A long idle refills no more than `burst` (not unbounded accrual).
        let t2 = t1 + Duration::from_secs(100);
        assert!(b.allow(t2, rate, burst));
        assert!(b.allow(t2, rate, burst));
        assert!(!b.allow(t2, rate, burst));
    }

    #[tokio::test]
    async fn rpc_submit_reports_mempool_full() {
        // M54: with `[mempool] capacity = 1`, the first tx is admitted and a second
        // distinct tx is rejected with MempoolFull, surfaced via Node::submit_tx.
        // Run as a pure follower (no key) so no block production drains the pool.
        let dir = tmp_dir("submit-tx-full");
        let mut cfg = node_config(21, 19741, &[21], dir.clone());
        cfg.mempool.capacity = 1;
        let node = Node::start(cfg, test_genesis(), None).await.expect("start node");

        let first = node.submit_tx(test_tx(1, 0, 1)).await.expect("actor alive");
        assert!(first.is_ok(), "first tx admitted, got {first:?}");
        let second = node.submit_tx(test_tx(2, 0, 1)).await.expect("actor alive");
        assert!(
            matches!(second, Err(crate::ChainError::MempoolFull { capacity: 1 })),
            "second tx rejected as MempoolFull, got {second:?}"
        );

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
            rpc: None,
            logging: None,
            mempool: crate::config::MempoolConfig::default(),
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

    // --- integration: TLS transport encryption (M41) -----------------------------

    #[tokio::test]
    async fn tls_mesh_converges() {
        // Three validators with `enable_tls = true` (auth off). Every P2P link runs
        // over TLS 1.3; the handshake + framing must work unchanged inside the
        // tunnel, so consensus proceeds and heads agree (quorum 3-of-4). Proves the
        // encrypted path is fully functional end-to-end over real sockets.
        let ids = [22u64, 23, 24];
        let port_base = 19851u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("tls-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.network.enable_tls = true;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        let states = await_converged(&nodes, 2, Duration::from_secs(30)).await;
        assert!(states.windows(2).all(|w| w[0] == w[1]), "TLS mesh converged");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn tls_and_auth_mesh_converges() {
        // Both layers on: `enable_tls = true` AND `require_peer_auth = true`. The
        // M40 authenticated handshake runs *inside* the TLS tunnel — encryption and
        // authentication compose. The mesh must still converge to a shared head.
        let ids = [22u64, 23, 24];
        let port_base = 19871u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("tlsauth-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.network.enable_tls = true;
            cfg.network.require_peer_auth = true;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        let states = await_converged(&nodes, 2, Duration::from_secs(30)).await;
        assert!(states.windows(2).all(|w| w[0] == w[1]), "TLS+auth mesh converged");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn plaintext_dialer_rejected_by_tls_node() {
        // One honest validator with `enable_tls = true`. A raw-TCP dialer (no TLS)
        // connects and writes plaintext. The server-side TLS handshake can't parse
        // it as a ClientHello, so the connection never reaches the app handshake and
        // no peer is registered — the network-wide TLS policy holds.
        let port_base = 19891u16;
        let genesis = test_genesis();
        let dir = tmp_dir("tls-plain-n21");
        let mut data_dirs = BTreeMap::new();
        data_dirs.insert(21u64, dir.clone());

        let mut cfg = node_config(21, port_base, &[21], dir);
        cfg.network.enable_tls = true;
        let node = Node::start(cfg, genesis.clone(), Some(kp(21))).await.expect("start node");

        tokio::time::sleep(Duration::from_millis(200)).await;
        let addr = format!("127.0.0.1:{port_base}");
        let mut stream = TcpStream::connect(&addr).await.expect("connect");
        let _ = stream.set_nodelay(true);
        // Plaintext bytes (the old cleartext 8-byte id hello) — not a TLS record.
        let _ = stream.write_all(&21u64.to_be_bytes()).await;
        let _ = stream.flush().await;

        tokio::time::sleep(Duration::from_millis(500)).await;
        let peers = node.metrics().await.map(|m| m.peers).unwrap_or(99);
        assert_eq!(peers, 0, "a plaintext dialer must not join a TLS node");

        cleanup(&data_dirs);
    }

    // --- integration: TLS channel binding (M42) ----------------------------------

    #[tokio::test]
    async fn channel_bound_mesh_converges() {
        // All three layers on: `enable_tls` + `require_peer_auth` + `bind_channel`.
        // Each honest link is a single TLS session, so both ends export the identical
        // keying material and fold the same 32 bytes into the auth transcript — the
        // signatures verify and the mesh converges. Proves channel binding doesn't
        // break honest, directly-connected peers.
        let ids = [22u64, 23, 24];
        let port_base = 19911u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("bind-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.network.enable_tls = true;
            cfg.network.require_peer_auth = true;
            cfg.network.bind_channel = true;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        let states = await_converged(&nodes, 2, Duration::from_secs(30)).await;
        assert!(states.windows(2).all(|w| w[0] == w[1]), "channel-bound mesh converged");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn bind_channel_without_tls_fails_fast() {
        // Channel binding is meaningless without a TLS channel to bind to and an auth
        // handshake to bind it into. `Node::start` must refuse the nonsensical combo
        // rather than emit transcripts no peer can match (mirrors the require_peer_auth
        // / pubkey fail-fasts).
        let genesis = test_genesis();
        let dir = tmp_dir("bind-nofast-n21");
        let mut data_dirs = BTreeMap::new();
        data_dirs.insert(21u64, dir.clone());

        // bind_channel on, but enable_tls left off (require_peer_auth also off).
        let mut cfg = node_config(21, 19931, &[21], dir);
        cfg.network.bind_channel = true;
        let err = Node::start(cfg, genesis.clone(), Some(kp(21))).await;
        assert!(err.is_err(), "bind_channel without enable_tls must fail fast");

        cleanup(&data_dirs);
    }

    // --- genesis-pinned mTLS (M43) ------------------------------------------------

    #[test]
    fn ed25519_spki_pkcs8_derivation_matches_genesis_key() {
        // The TLS credential derived from a genesis seed must round-trip back to the
        // consensus pubkey: build the signer from PKCS#8(seed), read its SPKI, and
        // decode it — the 32 key bytes must equal `Keypair::from_seed(seed).public()`.
        let seed = seed(22);
        let mut pkcs8 = Vec::new();
        pkcs8.extend_from_slice(&ED25519_PKCS8_PREFIX);
        pkcs8.extend_from_slice(&seed);
        let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8);
        let signing_key =
            rustls::crypto::ring::sign::any_eddsa_type(&key_der).expect("ed25519 signer");
        let spki = signing_key.public_key().expect("spki");
        assert_eq!(spki.as_ref().len(), ED25519_SPKI_PREFIX.len() + 32);
        let decoded = spki_to_ed25519(spki.as_ref()).expect("decode spki");
        assert_eq!(decoded, Keypair::from_seed(seed).public());

        // Malformed SPKIs are rejected rather than mis-sliced.
        assert!(spki_to_ed25519(&[0u8; 44]).is_none(), "wrong prefix rejected");
        assert!(spki_to_ed25519(&spki.as_ref()[..40]).is_none(), "wrong length rejected");
    }

    #[test]
    fn genesis_pinned_verifier_accepts_only_validators() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::server::danger::ClientCertVerifier;

        // Build an RFC 7250 raw-public-key SPKI (prefix || key) as rustls hands it to
        // the verifier's `end_entity`.
        let spki_of = |pk: &PubKey| {
            let mut v = Vec::with_capacity(44);
            v.extend_from_slice(&ED25519_SPKI_PREFIX);
            v.extend_from_slice(pk);
            rustls::pki_types::CertificateDer::from(v)
        };

        let members: HashSet<PubKey> = [22u64, 23, 24].iter().map(|&id| kp(id).public()).collect();
        let v = GenesisPinnedVerifier::new(Arc::new(members));

        let inside = spki_of(&kp(23).public());
        let outside = spki_of(&kp(99).public());
        let name = rustls::pki_types::ServerName::try_from("zhixing-node").unwrap();
        let now = rustls::pki_types::UnixTime::now();

        // An in-set key is admitted as both a server and a client credential.
        assert!(v.verify_server_cert(&inside, &[], &name, &[], now).is_ok());
        assert!(v.verify_client_cert(&inside, &[], now).is_ok());
        // An out-of-set key is rejected by both directions.
        assert!(v.verify_server_cert(&outside, &[], &name, &[], now).is_err());
        assert!(v.verify_client_cert(&outside, &[], now).is_err());
    }

    #[tokio::test]
    async fn mtls_mesh_converges() {
        // Three validators with genesis-pinned mTLS on (enable_tls + require_peer_certs
        // + require_peer_auth). Each node presents its genesis key as its TLS credential
        // and admits a peer only if the presented key is a genesis validator — mutually.
        // The honest validators are all in-set, so every link's mTLS handshake succeeds
        // and the mesh converges to a shared head (quorum 3-of-4).
        let ids = [22u64, 23, 24];
        let port_base = 19951u16;
        let genesis = test_genesis();

        let mut nodes = Vec::new();
        let mut data_dirs = BTreeMap::new();
        for &id in &ids {
            let dir = tmp_dir(&format!("mtls-n{id}"));
            data_dirs.insert(id, dir.clone());
            let mut cfg = node_config(id, port_base, &ids, dir);
            cfg.network.enable_tls = true;
            cfg.network.require_peer_certs = true;
            cfg.network.require_peer_auth = true;
            let node = Node::start(cfg, genesis.clone(), Some(kp(id))).await.expect("start node");
            nodes.push((id, node));
        }

        let states = await_converged(&nodes, 2, Duration::from_secs(30)).await;
        assert!(states.windows(2).all(|w| w[0] == w[1]), "mTLS mesh converged");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn tls_only_dialer_rejected_by_mtls_node() {
        // A node with encryption-only TLS cannot present a genesis raw-key credential,
        // so an mTLS node must refuse it at the TLS layer — the mTLS node's peer count
        // stays 0. Node 22 runs mTLS and dials peer 23; node 23 runs encryption-only
        // TLS. Neither handshake direction can complete (the mTLS side demands a raw-key
        // credential the plain-TLS side neither presents nor accepts).
        let port_base = 19971u16;
        let genesis = test_genesis();

        let dir22 = tmp_dir("mtls-reject-n22");
        let dir23 = tmp_dir("mtls-reject-n23");
        let mut data_dirs = BTreeMap::new();
        data_dirs.insert(22u64, dir22.clone());
        data_dirs.insert(23u64, dir23.clone());

        let mut cfg22 = node_config(22, port_base, &[22, 23], dir22);
        cfg22.network.enable_tls = true;
        cfg22.network.require_peer_certs = true;
        let node22 =
            Node::start(cfg22, genesis.clone(), Some(kp(22))).await.expect("start mtls node");

        let mut cfg23 = node_config(23, port_base, &[22, 23], dir23);
        cfg23.network.enable_tls = true; // encryption-only, no mTLS
        let _node23 =
            Node::start(cfg23, genesis.clone(), Some(kp(23))).await.expect("start tls node");

        tokio::time::sleep(Duration::from_millis(800)).await;
        let peers = node22.metrics().await.map(|m| m.peers).unwrap_or(99);
        assert_eq!(peers, 0, "a plain-TLS dialer must not join an mTLS node");

        cleanup(&data_dirs);
    }

    #[tokio::test]
    async fn require_peer_certs_without_tls_fails_fast() {
        // mTLS needs a TLS channel to authenticate over; `require_peer_certs` without
        // `enable_tls` is nonsensical and must be refused at start (mirrors the
        // bind_channel fail-fast).
        let genesis = test_genesis();
        let dir = tmp_dir("mtls-nofast-n21");
        let mut data_dirs = BTreeMap::new();
        data_dirs.insert(21u64, dir.clone());

        let mut cfg = node_config(21, 19991, &[21], dir);
        cfg.network.require_peer_certs = true; // but enable_tls left off
        let err = Node::start(cfg, genesis.clone(), Some(kp(21))).await;
        assert!(err.is_err(), "require_peer_certs without enable_tls must fail fast");

        cleanup(&data_dirs);
    }

    // --- M74: structured JSON for the shared `CertifiedHeader` envelope ----------

    #[test]
    fn json_certified_header_structured() {
        // Build a CertifiedHeader fixture with distinct sentinel values: a full
        // BlockHeader, plus a one-vote finality Commit.
        let header = crate::codec::BlockHeader {
            height: 7,
            prev_hash: [0x01; 32],
            timestamp_days: 1.5,
            next_validators_root: [0x02; 32],
            state_root: [0xAB; 32],
            accounts_root: [0x03; 32],
            graph_root: [0x04; 32],
            bridge_root: [0x05; 32],
            validator_updates: vec![crate::validator::ValidatorUpdate {
                id: 3,
                pubkey: [0x11; 32],
                power: 9,
            }],
            txs_commitment: [0x06; 32],
            stake_ops_commitment: [0x07; 32],
            evidence_commitment: [0x08; 32],
            bridge_locks_commitment: [0x09; 32],
            bridge_headers_commitment: [0x0a; 32],
            bridge_redeems_commitment: [0x0b; 32],
        };
        let cert = Commit {
            height: 7,
            round: 0,
            block_hash: [0xCD; 32],
            precommits: vec![crate::consensus::Vote::signed(
                21,
                7,
                0,
                [0xCD; 32],
                crate::consensus::VoteType::Precommit,
                &kp(21),
            )],
        };
        let ch = CertifiedHeader { header, cert };

        // The certified header renders as a nested object, not an opaque hex string.
        let j = json_certified_header(&ch);
        assert!(j.starts_with("{\"header\":{"), "nested header object: {j}");
        assert!(j.contains("\"height\":\"7\""), "u64 rendered as a quoted string: {j}");
        assert!(j.contains(&format!("\"state_root\":\"{}\"", "ab".repeat(32))), "{j}");
        assert!(j.contains("\"timestamp_days\":1.5"), "f32 as a bare number: {j}");
        assert!(j.contains("\"validator_updates\":[{"), "non-empty update array: {j}");
        assert!(j.contains("\"round\":\"0\""), "u32 as a quoted string: {j}");
        assert!(j.contains("\"vote_type\":\"precommit\""), "enum as discriminator: {j}");
        assert!(j.contains("\"precommits\":[{"), "non-empty precommit array: {j}");
        // Hand-rolled body is well-formed: braces balance.
        assert_eq!(
            j.matches('{').count(),
            j.matches('}').count(),
            "balanced braces: {j}"
        );

        // The proof read now embeds the structured header while `proof_entry` stays
        // an opaque hex string (M74 decodes the shared header only; leaves are M75+).
        let entry = ProofEntry::Account {
            id: 1,
            account: Account {
                pubkey: [0x22; 32],
                balance: 100,
                staked_total: 0,
                earned_total: 0,
                slashed_total: 0,
                submissions: 0,
                accepted: 0,
            },
            proof: crate::merkle::Proof { steps: vec![] },
        };
        let p = json_account_proof(&ch, &entry);
        assert!(p.starts_with("{\"certified_header\":{\"header\":{"), "embedded object: {p}");
        assert!(
            p.contains("\"proof_entry\":{\"kind\":\"account\""),
            "proof_entry is now a structured object: {p}"
        );
    }

    #[test]
    fn json_proof_entry_structured() {
        // M75: `json_proof_entry` renders each `ProofEntry` variant as a
        // `{"kind":…, <leaf fields>, "proof":{…}}` object — the account leaf via
        // `json_account`, the others mirroring `json_entity`, and every variant
        // carrying its merkle inclusion path via `json_merkle_proof`.
        use crate::light::ProofEntry;
        let path = crate::merkle::Proof {
            steps: vec![
                crate::merkle::Step::Left([0xAA; 32]),
                crate::merkle::Step::Right([0xBB; 32]),
            ],
        };
        let merkle_json = "\"proof\":{\"steps\":[{\"side\":\"left\",\"hash\":\"\
             aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},\
             {\"side\":\"right\",\"hash\":\"\
             bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"}]}";

        // Account leaf reuses `json_account` (balance / pubkey present).
        let acct = ProofEntry::Account {
            id: 1,
            account: crate::Account {
                pubkey: kp(1).public(),
                balance: 42,
                staked_total: 0,
                earned_total: 0,
                slashed_total: 0,
                submissions: 0,
                accepted: 0,
            },
            proof: path.clone(),
        };
        let a = json_proof_entry(&acct);
        assert!(a.starts_with("{\"kind\":\"account\",\"account\":{"), "{a}");
        assert!(a.contains("\"balance\":\"42\"") && a.contains("\"pubkey\":\""), "{a}");
        assert!(a.contains(merkle_json), "{a}");

        // Reviewer leaf: bare reputation.
        let rev = ProofEntry::Reviewer { id: 10, reputation: 1.5, proof: path.clone() };
        let r = json_proof_entry(&rev);
        assert!(r.starts_with("{\"kind\":\"reviewer\",\"id\":\"10\",\"reputation\":1.5,"), "{r}");
        assert!(r.contains(merkle_json), "{r}");

        // Validator leaf: id / power / pubkey.
        let val = ProofEntry::Validator {
            id: 21,
            validator: crate::validator::Validator { id: 21, pubkey: kp(21).public(), power: 7 },
            proof: path.clone(),
        };
        let v = json_proof_entry(&val);
        assert!(v.starts_with("{\"kind\":\"validator\",\"id\":\"21\",\"power\":\"7\","), "{v}");
        assert!(v.contains("\"pubkey\":\""), "{v}");
        assert!(v.contains(merkle_json), "{v}");

        // Graph leaf: node_id / domain / dim / embedding[].
        let gn = ProofEntry::GraphNode {
            node_id: 99,
            graph_node: crate::engine::GraphNode {
                node_id: 99,
                embedding: [0.0; 8],
                domain: 3,
            },
            proof: path.clone(),
        };
        let g = json_proof_entry(&gn);
        assert!(g.starts_with("{\"kind\":\"graph\",\"node_id\":\"99\","), "{g}");
        assert!(
            g.contains("\"domain\":\"3\"") && g.contains("\"dim\":\"8\"") && g.contains("\"embedding\":["),
            "{g}"
        );
        assert!(g.contains(merkle_json), "{g}");

        // Braces balance for every variant.
        for s in [&a, &r, &v, &g] {
            let opens = s.chars().filter(|&c| c == '{').count();
            let closes = s.chars().filter(|&c| c == '}').count();
            assert_eq!(opens, closes, "unbalanced braces: {s}");
        }
    }

    #[test]
    fn json_batch_envelope_structured() {
        // M76: `json_batch_envelope` renders every `BatchResponseItem` variant as a
        // kind-tagged object (`null` inner body on the empty/unknown Option); the graph
        // leaves reuse `json_graph_node`/`json_merkle_proof`, the Diff envelope reuses
        // `json_block_header`/`json_commit`. Fixtures mirror the codec round-trip tests.
        use crate::light::{
            BatchResponseEnvelope, BatchResponseItem, DiffEnvelope, KnnClaim, ProofEntry,
            RangeClaim,
        };
        let g = crate::engine::GraphNode { node_id: 7, embedding: [0.25; 8], domain: 42 };
        let path = crate::merkle::Proof {
            steps: vec![crate::merkle::Step::Left([0xAA; 32])],
        };
        let hdr = |height: u64| crate::codec::BlockHeader {
            height,
            prev_hash: [0u8; 32],
            timestamp_days: 0.0,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            validator_updates: vec![],
            txs_commitment: [0u8; 32],
            stake_ops_commitment: [0u8; 32],
            evidence_commitment: [0u8; 32],
            bridge_locks_commitment: [0u8; 32],
            bridge_headers_commitment: [0u8; 32],
            bridge_redeems_commitment: [0u8; 32],
        };
        let cert = |height: u64| Commit { height, round: 0, block_hash: [0u8; 32], precommits: vec![] };
        let acct = crate::Account {
            pubkey: kp(1).public(),
            balance: 100,
            staked_total: 0,
            earned_total: 0,
            slashed_total: 0,
            submissions: 0,
            accepted: 0,
        };
        let diff_env = DiffEnvelope {
            header_prev: hdr(1),
            cert_prev: cert(1),
            header_new: hdr(2),
            cert_new: cert(2),
            diff: crate::DiffClaim {
                added: vec![crate::GraphLeafAtHeight {
                    node_id: 7,
                    graph_node: g.clone(),
                    proof: path.clone(),
                }],
                dropped: vec![],
            },
            tracked_set_h1: crate::validator::ValidatorSet::new(vec![crate::validator::Validator {
                id: 21,
                pubkey: kp(21).public(),
                power: 1,
            }]),
            tracked_set_h2: crate::validator::ValidatorSet::new(vec![]),
        };
        let env = BatchResponseEnvelope {
            items: vec![
                BatchResponseItem::Inclusion(Some(ProofEntry::Account {
                    id: 1,
                    account: acct,
                    proof: path.clone(),
                })),
                BatchResponseItem::Inclusion(None),
                BatchResponseItem::Knn(Some(KnnClaim {
                    query: [0.5; 8],
                    k: 3,
                    neighbours: vec![(7, g.clone(), path.clone())],
                })),
                BatchResponseItem::Range(Some(RangeClaim {
                    query: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                    min_sim: 0.5,
                    nodes: vec![(7, g.clone(), path.clone())],
                })),
                BatchResponseItem::Diff(Box::new(diff_env)),
            ],
        };
        let j = json_batch_envelope(&env);

        // Envelope scaffold + each kind tag.
        assert!(j.starts_with("{\"items\":["), "{j}");
        assert!(j.contains("{\"kind\":\"inclusion\",\"entry\":{\"kind\":\"account\""), "{j}");
        assert!(j.contains("{\"kind\":\"inclusion\",\"entry\":null}"), "{j}");
        assert!(j.contains("{\"kind\":\"knn\",\"claim\":{\"query\":["), "{j}");
        assert!(j.contains("\"k\":\"3\""), "{j}");
        assert!(j.contains("{\"kind\":\"range\",\"claim\":{\"query\":["), "{j}");
        assert!(j.contains("\"min_sim\":0.5"), "{j}");
        assert!(j.contains("{\"kind\":\"diff\",\"envelope\":{\"header_prev\":{\"height\":\"1\""), "{j}");

        // Graph leaf: nested graph_node + merkle path.
        assert!(
            j.contains("\"graph_node\":{\"node_id\":\"7\",\"domain\":\"42\",\"dim\":\"8\",\"embedding\":["),
            "{j}"
        );
        assert!(j.contains("\"proof\":{\"steps\":[{\"side\":\"left\",\"hash\":\""), "{j}");

        // Diff body + tracked validator sets.
        assert!(j.contains("\"diff\":{\"added\":[{\"node_id\":\"7\""), "{j}");
        assert!(j.contains("\"tracked_set_h1\":[{\"id\":\"21\",\"pubkey\":"), "{j}");
        assert!(j.contains("\"tracked_set_h2\":[]"), "{j}");

        // Braces balance.
        let opens = j.chars().filter(|&c| c == '{').count();
        let closes = j.chars().filter(|&c| c == '}').count();
        assert_eq!(opens, closes, "unbalanced braces: {j}");
    }

    #[test]
    fn json_lock_envelope_structured() {
        // M77: `json_lock_envelope` renders the self-contained bridge-lock proof as a
        // structured object — certified source header + cert + tracked validator set +
        // the lock (all six fields incl. signature) + its merkle inclusion path. Reuses
        // json_block_header/json_commit/json_validator_set/json_merkle_proof; only the
        // `lock` leaf needs the new `json_bridge_lock`. `json_lock` wraps it in one field.
        let hdr = |height: u64| crate::codec::BlockHeader {
            height,
            prev_hash: [0u8; 32],
            timestamp_days: 0.0,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            validator_updates: vec![],
            txs_commitment: [0u8; 32],
            stake_ops_commitment: [0u8; 32],
            evidence_commitment: [0u8; 32],
            bridge_locks_commitment: [0u8; 32],
            bridge_headers_commitment: [0u8; 32],
            bridge_redeems_commitment: [0u8; 32],
        };
        let cert = |height: u64| Commit { height, round: 0, block_hash: [0u8; 32], precommits: vec![] };
        let env = crate::bridge::LockEnvelope {
            source_header: hdr(5),
            source_cert: cert(5),
            source_tracked_set: crate::validator::ValidatorSet::new(vec![crate::validator::Validator {
                id: 21,
                pubkey: kp(21).public(),
                power: 2,
            }]),
            lock_id: 3,
            lock: crate::BridgeLock {
                account: 1,
                amount: 4_000_000,
                dest_chain: [0xAB; 32],
                dest_account: 7,
                nonce: 2,
                signature: [9u8; 64],
            },
            proof: crate::merkle::Proof {
                steps: vec![crate::merkle::Step::Left([1u8; 32]), crate::merkle::Step::Right([2u8; 32])],
            },
        };
        let j = json_lock(&env);

        // One-field wrap + structured sub-envelopes.
        assert!(j.starts_with("{\"lock_envelope\":{"), "{j}");
        assert!(j.contains("\"source_header\":{\"height\":\"5\""), "{j}");
        assert!(j.contains("\"source_cert\":{"), "{j}");
        assert!(j.contains("\"source_tracked_set\":[{\"id\":\"21\",\"pubkey\":"), "{j}");
        assert!(j.contains("\"lock_id\":\"3\""), "{j}");

        // The lock leaf: all six fields incl. signature.
        assert!(j.contains("\"lock\":{\"account\":\"1\",\"amount\":\"4000000\",\"dest_chain\":\""), "{j}");
        assert!(j.contains("\"dest_account\":\"7\",\"nonce\":\"2\",\"signature\":\""), "{j}");

        // Merkle inclusion path.
        assert!(j.contains("\"proof\":{\"steps\":[{\"side\":\"left\",\"hash\":\""), "{j}");
        assert!(j.contains("{\"side\":\"right\",\"hash\":\""), "{j}");

        // Braces balance.
        let opens = j.chars().filter(|&c| c == '{').count();
        let closes = j.chars().filter(|&c| c == '}').count();
        assert_eq!(opens, closes, "unbalanced braces: {j}");
    }

    #[test]
    fn json_range_blocks_structured() {
        // M78: `json_range_blocks` decodes the batch read's `[(Block, Commit)]` range —
        // the last opaque-hex read field — as `[{"block":{…},"commit":{…}}, …]`. Build a
        // Block with every body vector non-empty (mirrors codec.rs sample_block plus the
        // bridge-headers/redeems round-trip) so each leaf renderer is exercised: txs with
        // nested reviews + embedding, validator_updates, stake_ops (both bond kinds),
        // slashing_evidence (via json_vote), bridge_locks, and bridge_headers/redeems that
        // reuse json_block_header/json_commit/json_validator_set/json_bridge_lock/
        // json_merkle_proof. After M78 no read-surface JSON field carries opaque hex.
        let hdr = |height: u64| crate::codec::BlockHeader {
            height,
            prev_hash: [0u8; 32],
            timestamp_days: 0.0,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            validator_updates: vec![],
            txs_commitment: [0u8; 32],
            stake_ops_commitment: [0u8; 32],
            evidence_commitment: [0u8; 32],
            bridge_locks_commitment: [0u8; 32],
            bridge_headers_commitment: [0u8; 32],
            bridge_redeems_commitment: [0u8; 32],
        };
        let cert = |height: u64| Commit { height, round: 0, block_hash: [0u8; 32], precommits: vec![] };
        let vote = |block_hash: [u8; 32]| crate::consensus::Vote {
            validator: 22,
            height: 9,
            round: 1,
            block_hash,
            vote_type: crate::consensus::VoteType::Precommit,
            signature: [3u8; 64],
        };
        let lock = crate::BridgeLock {
            account: 1,
            amount: 3_000_000,
            dest_chain: [77u8; 32],
            dest_account: 9,
            nonce: 1,
            signature: [6u8; 64],
        };
        let mut emb = [0.0f32; crate::DIM];
        emb[3] = 1.0;
        let block = crate::Block {
            height: 7,
            prev_hash: [42u8; 32],
            timestamp_days: 3.5,
            next_validators_root: [17u8; 32],
            state_root: [11u8; 32],
            accounts_root: [12u8; 32],
            graph_root: [13u8; 32],
            bridge_root: [14u8; 32],
            txs: vec![crate::SubmissionTx {
                author: 1,
                embedding: emb,
                domain: 2,
                stake: 2_000_000,
                reviews: vec![
                    crate::Review { reviewer: 10, score: 0.9 },
                    crate::Review { reviewer: 11, score: 0.75 },
                ],
                repl_success: 2,
                repl_total: 3,
                timestamp_days: 3.0,
                signature: [9u8; 64],
            }],
            validator_updates: vec![crate::validator::ValidatorUpdate { id: 25, pubkey: [5u8; 32], power: 3 }],
            stake_ops: vec![
                crate::StakeOp { account: 1, kind: crate::BondKind::Bond, amount: 5_000_000, signature: [7u8; 64] },
                crate::StakeOp { account: 2, kind: crate::BondKind::Unbond, amount: 2_000_000, signature: [8u8; 64] },
            ],
            slashing_evidence: vec![crate::SlashEvidence { vote_a: vote([1u8; 32]), vote_b: vote([2u8; 32]) }],
            bridge_locks: vec![lock.clone()],
            bridge_headers: vec![crate::BridgeHeader {
                source_chain: [55u8; 32],
                header: hdr(7),
                cert: cert(7),
                next_set: crate::validator::ValidatorSet::new(vec![crate::validator::Validator {
                    id: 21,
                    pubkey: kp(21).public(),
                    power: 2,
                }]),
            }],
            bridge_redeems: vec![crate::BridgeRedeem {
                source_chain: [55u8; 32],
                source_header: hdr(7),
                source_cert: cert(7),
                lock_id: 3,
                lock,
                proof: crate::merkle::Proof { steps: vec![crate::merkle::Step::Right([4u8; 32])] },
            }],
        };
        let j = json_range_blocks(&[(block, cert(7))]);

        // Range scaffold + block header scalars.
        assert!(j.starts_with("[{\"block\":{\"height\":\"7\""), "{j}");
        assert!(j.contains("\"state_root\":\""), "{j}");

        // Txs: nested embedding + reviews.
        assert!(j.contains("\"txs\":[{\"author\":\"1\",\"embedding\":["), "{j}");
        assert!(j.contains("\"reviews\":[{\"reviewer\":\"10\",\"score\":0.9"), "{j}");

        // Validator updates / stake ops (both bond kinds) / evidence.
        assert!(j.contains("\"validator_updates\":[{\"id\":\"25\""), "{j}");
        assert!(j.contains("\"stake_ops\":[{\"account\":\"1\",\"kind\":\"bond\""), "{j}");
        assert!(j.contains("\"kind\":\"unbond\""), "{j}");
        assert!(j.contains("\"slashing_evidence\":[{\"vote_a\":{\"validator\":\"22\""), "{j}");

        // Bridge ops: locks + headers + redeems (nested lock leaf).
        assert!(j.contains("\"bridge_locks\":[{\"account\":\"1\""), "{j}");
        assert!(j.contains("\"bridge_headers\":[{\"source_chain\":\""), "{j}");
        assert!(j.contains("\"bridge_redeems\":[{\"source_chain\":\""), "{j}");
        assert!(j.contains("\"lock\":{\"account\":\"1\""), "{j}");

        // Paired commit + range close.
        assert!(j.contains("},\"commit\":{\"height\":\"7\""), "{j}");
        assert!(j.ends_with("]"), "{j}");

        // Braces/brackets balance.
        let ob = j.chars().filter(|&c| c == '{').count();
        let cb = j.chars().filter(|&c| c == '}').count();
        assert_eq!(ob, cb, "unbalanced braces: {j}");
        let os = j.chars().filter(|&c| c == '[').count();
        let cs = j.chars().filter(|&c| c == ']').count();
        assert_eq!(os, cs, "unbalanced brackets: {j}");
    }
}
