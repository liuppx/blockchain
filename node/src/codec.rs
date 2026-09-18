//! Canonical, self-describing binary codec for blocks.
//!
//! The SAME byte layout is used for (a) the content-addressed block hash and
//! (b) the on-disk block log, so a block's hash covers exactly the bytes that
//! were persisted. Big-endian, length-prefixed, no external serialization crate.

use crate::consensus::{Commit, Vote, VoteType};
use crate::light::{ProofEntry, ProofKind};
use crate::merkle::{Proof, Step};
use crate::validator::{Validator, ValidatorUpdate};
use crate::{
    Account, Block, BondKind, Embedding, Review, Reviewer, SlashEvidence, StakeOp, SubmissionTx,
};
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
    e.raw(&b.next_validators_root);
    // M23: cert-signed state commitments, written into the cert-signed prefix
    // (between `next_validators_root` and the length-prefixed validator_updates
    // loop). The header codec writes the same fields at the same offsets, so
    // `encode_header(&BlockHeader::from_block(b)) == encode_block(b)[..header_end]`
    // for any block — the SPV contract holds.
    e.raw(&b.state_root);
    e.raw(&b.accounts_root);
    e.u64(b.validator_updates.len() as u64);
    for u in &b.validator_updates {
        e.u64(u.id);
        e.raw(&u.pubkey);
        e.u64(u.power);
    }
    e.u64(b.txs.len() as u64);
    for t in &b.txs {
        enc_tx(&mut e, t, true);
    }
    e.u64(b.stake_ops.len() as u64);
    for op in &b.stake_ops {
        enc_stakeop(&mut e, op, true);
    }
    e.u64(b.slashing_evidence.len() as u64);
    for ev in &b.slashing_evidence {
        enc_evidence(&mut e, ev);
    }
    e.0
}

/// Canonical bytes of a block's cert-signed projection: the header fields —
/// height, prev_hash, timestamp, the next-validator-set commitment, and any
/// validator_updates (M16) — in the same order as [`encode_block`] but stopped
/// before the tx/stake-op/evidence bodies. The SPV transport gossips only
/// these bytes, so a light client verifies state against a cert-signed header
/// without ever deserializing a transaction body.
///
/// The codec is **prefix-stable**: `encode_header(&BlockHeader::from_block(b))
/// == encode_block(b)[..header_end]` for any block `b`, including non-empty
/// ones. This means `sha256(encode_header(h)) == sha256(encode_block(...))`
/// only when the block's three body sections are empty — i.e. for a block with
/// no txs, no stake ops, and no evidence. Light clients only consume headers
/// for blocks whose bodies are empty (or whose bodies they never want), and
/// the cert that ships with the header binds `block_hash = header.hash()`.
pub fn encode_header(h: &BlockHeader) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.u64(h.height);
    e.raw(&h.prev_hash);
    e.f32(h.timestamp_days);
    e.raw(&h.next_validators_root);
    // M23: the two cert-signed state commitments travel with the header so a
    // light client can verify account-inclusion proofs (`accounts_root`) and
    // trust the full-state digest (`state_root`) without pulling any bodies.
    e.raw(&h.state_root);
    e.raw(&h.accounts_root);
    e.u64(h.validator_updates.len() as u64);
    for u in &h.validator_updates {
        e.u64(u.id);
        e.raw(&u.pubkey);
        e.u64(u.power);
    }
    // three SHA-256 commitments binding the three body lists — what makes
    // `header.hash() == block.hash()` hold for blocks with non-empty bodies
    // and is the only byte the light client needs to verify a body was not
    // tampered with.
    e.raw(&h.txs_commitment);
    e.raw(&h.stake_ops_commitment);
    e.raw(&h.evidence_commitment);
    e.0
}

/// Inverse of [`encode_header`]. Trailing bytes after the header are an error.
pub fn decode_header(buf: &[u8]) -> Result<BlockHeader, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let height = d.u64()?;
    let mut prev_hash = [0u8; 32];
    prev_hash.copy_from_slice(d.take(32)?);
    let timestamp_days = d.f32()?;
    let mut next_validators_root = [0u8; 32];
    next_validators_root.copy_from_slice(d.take(32)?);
    // M23: the two cert-signed state commitments sit in the same prefix slot
    // in the block codec — same offsets here as in `decode_block` so the
    // header prefix is identical.
    let mut state_root = [0u8; 32];
    state_root.copy_from_slice(d.take(32)?);
    let mut accounts_root = [0u8; 32];
    accounts_root.copy_from_slice(d.take(32)?);
    let n_upd = d.count()?;
    let mut validator_updates = Vec::with_capacity(n_upd as usize);
    for _ in 0..n_upd {
        let id = d.u64()?;
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(d.take(32)?);
        let power = d.u64()?;
        validator_updates.push(ValidatorUpdate { id, pubkey, power });
    }
    let mut txs_commitment = [0u8; 32];
    txs_commitment.copy_from_slice(d.take(32)?);
    let mut stake_ops_commitment = [0u8; 32];
    stake_ops_commitment.copy_from_slice(d.take(32)?);
    let mut evidence_commitment = [0u8; 32];
    evidence_commitment.copy_from_slice(d.take(32)?);
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(BlockHeader {
        height,
        prev_hash,
        timestamp_days,
        next_validators_root,
        state_root,
        accounts_root,
        validator_updates,
        txs_commitment,
        stake_ops_commitment,
        evidence_commitment,
    })
}

