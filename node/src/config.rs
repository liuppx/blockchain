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
    /// M177: a `[network]` tuning value was out of range (e.g. zero
    /// `max_inbound_connections`).
    BadNetwork { detail: String },
    /// SEC-04: a genesis consensus parameter is outside its safe range (e.g.
    /// `slash_bps > 10000` or an over-large `base_emission_micro`).
    BadGenesis { detail: String },
    /// M141: a `[snapshot]` boot config was internally incoherent (e.g. a cert
    /// without a trusted validator set, or vice versa).
    BadSnapshot { detail: String },
    /// M169: a `[validator] mnemonic` field failed BIP-39 validation
    /// (unknown word, bad length, bad checksum, or non-ASCII input).
    BadMnemonic { detail: String },
    /// M175: the encrypted keystore at `[validator] encrypted_keystore_path`
    /// could not be read, parsed, or decrypted (wrong password / tampered /
    /// unsupported params). The underlying cause is deliberately coarse on the
    /// decrypt side so a wrong password is not distinguishable from tampering.
    KeystoreDecrypt { detail: String },
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
                write!(
                    f,
                    "config bad log format: `{value}` (expected \"text\" or \"json\")"
                )
            }
            ConfigError::BadLogRotation { value } => {
                write!(
                    f,
                    "config bad log rotation: `{value}` (expected \"daily\", \"hourly\", \"minutely\", or \"never\")"
                )
            }
            ConfigError::BadMempool { detail } => write!(f, "config bad mempool: {detail}"),
            ConfigError::BadNetwork { detail } => write!(f, "config bad network: {detail}"),
            ConfigError::BadGenesis { detail } => write!(f, "config bad genesis: {detail}"),
            ConfigError::BadSnapshot { detail } => write!(f, "config bad snapshot: {detail}"),
            ConfigError::BadMnemonic { detail } => {
                write!(f, "config bad mnemonic: {detail}")
            }
            ConfigError::KeystoreDecrypt { detail } => {
                write!(f, "config keystore decrypt: {detail}")
            }
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
    /// M140: opt-in snapshot-boot config. When `Some`, `Node::start` reads the
    /// M131-format snapshot blob at `path`, verifies the three cert-signed
    /// roots in the supplied `BlockHeader` hex match the snapshot's `state_root`/
    /// `accounts_root`/`bridge_root` (the same `node check-snapshot` check at boot
    /// time, since the operator has no cert + tracked set yet), truncates the on-disk
    /// `blocks.log`/`certs.log` to zero, and constructs `GossipNode::from_snapshot`
    /// at the snapshot's tip. Absent (default) ⇒ byte-identical genesis-start boot.
    /// `path` is resolved relative to CWD; `header_hex` is the on-wire form
    /// `GET /header/{h}` returns.
    #[serde(default)]
    pub snapshot: Option<SnapshotBootConfig>,
    /// M154: opt-in weak-subjectivity checkpoint for a light node (`node light-run`).
    /// When `Some`, the light node seeds its tracker from this pinned
    /// `(height, head, validator set)` anchor and follows forward — no snapshot blob,
    /// no cert. Ignored by the full node. Mutually exclusive with `snapshot`.
    #[serde(default)]
    pub checkpoint: Option<CheckpointConfig>,
}

/// M154: a weak-subjectivity checkpoint a light node trusts as its subjectivity anchor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointConfig {
    /// The checkpoint height (the light node follows `height + 1` onward).
    pub height: u64,
    /// Hex of the checkpoint block's hash — the head the next header must chain to.
    pub head_hex: String,
    /// The validator set active at the checkpoint (certifies `height + 1`). Reuses the
    /// genesis `[[validators]]` encoding (id/pubkey_hex/power).
    pub validators: Vec<ValidatorConfig>,
}

