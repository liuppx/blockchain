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
}