/// Canonical bytes of a [`CertifiedHeader`] = `(BlockHeader, Commit)`. The unit
/// of header-sync gossip; binds an unforgeable > 2/3 certificate to the
/// cert-signed header hash. The cert's `block_hash` field equals
/// `header.hash()` — that is the only hash the light client trusts.
pub fn encode_certified_header(ch: &CertifiedHeader) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.raw(&encode_header(&ch.header));
    e.raw(&encode_commit(&ch.cert));
    e.0
}

/// Inverse of [`encode_certified_header`]. The header codec's trailing
/// three 32-byte commitments define a strict boundary: the rest of the
/// buffer must decode as exactly one [`Commit`] (the cert codec rejects
/// trailing bytes, so any padding after the cert is an error).
pub fn decode_certified_header(buf: &[u8]) -> Result<CertifiedHeader, CodecError> {
    // Header layout: 8 (height) + 32 (prev) + 4 (timestamp) + 32 (next_validators_root)
    //   + 32 (state_root) + 32 (accounts_root) + 8 (n_updates u64) = 148-byte fixed prefix ‖
    //   n_updates * 48 bytes (u64 id ‖ 32-byte pubkey ‖ u64 power) ‖
    //   3 * 32-byte commitments = 96 bytes tail.
    if buf.len() < 148 {
        return Err(CodecError::UnexpectedEof);
    }
    let n_updates = u64::from_be_bytes(buf[140..148].try_into().unwrap());
    let header_len = 148 + (n_updates as usize) * 48 + 96; // 244 base + 48 per update
    if buf.len() < header_len {
        return Err(CodecError::UnexpectedEof);
    }
    let header = decode_header(&buf[..header_len])?;
    let cert = decode_commit(&buf[header_len..])?;
    Ok(CertifiedHeader { header, cert })
}

/// A cert-signed projection of a [`Block`] — the header fields only, never the
/// tx / stake-op / evidence bodies themselves. Lives in this module alongside
/// the codec so `decode_block` / `encode_block` and `encode_header` /
/// `decode_header` stay trivially prefix-stable.
///
/// **SPV contract:** a light client can verify state against a cert-signed
/// header without ever seeing the block's bodies. To make that contract hold
/// end-to-end, the header carries a SHA-256 commitment to each body list:
/// `txs_commitment = sha256(encode_txs_list(&b.txs))`, and the same for stake
/// ops and slashing evidence. A full node MUST verify the supplied body hashes
/// to the committed root before applying; a light client never sees the body
/// and trusts the commitment (which the cert signs).
#[derive(Clone, Debug)]
pub struct BlockHeader {
    pub height: u64,
    pub prev_hash: crate::Hash,
    pub timestamp_days: f32,
    pub next_validators_root: crate::Hash,
    /// M23: full flat digest of every consensus-state field at this height
    /// (accounts/reviewers/graph/validators/bonds/bonded/unbonding/treasury/supply).
    /// Cert-signed. Light clients use this as a tamper-detector: the cert
    /// already vouches for it by binding `block_hash = header.hash()`.
    pub state_root: crate::Hash,
    /// M23: Merkle root over `(accounts ∪ reviewers)`. Cert-signed. Light
    /// clients use this to verify O(log n) account-inclusion proofs against
    /// `ChainState::account_proof(id)` sourced over the new
    /// `GetAccountProof`/`AccountProof` gossip pair.
    pub accounts_root: crate::Hash,
    pub validator_updates: Vec<ValidatorUpdate>,
    /// SHA-256 over the canonical encoding of the tx list (or zero for empty).
    pub txs_commitment: crate::Hash,
    /// SHA-256 over the canonical encoding of the stake-op list.
    pub stake_ops_commitment: crate::Hash,
    /// SHA-256 over the canonical encoding of the evidence list.
    pub evidence_commitment: crate::Hash,
}

