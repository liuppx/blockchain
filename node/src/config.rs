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
}

impl Default for NetworkConfig {
    fn default() -> Self {
        // These values MUST equal the pre-M36 daemon constants (single source of
        // truth now lives here): ANNOUNCE_SECS=2 (→ 2000 ms), STARTUP_DELAY=1000.
        Self {
            announce_interval_ms: 2000,
            startup_delay_ms: 1000,
        }
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
    toml::from_str(&s).map_err(|e| ConfigError::Toml(e.to_string()))
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

fn decode_seed(s: &str, field: &str) -> Result<[u8; 32], ConfigError> {
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
            };
            std::fs::write(dir.join(format!("node{id}.toml")), toml::to_string_pretty(&cfg).unwrap()).unwrap();
        }

        // Remove the now-obsolete pre-M33 keystore if a prior run left one.
        let _ = std::fs::remove_file(dir.join("keystore.toml"));
    }
}
