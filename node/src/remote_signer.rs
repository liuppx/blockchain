//! M217 (remote-signer S2 Step B): an out-of-process signer reached over a Unix domain
//! socket, and the reference signer process behind it — see
//! [`docs/design/remote-signer.md`](../../docs/design/remote-signer.md).
//!
//! The validator key lives only in the signer process ([`serve`]); the node holds just the
//! public key and asks for each signature over the socket ([`RemoteSigner`]). The signer
//! enforces an **anti-double-sign high-water-mark** over `(height, round, step)` persisted to
//! disk: even a fully compromised node cannot make it sign two different blocks at one
//! consensus step. Any socket/timeout error or a high-water-mark refusal surfaces as a
//! [`SignerError`], and consensus **fails closed** (see `round.rs`).
//!
//! Unix-only (domain socket). The protocol is a private node↔signer IPC — **not** the P2P
//! wire, so it has no `PROTOCOL_VERSION` bearing.

#![cfg(unix)]

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::signer::{ConsensusStep, SignDomain, Signer, SignerError};
use crate::{crypto, Keypair, PubKey, Sig};

/// Max framed request/response body — a signing message is small; this bounds a hostile
/// peer's declared length so a forged prefix can't drive a huge allocation.
const MAX_FRAME: usize = 64 * 1024;

// Request tags.
const REQ_CONSENSUS: u8 = 0;
const REQ_HANDSHAKE: u8 = 1;
const REQ_PUBLIC: u8 = 2;

// Response status bytes.
const RESP_OK: u8 = 0;
const RESP_REFUSED: u8 = 1;
const RESP_ERROR: u8 = 2;

fn step_byte(step: ConsensusStep) -> u8 {
    match step {
        ConsensusStep::Propose => 0,
        ConsensusStep::Prevote => 1,
        ConsensusStep::Precommit => 2,
    }
}

// --- u32-length-prefixed framing (blocking) --------------------------------------------

fn write_frame(w: &mut impl Write, body: &[u8]) -> io::Result<()> {
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(body)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_be_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    Ok(body)
}

// --- anti-double-sign high-water-mark --------------------------------------------------

/// The last consensus signature the signer produced, as `(height, round, step)`. A request
/// is signed only if **strictly greater** than this (lexicographic), then the mark advances.
/// Persisted (fsync + atomic rename) *before* the signature is returned, so a crash can
/// never leave the mark behind the key material it already signed with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct HighWaterMark {
    /// `None` ⇒ nothing signed yet.
    last: Option<(u64, u32, u8)>,
}

impl HighWaterMark {
    fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(b) if b.len() == 13 => Ok(HighWaterMark {
                last: Some((
                    u64::from_be_bytes(b[0..8].try_into().unwrap()),
                    u32::from_be_bytes(b[8..12].try_into().unwrap()),
                    b[12],
                )),
            }),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "high-water-mark file is corrupt (wrong length) — refusing to sign; inspect/reset it",
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HighWaterMark { last: None }),
            Err(e) => Err(e),
        }
    }

    fn allows(&self, h: u64, r: u32, step: u8) -> bool {
        self.last.is_none_or(|l| (h, r, step) > l)
    }

    /// Advance + persist atomically (tmp write → fsync → rename). Must complete before the
    /// signature is handed out.
    fn advance_and_persist(&mut self, path: &Path, h: u64, r: u32, step: u8) -> io::Result<()> {
        let mut body = Vec::with_capacity(13);
        body.extend_from_slice(&h.to_be_bytes());
        body.extend_from_slice(&r.to_be_bytes());
        body.push(step);
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        self.last = Some((h, r, step));
        Ok(())
    }
}

// --- reference signer process (`node signer`) ------------------------------------------

/// Run the reference signer: bind `socket_path` (perms 600), load the `(height,round,step)`
/// high-water-mark from `hwm_path`, and serve signing requests for `kp` **sequentially**
/// (one connection at a time — serialization is what makes the high-water-mark sound).
/// Blocks until the listener errors; the caller runs it as the long-lived `node signer`
/// process. The key never leaves this process.
pub fn serve(socket_path: &Path, kp: Keypair, hwm_path: &Path) -> io::Result<()> {
    // A stale socket file from a previous run would make bind fail with AddrInUse.
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut hwm = HighWaterMark::load(hwm_path)?;
    for conn in listener.incoming() {
        let mut stream = conn?;
        // Handle every framed request on this connection until the peer hangs up. A
        // malformed/oversized frame or EOF just drops the connection (serve stays up).
        while let Ok(req) = read_frame(&mut stream) {
            let resp = handle_request(&req, &kp, &mut hwm, hwm_path);
            if write_frame(&mut stream, &resp).is_err() {
                break;
            }
        }
    }
    Ok(())
}

