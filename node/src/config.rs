//! M32: file-based node configuration (serde + toml).
//!
//! The consensus core (`lib.rs`, `Genesis`, `Block`, …) deliberately stays
//! serde-free. This module holds serde-derive **mirror** structs that a node
//! operator writes as TOML and that convert into the plain engine types:
//!
//!   * [`NodeConfig`] — this process's identity, listen address, data dir, the
//!     static peer table, and (for a validator) the single signing key this
//!     process votes with (M33: one key per process, no sequencer).
//!   * [`GenesisConfig`] — the network's genesis, mirroring [`Genesis`]; the same
//!     file is shipped to every node.
//!   * [`KeystoreConfig`] — validator signing-key seeds; used only by the offline
//!     `ChainDriver` demos (`cmd_bft`/`cmd_live`/`cmd_chain`), not the daemon.
//!
//! Pubkeys and seeds are lowercase hex (encoded with [`crate::hex`]); this
//! module carries its own strict hex decoder since `hash.rs` only encodes.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use zhixing_engine::{DeltaKParams, Embedding, DIM};

use crate::{Genesis, PubKey};

/// Errors from loading or converting a config file.
#[derive(Debug)]
pub enum ConfigError {
    /// Failed to read the file from disk.
    Io(std::io::Error),
    /// The file was not valid TOML / did not match the schema.
    Toml(String),
    /// A hex field (`pubkey_hex` / `seed_hex`) was malformed or the wrong length.
    BadHex { field: String, detail: String },
    /// A `listen` / peer `addr` string did not parse as a socket address.
    BadAddr { value: String },
    /// A seed-node embedding did not have exactly `DIM` components.
    BadEmbedding { got: usize },
    /// M44: `[logging] format` was neither `"text"` nor `"json"`.
    BadLogFormat { value: String },
    /// M45: `[logging] rotation` was not one of daily/hourly/minutely/never.
    BadLogRotation { value: String },
    /// M54: a `[mempool]` tuning value was out of range (e.g. zero capacity).
    BadMempool { detail: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config i/o: {e}"),
            ConfigError::Toml(e) => write!(f, "config parse: {e}"),
            ConfigError::BadHex { field, detail } => {
                write!(f, "config bad hex in `{field}`: {detail}")
            }
            ConfigError::BadAddr { value } => write!(f, "config bad address: `{value}`"),
            ConfigError::BadEmbedding { got } => {
                write!(f, "config seed embedding has {got} dims, expected {DIM}")
            }
            ConfigError::BadLogFormat { value } => {
                write!(f, "config bad log format: `{value}` (expected \"text\" or \"json\")")
            }
            ConfigError::BadLogRotation { value } => {
                write!(
                    f,
                    "config bad log rotation: `{value}` (expected \"daily\", \"hourly\", \"minutely\", or \"never\")"
                )
            }
            ConfigError::BadMempool { detail } => write!(f, "config bad mempool: {detail}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

// ----------------------------------------------------------------------------
// node.toml
// ----------------------------------------------------------------------------

/// A node's operational config (one TOML file per process).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub node: NodeSection,
    /// Static peer table (M32 has no discovery). Each entry maps a validator/
    /// full-node id to its dialable address.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// Path to the shared genesis TOML (resolved relative to CWD).
    pub genesis: String,
    /// Present on validator nodes (M33): the single ed25519 key this process
    /// signs proposals/prevotes/precommits with. Absent ⇒ this node is a pure
    /// follower that syncs and verifies certs but never votes.
    #[serde(default)]
    pub validator: Option<ValidatorKeyConfig>,
    /// M35: operator-tunable consensus timing + empty-block policy. Absent (or a
    /// partial `[consensus]` table) falls back field-by-field to defaults that
    /// equal the pre-M35 hard-coded constants, so old configs behave identically.
    #[serde(default)]
    pub consensus: ConsensusConfig,
    /// M36: operator-tunable daemon-lifecycle/network timing (anti-entropy
    /// heartbeat + validator startup grace). Absent (or a partial `[network]`
    /// table) falls back field-by-field to defaults that equal the pre-M36
    /// hard-coded constants, so old configs behave identically.
    #[serde(default)]
    pub network: NetworkConfig,
    /// M38: opt-in read-only metrics/health endpoint. Absent ⇒ `None` ⇒ no
    /// endpoint is bound (behavior-preserving default), so old configs behave
    /// identically. A bare `[metrics]` table is inert (`enabled` defaults false),
    /// mirroring `[validator]`.
    #[serde(default)]
    pub metrics: Option<MetricsConfig>,
    /// M53: opt-in external transaction-ingress RPC. Absent ⇒ `None` ⇒ no
    /// endpoint is bound (behavior-preserving default), so old configs behave
    /// identically. A bare `[rpc]` table is inert (`enabled` defaults false),
    /// mirroring `[metrics]`.
    #[serde(default)]
    pub rpc: Option<RpcConfig>,
    /// M44: opt-in logging config. Absent ⇒ `None` ⇒ the M37 default
    /// (RUST_LOG-filtered, `info` fallback, text, stderr) — byte-identical to
    /// pre-M44. A bare `[logging]` table falls back field-by-field to those
    /// defaults, mirroring `[consensus]`/`[network]`.
    #[serde(default)]
    pub logging: Option<LoggingConfig>,
    /// M54: mempool DoS-hardening knobs — pending-pool capacity bound, per-block
    /// build cap, and per-peer gossip rate limiting. Every field is optional in
    /// TOML; the struct-level `#[serde(default)]` fills missing keys from
    /// [`MempoolConfig::default`], whose values preserve pre-M54 behavior (ample
    /// capacity, `max_block_txs = 64`, rate limiting disabled).
    #[serde(default)]
    pub mempool: MempoolConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSection {
    pub id: u64,
    /// e.g. `"0.0.0.0:9021"`.
    pub listen: String,
    /// Directory holding `blocks.log` + `certs.log`.
    pub data_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerConfig {
    pub id: u64,
    /// e.g. `"10.0.0.2:9022"`.
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorKeyConfig {
    /// When false the process still holds a key but does not participate in
    /// consensus (useful to stand a node up as a follower without editing the
    /// key out). Defaults to false so a bare `[validator]` table is inert.
    #[serde(default)]
    pub enabled: bool,
    /// 32-byte ed25519 seed, lowercase hex (64 chars). The derived public key
    /// must match this node's entry in `genesis.validators` (checked at startup).
    pub seed_hex: String,
}

impl ValidatorKeyConfig {
    /// Decode the seed and build the signing [`Keypair`].
    pub fn keypair(&self) -> Result<crate::Keypair, ConfigError> {
        let seed = decode_seed(&self.seed_hex, "validator.seed_hex")?;
        Ok(crate::Keypair::from_seed(seed))
    }
}

/// M35: consensus timing (milliseconds) + empty-block policy. Every field is
/// optional in TOML — the struct-level `#[serde(default)]` fills any missing key
/// from [`ConsensusConfig::default`], whose values are exactly the constants the
/// daemon hard-coded before M35. So a config with no `[consensus]` section, or a
/// partial one, reproduces the pre-M35 behavior verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsensusConfig {
    /// Round-0 propose-step timeout base.
    pub propose_timeout_ms: u64,
    /// Round-0 prevote-step timeout base.
    pub prevote_timeout_ms: u64,
    /// Round-0 precommit-step timeout base.
    pub precommit_timeout_ms: u64,
    /// Per-round linear back-off added to each step base (`base + round*delta`).
    pub timeout_delta_ms: u64,
    /// Pacing between committing one height and starting the next.
    pub block_interval_ms: u64,
    /// When false, a validator only starts a height (proposes) when there is
    /// pending work (txs / staged stake ops / slashing evidence) — an idle chain
    /// stops growing instead of sealing empty heartbeat blocks. Default true
    /// keeps the M33 heartbeat.
    pub create_empty_blocks: bool,
}

impl Default for ConsensusConfig {
    fn default() -> Self {
        // These values MUST equal the pre-M35 daemon constants (single source of
        // truth now lives here): PROPOSE/PREVOTE/PRECOMMIT_TIMEOUT_BASE=1000,
        // TIMEOUT_DELTA=500, BLOCK_INTERVAL=1000.
        Self {
            propose_timeout_ms: 1000,
            prevote_timeout_ms: 1000,
            precommit_timeout_ms: 1000,
            timeout_delta_ms: 500,
            block_interval_ms: 1000,
            create_empty_blocks: true,
        }
    }
}

/// M36 network/daemon-lifecycle timing. Every field is optional in TOML — the
/// struct-level `#[serde(default)]` fills any missing key from
/// [`NetworkConfig::default`], whose values equal the constants the daemon
/// hard-coded before M36. So a config with no `[network]` section, or a partial
/// one, reproduces the pre-M36 behavior verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Anti-entropy heartbeat interval — how often the node broadcasts a
    /// `Status` announce to pull missing certified blocks (was const
    /// `ANNOUNCE_SECS = 2`, i.e. 2000 ms).
    pub announce_interval_ms: u64,
    /// Boot grace before a validator kicks off its first height, letting the
    /// mesh dial + handshake first (was const `STARTUP_DELAY = 1000`).
    pub startup_delay_ms: u64,
    /// M39: gossip a peer address book and auto-dial discovered higher-id
    /// peers, so a connected-but-incomplete `[[peers]]` seed set self-completes
    /// into a full mesh. `false` pins the node to its static seed set (no
    /// discovery). Default `true`.
    pub enable_peer_exchange: bool,
    /// M40: require a mutually-authenticated ed25519 handshake before a peer is
    /// admitted — the peer must prove possession of the genesis signing key for
    /// the validator id it claims (replay-proof via per-session nonces). This is
    /// a network-wide policy: a node with this on won't complete a handshake with
    /// one that has it off. Default `false` keeps the pre-M40 cleartext 8-byte-id
    /// hello (byte-identical back-compat). A node with no validator signing key
    /// cannot run with this on (it could never prove its own identity).
    pub require_peer_auth: bool,
    /// M41: wrap every P2P connection in TLS 1.3 (encryption-only: an ephemeral
    /// self-signed cert, accept-any peer cert). This gives confidentiality +
    /// integrity on the wire; peer *authentication* is still `require_peer_auth`'s
    /// job (the M40 handshake runs inside the TLS tunnel). Network-wide policy: a
    /// TLS node and a plaintext node fail to handshake. Default `false` keeps the
    /// pre-M41 raw-TCP path (byte-identical back-compat).
    pub enable_tls: bool,
    /// M42: fold the TLS keying-material exporter (RFC 5705/8446) into the M40 auth
    /// transcript, binding the authenticated identity to *this* TLS channel. This
    /// defeats an active MITM that terminates TLS on both sides and relays the inner
    /// handshake (its two TLS legs derive different exporters, so a relayed signature
    /// no longer verifies). Requires `enable_tls` + `require_peer_auth` (the daemon
    /// fails fast otherwise). Network-wide policy: a bound node and an unbound node
    /// produce different transcripts and fail to authenticate. Default `false` keeps
    /// the pre-M42 transcript (byte-identical back-compat).
    pub bind_channel: bool,
    /// M43: genesis-pinned mutual TLS. Each node presents its genesis ed25519 key
    /// as its TLS credential (RFC 7250 raw public key), and accepts a connection
    /// only if the peer's presented key is a genesis validator — both directions.
    /// This gives real TLS-layer authentication (M41 alone is encryption-only with
    /// accept-any certs), so a non-validator can no longer even establish the
    /// tunnel. Requires `enable_tls` and a validator signing key (a keyless
    /// follower cannot present a genesis credential, so mTLS restricts the network
    /// to genesis validators); the daemon fails fast otherwise. Network-wide
    /// policy: an mTLS node and a non-mTLS node fail to handshake. Default `false`
    /// keeps the pre-M43 accept-any TLS path (byte-identical back-compat).
    pub require_peer_certs: bool,
    /// M51: the address this node advertises for itself in M39 peer exchange —
    /// the dialable `IP:port` other nodes should reach it on. Set this when the
    /// bind `listen` isn't reachable as-is (NAT, a port map, or a `0.0.0.0`
    /// wildcard bind): the node keeps *binding* `listen` but *gossips* this
    /// address so discovered peers dial a working target. Must parse as a
    /// `SocketAddr` (a DNS hostname is rejected — the dial path needs a literal
    /// address). Empty (the default) ⇒ advertise the bind `listen`, exactly the
    /// M39 behavior (byte-identical back-compat).
    pub advertise_addr: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        // These values MUST equal the pre-M36 daemon constants (single source of
        // truth now lives here): ANNOUNCE_SECS=2 (→ 2000 ms), STARTUP_DELAY=1000.
        Self {
            announce_interval_ms: 2000,
            startup_delay_ms: 1000,
            enable_peer_exchange: true,
            require_peer_auth: false,
            enable_tls: false,
            bind_channel: false,
            require_peer_certs: false,
            advertise_addr: String::new(),
        }
    }
}

/// M38: opt-in read-only metrics/health endpoint config. When present and
/// `enabled`, the daemon binds a second TCP listener that answers a minimal HTTP
/// `GET` with a Prometheus text-exposition body (a `200` also serving as a
/// health check). Defaults are inert: a bare `[metrics]` table (or the whole
/// `NodeConfig.metrics` being absent) leaves the endpoint off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Off by default — the endpoint is opt-in and behavior-preserving.
    pub enabled: bool,
    /// Address the metrics HTTP listener binds, e.g. `"127.0.0.1:9600"`.
    pub listen: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9600".into(),
        }
    }
}

