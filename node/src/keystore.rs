//! M175: an encrypted validator keystore — the audit-prep answer to "validator
//! private keys sit in plaintext `seed_hex` / `mnemonic` TOML lines".
//!
//! The on-disk format is a deliberate subset of the Ethereum **Web3 Secret
//! Storage definition, version 3** (the Geth/Parity keystore JSON): a
//! password-derived key via **scrypt**, the secret encrypted under
//! **AES-128-CTR**, and a **Keccak-256 MAC** over the second half of the
//! derived key concatenated with the ciphertext. We pick this format — rather
//! than inventing one — precisely because external auditors already know it:
//! the KDF choice, the MAC construction, and the parameter ranges are the same
//! ones every Ethereum client ships and every audit of those clients has
//! covered. The ONLY deviation is the plaintext: here it is a 32-byte ed25519
//! seed (what [`crate::Keypair::from_seed`] consumes), not a secp256k1 private
//! key. Nothing about the envelope cares what the 32 bytes mean.
//!
//! Threat model: this protects a key *at rest* (a stolen config dir / backup /
//! laptop) behind an operator password that never touches disk. It does NOT
//! protect a running process's memory, and it is only as strong as the
//! password — see the scrypt cost note on [`Params`]. The password is read
//! from stdin at node startup, never from the config file.
//!
//! All primitives are RustCrypto crates pinned to the same `digest 0.10` /
//! `cipher 0.4` generation as the node's existing `sha2` / `hmac` / `pbkdf2`,
//! so no second trait generation enters the dependency tree.

use aes::cipher::{KeyIvInit, StreamCipher};
use aes::Aes128;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

/// AES-128 in CTR mode with a 128-bit big-endian counter — the exact cipher
/// Web3SS v3 names `"aes-128-ctr"`.
type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// scrypt cost parameters. The defaults match the Web3SS "standard" profile
/// (`N = 2^18`, `r = 8`, `p = 1`) — the same values Geth writes by default.
///
/// `N = 2^18` (262144) is the 2017-era standard and is deliberately chosen for
/// audit-baseline parity rather than being the strongest possible today; a
/// validator key is decrypted once per process start, so the operator can raise
/// `log_n` in the JSON by hand (the file is plain text) without any code change.
const SCRYPT_LOG_N: u8 = 18;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
/// Derived-key length: 32 bytes = 16 (AES-128 key) + 16 (MAC-prefix material).
const DKLEN: usize = 32;

/// Web3SS v3 version tag.
const VERSION: u32 = 3;

/// Top-level keystore JSON document (Web3SS v3 subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedKeystore {
    pub crypto: CryptoJson,
    /// A UUID-v4 string. Purely metadata — not authenticated, not required for
    /// decryption. Formatted by hand from CSPRNG bytes (no `uuid` crate).
    pub id: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CryptoJson {
    /// Always `"scrypt"`.
    pub kdf: String,
    pub kdfparams: KdfParams,
    /// Always `"aes-128-ctr"`.
    pub cipher: String,
    pub cipherparams: CipherParams,
    /// Hex of the AES-128-CTR ciphertext (32 bytes for a seed).
    pub ciphertext: String,
    /// Hex of `Keccak256(derived_key[16..32] ‖ ciphertext)` (32 bytes).
    pub mac: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdfParams {
    pub dklen: u32,
    /// scrypt `N` (a power of two). Stored as the actual value (e.g. 262144),
    /// Web3SS-style — we convert to/from `log_n` internally.
    pub n: u64,
    pub r: u32,
    pub p: u32,
    /// Hex of the scrypt salt (16 bytes).
    pub salt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CipherParams {
    /// Hex of the AES-CTR initial counter / IV (16 bytes).
    pub iv: String,
}

/// Errors from building or opening a keystore. All variants are deliberately
/// coarse on the decryption side: a caller should not learn *why* a password
/// failed (wrong password vs. tampered file) beyond "it did not open".
#[derive(Debug)]
pub enum KeystoreError {
    /// The JSON did not parse or did not match the v3 schema.
    BadJson(String),
    /// A declared field (`kdf`/`cipher`/`version`) held an unsupported value.
    Unsupported(String),
    /// A hex field was malformed or the wrong length.
    BadHex(String),
    /// scrypt rejected the parameters (e.g. an absurd `n`).
    BadParams(String),
    /// The MAC did not match: wrong password, or the ciphertext/params were
    /// tampered with. The two cases are intentionally indistinguishable.
    MacMismatch,
    /// The CSPRNG failed while generating a salt/IV/id (encryption only).
    Rng(String),
}

impl std::fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeystoreError::BadJson(e) => write!(f, "keystore json: {e}"),
            KeystoreError::Unsupported(e) => write!(f, "keystore unsupported: {e}"),
            KeystoreError::BadHex(e) => write!(f, "keystore bad hex: {e}"),
            KeystoreError::BadParams(e) => write!(f, "keystore bad scrypt params: {e}"),
            KeystoreError::MacMismatch => {
                write!(f, "keystore MAC mismatch (wrong password or corrupt file)")
            }
            KeystoreError::Rng(e) => write!(f, "keystore rng: {e}"),
        }
    }
}