impl BlockHeader {
    /// Project a full block to its cert-signed header. The tx list, stake ops
    /// and slashing evidence are folded into per-body SHA-256 commitments —
    /// they are not in `block_hash` themselves.
    pub fn from_block(b: &Block) -> Self {
        BlockHeader {
            height: b.height,
            prev_hash: b.prev_hash,
            timestamp_days: b.timestamp_days,
            next_validators_root: b.next_validators_root,
            state_root: b.state_root,
            accounts_root: b.accounts_root,
            validator_updates: b.validator_updates.clone(),
            txs_commitment: list_commitment(&b.txs.iter().map(encode_tx).collect::<Vec<_>>()),
            stake_ops_commitment: list_commitment(&b.stake_ops.iter().map(encode_stakeop).collect::<Vec<_>>()),
            evidence_commitment: list_commitment(&b.slashing_evidence.iter().map(encode_evidence).collect::<Vec<_>>()),
        }
    }

    /// Content-addressed hash: the cert-signed bytes. **For any block**,
    /// `header.hash() == block.hash()` (because `Block::hash` is defined to
    /// hash the header projection, with the body bytes committed by the
    /// per-body SHA-256 roots above). This is the SPV contract: a light
    /// client verifies against `header.hash()`, and the cert that ships
    /// with the header binds `block_hash = header.hash()` regardless of
    /// whether the body is empty.
    pub fn hash(&self) -> crate::Hash {
        crate::hash::sha256(&encode_header(self))
    }

    /// Reassemble the full block from this header plus the three body vectors
    /// (in canonical order). Used by full nodes; light clients never call it.
    /// Each body MUST match its committed root — otherwise the cert-signed
    /// commitment is broken.
    pub fn to_block(
        &self,
        txs: Vec<SubmissionTx>,
        stake_ops: Vec<StakeOp>,
        slashing_evidence: Vec<SlashEvidence>,
    ) -> Block {
        Block {
            height: self.height,
            prev_hash: self.prev_hash,
            timestamp_days: self.timestamp_days,
            next_validators_root: self.next_validators_root,
            state_root: self.state_root,
            accounts_root: self.accounts_root,
            txs,
            validator_updates: self.validator_updates.clone(),
            stake_ops,
            slashing_evidence,
        }
    }
}

/// SHA-256 over the concatenated per-item encodings; zero for an empty list.
fn list_commitment(parts: &[Vec<u8>]) -> crate::Hash {
    use crate::hash::sha256;
    let mut buf = Vec::new();
    for p in parts {
        buf.extend_from_slice(&(p.len() as u64).to_be_bytes());
        buf.extend_from_slice(p);
    }
    sha256(&buf)
}

/// `(BlockHeader, Commit)` — the unit of header-sync gossip. The cert's
/// `block_hash` equals `header.hash()`; a light client verifies the header
/// against that signature target and never deserializes a body.
#[derive(Clone, Debug)]
pub struct CertifiedHeader {
    pub header: BlockHeader,
    pub cert: Commit,
}

impl CertifiedHeader {
    /// Project a full `(Block, Commit)` to its cert-signed header form. The
    /// block's body fields are dropped.
    pub fn from_certified(b: &Block, cert: &Commit) -> Self {
        CertifiedHeader { header: BlockHeader::from_block(b), cert: cert.clone() }
    }

    /// The header hash the cert signs.
    pub fn block_hash(&self) -> crate::Hash {
        self.header.hash()
    }

    /// Height of the certified header.
    pub fn height(&self) -> u64 {
        self.header.height
    }
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

// --- stake operations (bond / unbond) ----------------------------------------

/// The exact bytes an account signs to authorize a bond/unbond: all fields
/// EXCEPT the signature. Verifying `signature` over these authenticates the op.
pub fn stakeop_signing_bytes(op: &StakeOp) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    enc_stakeop(&mut e, op, false);
    e.0
}

/// Canonical bytes of a full (signed) stake op, used for its content-addressed hash.
pub fn encode_stakeop(op: &StakeOp) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    enc_stakeop(&mut e, op, true);
    e.0
}

/// Decode exactly one signed stake op (the inverse of [`encode_stakeop`]);
/// trailing bytes are an error.
pub fn decode_stakeop(buf: &[u8]) -> Result<StakeOp, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let op = dec_stakeop(&mut d)?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(op)
}

fn enc_stakeop(e: &mut Enc, op: &StakeOp, include_sig: bool) {
    e.u64(op.account);
    e.u32(op.kind.tag() as u32);
    e.u64(op.amount);
    if include_sig {
        e.raw(&op.signature);
    }
}

// --- votes & equivocation evidence -------------------------------------------

/// Encode one vote (validator, height, round, block_hash, vote_type, signature)
/// with the canonical layout shared by commit certificates and slashing
/// evidence. Always includes the signature (a vote's signature IS the artifact).
fn enc_vote(e: &mut Enc, v: &Vote) {
    e.u64(v.validator);
    e.u64(v.height);
    e.u32(v.round);
    e.raw(&v.block_hash);
    e.u32(v.vote_type.tag() as u32);
    e.raw(&v.signature);
}

