//! Canonical, self-describing binary codec for blocks.
//!
//! The SAME byte layout is used for (a) the content-addressed block hash and
//! (b) the on-disk block log, so a block's hash covers exactly the bytes that
//! were persisted. Big-endian, length-prefixed, no external serialization crate.

use crate::{Block, Embedding, Review, SubmissionTx};
use zhixing_engine::DIM;

#[derive(Debug)]
pub enum CodecError {
    UnexpectedEof,
    TrailingBytes,
    TooManyItems(u64),
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnexpectedEof => write!(f, "unexpected end of input"),
            CodecError::TrailingBytes => write!(f, "trailing bytes after block"),
            CodecError::TooManyItems(n) => write!(f, "implausible item count {n}"),
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
        txs.push(SubmissionTx {
            author,
            embedding,
            domain,
            stake,
            reviews,
            repl_success,
            repl_total,
            timestamp_days: ts,
            signature,
        });
    }
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Block {
        height,
        prev_hash,
        timestamp_days,
        txs,
    })
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
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
}
