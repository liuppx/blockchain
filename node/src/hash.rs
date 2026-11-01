//! SHA-256 content hashing (FIPS 180-4), backed by the audited `sha2` crate.
//!
//! M121: this was a hand-rolled in-tree SHA-256 (kept during bring-up for a
//! zero-dependency build). Content-addressed block/state hashing is
//! consensus-critical, so it now delegates to the vetted RustCrypto `sha2`
//! implementation. The output is byte-identical — SHA-256 is SHA-256 — so every
//! persisted hash, the `state_root`/`merkle_root` constants, and the localnet
//! `head` invariant are all unchanged; this swap is pure supply-chain hardening,
//! not a protocol change. The engine does no hashing and stays zero-dep.

use sha2::{Digest, Sha256};

/// SHA-256 digest of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Lowercase hex encoding of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// M165: reverse of [`hex`] — decode a 64-char lowercase hex string into the
/// 32 raw bytes it represents. Returns `None` if the input is the wrong length
/// or contains a non-lowercase-hex byte. Used to parse the `?cursor=<hex>`
/// query parameter for hash-keyed list endpoints (`/mempool`, `/block/{h}/txs`).
pub fn from_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for i in 0..32 {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // FIPS 180-4 test vectors — unchanged across the M121 hand-rolled → `sha2` swap,
        // proving the digest is byte-identical (so the chain's content hashes don't move).
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Multi-block (> 64 bytes) vector to exercise the padding/length path.
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hex_round_trips_via_from_hex() {
        // M165: any 32-byte sequence round-trips through hex/from_hex.
        let original = sha256(b"round-trip me");
        let s = hex(&original);
        assert_eq!(s.len(), 64);
        assert_eq!(from_hex(&s), Some(original));
    }

    #[test]
    fn from_hex_rejects_wrong_length_and_non_hex() {
        // M165: from_hex is strict — a wrong length or a non-lowercase-hex
        // character returns None (caller falls back to "no cursor").
        assert_eq!(from_hex(""), None);
        assert_eq!(from_hex("deadbeef"), None); // 8 chars, not 64
        assert_eq!(from_hex(&"x".repeat(64)), None);
        // Uppercase hex is rejected (lowercase-only, matching `hex()`'s output).
        assert_eq!(
            from_hex("BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD"),
            None
        );
    }
}