/// Decode one vote from the cursor (inverse of [`enc_vote`]).
fn dec_vote(d: &mut Dec) -> Result<Vote, CodecError> {
    let validator = d.u64()?;
    let height = d.u64()?;
    let round = d.u32()?;
    let mut block_hash = [0u8; 32];
    block_hash.copy_from_slice(d.take(32)?);
    let tag = d.u32()?;
    let vote_type = VoteType::from_tag(tag as u8).ok_or(CodecError::BadEnum(tag))?;
    let mut signature = [0u8; 64];
    signature.copy_from_slice(d.take(64)?);
    Ok(Vote {
        validator,
        height,
        round,
        block_hash,
        vote_type,
        signature,
    })
}

/// Canonical bytes of one [`SlashEvidence`] (two conflicting votes), used inside
/// blocks and for a standalone round-trip. Trailing bytes are an error on decode.
pub fn encode_evidence(ev: &SlashEvidence) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    enc_evidence(&mut e, ev);
    e.0
}

/// Decode exactly one [`SlashEvidence`] (inverse of [`encode_evidence`]).
pub fn decode_evidence(buf: &[u8]) -> Result<SlashEvidence, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let ev = dec_evidence(&mut d)?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(ev)
}

fn enc_evidence(e: &mut Enc, ev: &SlashEvidence) {
    enc_vote(e, &ev.vote_a);
    enc_vote(e, &ev.vote_b);
}

fn dec_evidence(d: &mut Dec) -> Result<SlashEvidence, CodecError> {
    let vote_a = dec_vote(d)?;
    let vote_b = dec_vote(d)?;
    Ok(SlashEvidence { vote_a, vote_b })
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
        enc_vote(&mut e, v);
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
        precommits.push(dec_vote(&mut d)?);
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

// --- M23: account + Merkle-proof wire codecs for account-proof gossip --------

/// Canonical bytes of one [`Account`] as it travels on the M23
/// `AccountProof` gossip variant. Fixed-layout, big-endian, no versioning —
/// mirrors [`encode_tx`] in shape.
pub fn encode_account(a: &Account) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.raw(&a.pubkey);
    e.u64(a.balance);
    e.u64(a.staked_total);
    e.u64(a.earned_total);
    e.u64(a.slashed_total);
    e.u64(a.submissions);
    e.u64(a.accepted);
    e.0
}

/// Decode exactly one [`Account`] (inverse of [`encode_account`]). Trailing
/// bytes are an error.
pub fn decode_account(buf: &[u8]) -> Result<Account, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(d.take(32)?);
    let balance = d.u64()?;
    let staked_total = d.u64()?;
    let earned_total = d.u64()?;
    let slashed_total = d.u64()?;
    let submissions = d.u64()?;
    let accepted = d.u64()?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Account {
        pubkey,
        balance,
        staked_total,
        earned_total,
        slashed_total,
        submissions,
        accepted,
    })
}

/// Canonical bytes of one [`Proof`]. Each step is one tag byte (0 = Left,
/// 1 = Right) followed by 32 bytes of sibling hash.
pub fn encode_proof(p: &Proof) -> Vec<u8> {
    let mut e = Enc(Vec::new());
    e.u64(p.steps.len() as u64);
    for s in &p.steps {
        match s {
            Step::Left(h) => {
                e.u32(0);
                e.raw(h);
            }
            Step::Right(h) => {
                e.u32(1);
                e.raw(h);
            }
        }
    }
    e.0
}

/// Inverse of [`encode_proof`]. Trailing bytes are an error.
pub fn decode_proof(buf: &[u8]) -> Result<Proof, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let n = d.count()?;
    let mut steps = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let tag = d.u32()?;
        let mut h = [0u8; 32];
        h.copy_from_slice(d.take(32)?);
        steps.push(match tag {
            0 => Step::Left(h),
            1 => Step::Right(h),
            other => return Err(CodecError::BadEnum(other)),
        });
    }
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Proof { steps })
}

// --- M24: typed ProofKind / Validator / Reviewer / ProofEntry wire codecs ----
//
// The M24 wire pair `GetProof { items }` / `Proof { items }` carries a typed
// `(kind, id)` request and a typed `ProofEntry` response. The body layout for
// one entry:
//   - 1 byte kind tag (0 = Account, 1 = Reviewer, 2 = Validator)
//   - 8 byte id (u64 BE; ignored for Validator but kept fixed-width)
//   - leaf bytes (canonical preimage the verifier hashes)
//   - length-prefixed Proof bytes
//
// `encode_proof_entry` re-uses the `Account::merkle_leaf(id)` /
// `Reviewer::merkle_leaf(id)` / `Validator::merkle_leaf()` paths so the wire
// layout is byte-identical to what the verifier recomputes locally — no
// separate canonicalization, no drift.

