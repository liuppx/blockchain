//! Append-only, length-prefixed logs — the node's durability layer.
//!
//! Format: a sequence of records, each `u32 big-endian length` followed by that
//! many bytes of payload. Crash-safe enough for a reference node: a torn final
//! record (truncated tail) is detected on read and reported, rather than
//! silently corrupting replay. Production would add checksums per record, fsync
//! policy, and segment rotation.
//!
//! Two logs share this framing: [`BlockLog`] persists `codec::encode_block`
//! (so a block's hash covers exactly the bytes on disk) and [`CertLog`] persists
//! `codec::encode_commit`. Keeping them in lockstep lets a replaying node
//! re-verify *finality* (each block's > 2/3 certificate), not just re-derive
//! state — see [`crate::Chain::replay_verified`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::codec::{decode_block, decode_commit, encode_block, encode_commit};
use crate::consensus::Commit;
use crate::Block;

/// Ensure the parent directory of `path` exists, then touch the file so a fresh
/// log reads back empty rather than erroring.
fn open_touch(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

/// Append one length-prefixed record and flush it to the OS.
fn append_record(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record too large"))?;
    let mut f = OpenOptions::new().append(true).open(path)?;
    f.write_all(&len.to_be_bytes())?;
    f.write_all(bytes)?;
    f.flush()?;
    f.sync_all()
}

/// Read every length-prefixed record in order, returning the raw payloads. A
/// truncated trailing record (e.g. a crash mid-append) is reported as an error,
/// not silently dropped.
fn read_records(path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        if pos + 4 > buf.len() {
            return Err(torn("truncated length prefix"));
        }
        let len = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let end = pos.checked_add(len).ok_or_else(|| torn("length overflow"))?;
        if end > buf.len() {
            return Err(torn("truncated record"));
        }
        records.push(buf[pos..end].to_vec());
        pos = end;
    }
    Ok(records)
}

fn decode_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Append-only log of blocks. Payload is `codec::encode_block`, the same bytes
/// the block hash covers.
pub struct BlockLog {
    path: PathBuf,
}

impl BlockLog {
    /// Open (creating if absent) the block log at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        open_touch(&path)?;
        Ok(BlockLog { path })
    }

    /// Append one block as a length-prefixed record and flush it to the OS.
    pub fn append(&self, block: &Block) -> io::Result<()> {
        append_record(&self.path, &encode_block(block))
    }

    /// Read and decode every block in order (torn tail reported as an error).
    pub fn read_all(&self) -> io::Result<Vec<Block>> {
        read_records(&self.path)?
            .iter()
            .map(|r| decode_block(r).map_err(decode_err))
            .collect()
    }
}

/// Append-only log of finality certificates, one per committed height and in the
/// same order as [`BlockLog`]. Payload is `codec::encode_commit`. Persisting
/// certificates lets replay re-establish that each block was finalized by > 2/3
/// voting power, so a restarted node (or a following light client) recovers
/// *finality*, not merely deterministic state.
pub struct CertLog {
    path: PathBuf,
}

impl CertLog {
    /// Open (creating if absent) the certificate log at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        open_touch(&path)?;
        Ok(CertLog { path })
    }

    /// Append one certificate as a length-prefixed record and flush it.
    pub fn append(&self, commit: &Commit) -> io::Result<()> {
        append_record(&self.path, &encode_commit(commit))
    }

    /// Read and decode every certificate in order (torn tail reported as an error).
    pub fn read_all(&self) -> io::Result<Vec<Commit>> {
        read_records(&self.path)?
            .iter()
            .map(|r| decode_commit(r).map_err(decode_err))
            .collect()
    }
}

fn torn(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Review, SubmissionTx, MICRO};
    use zhixing_engine::DIM;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "{}-{}-{:?}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        p.push(format!("zhixing-{uniq}.log"));
        p
    }

    fn blk(height: u64, prev: [u8; 32]) -> Block {
        let mut emb = [0.0f32; DIM];
        emb[height as usize % DIM] = 1.0;
        Block {
            height,
            prev_hash: prev,
            timestamp_days: height as f32,
            next_validators_root: [0u8; 32],
            // M23: state commitments stamped by Chain::commit.
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: vec![SubmissionTx {
                author: 1,
                embedding: emb,
                domain: height as u32,
                stake: 2 * MICRO,
                reviews: vec![Review { reviewer: 10, score: 0.9 }],
                repl_success: 3,
                repl_total: 3,
                timestamp_days: height as f32,
                signature: [7u8; 64],
            }],
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: Vec::new(),
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        }
    }

    #[test]
    fn append_then_read_back() {
        let path = tmp("rw");
        let log = BlockLog::open(&path).unwrap();
        let b1 = blk(1, [0u8; 32]);
        let b2 = blk(2, b1.hash());
        log.append(&b1).unwrap();
        log.append(&b2).unwrap();

        let read = BlockLog::open(&path).unwrap().read_all().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].hash(), b1.hash());
        assert_eq!(read[1].hash(), b2.hash());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_log_reads_empty() {
        let path = tmp("empty");
        let log = BlockLog::open(&path).unwrap();
        assert!(log.read_all().unwrap().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn torn_tail_is_detected() {
        let path = tmp("torn");
        let log = BlockLog::open(&path).unwrap();
        log.append(&blk(1, [0u8; 32])).unwrap();
        // truncate the file by one byte to simulate a crash mid-append
        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(BlockLog::open(&path).unwrap().read_all().is_err());
        std::fs::remove_file(&path).ok();
    }

    fn commit(height: u64, bh: [u8; 32]) -> Commit {
        use crate::consensus::{Vote, VoteType};
        let mut seed = [0u8; 32];
        seed[0] = height as u8;
        let kp = crate::Keypair::from_seed(seed);
        Commit {
            height,
            round: 0,
            block_hash: bh,
            precommits: vec![
                Vote::signed(21, height, 0, bh, VoteType::Precommit, &kp),
                Vote::signed(22, height, 0, bh, VoteType::Precommit, &kp),
            ],
        }
    }

    #[test]
    fn cert_log_append_then_read_back() {
        let path = tmp("certs");
        let log = CertLog::open(&path).unwrap();
        let c1 = commit(1, [1u8; 32]);
        let c2 = commit(2, [2u8; 32]);
        log.append(&c1).unwrap();
        log.append(&c2).unwrap();

        let read = CertLog::open(&path).unwrap().read_all().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].block_hash, c1.block_hash);
        assert_eq!(read[1].height, 2);
        assert_eq!(read[1].precommits.len(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cert_log_torn_tail_is_detected() {
        let path = tmp("certs-torn");
        let log = CertLog::open(&path).unwrap();
        log.append(&commit(1, [1u8; 32])).unwrap();
        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(CertLog::open(&path).unwrap().read_all().is_err());
        std::fs::remove_file(&path).ok();
    }
}