impl std::error::Error for KeystoreError {}

/// Encrypt a 32-byte seed under `password`, returning a serializable v3
/// keystore. A fresh random salt, IV, and id are drawn from the OS CSPRNG, so
/// two calls with the same seed and password produce different ciphertexts.
pub fn encrypt_seed(seed: &[u8; 32], password: &str) -> Result<EncryptedKeystore, KeystoreError> {
    let mut salt = [0u8; 16];
    let mut iv = [0u8; 16];
    let mut id_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt).map_err(|e| KeystoreError::Rng(e.to_string()))?;
    getrandom::getrandom(&mut iv).map_err(|e| KeystoreError::Rng(e.to_string()))?;
    getrandom::getrandom(&mut id_bytes).map_err(|e| KeystoreError::Rng(e.to_string()))?;

    let derived = scrypt_derive(password.as_bytes(), &salt)?;

    // AES-128-CTR over the seed, keyed by the first half of the derived key.
    let mut ciphertext = *seed;
    aes_128_ctr(&derived[..16], &iv, &mut ciphertext)?;

    // MAC = Keccak256(derived[16..32] ‖ ciphertext) — Web3SS construction.
    let mac = keystore_mac(&derived[16..32], &ciphertext);

    Ok(EncryptedKeystore {
        crypto: CryptoJson {
            kdf: "scrypt".to_string(),
            kdfparams: KdfParams {
                dklen: DKLEN as u32,
                n: 1u64 << SCRYPT_LOG_N,
                r: SCRYPT_R,
                p: SCRYPT_P,
                salt: hex(&salt),
            },
            cipher: "aes-128-ctr".to_string(),
            cipherparams: CipherParams { iv: hex(&iv) },
            ciphertext: hex(&ciphertext),
            mac: hex(&mac),
        },
        id: format_uuid_v4(&id_bytes),
        version: VERSION,
    })
}

/// Serialize an [`EncryptedKeystore`] to pretty JSON (the on-disk form).
pub fn to_json(ks: &EncryptedKeystore) -> Result<String, KeystoreError> {
    serde_json::to_string_pretty(ks).map_err(|e| KeystoreError::BadJson(e.to_string()))
}

/// Parse and structurally validate a keystore JSON document *without*
/// decrypting it (no password needed). Checks the schema, the supported
/// `version`/`kdf`/`cipher`/`dklen`, that every hex field is well-formed and
/// correctly sized, and that the scrypt cost parameters are sane and bounded.
/// Used by `check-config`'s dry-run, which must not prompt for a password.
/// Returns the parsed document on success.
pub fn parse_keystore(keystore_json: &str) -> Result<EncryptedKeystore, KeystoreError> {
    let ks: EncryptedKeystore =
        serde_json::from_str(keystore_json).map_err(|e| KeystoreError::BadJson(e.to_string()))?;
    validate(&ks)?;
    Ok(ks)
}

/// The decoded + validated pieces of a keystore, ready for the KDF/MAC/cipher
/// steps. Produced once by [`validate`] so [`decrypt_seed`] does not re-parse
/// what [`parse_keystore`] already parsed.
struct Validated {
    salt: Vec<u8>,
    iv: [u8; 16],
    ciphertext: [u8; 32],
    mac: [u8; 32],
    log_n: u8,
    r: u32,
    p: u32,
}

