//! Append-only, length-prefixed block log — the node's durability layer.
//!
//! Format: a sequence of records, each `u32 big-endian length` followed by that
//! many bytes of `codec::encode_block`. Crash-safe enough for a reference node:
//! a torn final record (truncated tail) is detected on read and reported, rather
//! than silently corrupting replay. Production would add checksums per record,
//! fsync policy, and segment rotation.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::codec::{decode_block, encode_block};
use crate::Block;

pub struct BlockLog {
    path: PathBuf,
}

impl BlockLog {
    /// Open (creating if absent) the block log at `path`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        // touch the file so subsequent reads succeed on a fresh log
        OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(BlockLog { path })
    }

    /// Append one block as a length-prefixed record and flush it to the OS.
    pub fn append(&self, block: &Block) -> io::Result<()> {
        let bytes = encode_block(block);
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "block too large"))?;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        f.write_all(&len.to_be_bytes())?;
        f.write_all(&bytes)?;
        f.flush()?;
        f.sync_all()
    }

    /// Read and decode every record in order. A truncated trailing record (e.g.
    /// from a crash mid-append) is reported as an error, not silently dropped.
    pub fn read_all(&self) -> io::Result<Vec<Block>> {
        let mut buf = Vec::new();
        File::open(&self.path)?.read_to_end(&mut buf)?;
        let mut blocks = Vec::new();
        let mut pos = 0usize;
        while pos < buf.len() {
            if pos + 4 > buf.len() {
                return Err(torn("truncated length prefix"));
            }
            let len = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            let end = pos
                .checked_add(len)
                .ok_or_else(|| torn("length overflow"))?;
            if end > buf.len() {
                return Err(torn("truncated block record"));
            }
            let block = decode_block(&buf[pos..end])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            blocks.push(block);
            pos = end;
        }
        Ok(blocks)
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
}
