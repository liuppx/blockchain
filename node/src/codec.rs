//! Canonical, self-describing binary codec for blocks.
//!
//! The SAME byte layout is used for (a) the content-addressed block hash and
//! (b) the on-disk block log, so a block's hash covers exactly the bytes that
//! were persisted. Big-endian, length-prefixed, no external serialization crate.

use crate::consensus::{Commit, Vote, VoteType};
use crate::validator::ValidatorUpdate;
use crate::{Block, Embedding, Review, SubmissionTx};
use zhixing_engine::DIM;

#[derive(Debug)]
pub enum CodecError {
    UnexpectedEof,
    TrailingBytes,
    TooManyItems(u64),
    BadEnum(u32),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnexpectedEof => write!(f, "unexpected end of input"),
            CodecError::TrailingBytes => write!(f, "trailing bytes after block"),
            CodecError::TooManyItems(n) => write!(f, "implausible item count {n}"),
            CodecError::BadEnum(v) => write!(f, "invalid enum discriminant {v}"),
        }
    }
}

impl std::error::Error for CodecError {}

// Guards against a corrupt length prefix forcing a huge allocation.
const MAX_ITEMS: u64 = 1_000_000;

// --- encode ------------------------------------------------------------------

pub fn encode_block(b: &Block) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.u64(b.height);
    e.raw(&b.prev_hash);
    e.f32(b.timestamp_days);
    e.u64(b.txs.len() as u64);
    for t in &b.txs {
        enc_tx(&mut e, t, true);
    }
    e.u64(b.validator_updates.len() as u64);
    for u in &b.validator_updates {
        e.u64(u.id);
        e.raw(&u.pubkey);
        e.u64(u.power);
    }
    e.0
}

/// The exact bytes a submission's author signs: all tx fields EXCEPT the
/// signature itself. Verifying `signature` over these bytes authenticates the tx.
pub fn tx_signing_bytes(t: &SubmissionTx) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    enc_tx(&mut e, t, false);
    e.0
}

/// Canonical bytes of a full (signed) transaction, used for the content-addressed
/// tx hash that gives the mempool a deterministic, builder-independent ordering.
pub fn encode_tx(t: &SubmissionTx) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    enc_tx(&mut e, t, true);
    e.0
}

/// Decode exactly one signed transaction (the inverse of [`encode_tx`]). Used by
/// the gossip layer to carry a pending tx on the wire; trailing bytes are an error.
pub fn decode_tx(buf: &[u8]) -> Result<SubmissionTx, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let tx = dec_tx(&mut d)?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(tx)
}

fn enc_tx(e: &mut Enc, t: &SubmissionTx, include_sig: bool) {
    e.u64(t.author);
    e.emb(&t.embedding);
    e.u32(t.domain);
    e.u64(t.stake);
    e.u64(t.reviews.len() as u64);
    for r in &t.reviews {
        e.u64(r.reviewer);
        e.f32(r.score);
    }
    e.u32(t.repl_success);
    e.u32(t.repl_total);
    e.f32(t.timestamp_days);
    if include_sig {
        e.raw(&t.signature);
    }
}

// --- commit certificates -----------------------------------------------------

/// Canonical bytes of a finality certificate ([`Commit`]) — used to persist
/// certificates alongside blocks so a replaying node can re-verify finality
/// (not just re-derive state). Same big-endian, length-prefixed layout.
pub fn encode_commit(c: &Commit) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.u64(c.height);
    e.u32(c.round);
    e.raw(&c.block_hash);
    e.u64(c.precommits.len() as u64);
    for v in &c.precommits {
        e.u64(v.validator);
        e.u64(v.height);
        e.u32(v.round);
        e.raw(&v.block_hash);
        e.u32(v.vote_type.tag() as u32);
        e.raw(&v.signature);
    }
    e.0
}