impl MetricsConfig {
    /// Parse `listen` into a `SocketAddr`.
    pub fn listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_addr(&self.listen)
    }
}

/// M53: opt-in external transaction-ingress RPC config. When present and
/// `enabled`, the daemon binds a third TCP listener that answers a minimal HTTP
/// `POST /submit_tx` whose body is raw `codec::encode_tx` bytes: the tx is decoded,
/// run through the normal mempool-admission path, and the response reports accept
/// (with the tx hash) or reject (with a reason). A `GET`/`HEAD` doubles as a
/// health probe. Defaults are inert: a bare `[rpc]` table (or the whole
/// `NodeConfig.rpc` being absent) leaves the endpoint off. Bind default is
/// loopback — do not expose publicly without a front proxy (no auth / rate
/// limiting yet; see M54 anti-spam slice).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcConfig {
    /// Off by default — the endpoint is opt-in and behavior-preserving.
    pub enabled: bool,
    /// Address the RPC HTTP listener binds, e.g. `"127.0.0.1:9700"`.
    pub listen: String,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9700".into(),
        }
    }
}

impl RpcConfig {
    /// Parse `listen` into a `SocketAddr`.
    pub fn listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_addr(&self.listen)
    }
}

/// M54: mempool DoS-hardening tuning. Every field is optional in TOML — the
/// struct-level `#[serde(default)]` fills any missing key from
/// [`MempoolConfig::default`], whose values preserve pre-M54 behavior: an ample
/// pending-pool `capacity` that localnet never hits, the historical per-block
/// build cap `max_block_txs = 64`, and per-peer gossip rate limiting **disabled**
/// (`per_peer_tx_per_sec = 0.0`). So an absent `[mempool]` section (or a partial
/// one) behaves exactly as pre-M54. Derives `PartialEq` but not `Eq`/`Hash` — the
/// rate/burst fields are `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MempoolConfig {
    /// Maximum transactions held in the pending pool. Admission past this bound
    /// is rejected with [`crate::ChainError::MempoolFull`] (distinct from
    /// `max_block_txs`, which only caps how many land in a single block).
    pub capacity: usize,
    /// Per-block build cap — the most txs the builder places in one block (was the
    /// daemon's hard-coded `64`).
    pub max_block_txs: usize,
    /// Per-peer gossip token-bucket refill rate (tx/sec). `0.0` disables rate
    /// limiting entirely (the behavior-preserving default).
    pub per_peer_tx_per_sec: f64,
    /// Per-peer gossip token-bucket capacity (max burst). Ignored when
    /// `per_peer_tx_per_sec == 0.0`.
    pub per_peer_tx_burst: f64,
    /// M55: per-set capacity for the gossip flood-dedup sets (`seen_tx` /
    /// `seen_evidence` / `seen_stake_op`), FIFO-evicting once full. `0` ⇒
    /// **unbounded** (the behavior-preserving default) — note the deliberate
    /// sentinel asymmetry vs `capacity` above, where `0` is *rejected*: a
    /// zero-size dedup cache would defeat flood suppression, so `0` is reserved
    /// as the off switch (mirroring `per_peer_tx_per_sec = 0.0`).
    pub seen_cache: usize,
    /// M57: max pending txs a single account (`author`) may hold at once; admission
    /// past it is rejected with [`crate::ChainError::AccountQuotaFull`], so one
    /// account cannot monopolize the pool. `0` ⇒ **unbounded** (the
    /// behavior-preserving default — no per-account cap), the same off-switch
    /// sentinel as `seen_cache`. A value above `capacity` is legal but never binds
    /// (the global cap trips first).
    pub per_account_limit: usize,
}