impl CheckpointConfig {
    /// M154: decode into `(head hash, ValidatorSet)`. Rejects a malformed `head_hex`,
    /// a bad validator pubkey, or an empty validator set.
    pub fn decode(&self) -> Result<([u8; 32], crate::validator::ValidatorSet), ConfigError> {
        let head = decode_hex_n::<32>(&self.head_hex, "checkpoint.head_hex")?;
        if self.validators.is_empty() {
            return Err(ConfigError::BadSnapshot {
                detail: "checkpoint.validators must be non-empty".into(),
            });
        }
        let mut vals = Vec::with_capacity(self.validators.len());
        for v in &self.validators {
            vals.push(crate::validator::Validator {
                id: v.id,
                pubkey: decode_pubkey(&v.pubkey_hex, "checkpoint.validators.pubkey_hex")?,
                power: v.power,
            });
        }
        Ok((head, crate::validator::ValidatorSet::new(vals)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotBootConfig {
    /// Path to a M131-format snapshot file.
    pub path: String,
    /// Hex-encoded cert-signed `BlockHeader` (the same on-wire form
    /// `GET /header/{h}` produces); the boot-time check verifies its
    /// `state_root`/`accounts_root`/`bridge_root` match the snapshot.
    pub header_hex: String,
    /// M141: optional hex-encoded finality `Commit` (the `codec::encode_commit`
    /// form) certifying the snapshot's tip header. When present, boot additionally
    /// requires it to reach `>2/3` of `trusted_validators` (a weak-subjectivity
    /// checkpoint the operator pins out-of-band). Paired with `trusted_validators`.
    #[serde(default)]
    pub cert_hex: Option<String>,
    /// M141: operator-pinned trusted validator set the `cert_hex` must reach
    /// `>2/3` of. Reuses the genesis `[[validators]]` encoding (id/pubkey_hex/power).
    /// Empty ⇒ no quorum check (M135 commitment-only). Paired with `cert_hex`.
    #[serde(default)]
    pub trusted_validators: Vec<ValidatorConfig>,
}

impl SnapshotBootConfig {
    /// M141: a snapshot-boot config is coherent only if `cert_hex` and
    /// `trusted_validators` are both present (quorum-bound boot) or both absent
    /// (M135 commitment-only boot). A cert without a set (nothing to verify it
    /// against) or a set without a cert (nothing to verify) is an operator error.
    /// This is the authoritative gate — it runs at `Node::start` (via
    /// [`crate::daemon`]) for both the TOML and the CLI keypath.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.cert_hex.is_some() != !self.trusted_validators.is_empty() {
            return Err(ConfigError::BadSnapshot {
                detail: "`cert_hex` and `trusted_validators` must be given together \
                         (a cert needs a trusted set to verify against, and vice versa)"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// M141: decode `trusted_validators` into `(id, PubKey, power)` tuples, reusing
    /// the genesis pubkey-hex decoder + `BadHex` errors. Empty ⇒ empty vec.
    pub fn trusted_validators_decoded(&self) -> Result<Vec<(u64, PubKey, u64)>, ConfigError> {
        let mut out = Vec::with_capacity(self.trusted_validators.len());
        for v in &self.trusted_validators {
            out.push((
                v.id,
                decode_pubkey(&v.pubkey_hex, "snapshot.trusted_validators.pubkey_hex")?,
                v.power,
            ));
        }
        Ok(out)
    }
}

/// M141: a standalone TOML file holding the trusted validator set for a
/// quorum-bound snapshot boot, supplied via `node run --snapshot-validators FILE`.
/// Reuses the genesis `[[validator]]` entry shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustedValidatorsFile {
    #[serde(default)]
    pub validator: Vec<ValidatorConfig>,
}

/// M141: read + parse a `--snapshot-validators FILE` TOML into the entry list the
/// CLI merges into `SnapshotBootConfig::trusted_validators`.
pub fn load_trusted_validators(path: &str) -> Result<Vec<ValidatorConfig>, ConfigError> {
    let s = std::fs::read_to_string(path)?;
    let f: TrustedValidatorsFile =
        toml::from_str(&s).map_err(|e| ConfigError::Toml(e.to_string()))?;
    Ok(f.validator)
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidatorKeyConfig {
    /// When false the process still holds a key but does not participate in
    /// consensus (useful to stand a node up as a follower without editing the
    /// key out). Defaults to false so a bare `[validator]` table is inert.
    #[serde(default)]
    pub enabled: bool,
    /// 32-byte ed25519 seed, lowercase hex (64 chars). The derived public key
    /// must match this node's entry in `genesis.validators` (checked at startup).
    /// `Option` so the M169 BIP-39 fields can substitute for it. If both
    /// `seed_hex` and `mnemonic` are set, `mnemonic` wins (BIP-39 is the
    /// human-readable form; seed_hex is a legacy machine-friendly alias).
    #[serde(default)]
    pub seed_hex: Option<String>,
    /// M169: BIP-39 12/15/18/21/24-word mnemonic (space-separated, lowercase).
    /// The 32-byte ed25519 seed is derived via PBKDF2-HMAC-SHA512
    /// (`"mnemonic" + passphrase` as salt, 2048 iters) and the first 32 bytes
    /// of the 64-byte output are taken. This is the operator-friendly backup
    /// form: write the words on paper, lose the file, recover anywhere.
    #[serde(default)]
    pub mnemonic: Option<String>,
    /// M169: optional BIP-39 "25th word" passphrase. Folded into the PBKDF2
    /// salt (`"mnemonic" + passphrase`). Empty string (the default) is the
    /// BIP-39 "no passphrase" case. The passphrase is never written to disk
    /// by `node keygen --new-mnemonic`; it lives only in the operator's head
    /// and in the optional `[validator] passphrase = "..."` TOML line.
    #[serde(default)]
    pub passphrase: Option<String>,
    /// M175: path to an encrypted JSON keystore (Web3 Secret Storage v3:
    /// scrypt KDF + AES-128-CTR + Keccak-256 MAC). When set, this takes
    /// precedence over `mnemonic` and `seed_hex` — the secret never lives in
    /// the config file. Decryption needs a password, which is read from stdin
    /// at node startup (never from the config), so this field is resolved via
    /// [`ValidatorKeyConfig::keypair_with_password`], not the bare
    /// [`ValidatorKeyConfig::keypair`].
    #[serde(default)]
    pub encrypted_keystore_path: Option<String>,
    /// M180: an optional **pending** signing key for an in-flight validator key
    /// rotation, as a nested `[validator.pending]` table of this same shape (it
    /// reuses `keypair()` / `keypair_with_password()`, so the pending key can come
    /// from `seed_hex`, `mnemonic`, or an encrypted keystore just like the active
    /// one). The nested `enabled` is ignored. When set, `node run` resolves it and
    /// hands it to the daemon, which swaps it into the active key once this
    /// validator's own `KeyRotation` takes effect (`Actor::reconcile_signing_key`).
    /// Unset (default) ⇒ no pending key (M179 behavior).
    #[serde(default)]
    pub pending: Option<Box<ValidatorKeyConfig>>,
    /// M218 (remote-signer S3): optional `[validator.signer]` sub-table. When present,
    /// this node holds **no** key on disk — it signs consensus votes/proposals and the
    /// peer-auth handshake by asking an out-of-process `node signer` over the Unix socket
    /// (see `docs/design/remote-signer.md`). `seed_hex`/`mnemonic`/`encrypted_keystore_path`
    /// are then unused. Mutually exclusive with `pending` (key rotation of a remote signer
    /// is handled out-of-band at the signer process).
    #[serde(default)]
    pub signer: Option<SignerConfig>,
}

/// M218: `[validator.signer]` — how to reach the out-of-process remote signer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignerConfig {
    /// Path to the signer's Unix domain socket (the `node signer --socket` path).
    pub socket: String,
    /// Per-request timeout; a slower response fails closed (the node does not vote that
    /// step). Defaults to 500ms — ample for a local socket, well under the block interval.
    #[serde(default = "default_signer_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_signer_timeout_ms() -> u64 {
    500
}

impl ValidatorKeyConfig {
    /// Decode the seed (either from a raw `seed_hex` or from a BIP-39
    /// `mnemonic` + `passphrase` derivation) and build the signing
    /// [`Keypair`]. `mnemonic` takes precedence over `seed_hex` when both
    /// are set; the legacy single-form configs (M94) keep working unchanged
    /// because `seed_hex` is the only field they set.
    ///
    /// M175: if `encrypted_keystore_path` is set, this method errors — an
    /// encrypted keystore needs a password, which must be supplied via
    /// [`Self::keypair_with_password`]. The daemon reads that password from
    /// stdin at startup; tests pass it directly.
    pub fn keypair(&self) -> Result<crate::Keypair, ConfigError> {
        if self.encrypted_keystore_path.is_some() {
            return Err(ConfigError::KeystoreDecrypt {
                detail: "encrypted_keystore_path is set; a password is required \
                         (resolve via keypair_with_password, not keypair)"
                    .to_string(),
            });
        }
        let seed = self.seed_from_mnemonic_or_hex()?;
        Ok(crate::Keypair::from_seed(seed))
    }

    /// M175: resolve the signing key, supplying `password` for the encrypted
    /// keystore path. Precedence: `encrypted_keystore_path` (decrypt with
    /// `password`) > `mnemonic` > `seed_hex`. The `password` is ignored for the
    /// mnemonic/seed_hex paths (they carry no at-rest encryption), so a caller
    /// can always route through this method; the bare [`Self::keypair`] stays
    /// for call sites that never use a keystore.
    pub fn keypair_with_password(&self, password: &str) -> Result<crate::Keypair, ConfigError> {
        if let Some(path) = &self.encrypted_keystore_path {
            let json = std::fs::read_to_string(path).map_err(|e| ConfigError::KeystoreDecrypt {
                detail: format!("read `{path}`: {e}"),
            })?;
            let seed = crate::keystore::decrypt_seed(&json, password).map_err(|e| {
                ConfigError::KeystoreDecrypt {
                    detail: e.to_string(),
                }
            })?;
            return Ok(crate::Keypair::from_seed(seed));
        }
        let seed = self.seed_from_mnemonic_or_hex()?;
        Ok(crate::Keypair::from_seed(seed))
    }

    /// Shared seed resolution for the two non-keystore paths (mnemonic, then
    /// seed_hex). Factored out so [`Self::keypair`] and
    /// [`Self::keypair_with_password`] stay byte-identical on these paths.
    fn seed_from_mnemonic_or_hex(&self) -> Result<[u8; 32], ConfigError> {
        if let Some(words) = &self.mnemonic {
            // M169: validate the mnemonic up front so a typo in the
            // operator's `mnemonic = "..."` line fails at startup with a
            // clear `BadMnemonic` error (the PBKDF2 derivation would
            // otherwise silently produce a 32-byte seed regardless of
            // checksum, masking the typo).
            crate::crypto::validate_mnemonic(words).map_err(|e| ConfigError::BadMnemonic {
                detail: e.to_string(),
            })?;
            let pass = self.passphrase.as_deref().unwrap_or("");
            let mut s = [0u8; 32];
            crate::crypto::mnemonic_to_seed(words, pass, &mut s).map_err(|e| {
                ConfigError::BadMnemonic {
                    detail: e.to_string(),
                }
            })?;
            Ok(s)
        } else {
            let hex = self
                .seed_hex
                .as_deref()
                .ok_or_else(|| ConfigError::BadMnemonic {
                    detail: "no seed_hex, no mnemonic, and no encrypted_keystore_path set"
                        .to_string(),
                })?;
            decode_seed(hex, "validator.seed_hex")
        }
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
    /// M177: ceiling on concurrently-served inbound connections per accept loop
    /// (the P2P listener, and — when enabled — the metrics and RPC endpoints).
    /// Past this many live connections, a newly accepted socket is dropped
    /// immediately instead of spawning another handler task, so a connection
    /// flood can't exhaust file descriptors / memory. Applies per listener.
    /// Default 256 — generous for any real validator mesh (inbound is bounded by
    /// peer count), tight enough to bound abuse. Must be ≥ 1.
    #[serde(default = "default_max_inbound_connections")]
    pub max_inbound_connections: usize,
    /// M178: persist the **encryption-only** TLS cert + key (DER) at this path
    /// instead of regenerating an ephemeral pair each boot. Paired with
    /// `tls_key_path` (both-or-neither). When set and the files exist they are
    /// loaded; otherwise a fresh pair is generated once and written (key `0600`).
    /// A stable cert ⇒ a stable fingerprint operators can pin (trust-on-first-use
    /// survives restarts). Shared by the P2P encryption-only, metrics, and RPC
    /// TLS acceptors, so there is one fingerprint to pin. **Ignored** by the
    /// genesis-pinned mTLS path (`require_peer_certs`), whose credential is
    /// already deterministic from the validator seed. Unset (default) ⇒ the
    /// pre-M178 ephemeral behavior (fresh cert each boot).
    #[serde(default)]
    pub tls_cert_path: Option<String>,
    /// M178: companion private-key DER path for `tls_cert_path` (both-or-neither).
    #[serde(default)]
    pub tls_key_path: Option<String>,
}

/// M177: default inbound-connection ceiling (see
/// [`NetworkConfig::max_inbound_connections`]). A free function so `#[serde(default
/// = ...)]` can fill it when the key is absent from an older `[network]` section.
fn default_max_inbound_connections() -> usize {
    256
}

impl NetworkConfig {
    /// M177: reject a nonsensical inbound-connection ceiling. Zero would make the
    /// listener accept then immediately drop every connection (a permanent
    /// outage), so it is a config error rather than a silent no-op.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_inbound_connections == 0 {
            return Err(ConfigError::BadNetwork {
                detail: "max_inbound_connections must be >= 1".into(),
            });
        }
        // M178: the persistent-cert paths are both-or-neither (a cert without its
        // key, or vice versa, can neither load nor be written coherently).
        if self.tls_cert_path.is_some() != self.tls_key_path.is_some() {
            return Err(ConfigError::BadNetwork {
                detail: "tls_cert_path and tls_key_path must be set together".into(),
            });
        }
        Ok(())
    }

    /// M178: the `(cert, key)` persistence paths when both are configured.
    pub fn tls_persist_paths(&self) -> Option<(&str, &str)> {
        match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(c), Some(k)) => Some((c.as_str(), k.as_str())),
            _ => None,
        }
    }
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
            max_inbound_connections: default_max_inbound_connections(),
            tls_cert_path: None,
            tls_key_path: None,
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
    /// M158: optional bearer token gating scrapes. Empty (the default) ⇒ open (the
    /// pre-M158 behavior). When set, every request must carry `Authorization:
    /// Bearer <token>`, else `401` — for private deployments that don't want metrics
    /// readable by anyone who can reach the port.
    #[serde(default)]
    pub auth_token: String,
    /// M164: optional encryption-only TLS for this endpoint. Off by default (the
    /// M37 plaintext behavior); `true` ⇒ every accepted connection is wrapped in
    /// TLS using an ephemeral self-signed cert — mirrors `[network] enable_tls` for
    /// the P2P transport. A scraper must connect via TLS (e.g. trust-on-first-use
    /// or `AcceptAnyServerCert`); a plaintext probe of the port fails the TLS
    /// handshake. Ephemeral cert rotates on every restart, matching P2P.
    #[serde(default)]
    pub enable_tls: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9600".into(),
            auth_token: String::new(),
            enable_tls: false,
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
    /// M156: optional bearer token gating the *write* path (tx submission). Empty
    /// (the default) ⇒ no auth — the write path is open exactly as pre-M156. When set,
    /// a `POST` that submits a tx must carry `Authorization: Bearer <token>` (reads —
    /// every `GET`, plus the read-only `POST /batch`/`/validate` — stay public).
    #[serde(default)]
    pub auth_token: String,
    /// M164: optional encryption-only TLS for this endpoint. Off by default (the
    /// M53 plaintext behavior); `true` ⇒ every accepted connection is wrapped in
    /// TLS using an ephemeral self-signed cert. A submitter must connect via TLS;
    /// a plaintext probe fails the handshake. Composable with `auth_token` (TLS
    /// encrypts the bearer token so it's no longer sniffable). Ephemeral cert
    /// rotates on every restart, matching `[network] enable_tls`.
    #[serde(default)]
    pub enable_tls: bool,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:9700".into(),
            auth_token: String::new(),
            enable_tls: false,
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
    /// M143: nonce pipelining window — how many future nonces beyond an account's
    /// current one the pool will buffer so they can be laid into one block in nonce
    /// order. `0` ⇒ strict one-pending-tx-per-account (the behavior-preserving
    /// default; byte-identical to pre-M143).
    pub nonce_lookahead: usize,
    /// M144: when `true`, a tx at a `(author, nonce)` already held is admitted by
    /// *replacing* the incumbent iff it bids a strictly higher fee. `false` (the
    /// behavior-preserving default) keeps same-slot competitors coexisting.
    pub replace_by_fee: bool,
    /// M145: minimum fee increment a replace-by-fee bid must clear
    /// (`new.fee >= incumbent.fee + min_fee_bump`). Default `1` (= strictly higher,
    /// byte-identical to M144); larger dampens replacement churn; only used when
    /// `replace_by_fee` is on.
    pub min_fee_bump: u64,
    /// M157: minimum fee a tx must pay to be admitted to the mempool (anti-spam floor).
    /// `0` (the behavior-preserving default) ⇒ no floor. A local admission policy, not a
    /// consensus rule.
    pub min_fee: u64,
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
            nonce_lookahead: 0,
            replace_by_fee: false,
            min_fee_bump: 1,
            min_fee: 0,
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
            _ => {
                return Err(ConfigError::BadLogFormat {
                    value: self.format.clone(),
                })
            }
        }
        match self.rotation.as_str() {
            "daily" | "hourly" | "minutely" | "never" => {}
            _ => {
                return Err(ConfigError::BadLogRotation {
                    value: self.rotation.clone(),
                })
            }
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
    /// M187 (§6.2): emission time-decay half-life in blocks. Optional; defaults to
    /// `0` (decay disabled → mint factor `1.0`, unchanged behavior).
    #[serde(default)]
    pub decay_half_life_blocks: u64,
    pub slash_bps: u32,
    /// M235 (§10.11): max share of total voting power any single non-genesis validator may
    /// hold, in basis points (`3333` = 1/3). Optional; defaults to `0` (disabled — unchanged
    /// behavior). Set it (e.g. `3333`) to stop a large bond from a non-running key from
    /// acquiring ≥1/3 and wedging quorum. Must be ≤ 10000.
    #[serde(default)]
    pub max_validator_power_bps: u32,
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
            accounts.push((
                a.id,
                a.balance_micro,
                decode_pubkey(&a.pubkey_hex, "accounts.pubkey_hex")?,
            ));
        }
        let reviewers = self.reviewers.iter().map(|r| (r.id, r.weight)).collect();

        let mut seed_nodes = Vec::with_capacity(self.seed_nodes.len());
        for s in &self.seed_nodes {
            if s.embedding.len() != DIM {
                return Err(ConfigError::BadEmbedding {
                    got: s.embedding.len(),
                });
            }
            let mut emb: Embedding = [0.0f32; DIM];
            emb.copy_from_slice(&s.embedding);
            seed_nodes.push((emb, s.domain));
        }

        let mut validators = Vec::with_capacity(self.validators.len());
        for v in &self.validators {
            validators.push((
                v.id,
                decode_pubkey(&v.pubkey_hex, "validators.pubkey_hex")?,
                v.power,
            ));
        }

        // SEC-04: reject out-of-safe-range genesis params (same bounds the governance
        // path enforces). ΔK params are the validated defaults; base_emission/slash_bps
        // come from TOML.
        crate::validate_consensus_params(
            self.base_emission_micro,
            self.slash_bps,
            &DeltaKParams::default(),
        )
        .map_err(|e| ConfigError::BadGenesis {
            detail: e.to_string(),
        })?;
        // SEC-03: reject a genesis validator set whose total power exceeds the protocol
        // cap (so `total_power()`/`quorum()` never saturate at runtime).
        let total_power: u128 = validators.iter().map(|&(_, _, p)| p as u128).sum();
        if total_power > crate::validator::MAX_TOTAL_POWER as u128 {
            return Err(ConfigError::BadGenesis {
                detail: format!(
                    "total validator power {total_power} exceeds cap {}",
                    crate::validator::MAX_TOTAL_POWER
                ),
            });
        }

        // M235 (§10.11): the validator power share cap is a basis-points fraction (≤ 10000).
        if self.max_validator_power_bps > 10_000 {
            return Err(ConfigError::BadGenesis {
                detail: format!(
                    "max_validator_power_bps {} exceeds 10000 (100%)",
                    self.max_validator_power_bps
                ),
            });
        }

        Ok(Genesis {
            accounts,
            reviewers,
            seed_nodes,
            params: DeltaKParams::default(),
            base_emission_micro: self.base_emission_micro,
            decay_half_life_blocks: self.decay_half_life_blocks,
            slash_bps: self.slash_bps,
            max_validator_power_bps: self.max_validator_power_bps,
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
    // M177: reject a nonsensical inbound-connection ceiling.
    cfg.network.validate()?;
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
    s.parse().map_err(|_| ConfigError::BadAddr {
        value: s.to_string(),
    })
}

/// Strict lowercase/uppercase hex decode into a fixed-size array of `N` bytes.
fn decode_hex_n<const N: usize>(s: &str, field: &str) -> Result<[u8; N], ConfigError> {
    let bytes = decode_hex(s, field)?;
    if bytes.len() != N {
        return Err(ConfigError::BadHex {
            field: field.to_string(),
            detail: format!(
                "expected {N} bytes ({} hex chars), got {}",
                N * 2,
                bytes.len()
            ),
        });
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Decode a 64-hex ed25519 public key into its 32 bytes. Public so offline tooling
/// (e.g. `node inspect-tx --pubkey`) can reuse the exact decoder + `BadHex` errors the
/// genesis loader uses, keeping the key-hex format consistent across the CLI.
pub fn decode_pubkey(s: &str, field: &str) -> Result<PubKey, ConfigError> {
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
                PeerConfig {
                    id: 22,
                    addr: "127.0.0.1:9022".to_string(),
                },
                PeerConfig {
                    id: 23,
                    addr: "127.0.0.1:9023".to_string(),
                },
            ],
            genesis: "genesis.toml".to_string(),
            validator: Some(ValidatorKeyConfig {
                enabled: true,
                seed_hex: Some(hex(&demo_seed(21))),
                ..Default::default()
            }),
            consensus: ConsensusConfig::default(),
            network: NetworkConfig::default(),
            metrics: None,
            rpc: None,
            logging: None,
            mempool: MempoolConfig::default(),
            snapshot: None,
            checkpoint: None,
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
        assert_eq!(
            vc.keypair().unwrap().public(),
            crate::Keypair::from_seed(demo_seed(21)).public()
        );
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
        assert!(
            c.create_empty_blocks,
            "empty-block heartbeat on by default (M33 behavior)"
        );
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
        // M177: inbound-connection ceiling defaults to 256 (DoS bound).
        assert_eq!(n.max_inbound_connections, 256);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // tests mutate one field for clarity
    fn max_inbound_connections_defaults_parses_and_validates() {
        // M177: absent from an older `[network]` section ⇒ the 256 default
        // (serde `default =`), so legacy configs keep working.
        let legacy = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            enable_peer_exchange = false
        "#;
        let cfg: NodeConfig = toml::from_str(legacy).unwrap();
        assert_eq!(cfg.network.max_inbound_connections, 256);
        cfg.network.validate().expect("default is valid");

        // An explicit value parses and survives validation.
        let set = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [network]
            max_inbound_connections = 32
        "#;
        let cfg: NodeConfig = toml::from_str(set).unwrap();
        assert_eq!(cfg.network.max_inbound_connections, 32);
        cfg.network.validate().expect("32 is valid");

        // Zero is rejected (it would accept-then-drop every connection forever).
        let mut bad = NetworkConfig::default();
        bad.max_inbound_connections = 0;
        assert!(matches!(
            bad.validate(),
            Err(ConfigError::BadNetwork { .. })
        ));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // tests mutate one field for clarity
    fn tls_persist_paths_are_both_or_neither() {
        // M178: default ⇒ no persistence (None), validate ok.
        let n = NetworkConfig::default();
        assert!(n.tls_persist_paths().is_none());
        n.validate().expect("default valid");

        // Both set ⇒ Some pair, validate ok.
        let mut both = NetworkConfig::default();
        both.tls_cert_path = Some("/d/cert.der".into());
        both.tls_key_path = Some("/d/key.der".into());
        assert_eq!(
            both.tls_persist_paths(),
            Some(("/d/cert.der", "/d/key.der"))
        );
        both.validate().expect("both set valid");

        // Only one set ⇒ validation error (can't load/write coherently).
        let mut only_cert = NetworkConfig::default();
        only_cert.tls_cert_path = Some("/d/cert.der".into());
        assert!(matches!(
            only_cert.validate(),
            Err(ConfigError::BadNetwork { .. })
        ));
        let mut only_key = NetworkConfig::default();
        only_key.tls_key_path = Some("/d/key.der".into());
        assert!(matches!(
            only_key.validate(),
            Err(ConfigError::BadNetwork { .. })
        ));
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
        assert!(LoggingConfig {
            format: "text".into(),
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(LoggingConfig {
            format: "json".into(),
            ..Default::default()
        }
        .validate()
        .is_ok());
        assert!(matches!(
            LoggingConfig {
                format: "yaml".into(),
                ..Default::default()
            }
            .validate(),
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
        let path =
            std::env::temp_dir().join(format!("zhixing-m44-badfmt-{}.toml", std::process::id()));
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
                LoggingConfig {
                    rotation: r.into(),
                    ..Default::default()
                }
                .validate()
                .is_ok(),
                "rotation {r} should be valid"
            );
        }
        assert!(matches!(
            LoggingConfig {
                rotation: "weekly".into(),
                ..Default::default()
            }
            .validate(),
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
        let path =
            std::env::temp_dir().join(format!("zhixing-m45-badrot-{}.toml", std::process::id()));
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
        l.validate()
            .expect("text + json per-sink formats are valid");

        // empty per-sink formats pass validate (inherit `format`)
        LoggingConfig::default()
            .validate()
            .expect("empty per-sink formats inherit");
        // an unknown per-sink format is a typed rejection, like the scalar `format`
        assert!(matches!(
            LoggingConfig {
                stderr_format: "yaml".into(),
                ..Default::default()
            }
            .validate(),
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
    fn validator_signer_subtable_parses() {
        // M218: `[validator.signer]` configures the out-of-process remote signer; the
        // node then holds no key on disk. `timeout_ms` defaults to 500.
        let s = r#"
            genesis = "genesis.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./data/n21"
            [validator]
            enabled = true
            [validator.signer]
            socket = "/run/zhixing/signer.sock"
        "#;
        let cfg: NodeConfig = toml::from_str(s).unwrap();
        let vc = cfg.validator.as_ref().unwrap();
        assert!(vc.enabled);
        let sc = vc.signer.as_ref().expect("signer sub-table present");
        assert_eq!(sc.socket, "/run/zhixing/signer.sock");
        assert_eq!(sc.timeout_ms, 500, "timeout_ms defaults to 500");
        // explicit timeout_ms overrides the default
        let s2 = r#"
            genesis = "g.toml"
            [node]
            id = 21
            listen = "0.0.0.0:9021"
            data_dir = "./d"
            [validator]
            enabled = true
            [validator.signer]
            socket = "/s.sock"
            timeout_ms = 250
        "#;
        let cfg2: NodeConfig = toml::from_str(s2).unwrap();
        assert_eq!(cfg2.validator.unwrap().signer.unwrap().timeout_ms, 250);
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
            reviewers: (10..=12)
                .map(|id| ReviewerConfig { id, weight: 1.0 })
                .collect(),
            seed_nodes: vec![SeedNodeConfig {
                embedding: unit_vec(0),
                domain: 0,
            }],
            base_emission_micro: 8 * crate::MICRO,
            decay_half_life_blocks: 0,
            slash_bps: 10_000,
            max_validator_power_bps: 0,
            timestamp_days: 0.0,
            validators: (21..=24)
                .map(|id| ValidatorConfig {
                    id,
                    pubkey_hex: hex(&kp(id).public()),
                    power: 1,
                })
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
            keys: (21..=24)
                .map(|id| KeyEntry {
                    id,
                    seed_hex: hex(&demo_seed(id)),
                })
                .collect(),
        };
        let seeds = ks.to_seeds().unwrap();
        assert_eq!(seeds.len(), 4);
        assert_eq!(seeds[&21], demo_seed(21));
        assert_eq!(seeds[&24], demo_seed(24));
    }

    #[test]
    fn bad_hex_and_addr_are_typed_errors() {
        let bad_pk = AccountConfig {
            id: 1,
            balance_micro: 0,
            pubkey_hex: "zz".to_string(),
        };
        let gc = GenesisConfig {
            accounts: vec![bad_pk],
            reviewers: vec![],
            seed_nodes: vec![],
            base_emission_micro: 0,
            decay_half_life_blocks: 0,
            slash_bps: 0,
            max_validator_power_bps: 0,
            timestamp_days: 0.0,
            validators: vec![],
            params: None,
        };
        assert!(matches!(gc.to_genesis(), Err(ConfigError::BadHex { .. })));

        // wrong length hex
        let short = KeystoreConfig {
            keys: vec![KeyEntry {
                id: 1,
                seed_hex: "abcd".to_string(),
            }],
        };
        assert!(matches!(short.to_seeds(), Err(ConfigError::BadHex { .. })));

        let pc = PeerConfig {
            id: 1,
            addr: "not-an-addr".to_string(),
        };
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
        let by_id: std::collections::BTreeMap<u64, PubKey> = genesis
            .validators
            .iter()
            .map(|(id, pk, _)| (*id, *pk))
            .collect();
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
        let vc = ValidatorKeyConfig {
            enabled: true,
            seed_hex: Some(hex(&demo_seed(99))),
            ..Default::default()
        };
        assert_ne!(vc.keypair().unwrap().public(), genesis_pk);
        // the matching seed agrees
        let ok = ValidatorKeyConfig {
            enabled: true,
            seed_hex: Some(hex(&demo_seed(21))),
            ..Default::default()
        };
        assert_eq!(ok.keypair().unwrap().public(), genesis_pk);
    }

    #[test]
    fn validator_key_config_accepts_mnemonic_field() {
        // M169: the BIP-39 form derives a keypair via PBKDF2-HMAC-SHA512
        // and the result must match the same seed as the legacy `seed_hex`
        // form would produce. We use the same 12-word vector that the
        // crypto.rs tests exercise so a regression there surfaces here too.
        let seed_a = {
            let mut s = [0u8; 32];
            crate::crypto::mnemonic_to_seed(
                "abandon abandon abandon abandon abandon abandon \
                 abandon abandon abandon abandon abandon about",
                "",
                &mut s,
            )
            .unwrap();
            s
        };
        let via_mnemonic = ValidatorKeyConfig {
            enabled: true,
            seed_hex: None,
            mnemonic: Some(
                "abandon abandon abandon abandon abandon abandon \
                 abandon abandon abandon abandon abandon about"
                    .to_string(),
            ),
            passphrase: None,
            encrypted_keystore_path: None,
            pending: None,
            signer: None,
        }
        .keypair()
        .unwrap();
        let via_seed_hex = ValidatorKeyConfig {
            enabled: true,
            seed_hex: Some(hex(&seed_a)),
            mnemonic: None,
            passphrase: None,
            encrypted_keystore_path: None,
            pending: None,
            signer: None,
        }
        .keypair()
        .unwrap();
        assert_eq!(via_mnemonic.public(), via_seed_hex.public());
    }

    #[test]
    fn validator_key_config_rejects_invalid_mnemonic() {
        // Bad checksum (last word flipped from "about" to "ability") ⇒
        // a clear `BadMnemonic` error rather than a panic. The M169
        // `keypair()` calls `validate_mnemonic` first so the typo is
        // caught before any derivation runs.
        let bad = ValidatorKeyConfig {
            enabled: true,
            seed_hex: None,
            mnemonic: Some(
                "abandon abandon abandon abandon abandon abandon \
                 abandon abandon abandon abandon abandon ability"
                    .to_string(),
            ),
            passphrase: None,
            encrypted_keystore_path: None,
            pending: None,
            signer: None,
        };
        match bad.keypair() {
            Err(ConfigError::BadMnemonic { detail }) => {
                assert!(detail.contains("checksum"), "{detail}");
            }
            other => panic!("expected BadMnemonic, got {other:?}"),
        }
    }

    #[test]
    fn validator_key_config_requires_seed_or_mnemonic() {
        // Bare `[validator]` table (no `seed_hex`, no `mnemonic`) ⇒ error
        // explaining the omission, not a panic. This is the path an
        // operator who forgot to configure the key would hit at startup.
        let empty = ValidatorKeyConfig::default();
        match empty.keypair() {
            Err(ConfigError::BadMnemonic { detail }) => {
                assert!(detail.contains("no seed_hex"), "{detail}");
            }
            other => panic!("expected BadMnemonic, got {other:?}"),
        }
    }

    #[test]
    fn encrypted_keystore_path_requires_password() {
        // M175: `keypair()` (the password-less entry point) must refuse an
        // encrypted keystore rather than silently falling through to another
        // field — the secret cannot be resolved without a password.
        let vc = ValidatorKeyConfig {
            enabled: true,
            seed_hex: None,
            mnemonic: None,
            passphrase: None,
            encrypted_keystore_path: Some("/nonexistent/keystore.json".to_string()),
            pending: None,
            signer: None,
        };
        match vc.keypair() {
            Err(ConfigError::KeystoreDecrypt { detail }) => {
                assert!(detail.contains("password"), "{detail}");
            }
            other => panic!("expected KeystoreDecrypt, got {other:?}"),
        }
    }

    #[test]
    fn encrypted_keystore_round_trips_through_config() {
        // M175: write an encrypted keystore to a temp file, point a
        // `ValidatorKeyConfig` at it, and confirm `keypair_with_password`
        // recovers the same signing key the seed produces directly — and that
        // a wrong password fails.
        let seed = [0x3cu8; 32];
        let ks = crate::keystore::encrypt_seed(&seed, "s3cret-pass").unwrap();
        let json = crate::keystore::to_json(&ks).unwrap();
        let dir = std::env::temp_dir().join(format!("zx-ks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keystore.json");
        std::fs::write(&path, json).unwrap();

        let vc = ValidatorKeyConfig {
            enabled: true,
            seed_hex: None,
            mnemonic: None,
            passphrase: None,
            encrypted_keystore_path: Some(path.to_string_lossy().to_string()),
            pending: None,
            signer: None,
        };
        let from_keystore = vc.keypair_with_password("s3cret-pass").unwrap();
        let direct = crate::Keypair::from_seed(seed);
        assert_eq!(from_keystore.public(), direct.public());

        // Wrong password ⇒ KeystoreDecrypt (MAC mismatch underneath).
        match vc.keypair_with_password("wrong") {
            Err(ConfigError::KeystoreDecrypt { .. }) => {}
            other => panic!("expected KeystoreDecrypt, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keystore_takes_precedence_over_mnemonic_and_seed_hex() {
        // M175: when all three key sources are set, the encrypted keystore
        // wins in `keypair_with_password`. We prove this by pointing at a
        // keystore holding a DIFFERENT seed than the seed_hex/mnemonic would
        // produce, and checking the resolved key matches the keystore's seed.
        let keystore_seed = [0xAAu8; 32];
        let ks = crate::keystore::encrypt_seed(&keystore_seed, "pw").unwrap();
        let json = crate::keystore::to_json(&ks).unwrap();
        let dir = std::env::temp_dir().join(format!("zx-ksprec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keystore.json");
        std::fs::write(&path, json).unwrap();

        let other_seed = [0xBBu8; 32]; // what seed_hex alone would yield
        let vc = ValidatorKeyConfig {
            enabled: true,
            seed_hex: Some(hex(&other_seed)),
            mnemonic: None,
            passphrase: None,
            encrypted_keystore_path: Some(path.to_string_lossy().to_string()),
            pending: None,
            signer: None,
        };
        let resolved = vc.keypair_with_password("pw").unwrap();
        assert_eq!(
            resolved.public(),
            crate::Keypair::from_seed(keystore_seed).public(),
            "keystore seed must win over seed_hex"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keypair_with_password_falls_back_to_seed_hex_when_no_keystore() {
        // M175: with no keystore path, `keypair_with_password` behaves exactly
        // like `keypair` (password ignored) — proving a single call site can
        // route every config shape through the password-aware method.
        let seed = [0x44u8; 32];
        let vc = ValidatorKeyConfig {
            enabled: true,
            seed_hex: Some(hex(&seed)),
            mnemonic: None,
            passphrase: None,
            encrypted_keystore_path: None,
            pending: None,
            signer: None,
        };
        let a = vc.keypair().unwrap();
        let b = vc.keypair_with_password("ignored").unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(a.public(), crate::Keypair::from_seed(seed).public());
    }

    #[test]
    fn pending_validator_key_parses_and_resolves() {
        // M180: a nested `[validator.pending]` table resolves to its own keypair
        // via the same sourcing as the active key (here seed_hex). Default (no
        // `pending`) is None, preserving M179 behavior.
        let seed = [0x7eu8; 32];
        let toml = format!(
            "genesis = \"g.toml\"\n[node]\nid = 21\nlisten = \"0.0.0.0:9021\"\ndata_dir = \"./d\"\n\
             [validator]\nenabled = true\nseed_hex = \"{}\"\n\
             [validator.pending]\nseed_hex = \"{}\"\n",
            hex(&[0x11u8; 32]),
            hex(&seed),
        );
        let cfg: NodeConfig = toml::from_str(&toml).unwrap();
        let vc = cfg.validator.as_ref().unwrap();
        let pending = vc.pending.as_deref().expect("pending present");
        assert_eq!(
            pending.keypair().unwrap().public(),
            crate::Keypair::from_seed(seed).public()
        );
        // A config without [validator.pending] has None.
        let bare = "genesis = \"g.toml\"\n[node]\nid = 21\nlisten = \"0.0.0.0:9021\"\ndata_dir = \"./d\"\n[validator]\nenabled = true\nseed_hex = \"0000000000000000000000000000000000000000000000000000000000000001\"\n";
        let cfg: NodeConfig = toml::from_str(bare).unwrap();
        assert!(cfg.validator.unwrap().pending.is_none());
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
                .map(|id| AccountConfig {
                    id,
                    balance_micro: 30 * crate::MICRO,
                    pubkey_hex: hex(&kp(id).public()),
                })
                .collect(),
            reviewers: (10..=12)
                .map(|id| ReviewerConfig { id, weight: 1.0 })
                .collect(),
            seed_nodes: vec![SeedNodeConfig {
                embedding: unit_vec(0),
                domain: 0,
            }],
            base_emission_micro: 8 * crate::MICRO,
            decay_half_life_blocks: 0,
            slash_bps: 10_000,
            max_validator_power_bps: 0,
            timestamp_days: 0.0,
            validators: (21..=24)
                .map(|id| ValidatorConfig {
                    id,
                    pubkey_hex: hex(&kp(id).public()),
                    power: 1,
                })
                .collect(),
            params: None,
        };
        std::fs::write(
            dir.join("genesis.toml"),
            toml::to_string_pretty(&genesis).unwrap(),
        )
        .unwrap();

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
                    .map(|&p| PeerConfig {
                        id: p,
                        addr: format!("127.0.0.1:{}", 9000 + p),
                    })
                    .collect(),
                genesis: "testnet/genesis.toml".to_string(),
                validator: Some(ValidatorKeyConfig {
                    enabled: true,
                    seed_hex: Some(hex(&demo_seed(id))),
                    ..Default::default()
                }),
                consensus: ConsensusConfig::default(),
                network: NetworkConfig::default(),
                metrics: None,
                rpc: None,
                logging: None,
                mempool: MempoolConfig::default(),
                snapshot: None,
                checkpoint: None,
            };
            std::fs::write(
                dir.join(format!("node{id}.toml")),
                toml::to_string_pretty(&cfg).unwrap(),
            )
            .unwrap();
        }

        // Remove the now-obsolete pre-M33 keystore if a prior run left one.
        let _ = std::fs::remove_file(dir.join("keystore.toml"));
    }

    #[test]
    fn mempool_config_nonce_lookahead_defaults_zero() {
        // M143: a `[mempool]` section without `nonce_lookahead` (or no section at all)
        // defaults to 0 — strict one-pending-tx-per-account, behavior-preserving.
        let toml = r#"
            genesis = "genesis.toml"

            [node]
            id = 21
            listen = "127.0.0.1:9021"
            data_dir = "/tmp/x"

            [mempool]
            capacity = 100
        "#;
        let cfg: NodeConfig = toml::from_str(toml).expect("parse mempool config");
        assert_eq!(cfg.mempool.capacity, 100);
        assert_eq!(cfg.mempool.nonce_lookahead, 0);
        assert_eq!(MempoolConfig::default().nonce_lookahead, 0);
    }

    #[test]
    fn mempool_config_replace_by_fee_defaults_false() {
        // M144: a `[mempool]` without `replace_by_fee` defaults to false (off).
        let toml = r#"
            genesis = "genesis.toml"

            [node]
            id = 21
            listen = "127.0.0.1:9021"
            data_dir = "/tmp/x"

            [mempool]
            capacity = 100
        "#;
        let cfg: NodeConfig = toml::from_str(toml).expect("parse mempool config");
        assert!(!cfg.mempool.replace_by_fee);
        assert!(!MempoolConfig::default().replace_by_fee);
        // M145: min_fee_bump defaults to 1 (= strictly higher).
        assert_eq!(cfg.mempool.min_fee_bump, 1);
        assert_eq!(MempoolConfig::default().min_fee_bump, 1);
        // M157: min_fee defaults to 0 (no floor).
        assert_eq!(cfg.mempool.min_fee, 0);
        assert_eq!(MempoolConfig::default().min_fee, 0);
    }

    #[test]
    fn snapshot_config_m140_two_field_toml_parses() {
        // M141: an M140-era `[snapshot]` with only `path` + `header_hex` must still
        // deserialize — the two new fields are `#[serde(default)]` (None / empty).
        let toml = r#"
            genesis = "genesis.toml"

            [node]
            id = 21
            listen = "127.0.0.1:9021"
            data_dir = "/tmp/x"

            [snapshot]
            path = "snap.bin"
            header_hex = "abcd"
        "#;
        let cfg: NodeConfig = toml::from_str(toml).expect("parse M140 snapshot config");
        let s = cfg.snapshot.expect("snapshot section present");
        assert_eq!(s.path, "snap.bin");
        assert_eq!(s.header_hex, "abcd");
        assert!(s.cert_hex.is_none(), "cert_hex defaults to None");
        assert!(
            s.trusted_validators.is_empty(),
            "trusted_validators defaults to empty"
        );
        // And an M135-only config (no cert, no set) is coherent.
        assert!(s.validate().is_ok());
    }

    #[test]
    fn snapshot_config_coherence_validate() {
        // M141: `cert_hex` and `trusted_validators` must be given together (XOR is an error).
        let one_validator = vec![ValidatorConfig {
            id: 21,
            pubkey_hex: hex(&demo_seed(21)), // shape only; validate() doesn't decode
            power: 1,
        }];
        // Both present → ok.
        let both = SnapshotBootConfig {
            path: "p".into(),
            header_hex: "ab".into(),
            cert_hex: Some("cd".into()),
            trusted_validators: one_validator.clone(),
        };
        assert!(both.validate().is_ok());
        // Neither present → ok (M135-only).
        let neither = SnapshotBootConfig {
            path: "p".into(),
            header_hex: "ab".into(),
            cert_hex: None,
            trusted_validators: vec![],
        };
        assert!(neither.validate().is_ok());
        // Cert without a set → error.
        let cert_only = SnapshotBootConfig {
            path: "p".into(),
            header_hex: "ab".into(),
            cert_hex: Some("cd".into()),
            trusted_validators: vec![],
        };
        assert!(matches!(
            cert_only.validate(),
            Err(ConfigError::BadSnapshot { .. })
        ));
        // Set without a cert → error.
        let set_only = SnapshotBootConfig {
            path: "p".into(),
            header_hex: "ab".into(),
            cert_hex: None,
            trusted_validators: one_validator,
        };
        assert!(matches!(
            set_only.validate(),
            Err(ConfigError::BadSnapshot { .. })
        ));
    }

    #[test]
    fn checkpoint_config_decodes_and_rejects_bad_input() {
        // M154: a well-formed checkpoint decodes to (head, set); bad hex / empty set reject.
        let ok = CheckpointConfig {
            height: 7,
            head_hex: hex(&[9u8; 32]),
            validators: vec![ValidatorConfig {
                id: 21,
                pubkey_hex: hex(&demo_seed(21)),
                power: 1,
            }],
        };
        let (head, set) = ok.decode().expect("valid checkpoint decodes");
        assert_eq!(head, [9u8; 32]);
        assert_eq!(set.len(), 1);

        // Empty validator set → error.
        let empty = CheckpointConfig {
            height: 7,
            head_hex: hex(&[9u8; 32]),
            validators: vec![],
        };
        assert!(matches!(
            empty.decode(),
            Err(ConfigError::BadSnapshot { .. })
        ));

        // Malformed head hex → BadHex.
        let bad_head = CheckpointConfig {
            height: 7,
            head_hex: "zz".into(),
            validators: ok.validators.clone(),
        };
        assert!(matches!(bad_head.decode(), Err(ConfigError::BadHex { .. })));
    }
}