/// Process one decoded request body, returning the response body. Pure given `(kp, hwm)`
/// except for the HWM persist side effect.
fn handle_request(req: &[u8], kp: &Keypair, hwm: &mut HighWaterMark, hwm_path: &Path) -> Vec<u8> {
    let err = || vec![RESP_ERROR];
    let Some((&tag, rest)) = req.split_first() else {
        return err();
    };
    match tag {
        REQ_PUBLIC => {
            let mut out = Vec::with_capacity(33);
            out.push(RESP_OK);
            out.extend_from_slice(&kp.public());
            out
        }
        REQ_HANDSHAKE => {
            // No high-water-mark: handshake transcripts legitimately repeat.
            let mut out = Vec::with_capacity(65);
            out.push(RESP_OK);
            out.extend_from_slice(&kp.sign(rest));
            out
        }
        REQ_CONSENSUS => {
            // height(8) round(4) step(1) msg_len(4) msg
            if rest.len() < 17 {
                return err();
            }
            let h = u64::from_be_bytes(rest[0..8].try_into().unwrap());
            let r = u32::from_be_bytes(rest[8..12].try_into().unwrap());
            let step = rest[12];
            let mlen = u32::from_be_bytes(rest[13..17].try_into().unwrap()) as usize;
            let msg = &rest[17..];
            if step > 2 || msg.len() != mlen {
                return err();
            }
            if !hwm.allows(h, r, step) {
                return vec![RESP_REFUSED];
            }
            // Persist the advanced mark BEFORE releasing the signature.
            if hwm.advance_and_persist(hwm_path, h, r, step).is_err() {
                return err();
            }
            let mut out = Vec::with_capacity(65);
            out.push(RESP_OK);
            out.extend_from_slice(&kp.sign(msg));
            out
        }
        _ => err(),
    }
}

// --- client (`RemoteSigner`) -----------------------------------------------------------

/// A [`Signer`] that delegates to an external [`serve`] process over a Unix socket. Connects
/// per request (local sockets are cheap) with a read/write timeout; any I/O error or timeout
/// is [`SignerError::Unavailable`] and a high-water-mark refusal is
/// [`SignerError::DoubleSignRefused`] — either way consensus fails closed.
pub struct RemoteSigner {
    socket_path: PathBuf,
    public: PubKey,
    timeout: Duration,
}

impl RemoteSigner {
    /// Connect, fetch the signer's public key (so the caller can check it against genesis),
    /// and keep the parameters for per-request connections.
    pub fn connect(
        socket_path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, SignerError> {
        let socket_path = socket_path.into();
        let public = Self::request(&socket_path, timeout, &[REQ_PUBLIC]).and_then(|r| {
            match r.split_first() {
                Some((&RESP_OK, pk)) if pk.len() == 32 => {
                    let mut out = [0u8; 32];
                    out.copy_from_slice(pk);
                    Ok(out)
                }
                _ => Err(SignerError::Unavailable("bad public-key response".into())),
            }
        })?;
        Ok(RemoteSigner {
            socket_path,
            public,
            timeout,
        })
    }

    fn request(path: &Path, timeout: Duration, body: &[u8]) -> Result<Vec<u8>, SignerError> {
        let io_err = |e: io::Error| SignerError::Unavailable(e.to_string());
        let mut stream = UnixStream::connect(path).map_err(io_err)?;
        stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
        stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
        write_frame(&mut stream, body).map_err(io_err)?;
        read_frame(&mut stream).map_err(io_err)
    }

    fn parse_sig(resp: Vec<u8>) -> Result<Sig, SignerError> {
        match resp.split_first() {
            Some((&RESP_OK, sig)) if sig.len() == 64 => {
                let mut out = [0u8; 64];
                out.copy_from_slice(sig);
                Ok(out)
            }
            Some((&RESP_REFUSED, _)) => Err(SignerError::DoubleSignRefused),
            _ => Err(SignerError::Unavailable("bad sign response".into())),
        }
    }
}

impl Signer for RemoteSigner {
    fn public(&self) -> PubKey {
        self.public
    }