/// Shared schema + hex + scrypt-cost validation for [`parse_keystore`] and
/// [`decrypt_seed`]. Does no cryptography. Returns the decoded fields so the
/// caller need not parse the hex a second time.
fn validate(ks: &EncryptedKeystore) -> Result<Validated, KeystoreError> {
    if ks.version != VERSION {
        return Err(KeystoreError::Unsupported(format!(
            "version {} (only v{VERSION} supported)",
            ks.version
        )));
    }
    if ks.crypto.kdf != "scrypt" {
        return Err(KeystoreError::Unsupported(format!(
            "kdf `{}` (only scrypt supported)",
            ks.crypto.kdf
        )));
    }
    if ks.crypto.cipher != "aes-128-ctr" {
        return Err(KeystoreError::Unsupported(format!(
            "cipher `{}` (only aes-128-ctr supported)",
            ks.crypto.cipher
        )));
    }
    let kp = &ks.crypto.kdfparams;
    if kp.dklen as usize != DKLEN {
        return Err(KeystoreError::Unsupported(format!(
            "dklen {} (only {DKLEN} supported)",
            kp.dklen
        )));
    }
    let salt = decode_hex_vec(&kp.salt, "salt")?;
    let iv = decode_hex_n::<16>(&ks.crypto.cipherparams.iv, "iv")?;
    let ciphertext = decode_hex_n::<32>(&ks.crypto.ciphertext, "ciphertext")?;
    let mac = decode_hex_n::<32>(&ks.crypto.mac, "mac")?;
    // Full scrypt-cost validation (n power-of-two → log_n, plus r/p ranges and
    // a memory ceiling) so a tampered or malformed keystore is rejected HERE —
    // by the dry-run too — rather than surfacing as a start-time `Params::new`
    // error or, worse, an unbounded KDF allocation. See `validate_scrypt_cost`.
    let (log_n, r, p) = validate_scrypt_cost(kp.n, kp.r, kp.p)?;
    Ok(Validated {
        salt,
        iv,
        ciphertext,
        mac,
        log_n,
        r,
        p,
    })
}

/// Decrypt a keystore JSON document under `password`, returning the 32-byte
/// seed. Verifies the MAC *before* decrypting, so a wrong password or a
/// tampered ciphertext both fail with [`KeystoreError::MacMismatch`] and never
/// yield key material.
pub fn decrypt_seed(keystore_json: &str, password: &str) -> Result<[u8; 32], KeystoreError> {
    let ks: EncryptedKeystore =
        serde_json::from_str(keystore_json).map_err(|e| KeystoreError::BadJson(e.to_string()))?;
    let v = validate(&ks)?;

    let derived = scrypt_derive_params(password.as_bytes(), &v.salt, v.log_n, v.r, v.p)?;

    // Verify the MAC over the (still-encrypted) ciphertext BEFORE decrypting.
    let mac = keystore_mac(&derived[16..32], &v.ciphertext);
    if !ct_eq(&mac, &v.mac) {
        return Err(KeystoreError::MacMismatch);
    }

    let mut plaintext = v.ciphertext;
    aes_128_ctr(&derived[..16], &v.iv, &mut plaintext)?;
    Ok(plaintext)
}

// ---- primitives -------------------------------------------------------------

/// scrypt with this module's standard parameters.
fn scrypt_derive(password: &[u8], salt: &[u8]) -> Result<[u8; DKLEN], KeystoreError> {
    scrypt_derive_params(password, salt, SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P)
}

/// scrypt with explicit parameters (used on decode, where they come from the
/// file). `log_n`/`r`/`p` are validated by `scrypt::Params::new`.
fn scrypt_derive_params(
    password: &[u8],
    salt: &[u8],
    log_n: u8,
    r: u32,
    p: u32,
) -> Result<[u8; DKLEN], KeystoreError> {
    let params = scrypt::Params::new(log_n, r, p, DKLEN)
        .map_err(|e| KeystoreError::BadParams(e.to_string()))?;
    let mut out = [0u8; DKLEN];
    scrypt::scrypt(password, salt, &params, &mut out)
        .map_err(|e| KeystoreError::BadParams(e.to_string()))?;
    Ok(out)
}

/// AES-128-CTR, in place. `key` must be 16 bytes, `iv` 16 bytes. CTR is a
/// stream cipher, so the same call both encrypts and decrypts.
fn aes_128_ctr(key: &[u8], iv: &[u8; 16], buf: &mut [u8]) -> Result<(), KeystoreError> {
    if key.len() != 16 {
        return Err(KeystoreError::BadParams(format!(
            "aes key is {} bytes, expected 16",
            key.len()
        )));
    }
    let mut cipher = Aes128Ctr::new(key.into(), iv.into());
    cipher.apply_keystream(buf);
    Ok(())
}