impl Default for MempoolConfig {
    fn default() -> Self {
        // capacity: ample headroom localnet never reaches (head invariant intact);
        // max_block_txs: the pre-M54 daemon literal; rate limiting: off.
        Self {
            capacity: 4096,
            max_block_txs: 64,
            per_peer_tx_per_sec: 0.0,
            per_peer_tx_burst: 256.0,
            seen_cache: 0,
            per_account_limit: 0,
        }
    }
}

impl MempoolConfig {
    /// Reject nonsensical tuning at load time: a zero pool capacity or zero
    /// per-block cap would stall all admission/production, and negative
    /// rate/burst values are meaningless.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.capacity == 0 {
            return Err(ConfigError::BadMempool {
                detail: "capacity must be > 0".into(),
            });
        }
        if self.max_block_txs == 0 {
            return Err(ConfigError::BadMempool {
                detail: "max_block_txs must be > 0".into(),
            });
        }
        if self.per_peer_tx_per_sec < 0.0 || self.per_peer_tx_burst < 0.0 {
            return Err(ConfigError::BadMempool {
                detail: "per_peer_tx_per_sec / per_peer_tx_burst must be >= 0".into(),
            });
        }
        Ok(())
    }
}

/// M44: operator-tunable daemon logging. Every field is optional in TOML — the
/// struct-level `#[serde(default)]` fills any missing key from
/// [`LoggingConfig::default`], whose values reproduce the M37 subscriber
/// (RUST_LOG-filtered, `info` fallback, text, stderr). So an absent `[logging]`
/// section (or a partial one) behaves exactly as pre-M44.
///
/// M45: adds a rolling-file target. `file` empty (the default) keeps the M37/M44
/// stderr writer verbatim; a non-empty path switches the writer to a rolling log
/// file at the `rotation` schedule.
///
/// M46: adds `stderr` — when a `file` is configured, also mirror lines to stderr
/// (a tee). Default `false` keeps the M45 single-sink behavior byte-identical.
///
/// M47: adds `stderr_level`/`file_level` — per-sink filter overrides for the tee.
/// Both empty (the default) ⇒ both sinks share `level` (the M46 tee, byte-identical).
///
/// M48: adds `levels`/`stderr_levels`/`file_levels` — array forms of the three
/// scalar filter knobs; a non-empty array (joined by `,`) overrides its scalar. All
/// empty (the default) ⇒ the scalars are used verbatim (byte-identical to M47).
///
/// M49: adds `stderr_format`/`file_format` — per-sink formatter overrides for the
/// tee. Both empty (the default) ⇒ both sinks share `format` (the M48/M46 tee,
/// byte-identical). Validated like `format` (`text`/`json`; empty ⇒ inherit).
///
/// M50: adds `max_files` — a cap on retained rotated log files (oldest pruned).
/// `0` (the default) ⇒ unbounded, exactly the M49/M45 `RollingFileAppender::new`
/// path (byte-identical). Applies only to a `file` target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Default `EnvFilter` directive used only when `RUST_LOG` is unset — a plain
    /// level (`"info"`, `"debug"`) or a full directive string. `RUST_LOG` still
    /// wins when set, matching M37.
    pub level: String,
    /// Output formatter: `"text"` (the M37 default human-readable `fmt`) or
    /// `"json"` (machine-parseable one-object-per-line). Validated at load time.
    pub format: String,
    /// M45: destination. Empty (default) ⇒ stderr, exactly the M37/M44 behavior.
    /// A non-empty path (e.g. `"data/logs/node.log"`) ⇒ a rolling log file; the
    /// parent directory is created at init and the file name is used as the
    /// rotation prefix.
    pub file: String,
    /// M45: rotation schedule for a file target: `"daily"` (default), `"hourly"`,
    /// `"minutely"`, or `"never"`. Ignored when `file` is empty. Validated at load.
    pub rotation: String,
    /// M46: when a `file` target is set, also mirror log lines to stderr (a tee).
    /// Default `false` ⇒ file only (M45 behavior). Takes effect **only** when
    /// `file` is non-empty — with no file, output always goes to stderr regardless.
    pub stderr: bool,
    /// M47: per-sink filter override for the stderr side of the tee. A free-form
    /// `EnvFilter` directive (like `level`); empty (default) ⇒ inherit `level`.
    /// Applies **only** in the tee (`file` set + `stderr = true`) and only when
    /// `RUST_LOG` is unset — `RUST_LOG` still wins globally.
    pub stderr_level: String,
    /// M47: per-sink filter override for the file side of the tee. Same rules as
    /// [`stderr_level`](Self::stderr_level): free-form directive, empty ⇒ inherit
    /// `level`, tee-only, `RUST_LOG` overrides.
    pub file_level: String,
    /// M48: array form of [`level`](Self::level). A non-empty array is joined by
    /// `,` into one multi-directive filter string (e.g.
    /// `["info", "tokio=warn", "zhixing_node::daemon=debug"]`) and overrides the
    /// scalar `level`; empty (default) ⇒ the scalar is used verbatim. Empty/whitespace
    /// entries are dropped. `RUST_LOG` still wins globally.
    pub levels: Vec<String>,
    /// M48: array form of [`stderr_level`](Self::stderr_level) — joined by `,`,
    /// overrides the scalar when non-empty. Empty (default) ⇒ inherit the resolved
    /// base `level`/`levels`. Tee-only, `RUST_LOG` overrides.
    pub stderr_levels: Vec<String>,
    /// M48: array form of [`file_level`](Self::file_level) — same rules as
    /// [`stderr_levels`](Self::stderr_levels).
    pub file_levels: Vec<String>,
    /// M49: per-sink formatter override for the stderr side of the tee: `"text"`
    /// or `"json"`; empty (default) ⇒ inherit `format`. Applies **only** in the tee
    /// (`file` set + `stderr = true`); independent of `RUST_LOG` (which governs
    /// filtering, not formatting). Validated at load like `format`.
    pub stderr_format: String,
    /// M49: per-sink formatter override for the file side of the tee. Same rules as
    /// [`stderr_format`](Self::stderr_format): `"text"`/`"json"`, empty ⇒ inherit
    /// `format`, tee-only.
    pub file_format: String,
    /// M50: cap on retained rotated log files — the appender keeps the `max_files`
    /// most recent and deletes the oldest. `0` (default) ⇒ unbounded (keep every
    /// rotated file), the M49/M45 behavior byte-for-byte. Applies **only** to a
    /// `file` target (ignored when `file` is empty); harmless with `rotation =
    /// "never"` (a single file, nothing to prune).
    pub max_files: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        // These MUST reproduce the M37 hard-coded subscriber: an `info` default
        // filter and the text formatter. `file` empty ⇒ stderr (M45), `stderr`
        // false ⇒ no tee (M46), the per-sink levels empty ⇒ inherit `level` (M47),
        // the directive arrays empty ⇒ scalars used verbatim (M48), and the per-sink
        // formats empty ⇒ inherit `format` (M49), so the default LoggingConfig is
        // byte-identical to pre-M44 behavior.
        // byte-identical to pre-M44 behavior. M50: `max_files: 0` ⇒ unbounded, the
        // M45 `RollingFileAppender::new` path unchanged.
        Self {
            level: "info".into(),
            format: "text".into(),
            file: String::new(),
            rotation: "daily".into(),
            stderr: false,
            stderr_level: String::new(),
            file_level: String::new(),
            levels: Vec::new(),
            stderr_levels: Vec::new(),
            file_levels: Vec::new(),
            stderr_format: String::new(),
            file_format: String::new(),
            max_files: 0,
        }
    }
}