    fn sign(&self, domain: SignDomain, msg: &[u8]) -> Result<Sig, SignerError> {
        let mut body = Vec::with_capacity(1 + 17 + msg.len());
        match domain {
            SignDomain::Consensus {
                height,
                round,
                step,
            } => {
                body.push(REQ_CONSENSUS);
                body.extend_from_slice(&height.to_be_bytes());
                body.extend_from_slice(&round.to_be_bytes());
                body.push(step_byte(step));
                body.extend_from_slice(&(msg.len() as u32).to_be_bytes());
                body.extend_from_slice(msg);
            }
            SignDomain::Handshake => {
                body.push(REQ_HANDSHAKE);
                body.extend_from_slice(msg);
            }
        }
        let resp = Self::request(&self.socket_path, self.timeout, &body)?;
        let sig = Self::parse_sig(resp)?;
        // Defense in depth: verify the external signer returned a valid signature for the
        // pubkey we trust, so a buggy/hostile signer can't feed us garbage we'd broadcast.
        if !crypto::verify(&self.public, msg, &sig) {
            return Err(SignerError::Unavailable(
                "signer returned an invalid signature".into(),
            ));
        }
        Ok(sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "zhixing-remote-signer-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Start a `serve` loop on a background thread, return the socket path + signer pubkey.
    fn spawn_signer(dir: &Path, kp: Keypair) -> PathBuf {
        // Unix socket paths are bounded by `sun_path` (~104 bytes on macOS), so keep the
        // socket short under /tmp; the (unbounded) hwm file can live in the temp dir.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let socket = PathBuf::from(format!(
            "/tmp/zxs{}{}.sock",
            std::process::id(),
            nanos % 1_000_000
        ));
        let hwm = dir.join("hwm");
        let s2 = socket.clone();
        std::thread::spawn(move || {
            let _ = serve(&s2, kp, &hwm);
        });
        // Wait for the socket to appear (serve binds on the thread).
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        socket
    }

    #[test]
    fn remote_signer_signs_and_enforces_high_water_mark() {
        let dir = tmp("hwm");
        let kp = Keypair::from_seed([9u8; 32]);
        let pubkey = kp.public();
        let socket = spawn_signer(&dir, kp);

        let rs = RemoteSigner::connect(&socket, Duration::from_secs(2)).expect("connect");
        assert_eq!(rs.public(), pubkey, "signer reports its genesis pubkey");

        let dom = |h, r, step| SignDomain::Consensus {
            height: h,
            round: r,
            step,
        };
        let msg = b"consensus signing bytes";

        // (1,0,Prevote) signs and verifies against the pubkey.
        let sig = rs
            .sign(dom(1, 0, ConsensusStep::Prevote), msg)
            .expect("first sign ok");
        assert!(crypto::verify(&pubkey, msg, &sig));

        // Re-signing the same (1,0,Prevote) is refused (not strictly greater) — anti-double-sign.
        assert_eq!(
            rs.sign(dom(1, 0, ConsensusStep::Prevote), b"a different block"),
            Err(SignerError::DoubleSignRefused)
        );

        // A greater step (Precommit) at the same (h,r) is allowed.
        rs.sign(dom(1, 0, ConsensusStep::Precommit), msg)
            .expect("precommit ok");
        // Regressing back to Prevote is refused.
        assert_eq!(
            rs.sign(dom(1, 0, ConsensusStep::Prevote), msg),
            Err(SignerError::DoubleSignRefused)
        );
        // A greater height is allowed.
        rs.sign(dom(2, 0, ConsensusStep::Prevote), msg)
            .expect("next height ok");

        // Handshake signatures have no high-water-mark (repeat freely).
        let h1 = rs
            .sign(SignDomain::Handshake, b"nonce-a")
            .expect("handshake a");
        let h2 = rs
            .sign(SignDomain::Handshake, b"nonce-b")
            .expect("handshake b");
        assert!(crypto::verify(&pubkey, b"nonce-a", &h1));
        assert!(crypto::verify(&pubkey, b"nonce-b", &h2));

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn high_water_mark_survives_reload() {
        let dir = tmp("reload");
        let path = dir.join("hwm");
        let mut hwm = HighWaterMark::load(&path).unwrap();
        assert_eq!(hwm.last, None);
        hwm.advance_and_persist(&path, 5, 2, 1).unwrap();
        // A fresh load sees the persisted mark and refuses anything not strictly greater.
        let reloaded = HighWaterMark::load(&path).unwrap();
        assert_eq!(reloaded.last, Some((5, 2, 1)));
        assert!(!reloaded.allows(5, 2, 1));
        assert!(!reloaded.allows(5, 2, 0));
        assert!(!reloaded.allows(4, 9, 2));
        assert!(reloaded.allows(5, 2, 2));
        assert!(reloaded.allows(6, 0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unavailable_signer_is_fail_closed_error() {
        // Connecting to a nonexistent socket is Unavailable (not a panic) — consensus then
        // fails closed.
        let dir = tmp("down");
        let missing = dir.join("nope.sock");
        let r = RemoteSigner::connect(&missing, Duration::from_millis(200));
        assert!(matches!(r, Err(SignerError::Unavailable(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