/// M24: wire tag for one [`ProofKind`]. The numeric value matches the
/// encoding order in [`ProofKind`] (Account=0, Reviewer=1, Validator=2) so
/// adding a new kind means appending — never renumbering.
pub fn encode_proof_kind(k: ProofKind) -> u8 {
    match k {
        ProofKind::Account => 0,
        ProofKind::Reviewer => 1,
        ProofKind::Validator => 2,
    }
}

/// M24: inverse of [`encode_proof_kind`].
pub fn decode_proof_kind(b: u8) -> Result<ProofKind, CodecError> {
    match b {
        0 => Ok(ProofKind::Account),
        1 => Ok(ProofKind::Reviewer),
        2 => Ok(ProofKind::Validator),
        other => Err(CodecError::BadEnum(other as u32)),
    }
}

/// M24: canonical bytes of one [`Validator`] for the M24 `Proof` response.
/// Byte-identical to [`Validator::merkle_leaf`] so the verifier can recompute
/// the leaf locally and reject a prover that swaps one for the other.
pub fn encode_validator(v: &Validator) -> Vec<u8> {
    v.merkle_leaf()
}

/// M24: inverse of [`encode_validator`].
pub fn decode_validator(buf: &[u8]) -> Result<Validator, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let id = d.u64()?;
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(d.take(32)?);
    let power = d.u64()?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Validator { id, pubkey, power })
}

/// M24: canonical bytes of one [`Reviewer`] (id ‖ reputation) for the M24
/// `Proof` response. Same shape as [`Reviewer::merkle_leaf`].
pub fn encode_reviewer(id: u64, reputation: f32) -> Vec<u8> {
    Reviewer { id, reputation }.merkle_leaf()
}

/// M24: inverse of [`encode_reviewer`].
pub fn decode_reviewer(buf: &[u8]) -> Result<(u64, f32), CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let id = d.u64()?;
    let reputation = d.f32()?;
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok((id, reputation))
}

/// M24: canonical bytes of one [`ProofEntry`]. Carries the typed leaf
/// (so the verifier recomputes the same bytes locally and rejects a prover
/// that swaps the leaf) plus the inclusion proof. The leaf already encodes
/// the kind-specific id (e.g. `Account::merkle_leaf(id)` writes id first),
/// so we don't repeat it on the wire.
pub fn encode_proof_entry(e: &ProofEntry) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(encode_proof_kind(e.kind()));
    out.extend_from_slice(&e.leaf());
    out.extend_from_slice(&encode_proof(e.proof()));
    out
}