impl LoggingConfig {
    /// Reject an unknown `format`/`rotation` up front (typed errors at config-load
    /// time) rather than silently falling back at subscriber-init.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.format.as_str() {
            "text" | "json" => {}
            _ => return Err(ConfigError::BadLogFormat { value: self.format.clone() }),
        }
        match self.rotation.as_str() {
            "daily" | "hourly" | "minutely" | "never" => {}
            _ => return Err(ConfigError::BadLogRotation { value: self.rotation.clone() }),
        }
        // M49: per-sink formats are validated like `format`; empty ⇒ inherit `format`.
        for fmt in [&self.stderr_format, &self.file_format] {
            match fmt.as_str() {
                "" | "text" | "json" => {}
                _ => return Err(ConfigError::BadLogFormat { value: fmt.clone() }),
            }
        }
        Ok(())
    }
}

impl NodeConfig {
    /// Parse `listen` into a `SocketAddr`.
    pub fn listen_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_addr(&self.node.listen)
    }
}

impl PeerConfig {
    pub fn socket_addr(&self) -> Result<SocketAddr, ConfigError> {
        parse_addr(&self.addr)
    }
}

// ----------------------------------------------------------------------------
// genesis.toml
// ----------------------------------------------------------------------------

/// Serde mirror of [`Genesis`]. `params` and `seed_nodes` default so a minimal
/// testnet genesis need only list accounts, reviewers, and validators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisConfig {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub reviewers: Vec<ReviewerConfig>,
    #[serde(default)]
    pub seed_nodes: Vec<SeedNodeConfig>,
    pub base_emission_micro: u64,
    pub slash_bps: u32,
    #[serde(default)]
    pub timestamp_days: f32,
    #[serde(default)]
    pub validators: Vec<ValidatorConfig>,
    /// Optional ΔK params; defaults to `DeltaKParams::default()`.
    #[serde(default)]
    pub params: Option<DeltaKParamsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: u64,
    pub balance_micro: u64,
    pub pubkey_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerConfig {
    pub id: u64,
    pub weight: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedNodeConfig {
    /// Exactly `DIM` components.
    pub embedding: Vec<f32>,
    pub domain: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorConfig {
    pub id: u64,
    pub pubkey_hex: String,
    pub power: u64,
}

/// Mirror of the subset of `DeltaKParams` a genesis might override. Any omitted
/// field falls back to `DeltaKParams::default()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaKParamsConfig {
    // Kept opaque on purpose: the engine owns the canonical param set and its
    // default. A future milestone can expand this to individual knobs; for now
    // a genesis either uses the default (omit `params`) or we treat any present
    // table as "use default" to avoid drift with the engine's field list.
    #[serde(default)]
    pub _reserved: Option<()>,
}

impl GenesisConfig {
    /// Convert into the consensus [`Genesis`], hex-decoding pubkeys.
    pub fn to_genesis(&self) -> Result<Genesis, ConfigError> {
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for a in &self.accounts {
            accounts.push((a.id, a.balance_micro, decode_pubkey(&a.pubkey_hex, "accounts.pubkey_hex")?));
        }
        let reviewers = self.reviewers.iter().map(|r| (r.id, r.weight)).collect();

        let mut seed_nodes = Vec::with_capacity(self.seed_nodes.len());
        for s in &self.seed_nodes {
            if s.embedding.len() != DIM {
                return Err(ConfigError::BadEmbedding { got: s.embedding.len() });
            }
            let mut emb: Embedding = [0.0f32; DIM];
            emb.copy_from_slice(&s.embedding);
            seed_nodes.push((emb, s.domain));
        }

        let mut validators = Vec::with_capacity(self.validators.len());
        for v in &self.validators {
            validators.push((v.id, decode_pubkey(&v.pubkey_hex, "validators.pubkey_hex")?, v.power));
        }

        Ok(Genesis {
            accounts,
            reviewers,
            seed_nodes,
            params: DeltaKParams::default(),
            base_emission_micro: self.base_emission_micro,
            slash_bps: self.slash_bps,
            timestamp_days: self.timestamp_days,
            validators,
            bridge_sources: Vec::new(),
        })
    }
}

// ----------------------------------------------------------------------------
// keystore.toml
// ----------------------------------------------------------------------------

/// Validator signing-key seeds (private to a producer node).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeystoreConfig {
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub id: u64,
    /// 32-byte ed25519 seed, lowercase hex (64 chars).
    pub seed_hex: String,
}

impl KeystoreConfig {
    /// Convert into the `id -> 32-byte seed` map `ChainDriver::new` expects.
    pub fn to_seeds(&self) -> Result<BTreeMap<u64, [u8; 32]>, ConfigError> {
        let mut out = BTreeMap::new();
        for k in &self.keys {
            out.insert(k.id, decode_seed(&k.seed_hex, "keys.seed_hex")?);
        }
        Ok(out)
    }
}

// ----------------------------------------------------------------------------
// loaders
// ----------------------------------------------------------------------------

/// Read + parse a node config TOML.
pub fn load_node_config(path: &str) -> Result<NodeConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    let cfg: NodeConfig = toml::from_str(&s).map_err(|e| ConfigError::Toml(e.to_string()))?;
    // M44: reject an unknown `[logging] format` at load time.
    if let Some(l) = &cfg.logging {
        l.validate()?;
    }
    // M51: a non-empty advertised address must be a dialable `SocketAddr`
    // (the M39 dial path parses it as one; a hostname would be un-dialable).
    if !cfg.network.advertise_addr.is_empty() {
        parse_addr(&cfg.network.advertise_addr)?;
    }
    // M53: a present `[rpc]` section must carry a dialable `listen` address so a
    // typo fails fast at load rather than at bind time.
    if let Some(r) = &cfg.rpc {
        r.listen_addr()?;
    }
    // M54: reject out-of-range `[mempool]` tuning (zero capacity/cap, negatives).
    cfg.mempool.validate()?;
    Ok(cfg)
}

/// Read + parse a genesis TOML.
pub fn load_genesis(path: &str) -> Result<GenesisConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    toml::from_str(&s).map_err(|e| ConfigError::Toml(e.to_string()))
}

/// Read + parse a keystore TOML.
pub fn load_keystore(path: &str) -> Result<KeystoreConfig, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    toml::from_str(&s).map_err(|e| ConfigError::Toml(e.to_string()))
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

fn parse_addr(s: &str) -> Result<SocketAddr, ConfigError> {
    s.parse().map_err(|_| ConfigError::BadAddr { value: s.to_string() })
}