pub fn decode_commit(buf: &[u8]) -> Result<Commit, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let height = d.u64()?;
    let round = d.u32()?;
    let mut block_hash = [0u8; 32];
    block_hash.copy_from_slice(d.take(32)?);
    let n = d.count()?;
    let mut precommits = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let validator = d.u64()?;
        let v_height = d.u64()?;
        let v_round = d.u32()?;
        let mut v_hash = [0u8; 32];
        v_hash.copy_from_slice(d.take(32)?);
        let tag = d.u32()?;
        let vote_type =
            VoteType::from_tag(tag as u8).ok_or(CodecError::BadEnum(tag))?;
        let mut signature = [0u8; 64];
        signature.copy_from_slice(d.take(64)?);
        precommits.push(Vote {
            validator,
            height: v_height,
            round: v_round,
            block_hash: v_hash,
            vote_type,
            signature,
        });
    }
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Commit {
        height,
        round,
        block_hash,
        precommits,
    })
}

pub(crate) struct Enc(pub Vec<u8>);

impl Enc {
    pub fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        // canonicalize NaN so equal states hash/encode equal
        let bits = if v.is_nan() { 0x7fc0_0000 } else { v.to_bits() };
        self.0.extend_from_slice(&bits.to_be_bytes());
    }
    pub fn emb(&mut self, e: &Embedding) {
        for x in e {
            self.f32(*x);
        }
    }
}

// --- decode ------------------------------------------------------------------

pub fn decode_block(buf: &[u8]) -> Result<Block, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let height = d.u64()?;
    let mut prev_hash = [0u8; 32];
    prev_hash.copy_from_slice(d.take(32)?);
    let timestamp_days = d.f32()?;
    let n_txs = d.count()?;
    let mut txs = Vec::with_capacity(n_txs as usize);
    for _ in 0..n_txs {
        txs.push(dec_tx(&mut d)?);
    }
    let n_upd = d.count()?;
    let mut validator_updates = Vec::with_capacity(n_upd as usize);
    for _ in 0..n_upd {
        let id = d.u64()?;
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(d.take(32)?);
        let power = d.u64()?;
        validator_updates.push(ValidatorUpdate { id, pubkey, power });
    }
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Block {
        height,
        prev_hash,
        timestamp_days,
        txs,
        validator_updates,
    })
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

/// Decode one signed transaction from the cursor (shared by [`decode_block`] and
/// [`decode_tx`]).
fn dec_tx(d: &mut Dec) -> Result<SubmissionTx, CodecError> {
    let author = d.u64()?;
    let embedding = d.emb()?;
    let domain = d.u32()?;
    let stake = d.u64()?;
    let n_rev = d.count()?;
    let mut reviews = Vec::with_capacity(n_rev as usize);
    for _ in 0..n_rev {
        reviews.push(Review {
            reviewer: d.u64()?,
            score: d.f32()?,
        });
    }
    let repl_success = d.u32()?;
    let repl_total = d.u32()?;
    let ts = d.f32()?;
    let mut signature = [0u8; 64];
    signature.copy_from_slice(d.take(64)?);
    Ok(SubmissionTx {
        author,
        embedding,
        domain,
        stake,
        reviews,
        repl_success,
        repl_total,
        timestamp_days: ts,
        signature,
    })
}