/// M24: inverse of [`encode_proof_entry`]. Wire layout per entry is
/// `kind(1) || leaf_bytes || encode_proof(proof)` — the leaf bytes ARE
/// `T::merkle_leaf(...)` (id-prefixed, kind-specific length), and we
/// reconstruct the typed object by parsing that layout directly. We
/// re-derive the leaf byte length from the kind tag (the three kinds have
/// distinct fixed sizes), then split.
pub fn decode_proof_entry(buf: &[u8]) -> Result<ProofEntry, CodecError> {
    let mut d = Dec { buf, pos: 0 };
    let kind = decode_proof_kind(d.u8()?)?;
    // Each kind has a fixed leaf byte length because all the inner fields
    // are fixed-width (u64 / f32 / [u8;32]). Keeping a single match here
    // means the wire doesn't need an explicit leaf-length prefix — saving
    // 4 bytes per entry.
    let leaf_byte_len = match kind {
        // Account: u64 id ‖ raw pubkey ‖ 6 × u64 (balance, staked, earned,
        // slashed, submissions, accepted) = 8 + 32 + 48 = 88.
        ProofKind::Account => 88,
        // Reviewer: u64 id ‖ f32 reputation = 8 + 4 = 12.
        ProofKind::Reviewer => 12,
        // Validator: u64 id ‖ raw pubkey ‖ u64 power = 8 + 32 + 8 = 48.
        ProofKind::Validator => 48,
    };
    if buf.len() < 1 + leaf_byte_len {
        return Err(CodecError::UnexpectedEof);
    }
    let leaf_buf = &buf[1..1 + leaf_byte_len];
    let proof = decode_proof(&buf[1 + leaf_byte_len..])?;
    let entry = match kind {
        ProofKind::Account => {
            let mut d2 = Dec { buf: leaf_buf, pos: 0 };
            let id = d2.u64()?;
            let mut pubkey = [0u8; 32];
            pubkey.copy_from_slice(d2.take(32)?);
            let balance = d2.u64()?;
            let staked_total = d2.u64()?;
            let earned_total = d2.u64()?;
            let slashed_total = d2.u64()?;
            let submissions = d2.u64()?;
            let accepted = d2.u64()?;
            ProofEntry::Account {
                id,
                account: Account {
                    pubkey,
                    balance,
                    staked_total,
                    earned_total,
                    slashed_total,
                    submissions,
                    accepted,
                },
                proof,
            }
        }
        ProofKind::Reviewer => {
            let mut d2 = Dec { buf: leaf_buf, pos: 0 };
            let id = d2.u64()?;
            let reputation = d2.f32()?;
            ProofEntry::Reviewer { id, reputation, proof }
        }
        ProofKind::Validator => {
            let mut d2 = Dec { buf: leaf_buf, pos: 0 };
            let id = d2.u64()?;
            let mut pubkey = [0u8; 32];
            pubkey.copy_from_slice(d2.take(32)?);
            let power = d2.u64()?;
            ProofEntry::Validator {
                id,
                validator: Validator { id, pubkey, power },
                proof,
            }
        }
    };
    Ok(entry)
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
    let mut next_validators_root = [0u8; 32];
    next_validators_root.copy_from_slice(d.take(32)?);
    // M23: cert-signed state commitments travel in the same prefix slot the
    // header codec uses, so `encode_header(&BlockHeader::from_block(b)) ==
    // encode_block(b)[..validator_updates_offset]` for any block.
    let mut state_root = [0u8; 32];
    state_root.copy_from_slice(d.take(32)?);
    let mut accounts_root = [0u8; 32];
    accounts_root.copy_from_slice(d.take(32)?);
    let n_upd = d.count()?;
    let mut validator_updates = Vec::with_capacity(n_upd as usize);
    for _ in 0..n_upd {
        let id = d.u64()?;
        let mut pubkey = [0u8; 32];
        pubkey.copy_from_slice(d.take(32)?);
        let power = d.u64()?;
        validator_updates.push(ValidatorUpdate { id, pubkey, power });
    }
    let n_txs = d.count()?;
    let mut txs = Vec::with_capacity(n_txs as usize);
    for _ in 0..n_txs {
        txs.push(dec_tx(&mut d)?);
    }
    let n_ops = d.count()?;
    let mut stake_ops = Vec::with_capacity(n_ops as usize);
    for _ in 0..n_ops {
        stake_ops.push(dec_stakeop(&mut d)?);
    }
    let n_ev = d.count()?;
    let mut slashing_evidence = Vec::with_capacity(n_ev as usize);
    for _ in 0..n_ev {
        slashing_evidence.push(dec_evidence(&mut d)?);
    }
    if d.pos != d.buf.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(Block {
        height,
        prev_hash,
        timestamp_days,
        next_validators_root,
        state_root,
        accounts_root,
        txs,
        validator_updates,
        stake_ops,
        slashing_evidence,
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

/// Decode one signed stake op from the cursor (shared by [`decode_block`] and
/// [`decode_stakeop`]).
fn dec_stakeop(d: &mut Dec) -> Result<StakeOp, CodecError> {
    let account = d.u64()?;
    let tag = d.u32()?;
    let kind = BondKind::from_tag(tag as u8).ok_or(CodecError::BadEnum(tag))?;
    let amount = d.u64()?;
    let mut signature = [0u8; 64];
    signature.copy_from_slice(d.take(64)?);
    Ok(StakeOp {
        account,
        kind,
        amount,
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
    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(u8::from_be_bytes(self.take(1)?.try_into().unwrap()))
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
    use crate::net::{decode_gossip, encode_gossip, GossipMsg};

    /// A conflicting-precommit pair for validator `v` at (h, r) — dummy
    /// signatures (the codec does not verify them; that is the chain's job).
    fn sample_evidence(v: u64) -> SlashEvidence {
        SlashEvidence {
            vote_a: Vote {
                validator: v,
                height: 9,
                round: 1,
                block_hash: [1u8; 32],
                vote_type: VoteType::Precommit,
                signature: [3u8; 64],
            },
            vote_b: Vote {
                validator: v,
                height: 9,
                round: 1,
                block_hash: [2u8; 32],
                vote_type: VoteType::Precommit,
                signature: [4u8; 64],
            },
        }
    }

    fn sample_block() -> Block {
        let mut emb = [0.0f32; DIM];
        emb[3] = 1.0;
        Block {
            height: 7,
            prev_hash: [42u8; 32],
            timestamp_days: 3.5,
            next_validators_root: [17u8; 32],
            // M23: cert-signed state commitments. Tests below assert they
            // round-trip through encode_block/decode_block and that the header
            // projection carries them through unchanged.
            state_root: [11u8; 32],
            accounts_root: [12u8; 32],
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
            stake_ops: vec![
                StakeOp { account: 1, kind: BondKind::Bond, amount: 5 * MICRO, signature: [7u8; 64] },
                StakeOp { account: 2, kind: BondKind::Unbond, amount: 2 * MICRO, signature: [8u8; 64] },
            ],
            slashing_evidence: vec![sample_evidence(22)],
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
    fn stakeop_round_trip() {
        let op = StakeOp {
            account: 3,
            kind: BondKind::Unbond,
            amount: 4 * MICRO,
            signature: [6u8; 64],
        };
        let bytes = encode_stakeop(&op);
        let back = decode_stakeop(&bytes).unwrap();
        assert_eq!(encode_stakeop(&back), bytes);
        assert_eq!(back.hash(), op.hash());
        // signing bytes exclude the signature
        assert!(stakeop_signing_bytes(&op).len() < bytes.len());
        // trailing bytes are rejected
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_stakeop(&extra), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn stake_ops_round_trip_in_a_block() {
        let b = sample_block();
        let back = decode_block(&encode_block(&b)).unwrap();
        assert_eq!(back.stake_ops.len(), 2);
        assert_eq!(back.stake_ops[0].account, 1);
        assert_eq!(back.stake_ops[0].kind, BondKind::Bond);
        assert_eq!(back.stake_ops[0].amount, 5 * MICRO);
        assert_eq!(back.stake_ops[1].kind, BondKind::Unbond);
        // a block with no stake ops still round-trips, with a distinct hash
        let mut plain = sample_block();
        plain.stake_ops.clear();
        let back2 = decode_block(&encode_block(&plain)).unwrap();
        assert!(back2.stake_ops.is_empty());
        assert_ne!(back.hash(), back2.hash()); // stake ops are covered by the hash
    }

    #[test]
    fn evidence_round_trip() {
        let ev = sample_evidence(21);
        let bytes = encode_evidence(&ev);
        let back = decode_evidence(&bytes).unwrap();
        assert_eq!(encode_evidence(&back), bytes);
        assert_eq!(back.vote_a.validator, 21);
        assert_eq!(back.vote_a.block_hash, [1u8; 32]);
        assert_eq!(back.vote_b.block_hash, [2u8; 32]);
        assert!(back.is_well_formed());
        // trailing bytes are rejected
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_evidence(&extra), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn slashing_evidence_round_trip_in_a_block() {
        let b = sample_block();
        let back = decode_block(&encode_block(&b)).unwrap();
        assert_eq!(back.slashing_evidence.len(), 1);
        assert_eq!(back.slashing_evidence[0].vote_a.validator, 22);
        assert_ne!(
            back.slashing_evidence[0].vote_a.block_hash,
            back.slashing_evidence[0].vote_b.block_hash
        );
        // a block with no evidence still round-trips, with a distinct hash
        let mut plain = sample_block();
        plain.slashing_evidence.clear();
        let back2 = decode_block(&encode_block(&plain)).unwrap();
        assert!(back2.slashing_evidence.is_empty());
        assert_ne!(back.hash(), back2.hash()); // evidence is covered by the hash
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

    // --- header codec (M22) ------------------------------------------------

    #[test]
    fn header_round_trip() {
        let b = sample_block();
        let h = BlockHeader::from_block(&b);
        let bytes = encode_header(&h);
        let back = decode_header(&bytes).unwrap();
        assert_eq!(encode_header(&back), bytes);
        assert_eq!(back.height, b.height);
        assert_eq!(back.prev_hash, b.prev_hash);
        assert_eq!(back.timestamp_days.to_bits(), b.timestamp_days.to_bits());
        assert_eq!(back.next_validators_root, b.next_validators_root);
        assert_eq!(back.validator_updates, b.validator_updates);
    }

    #[test]
    fn block_hash_equals_header_hash() {
        // The SPV contract: header.hash() == block.hash() for ANY block,
        // because `Block::hash` now hashes the header projection (which folds
        // in per-body SHA-256 commitments). A light client can verify against
        // header.hash() without ever seeing the body, and the cert binds
        // block_hash = header.hash() regardless of whether the body is empty.
        let b = sample_block();
        let h = BlockHeader::from_block(&b);
        assert_eq!(h.hash(), b.hash());
        // and a block whose body differs must hash differently (the
        // commitment inside the header changes), so a cert-signed header
        // binds its body uniquely.
        let mut tampered = b.clone();
        tampered.txs[0].stake += 1;
        let h2 = BlockHeader::from_block(&tampered);
        assert_ne!(h2.hash(), h.hash(), "tampering a body changes the header hash");
    }

    #[test]
    fn block_hash_matches_header_hash_even_with_bodies() {
        // The SPV contract: header.hash() == block.hash() regardless of body
        // contents, because Block::hash now hashes the header projection (with
        // per-body commitments). Light clients can verify against
        // header.hash() without ever seeing the body, and the cert binds
        // block_hash = header.hash() for both empty and non-empty bodies.
        let mut b = sample_block();
        let h = BlockHeader::from_block(&b);
        assert_eq!(h.hash(), b.hash());
        b.txs.clear();
        b.stake_ops.clear();
        b.slashing_evidence.clear();
        let h2 = BlockHeader::from_block(&b);
        assert_eq!(h2.hash(), b.hash());
    }

    #[test]
    fn certified_header_round_trip() {
        let b = sample_block();
        let cert = crate::consensus::Commit {
            height: b.height,
            round: 0,
            block_hash: BlockHeader::from_block(&b).hash(),
            precommits: Vec::new(),
        };
        let ch = CertifiedHeader::from_certified(&b, &cert);
        let bytes = encode_certified_header(&ch);
        let back = decode_certified_header(&bytes).unwrap();
        assert_eq!(encode_certified_header(&back), bytes);
        assert_eq!(back.header.height, ch.header.height);
        assert_eq!(back.header.next_validators_root, ch.header.next_validators_root);
        assert_eq!(back.cert.height, cert.height);
        assert_eq!(back.cert.block_hash, ch.cert.block_hash);
        // trailing bytes are rejected
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(decode_certified_header(&extra), Err(CodecError::TrailingBytes)));
    }

    #[test]
    fn header_decode_rejects_trailing_bytes() {
        let b = sample_block();
        let h = BlockHeader::from_block(&b);
        let mut bytes = encode_header(&h);
        bytes.push(0);
        assert!(matches!(decode_header(&bytes), Err(CodecError::TrailingBytes)));
    }

    // --- M24: typed proof wire codecs ---------------------------------------

    #[test]
    fn proof_kind_round_trip() {
        use crate::light::ProofKind;
        for k in [ProofKind::Account, ProofKind::Reviewer, ProofKind::Validator] {
            assert_eq!(decode_proof_kind(encode_proof_kind(k)).unwrap(), k);
        }
        assert!(matches!(decode_proof_kind(7), Err(CodecError::BadEnum(7))));
    }

    #[test]
    fn batch_getproof_and_proof_round_trip() {
        use crate::light::{ProofEntry, ProofKind};
        // Build a `GetProof` covering all three kinds, encode -> decode, and
        // assert the request survives the wire byte-for-byte.
        let request_items: Vec<(ProofKind, u64)> = vec![
            (ProofKind::Account, 1),
            (ProofKind::Reviewer, 10),
            (ProofKind::Validator, 25),
        ];
        let req_bytes = encode_gossip(&GossipMsg::GetProof { items: request_items.clone() });
        let req_back: GossipMsg = decode_gossip(&req_bytes).unwrap();
        let req_items_back = match &req_back {
            GossipMsg::GetProof { items } => items.clone(),
            other => panic!("expected GetProof, got {other:?}"),
        };
        assert_eq!(req_items_back, request_items);
        assert_eq!(encode_gossip(&req_back), req_bytes, "GetProof must be self-stable");

        // Build a `Proof` covering all three kinds plus a None slot.
        let account = Account {
            pubkey: [0xA1; 32],
            balance: 42 * MICRO,
            staked_total: 5 * MICRO,
            earned_total: 7 * MICRO,
            slashed_total: 0,
            submissions: 4,
            accepted: 3,
        };
        let validator = Validator {
            id: 25,
            pubkey: [0xB2; 32],
            power: 11,
        };
        let fake_proof = Proof {
            steps: vec![Step::Right([0xCC; 32])],
        };
        let resp_items: Vec<Option<ProofEntry>> = vec![
            Some(ProofEntry::Account { id: 1, account: account.clone(), proof: fake_proof.clone() }),
            None,
            Some(ProofEntry::Reviewer { id: 10, reputation: 0.91, proof: fake_proof.clone() }),
            Some(ProofEntry::Validator { id: 25, validator: validator.clone(), proof: fake_proof.clone() }),
        ];
        let resp_bytes = encode_gossip(&GossipMsg::Proof { items: resp_items.clone() });
        let resp_back: GossipMsg = decode_gossip(&resp_bytes).unwrap();
        let resp_items_back = match &resp_back {
            GossipMsg::Proof { items } => items.clone(),
            other => panic!("expected Proof, got {other:?}"),
        };
        assert_eq!(resp_items_back.len(), resp_items.len());
        for (i, (a, b)) in resp_items_back.iter().zip(resp_items.iter()).enumerate() {
            assert_eq!(a, b, "entry {i} mismatch after round-trip");
        }
        assert_eq!(encode_gossip(&resp_back), resp_bytes, "Proof must be self-stable");
    }
}