/// Strict lowercase/uppercase hex decode into a fixed-size array of `N` bytes.
fn decode_hex_n<const N: usize>(s: &str, field: &str) -> Result<[u8; N], ConfigError> {
    let bytes = decode_hex(s, field)?;
    if bytes.len() != N {
        return Err(ConfigError::BadHex {
            field: field.to_string(),
            detail: format!("expected {N} bytes ({} hex chars), got {}", N * 2, bytes.len()),
        });
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn decode_pubkey(s: &str, field: &str) -> Result<PubKey, ConfigError> {
    decode_hex_n::<32>(s, field)
}

/// Decode a 64-char lowercase/uppercase hex string into a 32-byte ed25519 seed.
/// Public so the `encode-tx` CLI parses a `--key-file` with the exact same format
/// and `ConfigError::BadHex` errors as a validator's `seed_hex`.
pub fn decode_seed(s: &str, field: &str) -> Result<[u8; 32], ConfigError> {
    decode_hex_n::<32>(s, field)
}

fn decode_hex(s: &str, field: &str) -> Result<Vec<u8>, ConfigError> {
    if !s.len().is_multiple_of(2) {
        return Err(ConfigError::BadHex {
            field: field.to_string(),
            detail: "odd number of hex digits".to_string(),
        });
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i]).ok_or_else(|| ConfigError::BadHex {
            field: field.to_string(),
            detail: format!("non-hex byte `{}`", bytes[i] as char),
        })?;
        let lo = hex_val(bytes[i + 1]).ok_or_else(|| ConfigError::BadHex {
            field: field.to_string(),
            detail: format!("non-hex byte `{}`", bytes[i + 1] as char),
        })?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex;

    fn demo_seed(id: u64) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&id.to_le_bytes());
        s
    }

    #[test]
    fn node_config_round_trip() {
        let cfg = NodeConfig {
            node: NodeSection {
                id: 21,
                listen: "0.0.0.0:9021".to_string(),
                data_dir: "./data/n21".to_string(),
            },
            peers: vec![
                PeerConfig { id: 22, addr: "127.0.0.1:9022".to_string() },
                PeerConfig { id: 23, addr: "127.0.0.1:9023".to_string() },
            ],
            genesis: "genesis.toml".to_string(),
            validator: Some(ValidatorKeyConfig {
                enabled: true,
                seed_hex: hex(&demo_seed(21)),
            }),
            consensus: ConsensusConfig::default(),
            network: NetworkConfig::default(),
            metrics: None,
            rpc: None,
            logging: None,
            mempool: MempoolConfig::default(),
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: NodeConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.node.id, 21);
        assert_eq!(back.peers.len(), 2);
        assert_eq!(back.listen_addr().unwrap().port(), 9021);
        assert_eq!(back.peers[0].socket_addr().unwrap().port(), 9022);
        let vc = back.validator.as_ref().unwrap();
        assert!(vc.enabled);
        // the seed decodes to the id-21 demo keypair
        assert_eq!(vc.keypair().unwrap().public(), crate::Keypair::from_seed(demo_seed(21)).public());
    }

    #[test]
    fn consensus_config_defaults_match_legacy_constants() {
        // The Default is now the single source of truth for the timing numbers the
        // daemon used to hard-code — guard them so a drift is caught here.
        let c = ConsensusConfig::default();
        assert_eq!(c.propose_timeout_ms, 1000);
        assert_eq!(c.prevote_timeout_ms, 1000);
        assert_eq!(c.precommit_timeout_ms, 1000);
        assert_eq!(c.timeout_delta_ms, 500);
        assert_eq!(c.block_interval_ms, 1000);
        assert!(c.create_empty_blocks, "empty-block heartbeat on by default (M33 behavior)");
    }

    #[test]
    fn node_config_without_consensus_section_uses_defaults() {
        // Back-compat: a pre-M35 config (no `[consensus]`) must parse and yield the
        // exact legacy timing + heartbeat.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.consensus, ConsensusConfig::default());
    }

    #[test]
    fn consensus_section_partial_override_fills_from_default() {
        // A `[consensus]` table that sets only some keys: overridden keys take,
        // every unset key falls back to Default (struct-level serde default).
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [consensus]
            block_interval_ms = 250
            create_empty_blocks = false
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.consensus.block_interval_ms, 250);
        assert!(!cfg.consensus.create_empty_blocks);
        // untouched keys keep their defaults
        assert_eq!(cfg.consensus.propose_timeout_ms, 1000);
        assert_eq!(cfg.consensus.timeout_delta_ms, 500);
    }

    #[test]
    fn network_config_defaults_match_legacy_constants() {
        // The Default is now the single source of truth for the daemon's
        // network-timing constants — guard them so a drift is caught here.
        // ANNOUNCE_SECS=2 became announce_interval_ms=2000 (same duration).
        let n = NetworkConfig::default();
        assert_eq!(n.announce_interval_ms, 2000);
        assert_eq!(n.startup_delay_ms, 1000);
        // M39: peer discovery is on by default (fully-meshed configs are inert).
        assert!(n.enable_peer_exchange);
        // M40: peer auth is off by default (byte-identical pre-M40 hello).
        assert!(!n.require_peer_auth);
        // M41: TLS is off by default (byte-identical pre-M41 raw-TCP path).
        assert!(!n.enable_tls);
        // M42: channel binding is off by default (byte-identical pre-M42 transcript).
        assert!(!n.bind_channel);
        // M43: genesis-pinned mTLS is off by default (byte-identical pre-M43 TLS path).
        assert!(!n.require_peer_certs);
    }

    #[test]
    fn peer_exchange_can_be_disabled() {
        // The M39 opt-out: `enable_peer_exchange = false` pins the node to its
        // static `[[peers]]` seed set (no address-book gossip / auto-dial).
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            enable_peer_exchange = false
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(!cfg.network.enable_peer_exchange);
        // untouched keys keep their defaults
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert_eq!(cfg.network.startup_delay_ms, 1000);
    }

    #[test]
    fn require_peer_auth_defaults_off_and_parses() {
        // M40: the authenticated handshake is opt-in. Default off ⇒ the pre-M40
        // cleartext hello (back-compat); `require_peer_auth = true` opts in while
        // every untouched key still falls back to its default.
        assert!(!NetworkConfig::default().require_peer_auth);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            require_peer_auth = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.network.require_peer_auth);
        // untouched keys keep their defaults
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert!(cfg.network.enable_peer_exchange);
    }

    #[test]
    fn enable_tls_defaults_off_and_parses() {
        // M41: TLS transport encryption is opt-in. Default off ⇒ the pre-M41
        // raw-TCP path (back-compat); `enable_tls = true` opts in while every
        // untouched key still falls back to its default.
        assert!(!NetworkConfig::default().enable_tls);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            enable_tls = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.network.enable_tls);
        // untouched keys keep their defaults
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert!(cfg.network.enable_peer_exchange);
        assert!(!cfg.network.require_peer_auth);
    }

    #[test]
    fn bind_channel_defaults_off_and_parses() {
        // M42: channel binding is opt-in. Default off ⇒ the pre-M42 transcript
        // (back-compat); `bind_channel = true` opts in while every untouched key
        // still falls back to its default.
        assert!(!NetworkConfig::default().bind_channel);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            bind_channel = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.network.bind_channel);
        // untouched keys keep their defaults
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert!(cfg.network.enable_peer_exchange);
        assert!(!cfg.network.require_peer_auth);
        assert!(!cfg.network.enable_tls);
    }

    #[test]
    fn require_peer_certs_defaults_off_and_parses() {
        // M43: genesis-pinned mTLS is opt-in. Default off ⇒ the pre-M43 accept-any
        // TLS path (back-compat); `require_peer_certs = true` opts in while every
        // untouched key still falls back to its default.
        assert!(!NetworkConfig::default().require_peer_certs);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            require_peer_certs = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.network.require_peer_certs);
        // untouched keys keep their defaults
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert!(cfg.network.enable_peer_exchange);
        assert!(!cfg.network.require_peer_auth);
        assert!(!cfg.network.enable_tls);
        assert!(!cfg.network.bind_channel);
    }

    #[test]
    fn network_advertise_addr_parses_and_validates() {
        // M51: `advertise_addr` is off by default (empty ⇒ advertise the bind
        // `listen`, the M39 behavior). A literal `IP:port` parses and every
        // untouched key falls back to its default.
        assert_eq!(NetworkConfig::default().advertise_addr, "");
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            advertise_addr = "203.0.113.7:9021"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.network.advertise_addr, "203.0.113.7:9021");
        assert_eq!(cfg.network.announce_interval_ms, 2000);
        assert!(cfg.network.enable_peer_exchange);

        // And `load_node_config` rejects a non-`SocketAddr` value end-to-end
        // (a DNS hostname would be un-dialable by the M39 dial path).
        let bad = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            advertise_addr = "not-an-address"
        "#;
        let path =
            std::env::temp_dir().join(format!("zhixing-m51-badadv-{}.toml", std::process::id()));
        std::fs::write(&path, bad).unwrap();
        let got = load_node_config(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(matches!(got, Err(ConfigError::BadAddr { .. })));
    }

    #[test]
    fn node_config_without_network_section_uses_defaults() {
        // Back-compat: a pre-M36 config (no `[network]`) must parse and yield the
        // exact legacy heartbeat + startup grace.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.network, NetworkConfig::default());
    }

    #[test]
    fn network_section_partial_override_fills_from_default() {
        // A `[network]` table that sets only one key: the overridden key takes,
        // the unset key falls back to Default (struct-level serde default).
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            announce_interval_ms = 500
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.network.announce_interval_ms, 500);
        // untouched key keeps its default
        assert_eq!(cfg.network.startup_delay_ms, 1000);
    }

    #[test]
    fn metrics_config_default_is_disabled() {
        // The metrics endpoint is opt-in: the Default must be inert so a bare
        // `[metrics]` table (or an absent one) never binds a listener.
        let m = MetricsConfig::default();
        assert!(!m.enabled);
        assert_eq!(m.listen, "127.0.0.1:9600");
    }

    #[test]
    fn node_config_without_metrics_section_is_none() {
        // Back-compat: a config with no `[metrics]` yields `None` ⇒ endpoint off.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.metrics.is_none());
    }

    #[test]
    fn metrics_section_enables_endpoint() {
        // A bare `[metrics] enabled = true` opts in; `listen` falls back to the
        // struct-level serde default.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [metrics]
            enabled = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        let m = cfg.metrics.expect("metrics section present");
        assert!(m.enabled);
        assert_eq!(m.listen, "127.0.0.1:9600");
    }

    #[test]
    fn rpc_config_default_is_disabled() {
        // M53: the ingress RPC is opt-in — the Default must be inert so a bare
        // `[rpc]` table (or an absent one) never binds a listener.
        let r = RpcConfig::default();
        assert!(!r.enabled);
        assert_eq!(r.listen, "127.0.0.1:9700");
    }

    #[test]
    fn node_config_without_rpc_section_is_none() {
        // Back-compat: a config with no `[rpc]` yields `None` ⇒ endpoint off.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert!(cfg.rpc.is_none());
    }

    #[test]
    fn rpc_section_enables_endpoint() {
        // A bare `[rpc] enabled = true` opts in; `listen` falls back to the
        // struct-level serde default.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [rpc]
            enabled = true
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        let r = cfg.rpc.expect("rpc section present");
        assert!(r.enabled);
        assert_eq!(r.listen, "127.0.0.1:9700");
        // A valid listen address parses to a SocketAddr.
        assert!(r.listen_addr().is_ok());
    }

    #[test]
    fn rpc_bad_listen_is_rejected_by_loader() {
        // M53: load_node_config validates a present `[rpc]` listen up front so a
        // typo fails fast with BadAddr rather than at bind time.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zhixing_rpc_badaddr_{}.toml", std::process::id()));
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [rpc]
            enabled = true
            listen = "not-an-addr"
        "#;
        std::fs::write(&path, s).unwrap();
        let err = load_node_config(path.to_str().unwrap()).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ConfigError::BadAddr { .. }), "got {err:?}");
    }

    #[test]
    fn mempool_config_has_safe_defaults() {
        // M54: an absent `[mempool]` section must reproduce pre-M54 behavior —
        // ample capacity, the historical per-block cap, rate limiting disabled.
        let m = MempoolConfig::default();
        assert_eq!(m.capacity, 4096);
        assert_eq!(m.max_block_txs, 64);
        assert_eq!(m.per_peer_tx_per_sec, 0.0);
        assert_eq!(m.per_peer_tx_burst, 256.0);
        assert_eq!(m.seen_cache, 0); // M55: 0 ⇒ dedup sets unbounded (pre-M55)
        assert_eq!(m.per_account_limit, 0); // M57: 0 ⇒ per-account quota off (pre-M57)
        // And a config with no `[mempool]` table yields exactly those defaults.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.mempool, MempoolConfig::default());
    }

    #[test]
    fn mempool_section_overrides_defaults() {
        // A partial `[mempool]` table overrides field-by-field; unset keys fall
        // back to the struct-level serde default.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [mempool]
            capacity = 10
            per_peer_tx_per_sec = 50.0
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.mempool.capacity, 10);
        assert_eq!(cfg.mempool.per_peer_tx_per_sec, 50.0);
        assert_eq!(cfg.mempool.max_block_txs, 64); // untouched default
        assert_eq!(cfg.mempool.per_peer_tx_burst, 256.0); // untouched default
    }

    #[test]
    fn mempool_seen_cache_overrides_defaults() {
        // M55: `seen_cache` defaults to 0 (unbounded dedup sets) and is overridable
        // field-by-field like the other `[mempool]` knobs.
        assert_eq!(MempoolConfig::default().seen_cache, 0);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [mempool]
            seen_cache = 1024
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.mempool.seen_cache, 1024);
        assert_eq!(cfg.mempool.capacity, 4096); // untouched default
    }

    #[test]
    fn mempool_per_account_limit_overrides_defaults() {
        // M57: `per_account_limit` defaults to 0 (quota off) and is overridable
        // field-by-field like the other `[mempool]` knobs.
        assert_eq!(MempoolConfig::default().per_account_limit, 0);
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [mempool]
            per_account_limit = 32
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        assert_eq!(cfg.mempool.per_account_limit, 32);
        assert_eq!(cfg.mempool.capacity, 4096); // untouched default
        assert_eq!(cfg.mempool.seen_cache, 0); // untouched default
    }

    #[test]
    fn mempool_zero_capacity_rejected_by_loader() {
        // M54: load_node_config validates `[mempool]` up front so a nonsensical
        // zero capacity fails fast with BadMempool rather than stalling admission.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("zhixing_mempool_zero_{}.toml", std::process::id()));
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [mempool]
            capacity = 0
        "#;
        std::fs::write(&path, s).unwrap();
        let err = load_node_config(path.to_str().unwrap()).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(matches!(err, ConfigError::BadMempool { .. }), "got {err:?}");
    }

    #[test]
    fn logging_defaults_off_and_parses() {
        // M44: the `[logging]` section is opt-in. Absent ⇒ `None` ⇒ the daemon's
        // subscriber init uses the M37 default (info/text/stderr). The Default
        // MUST reproduce that (info + text) so a bare/absent section is byte-
        // identical to pre-M44. A present section parses both knobs.
        let d = LoggingConfig::default();
        assert_eq!(d.level, "info");
        assert_eq!(d.format, "text");
        // M45: the file target is off by default (stderr), rotation defaults daily.
        assert_eq!(d.file, "");
        assert_eq!(d.rotation, "daily");

        let absent = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
        "#;
        let cfg: NodeConfig = toml::from_str(absent).unwrap();
        assert!(cfg.logging.is_none());

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            level = "debug"
            format = "json"
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.level, "debug");
        assert_eq!(l.format, "json");

        // a bare `[logging]` table falls back field-by-field to the defaults
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        assert_eq!(cfg.logging.expect("present"), LoggingConfig::default());
    }

    #[test]
    fn logging_rejects_bad_format() {
        // Valid formats pass `validate`; anything else is a typed error.
        assert!(LoggingConfig { format: "text".into(), ..Default::default() }.validate().is_ok());
        assert!(LoggingConfig { format: "json".into(), ..Default::default() }.validate().is_ok());
        assert!(matches!(
            LoggingConfig { format: "yaml".into(), ..Default::default() }.validate(),
            Err(ConfigError::BadLogFormat { .. })
        ));

        // And `load_node_config` surfaces the rejection end-to-end (validation
        // runs after the TOML parse).
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            format = "yaml"
        "#;
        let path = std::env::temp_dir().join(format!("zhixing-m44-badfmt-{}.toml", std::process::id()));
        std::fs::write(&path, s).unwrap();
        let got = load_node_config(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(matches!(got, Err(ConfigError::BadLogFormat { .. })));
    }

    #[test]
    fn logging_file_rotation_parses_and_defaults() {
        // M45: file/rotation are two more `[logging]` knobs. The Default keeps the
        // file target off (empty ⇒ stderr) with a daily rotation, so an
        // absent/bare section is still byte-identical to M44/M37.
        let d = LoggingConfig::default();
        assert_eq!(d.file, "");
        assert_eq!(d.rotation, "daily");
        assert!(!d.stderr); // M46: tee off by default
        assert_eq!(d.stderr_level, ""); // M47: per-sink levels inherit `level`
        assert_eq!(d.file_level, "");
        assert!(d.levels.is_empty()); // M48: directive arrays default empty
        assert!(d.stderr_levels.is_empty());
        assert!(d.file_levels.is_empty());
        assert_eq!(d.stderr_format, ""); // M49: per-sink formats inherit `format`
        assert_eq!(d.file_format, "");
        assert_eq!(d.max_files, 0); // M50: retention unbounded by default

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            rotation = "hourly"
            format = "json"
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.file, "data/logs/node.log");
        assert_eq!(l.rotation, "hourly");
        assert_eq!(l.format, "json");
        // level unspecified ⇒ field-level default fills it
        assert_eq!(l.level, "info");
        l.validate().expect("file + hourly + json is valid");

        // a bare `[logging]` table still falls back to the (stderr) defaults
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        assert_eq!(cfg.logging.expect("present"), LoggingConfig::default());
    }

    #[test]
    fn logging_rejects_bad_rotation() {
        // Every valid rotation passes `validate`; anything else is a typed error.
        for r in ["daily", "hourly", "minutely", "never"] {
            assert!(
                LoggingConfig { rotation: r.into(), ..Default::default() }.validate().is_ok(),
                "rotation {r} should be valid"
            );
        }
        assert!(matches!(
            LoggingConfig { rotation: "weekly".into(), ..Default::default() }.validate(),
            Err(ConfigError::BadLogRotation { .. })
        ));

        // And `load_node_config` surfaces the rejection end-to-end.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            rotation = "weekly"
        "#;
        let path = std::env::temp_dir().join(format!("zhixing-m45-badrot-{}.toml", std::process::id()));
        std::fs::write(&path, s).unwrap();
        let got = load_node_config(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(matches!(got, Err(ConfigError::BadLogRotation { .. })));
    }

    #[test]
    fn logging_stderr_tee_parses_and_defaults() {
        // M46: `stderr` is one more `[logging]` knob — a tee that mirrors a file
        // target to stderr. Default `false` keeps M45's single-sink behavior, so an
        // absent/bare section stays byte-identical to M45/M44/M37.
        assert!(!LoggingConfig::default().stderr);

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            rotation = "hourly"
            stderr = true
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert!(l.stderr);
        // the other knobs parse alongside `stderr`
        assert_eq!(l.file, "data/logs/node.log");
        assert_eq!(l.rotation, "hourly");
        assert_eq!(l.format, "text"); // unspecified ⇒ field default
        l.validate().expect("file + hourly + tee is valid");

        // a bare `[logging]` table ⇒ tee off (single-sink, back-compat)
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        assert!(!cfg.logging.expect("present").stderr);
    }

    #[test]
    fn logging_per_sink_levels_parse_and_default() {
        // M47: `stderr_level`/`file_level` are per-sink filter overrides for the tee.
        // Both empty by default ⇒ both sinks share `level` (the M46 tee), so an
        // absent/bare section stays byte-identical to M46/M45/M44/M37.
        let d = LoggingConfig::default();
        assert_eq!(d.stderr_level, "");
        assert_eq!(d.file_level, "");

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            stderr = true
            stderr_level = "info"
            file_level = "debug"
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.stderr_level, "info");
        assert_eq!(l.file_level, "debug");
        // per-sink levels are free-form (like `level`) ⇒ no validate rejection
        l.validate().expect("per-sink levels are not validated");

        // a bare `[logging]` table ⇒ both empty (⇒ shared-filter tee, back-compat)
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        let l = cfg.logging.expect("present");
        assert_eq!(l.stderr_level, "");
        assert_eq!(l.file_level, "");
    }

    #[test]
    fn logging_directive_arrays_parse_and_default() {
        // M48: `levels`/`stderr_levels`/`file_levels` are array forms of the three
        // scalar filter knobs. All empty by default ⇒ the scalars are used verbatim,
        // so an absent/bare section stays byte-identical to M47/M46/M45/M44/M37.
        let d = LoggingConfig::default();
        assert!(d.levels.is_empty());
        assert!(d.stderr_levels.is_empty());
        assert!(d.file_levels.is_empty());

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            stderr = true
            levels = ["info", "tokio=warn"]
            file_levels = ["debug"]
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.levels, vec!["info", "tokio=warn"]);
        assert_eq!(l.file_levels, vec!["debug"]);
        assert!(l.stderr_levels.is_empty()); // unspecified ⇒ empty
        // arrays are free-form directives (like `level`) ⇒ no validate rejection
        l.validate().expect("directive arrays are not validated");

        // a bare `[logging]` table ⇒ all arrays empty (⇒ scalars verbatim, back-compat)
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        let l = cfg.logging.expect("present");
        assert!(l.levels.is_empty());
        assert!(l.stderr_levels.is_empty());
        assert!(l.file_levels.is_empty());
    }

    #[test]
    fn logging_per_sink_formats_parse_and_default() {
        // M49: `stderr_format`/`file_format` are per-sink formatter overrides for the
        // tee. Both empty by default ⇒ both sinks inherit `format`, so an absent/bare
        // section stays byte-identical to M48/M47/M46/M45/M44/M37.
        let d = LoggingConfig::default();
        assert_eq!(d.stderr_format, "");
        assert_eq!(d.file_format, "");

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            stderr = true
            stderr_format = "text"
            file_format = "json"
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.stderr_format, "text");
        assert_eq!(l.file_format, "json");
        // per-sink formats are enum-like (like `format`) ⇒ validated, both valid here
        l.validate().expect("text + json per-sink formats are valid");

        // empty per-sink formats pass validate (inherit `format`)
        LoggingConfig::default().validate().expect("empty per-sink formats inherit");
        // an unknown per-sink format is a typed rejection, like the scalar `format`
        assert!(matches!(
            LoggingConfig { stderr_format: "yaml".into(), ..Default::default() }.validate(),
            Err(ConfigError::BadLogFormat { .. })
        ));

        // a bare `[logging]` table ⇒ both per-sink formats empty (⇒ inherit, back-compat)
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        let l = cfg.logging.expect("present");
        assert_eq!(l.stderr_format, "");
        assert_eq!(l.file_format, "");
    }

    #[test]
    fn logging_max_files_parses_and_defaults() {
        // M50: `max_files` caps retained rotated files. `0` (default) ⇒ unbounded,
        // so an absent/bare section stays byte-identical to M49/…/M37.
        let d = LoggingConfig::default();
        assert_eq!(d.max_files, 0);

        let present = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
            file = "data/logs/node.log"
            rotation = "daily"
            max_files = 7
        "#;
        let cfg: NodeConfig = toml::from_str(present).unwrap();
        let l = cfg.logging.expect("logging section present");
        assert_eq!(l.max_files, 7);
        // `max_files` is a plain count, so it needs no enum validation.
        l.validate().expect("a numeric max_files is always valid");

        // a bare `[logging]` table ⇒ max_files 0 (⇒ unbounded, back-compat)
        let bare = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [logging]
        "#;
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        assert_eq!(cfg.logging.expect("present").max_files, 0);
    }

    #[test]
    fn validator_section_defaults_to_disabled() {
        // A `[validator]` table with only a seed (no `enabled`) parses as inert.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [validator]
            seed_hex = "0000000000000000000000000000000000000000000000000000000000000000"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        let vc = cfg.validator.as_ref().unwrap();
        assert!(!vc.enabled, "missing `enabled` defaults to false");
        // no `[validator]` at all ⇒ pure follower
        let s2 = r#"
            genesis = "genesis.toml"
            [node]
            id = 30
            listen = "0.0.0.0:9030"
            data_dir = "./data/n30"
        "#;
        let follower: NodeConfig = toml::from_str(s2).unwrap();
        assert!(follower.validator.is_none());
    }

    #[test]
    fn genesis_config_converts_to_demo_genesis() {
        // Mirror main.rs demo_genesis: accounts 1..=3 @ 30 COG, reviewers 10..=12,
        // validators 21..=24 equal power. Verify to_genesis() reproduces the fields.
        let kp = |id: u64| crate::Keypair::from_seed(demo_seed(id));
        let gc = GenesisConfig {
            accounts: (1..=3)
                .map(|id| AccountConfig {
                    id,
                    balance_micro: 30 * crate::MICRO,
                    pubkey_hex: hex(&kp(id).public()),
                })
                .collect(),
            reviewers: (10..=12).map(|id| ReviewerConfig { id, weight: 1.0 }).collect(),
            seed_nodes: vec![SeedNodeConfig { embedding: unit_vec(0), domain: 0 }],
            base_emission_micro: 8 * crate::MICRO,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: (21..=24)
                .map(|id| ValidatorConfig { id, pubkey_hex: hex(&kp(id).public()), power: 1 })
                .collect(),
            params: None,
        };
        let g = gc.to_genesis().unwrap();
        assert_eq!(g.accounts.len(), 3);
        assert_eq!(g.accounts[0], (1, 30 * crate::MICRO, kp(1).public()));
        assert_eq!(g.validators.len(), 4);
        assert_eq!(g.validators[3], (24, kp(24).public(), 1));
        assert_eq!(g.reviewers, vec![(10, 1.0), (11, 1.0), (12, 1.0)]);
        assert_eq!(g.base_emission_micro, 8 * crate::MICRO);
    }

    #[test]
    fn keystore_converts_to_seed_map() {
        let ks = KeystoreConfig {
            keys: (21..=24).map(|id| KeyEntry { id, seed_hex: hex(&demo_seed(id)) }).collect(),
        };
        let seeds = ks.to_seeds().unwrap();
        assert_eq!(seeds.len(), 4);
        assert_eq!(seeds[&21], demo_seed(21));
        assert_eq!(seeds[&24], demo_seed(24));
    }

    #[test]
    fn bad_hex_and_addr_are_typed_errors() {
        let bad_pk = AccountConfig { id: 1, balance_micro: 0, pubkey_hex: "zz".to_string() };
        let gc = GenesisConfig {
            accounts: vec![bad_pk],
            reviewers: vec![],
            seed_nodes: vec![],
            base_emission_micro: 0,
            slash_bps: 0,
            timestamp_days: 0.0,
            validators: vec![],
            params: None,
        };
        assert!(matches!(gc.to_genesis(), Err(ConfigError::BadHex { .. })));

        // wrong length hex
        let short = KeystoreConfig { keys: vec![KeyEntry { id: 1, seed_hex: "abcd".to_string() }] };
        assert!(matches!(short.to_seeds(), Err(ConfigError::BadHex { .. })));

        let pc = PeerConfig { id: 1, addr: "not-an-addr".to_string() };
        assert!(matches!(pc.socket_addr(), Err(ConfigError::BadAddr { .. })));
    }

    #[test]
    fn checked_in_testnet_samples_load() {
        // The `testnet/` sample configs must always parse + convert cleanly, so a
        // `node run --config testnet/node21.toml` recipe never ships broken.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testnet");
        let g = load_genesis(dir.join("genesis.toml").to_str().unwrap()).unwrap();
        let genesis = g.to_genesis().unwrap();
        assert_eq!(genesis.validators.len(), 4);
        assert_eq!(genesis.accounts.len(), 3);

        // M33: every one of the four nodes is an enabled validator (no
        // sequencer), and each node's own seed must reproduce its genesis pubkey.
        let by_id: std::collections::BTreeMap<u64, PubKey> =
            genesis.validators.iter().map(|(id, pk, _)| (*id, *pk)).collect();
        for name in ["node21.toml", "node22.toml", "node23.toml", "node24.toml"] {
            let n = load_node_config(dir.join(name).to_str().unwrap()).unwrap();
            assert!(n.listen_addr().is_ok());
            assert_eq!(n.peers.len(), 3);
            let vc = n.validator.as_ref().expect("testnet node is a validator");
            assert!(vc.enabled);
            assert_eq!(
                vc.keypair().unwrap().public(),
                by_id[&n.node.id],
                "node {}'s seed must match its genesis validator pubkey",
                n.node.id
            );
        }
    }

    #[test]
    fn validator_pubkey_mismatch_is_detectable() {
        // Startup fails fast when a node's configured seed does not derive the
        // pubkey the genesis assigns its id. We model that check here: a seed for
        // id 99 does not match the id-21 genesis validator.
        let genesis_pk = crate::Keypair::from_seed(demo_seed(21)).public();
        let vc = ValidatorKeyConfig { enabled: true, seed_hex: hex(&demo_seed(99)) };
        assert_ne!(vc.keypair().unwrap().public(), genesis_pk);
        // the matching seed agrees
        let ok = ValidatorKeyConfig { enabled: true, seed_hex: hex(&demo_seed(21)) };
        assert_eq!(ok.keypair().unwrap().public(), genesis_pk);
    }

    fn unit_vec(dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; DIM];
        v[dim % DIM] = 1.0;
        v
    }

    /// Throwaway generator for the checked-in `testnet/` sample configs. Run with
    /// `cargo test -- --ignored write_sample_testnet` to regenerate them from the
    /// demo seed scheme; not part of the normal suite.
    #[test]
    #[ignore]
    fn write_sample_testnet() {
        use crate::Keypair;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testnet");
        std::fs::create_dir_all(&dir).unwrap();
        let kp = |id: u64| Keypair::from_seed(demo_seed(id));

        let genesis = GenesisConfig {
            accounts: (1..=3)
                .map(|id| AccountConfig { id, balance_micro: 30 * crate::MICRO, pubkey_hex: hex(&kp(id).public()) })
                .collect(),
            reviewers: (10..=12).map(|id| ReviewerConfig { id, weight: 1.0 }).collect(),
            seed_nodes: vec![SeedNodeConfig { embedding: unit_vec(0), domain: 0 }],
            base_emission_micro: 8 * crate::MICRO,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: (21..=24)
                .map(|id| ValidatorConfig { id, pubkey_hex: hex(&kp(id).public()), power: 1 })
                .collect(),
            params: None,
        };
        std::fs::write(dir.join("genesis.toml"), toml::to_string_pretty(&genesis).unwrap()).unwrap();

        // M33: no keystore.toml and no sequencer — each node carries only its own
        // signing key in a `[validator]` section.
        let ids = [21u64, 22, 23, 24];
        for &id in &ids {
            let cfg = NodeConfig {
                node: NodeSection {
                    id,
                    listen: format!("0.0.0.0:{}", 9000 + id),
                    data_dir: format!("./data/n{id}"),
                },
                peers: ids
                    .iter()
                    .filter(|&&p| p != id)
                    .map(|&p| PeerConfig { id: p, addr: format!("127.0.0.1:{}", 9000 + p) })
                    .collect(),
                genesis: "testnet/genesis.toml".to_string(),
                validator: Some(ValidatorKeyConfig {
                    enabled: true,
                    seed_hex: hex(&demo_seed(id)),
                }),
                consensus: ConsensusConfig::default(),
                network: NetworkConfig::default(),
                metrics: None,
                rpc: None,
                logging: None,
                mempool: MempoolConfig::default(),
            };
            std::fs::write(dir.join(format!("node{id}.toml")), toml::to_string_pretty(&cfg).unwrap()).unwrap();
        }

        // Remove the now-obsolete pre-M33 keystore if a prior run left one.
        let _ = std::fs::remove_file(dir.join("keystore.toml"));
    }
}