impl<'a> Dec<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let end = self.pos.checked_add(n).ok_or(CodecError::UnexpectedEof)?;
        if end > self.buf.len() {
            return Err(CodecError::UnexpectedEof);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn f32(&mut self) -> Result<f32, CodecError> {
        Ok(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().unwrap())))
    }
    fn emb(&mut self) -> Result<Embedding, CodecError> {
        let mut e = [0.0f32; DIM];
        for slot in e.iter_mut() {
            *slot = self.f32()?;
        }
        Ok(e)
    }
    fn count(&mut self) -> Result<u64, CodecError> {
        let n = self.u64()?;
        if n > MAX_ITEMS {
            return Err(CodecError::TooManyItems(n));
        }
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MICRO;

    fn sample_block() -> Block {
        let mut emb = [0.0f32; DIM];
        emb[3] = 1.0;
        Block {
            height: 7,
            prev_hash: [42u8; 32],
            timestamp_days: 3.5,
            txs: vec![SubmissionTx {
                author: 1,
                embedding: emb,
                domain: 2,
                stake: 2 * MICRO,
                reviews: vec![
                    Review { reviewer: 10, score: 0.9 },
                    Review { reviewer: 11, score: 0.75 },
                ],
                repl_success: 2,
                repl_total: 3,
                timestamp_days: 3.0,
                signature: [9u8; 64],
            }],
            validator_updates: vec![
                ValidatorUpdate { id: 25, pubkey: [5u8; 32], power: 3 },
                ValidatorUpdate { id: 21, pubkey: [0u8; 32], power: 0 },
            ],
        }
    }

    #[test]
    fn round_trip() {
        let b = sample_block();
        let bytes = encode_block(&b);
        let back = decode_block(&bytes).unwrap();
        assert_eq!(encode_block(&back), bytes);
        assert_eq!(back.hash(), b.hash());
    }

    #[test]
    fn validator_updates_round_trip_in_a_block() {
        let b = sample_block();
        let back = decode_block(&encode_block(&b)).unwrap();
        assert_eq!(back.validator_updates.len(), 2);
        assert_eq!(back.validator_updates[0].id, 25);
        assert_eq!(back.validator_updates[0].power, 3);
        assert_eq!(back.validator_updates[1].id, 21);
        assert_eq!(back.validator_updates[1].power, 0); // removal encoded as power 0
        // a block with no updates still round-trips (empty length prefix)
        let mut plain = sample_block();
        plain.validator_updates.clear();
        let back2 = decode_block(&encode_block(&plain)).unwrap();
        assert!(back2.validator_updates.is_empty());
        assert_ne!(back.hash(), back2.hash()); // updates are covered by the hash
    }

    #[test]
    fn tx_round_trip() {
        let b = sample_block();
        let tx = &b.txs[0];
        let bytes = encode_tx(tx);
        let back = decode_tx(&bytes).unwrap();
        assert_eq!(back.hash(), tx.hash());
        assert_eq!(encode_tx(&back), bytes);
        // trailing bytes are rejected
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_tx(&extra), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn truncated_input_errors() {
        let bytes = encode_block(&sample_block());
        assert!(decode_block(&bytes[..bytes.len() - 3]).is_err());
    }

    #[test]
    fn trailing_bytes_error() {
        let mut bytes = encode_block(&sample_block());
        bytes.push(0);
        assert!(matches!(decode_block(&bytes), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn commit_round_trip() {
        use crate::consensus::{Vote, VoteType};
        use crate::Keypair;

        let mut seed = [0u8; 32];
        seed[0] = 9;
        let kp = Keypair::from_seed(seed);
        let bh = [3u8; 32];
        let commit = crate::consensus::Commit {
            height: 42,
            round: 2,
            block_hash: bh,
            precommits: vec![
                Vote::signed(21, 42, 2, bh, VoteType::Precommit, &kp),
                Vote::signed(22, 42, 2, bh, VoteType::Precommit, &kp),
            ],
        };
        let bytes = encode_commit(&commit);
        let back = decode_commit(&bytes).unwrap();
        assert_eq!(encode_commit(&back), bytes); // stable re-encoding
        assert_eq!(back.height, 42);
        assert_eq!(back.round, 2);
        assert_eq!(back.block_hash, bh);
        assert_eq!(back.precommits.len(), 2);
        assert_eq!(back.precommits[1].validator, 22);
        assert_eq!(back.precommits[0].signature, commit.precommits[0].signature);
    }

    #[test]
    fn decode_commit_rejects_trailing_bytes() {
        let commit = crate::consensus::Commit {
            height: 1,
            round: 0,
            block_hash: [0u8; 32],
            precommits: Vec::new(),
        };
        let mut bytes = encode_commit(&commit);
        bytes.push(0);
        assert!(matches!(decode_commit(&bytes), Err(CodecError::TrailingBytes)));
    }
}