/// The Web3SS MAC: `Keccak256(mac_prefix ‖ ciphertext)`.
fn keystore_mac(mac_prefix: &[u8], ciphertext: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(mac_prefix);
    h.update(ciphertext);
    h.finalize().into()
}

/// scrypt `N` → `log_n`, rejecting a non-power-of-two or out-of-range value.
fn checked_log_n(n: u64) -> Result<u8, KeystoreError> {
    if n < 2 || !n.is_power_of_two() {
        return Err(KeystoreError::BadParams(format!(
            "n = {n} is not a power of two ≥ 2"
        )));
    }
    Ok(n.trailing_zeros() as u8)
}

/// Upper bound on the scrypt working-memory estimate (`128 · N · r` bytes).
/// 1 GiB comfortably admits the Web3SS standard profile (N=2^18, r=8 ⇒ 256 MiB)
/// with headroom, while refusing a tampered keystore that declares, say,
/// N=2^40: without this cap such a file passes `check-config` and then OOMs the
/// validator at `node run` (the at-rest-tamper DoS this feature is meant to
/// resist). The cap is a defensive ceiling, not a recommended value.
const MAX_SCRYPT_MEM_BYTES: u128 = 1 << 30;
/// Upper bound on scrypt `p`, bounding total work (`∝ N·r·p`) independently of
/// the memory ceiling.
const MAX_SCRYPT_P: u32 = 16;

/// Validate and normalize the scrypt cost parameters from a keystore file.
/// Returns `(log_n, r, p)` for [`scrypt::Params::new`]. Rejects a non-power-of-
/// two `n`, `r`/`p` of zero, a `p` beyond [`MAX_SCRYPT_P`], and any `(n, r)`
/// whose working-memory estimate exceeds [`MAX_SCRYPT_MEM_BYTES`].
fn validate_scrypt_cost(n: u64, r: u32, p: u32) -> Result<(u8, u32, u32), KeystoreError> {
    let log_n = checked_log_n(n)?;
    if r == 0 {
        return Err(KeystoreError::BadParams("r must be ≥ 1".to_string()));
    }
    if p == 0 || p > MAX_SCRYPT_P {
        return Err(KeystoreError::BadParams(format!(
            "p = {p} out of range (1..={MAX_SCRYPT_P})"
        )));
    }
    // 128 · N · r — the scrypt working-set size. Done in u128 so the product
    // never overflows regardless of the (already power-of-two-bounded) n.
    let mem = 128u128 * (n as u128) * (r as u128);
    if mem > MAX_SCRYPT_MEM_BYTES {
        return Err(KeystoreError::BadParams(format!(
            "scrypt memory 128·N·r = {mem} bytes exceeds the {MAX_SCRYPT_MEM_BYTES}-byte cap \
             (n={n}, r={r})"
        )));
    }
    Ok((log_n, r, p))
}

/// Constant-time 32-byte comparison, so MAC verification does not leak via
/// timing. (The MAC is a public value, but matching the discipline of the rest
/// of the node's secret-adjacent comparisons costs nothing.)
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---- hex + uuid helpers -----------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    crate::hash::hex(bytes)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode a hex string of any even length into bytes (used for the salt, whose
/// length is operator-chosen in principle).
fn decode_hex_vec(s: &str, field: &str) -> Result<Vec<u8>, KeystoreError> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(KeystoreError::BadHex(format!("{field}: odd length")));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = hex_nibble(pair[0]).ok_or_else(|| KeystoreError::BadHex(field.to_string()))?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| KeystoreError::BadHex(field.to_string()))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// Decode a hex string into exactly `N` bytes.
fn decode_hex_n<const N: usize>(s: &str, field: &str) -> Result<[u8; N], KeystoreError> {
    let v = decode_hex_vec(s, field)?;
    if v.len() != N {
        return Err(KeystoreError::BadHex(format!(
            "{field}: {} bytes, expected {N}",
            v.len()
        )));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Format 16 random bytes as a UUID-v4 string (`8-4-4-4-12` hex groups with the
/// version/variant nibbles set). Metadata only; never parsed back.
fn format_uuid_v4(b: &[u8; 16]) -> String {
    let mut u = *b;
    u[6] = (u[6] & 0x0f) | 0x40; // version 4
    u[8] = (u[8] & 0x3f) | 0x80; // variant 1 (RFC 4122)
    let h = crate::hash::hex(&u);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_hash_is_keccak256_not_sha3() {
        // The Web3SS MAC is the *original* Keccak-256 (Ethereum's hash), NOT the
        // FIPS-202 SHA3-256 — they share a sponge but differ in padding, so a
        // SHA3-256 mix-up would be a silent interop break that round-trip tests
        // (which only check self-consistency) would NOT catch. Pin it with the
        // canonical empty-input digest: Keccak-256("") = c5d2…a470, whereas
        // SHA3-256("") = a7ff…434a. `keystore_mac(b"", b"")` is exactly
        // Keccak256 of the empty input.
        let d = keystore_mac(b"", b"");
        assert_eq!(
            hex(&d),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            "MAC must use Keccak-256 (Ethereum), not SHA3-256"
        );
    }

    // NOTE (M175 follow-up): an end-to-end known-answer test against the
    // canonical Ethereum Web3SS *scrypt* example (decrypt its published
    // keystore JSON with password "testpassword" → the 32-byte private key
    // 7a28b5ba…14fe9d) would additionally pin the full composition against an
    // external reference. It is deferred only because the exact ciphertext/mac
    // bytes must be copied verbatim from the spec and this build environment has
    // no network access to fetch them; the construction is spec-exact by code
    // review (AES key = DK[0..16], MAC = Keccak256(DK[16..32] ‖ ciphertext),
    // AES-128-CTR with a 128-bit big-endian counter) and the Keccak/round-trip
    // tests cover the primitives and self-consistency.

    #[test]
    fn encrypt_decrypt_round_trip() {
        let seed = [7u8; 32];
        let ks = encrypt_seed(&seed, "correct horse battery staple").unwrap();
        let json = to_json(&ks).unwrap();
        let back = decrypt_seed(&json, "correct horse battery staple").unwrap();
        assert_eq!(back, seed);
    }

    #[test]
    fn distinct_salts_give_distinct_ciphertexts() {
        // Same seed + password, two encryptions ⇒ different salt/iv/ciphertext
        // (fresh CSPRNG draws), but both decrypt back to the seed.
        let seed = [0x11u8; 32];
        let a = encrypt_seed(&seed, "pw").unwrap();
        let b = encrypt_seed(&seed, "pw").unwrap();
        assert_ne!(a.crypto.ciphertext, b.crypto.ciphertext);
        assert_ne!(a.crypto.kdfparams.salt, b.crypto.kdfparams.salt);
        assert_eq!(decrypt_seed(&to_json(&a).unwrap(), "pw").unwrap(), seed);
        assert_eq!(decrypt_seed(&to_json(&b).unwrap(), "pw").unwrap(), seed);
    }

    #[test]
    fn wrong_password_fails_with_mac_mismatch() {
        let ks = encrypt_seed(&[9u8; 32], "right").unwrap();
        let json = to_json(&ks).unwrap();
        match decrypt_seed(&json, "wrong") {
            Err(KeystoreError::MacMismatch) => {}
            other => panic!("expected MacMismatch, got {other:?}"),
        }
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut ks = encrypt_seed(&[3u8; 32], "pw").unwrap();
        // Flip the first ciphertext byte's low nibble.
        let mut ct = ks.crypto.ciphertext.clone().into_bytes();
        ct[1] = if ct[1] == b'0' { b'1' } else { b'0' };
        ks.crypto.ciphertext = String::from_utf8(ct).unwrap();
        let json = to_json(&ks).unwrap();
        assert!(matches!(
            decrypt_seed(&json, "pw"),
            Err(KeystoreError::MacMismatch)
        ));
    }

    #[test]
    fn rejects_unsupported_fields() {
        let ks = encrypt_seed(&[1u8; 32], "pw").unwrap();

        let mut bad_version = ks.clone();
        bad_version.version = 2;
        assert!(matches!(
            decrypt_seed(&to_json(&bad_version).unwrap(), "pw"),
            Err(KeystoreError::Unsupported(_))
        ));

        let mut bad_kdf = ks.clone();
        bad_kdf.crypto.kdf = "pbkdf2".to_string();
        assert!(matches!(
            decrypt_seed(&to_json(&bad_kdf).unwrap(), "pw"),
            Err(KeystoreError::Unsupported(_))
        ));

        let mut bad_cipher = ks.clone();
        bad_cipher.crypto.cipher = "aes-256-gcm".to_string();
        assert!(matches!(
            decrypt_seed(&to_json(&bad_cipher).unwrap(), "pw"),
            Err(KeystoreError::Unsupported(_))
        ));
    }

    #[test]
    fn json_schema_is_web3ss_v3_shaped() {
        let ks = encrypt_seed(&[0u8; 32], "pw").unwrap();
        let json = to_json(&ks).unwrap();
        // Spot-check the Web3SS field names a cross-tool reader would expect.
        for needle in [
            "\"kdf\": \"scrypt\"",
            "\"cipher\": \"aes-128-ctr\"",
            "\"kdfparams\"",
            "\"cipherparams\"",
            "\"ciphertext\"",
            "\"mac\"",
            "\"version\": 3",
        ] {
            assert!(json.contains(needle), "missing {needle} in {json}");
        }
        // id is a UUID-v4 shape: 8-4-4-4-12 with version nibble '4'.
        assert_eq!(ks.id.len(), 36);
        assert_eq!(ks.id.as_bytes()[14], b'4');
    }

    #[test]
    fn checked_log_n_rejects_non_power_of_two() {
        assert!(checked_log_n(262144).is_ok()); // 2^18
        assert_eq!(checked_log_n(262144).unwrap(), 18);
        assert!(checked_log_n(3).is_err());
        assert!(checked_log_n(0).is_err());
        assert!(checked_log_n(1).is_err());
    }

    #[test]
    fn scrypt_cost_bounds_are_enforced() {
        // The Web3SS standard profile is accepted.
        assert!(validate_scrypt_cost(262144, 8, 1).is_ok());
        // A tampered keystore declaring an enormous (still power-of-two) N is
        // rejected by the memory ceiling, not run into an OOM.
        assert!(matches!(
            validate_scrypt_cost(1 << 40, 8, 1),
            Err(KeystoreError::BadParams(_))
        ));
        // Likewise a large r that blows the memory budget.
        assert!(matches!(
            validate_scrypt_cost(262144, 1024, 1),
            Err(KeystoreError::BadParams(_))
        ));
        // Zero r / zero or oversized p are rejected.
        assert!(validate_scrypt_cost(4096, 0, 1).is_err());
        assert!(validate_scrypt_cost(4096, 8, 0).is_err());
        assert!(validate_scrypt_cost(4096, 8, MAX_SCRYPT_P + 1).is_err());
    }

    #[test]
    fn decrypt_rejects_resource_exhaustion_keystore() {
        // End-to-end: a valid-looking keystore whose kdfparams.n is absurd is
        // rejected by both the dry-run parser and decrypt BEFORE any KDF work,
        // so a writable config dir cannot turn `node run` into an OOM.
        let mut ks = encrypt_seed(&[1u8; 32], "pw").unwrap();
        ks.crypto.kdfparams.n = 1 << 40; // power of two, but way past the cap
        let json = to_json(&ks).unwrap();
        assert!(matches!(
            parse_keystore(&json),
            Err(KeystoreError::BadParams(_))
        ));
        assert!(matches!(
            decrypt_seed(&json, "pw"),
            Err(KeystoreError::BadParams(_))
        ));
    }

    #[test]
    fn decrypt_with_operator_raised_cost_works() {
        // The operator may hand-edit `n` upward in the JSON. Re-encrypt by
        // deriving at a higher cost and confirm decrypt honours the file's `n`.
        // Use a small bump (2^14) to keep the test fast while proving the
        // param plumbing reads from the file, not the compiled-in default.
        let seed = [0x5au8; 32];
        let salt = [0x01u8; 16];
        let iv = [0x02u8; 16];
        let derived = scrypt_derive_params(b"pw", &salt, 14, 8, 1).unwrap();
        let mut ct = seed;
        aes_128_ctr(&derived[..16], &iv, &mut ct).unwrap();
        let mac = keystore_mac(&derived[16..32], &ct);
        let ks = EncryptedKeystore {
            crypto: CryptoJson {
                kdf: "scrypt".into(),
                kdfparams: KdfParams {
                    dklen: 32,
                    n: 1 << 14,
                    r: 8,
                    p: 1,
                    salt: hex(&salt),
                },
                cipher: "aes-128-ctr".into(),
                cipherparams: CipherParams { iv: hex(&iv) },
                ciphertext: hex(&ct),
                mac: hex(&mac),
            },
            id: format_uuid_v4(&[0u8; 16]),
            version: 3,
        };
        let json = to_json(&ks).unwrap();
        assert_eq!(decrypt_seed(&json, "pw").unwrap(), seed);
    }
}
