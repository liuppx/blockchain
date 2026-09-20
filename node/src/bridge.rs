//! M30 — trustless bridge (relay + verify-from-counterparty).
//!
//! Two chains A and B running this *same protocol* (distinct genesis → distinct
//! `genesis_hash`) move value trustlessly. Chain A commits a [`BridgeLock`] to a
//! cert-signed root (`header.bridge_root`); a relayer ferries the
//! `(header, cert, proof)` bytes to chain B; chain B's [`BridgeEndpoint`] — which
//! is nothing more than a **light client of A** — verifies the lock against A's
//! cert-signed root exactly like any other SPV read (M22–M29), then credits the
//! destination account, with a dedup set preventing replay.
//!
//! **No relayer trust.** The relayer moves bytes but cannot forge a lock A's
//! validators didn't sign: `verify_lock` performs *no new SPV logic* — it
//! reuses [`ValidatorTracker::verify_state_root_against_header`] (cert-binding,
//! M22) + [`merkle::verify`] (inclusion, M25–M28). Steps 3–4 (destination match,
//! replay) are bridge policy, not consensus. This is the milestone's soundness
//! claim: *a bridge is a light client + a dedup set*.

use std::collections::{BTreeMap, BTreeSet};

use crate::codec::BlockHeader;
use crate::consensus::Commit;
use crate::light::{LightError, ValidatorTracker};
use crate::validator::ValidatorSet;
use crate::{merkle, BridgeLock, ChainState, Genesis, Hash};

/// A relayed lock: everything chain B needs to verify a single cross-chain lock
/// against chain A's cert-signed `bridge_root`, without replaying A. Mirrors
/// [`crate::light::DiffEnvelope`] (M28): the source header + cert + the active
/// validator set at that height, plus the lock and its inclusion proof.
#[derive(Clone, Debug)]
pub struct LockEnvelope {
    /// The source chain's cert-signed header carrying the `bridge_root` the
    /// lock proof opens against.
    pub source_header: BlockHeader,
    /// The > 2/3 finality certificate binding `source_header.hash()`.
    pub source_cert: Commit,
    /// The active validator set at the lock's height (the set that certifies
    /// `source_header`). The endpoint re-verifies the cert against *its own*
    /// genesis-rooted tracker's set, not this one — this field lets a relayer
    /// hand the endpoint the set it needs to follow the header first.
    pub source_tracked_set: ValidatorSet,
    /// The lock's assigned id on the source chain (its key in
    /// `ChainState::bridge_locks`).
    pub lock_id: u64,
    /// The lock op itself.
    pub lock: BridgeLock,
    /// Merkle proof opening `lock.merkle_leaf(lock_id)` against
    /// `source_header.bridge_root`.
    pub proof: merkle::Proof,
}

/// The result of a successful [`BridgeEndpoint::verify_lock`]: a lock that the
/// source chain's validators signed, destined for this chain, not yet consumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedLock {
    /// The source chain's `genesis_hash` (its identity). Together with
    /// `lock_id` this is the dedup key.
    pub source_chain: Hash,
    pub lock_id: u64,
    pub dest_account: u64,
    pub amount: u64,
}

/// Why a [`BridgeEndpoint`] rejected a relayed lock.
#[derive(Debug)]
pub enum BridgeError {
    /// Cert-binding or Merkle-inclusion failure (wraps the underlying
    /// [`LightError`]). A tampered proof, a tampered `bridge_root`, or a
    /// mismatched cert all surface here — the relayer cannot forge past it.
    Cert(LightError),
    /// The lock's `dest_chain` is not this endpoint's chain — a lock destined
    /// for a *different* chain must not be credited here.
    WrongDestination { expected: Hash, got: Hash },
    /// This `(source_chain, lock_id)` was already consumed — replay rejected.
    AlreadyConsumed { source_chain: Hash, lock_id: u64 },
    /// The endpoint has not followed the source chain up to the lock's height
    /// yet (call [`BridgeEndpoint::follow_source`] first).
    SourceNotFollowed { height: u64 },
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Cert(e) => write!(f, "bridge: cert/inclusion failure: {e}"),
            BridgeError::WrongDestination { expected, got } => write!(
                f,
                "bridge: lock destined for {} but this chain is {}",
                hex8(got),
                hex8(expected)
            ),
            BridgeError::AlreadyConsumed { source_chain, lock_id } => write!(
                f,
                "bridge: lock {lock_id} from {} already consumed",
                hex8(source_chain)
            ),
            BridgeError::SourceNotFollowed { height } => {
                write!(f, "bridge: source chain not followed up to height {height}")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

fn hex8(h: &Hash) -> String {
    let mut s = String::with_capacity(16);
    for b in &h[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A destination-side bridge endpoint: a light client of the source chain plus a
/// replay-protected dedup set. Trustless — anchored on its own view of the
/// source chain's genesis-rooted [`ValidatorTracker`], so a relayer can only
/// deliver bytes, never forge a lock.
pub struct BridgeEndpoint {
    /// This (destination) chain's identity — the `genesis_hash` a lock must
    /// name in `dest_chain` to be credited here.
    my_genesis_hash: Hash,
    /// The source chain's identity — its `genesis_hash`. Half of the dedup key.
    source_genesis_hash: Hash,
    /// Follows the SOURCE chain from *its* genesis (this endpoint's root of
    /// trust for the counterparty). Cert-binding in `verify_lock` checks
    /// against this tracker's set, not the envelope's.
    tracker: ValidatorTracker,
    /// `(source_genesis, lock_id)` dedup set — replay protection. Lives here
    /// (bridge module), not in consensus.
    consumed: BTreeSet<(Hash, u64)>,
    /// Demo ledger: `dest_account -> total credited`. A consumed lock mints new
    /// supply on this chain backed 1:1 by the source's locked pool (a
    /// consensus-level mint is a future milestone).
    minted: BTreeMap<u64, u64>,
}

impl BridgeEndpoint {
    /// Build an endpoint on `my_genesis` that will verify locks from
    /// `source_genesis`. Both identities are the respective genesis hashes;
    /// the source tracker is bootstrapped from `source_genesis` (the endpoint's
    /// trustless root for the counterparty).
    pub fn new(my_genesis: &Genesis, source_genesis: &Genesis) -> Self {
        let my_genesis_hash = ChainState::genesis(my_genesis.clone()).1;
        let source_genesis_hash = ChainState::genesis(source_genesis.clone()).1;
        BridgeEndpoint {
            my_genesis_hash,
            source_genesis_hash,
            tracker: ValidatorTracker::from_genesis(source_genesis),
            consumed: BTreeSet::new(),
            minted: BTreeMap::new(),
        }
    }

    /// This endpoint's chain identity.
    pub fn my_genesis_hash(&self) -> Hash {
        self.my_genesis_hash
    }

    /// The source chain identity this endpoint follows.
    pub fn source_genesis_hash(&self) -> Hash {
        self.source_genesis_hash
    }

    /// Height the source tracker has followed to (0 = only genesis).
    pub fn source_height(&self) -> u64 {
        self.tracker.height()
    }

    /// Total credited to `dest_account` by consumed locks (demo ledger).
    pub fn minted(&self, dest_account: u64) -> u64 {
        self.minted.get(&dest_account).copied().unwrap_or(0)
    }

    /// Advance the source tracker over one certified header of the source
    /// chain — the trustless, genesis-rooted follow from M22. After this the
    /// endpoint's `tracker` validator set is the one that certifies the next
    /// source height, so `verify_lock` for a lock at this height binds against
    /// the correct set.
    pub fn follow_source(
        &mut self,
        header: &BlockHeader,
        cert: &Commit,
        next_set: &ValidatorSet,
    ) -> Result<(), BridgeError> {
        self.tracker
            .follow_header(header, cert, next_set)
            .map(|_| ())
            .map_err(BridgeError::Cert)
    }

    /// Verify a relayed lock against the source chain's cert-signed
    /// `bridge_root`. Performs **no new SPV logic** — steps 1–2 are the
    /// existing cert-binding + Merkle primitives; steps 3–4 are bridge policy.
    ///
    /// 1. **cert-binding** — reuse
    ///    [`ValidatorTracker::verify_state_root_against_header`] with the
    ///    endpoint's own tracked set (the set the genesis-rooted tracker
    ///    derived, *not* the envelope's — a relayer cannot substitute a
    ///    validator set it prefers). The source header must be at (or before)
    ///    the tracker frontier so the endpoint actually knows the certifying
    ///    set.
    /// 2. **inclusion** — recompute `lock.merkle_leaf(lock_id)` and
    ///    [`merkle::verify`] it against `source_header.bridge_root`.
    /// 3. **destination match** — `lock.dest_chain == my_genesis_hash`.
    /// 4. **replay** — `(source_genesis, lock_id)` not already consumed.
    pub fn verify_lock(&self, env: &LockEnvelope) -> Result<VerifiedLock, BridgeError> {
        // The endpoint must have followed the source chain up to (at least)
        // the lock's height — otherwise it does not know the certifying set
        // and cannot trustlessly bind the cert.
        if env.source_header.height > self.tracker.height() {
            return Err(BridgeError::SourceNotFollowed {
                height: env.source_header.height,
            });
        }

        // 1. cert-binding, against the endpoint's OWN tracked set.
        ValidatorTracker::verify_state_root_against_header(
            &env.source_header,
            &env.source_cert,
            self.tracker.validators(),
        )
        .map_err(BridgeError::Cert)?;

        // 2. inclusion against `bridge_root` (NOT accounts_root / graph_root).
        let leaf = env.lock.merkle_leaf(env.lock_id);
        let leaf_hash = merkle::leaf_hash(&leaf);
        if !merkle::verify(&env.source_header.bridge_root, &leaf_hash, &env.proof) {
            return Err(BridgeError::Cert(LightError::MembershipProofInvalid {
                height: env.source_header.height,
            }));
        }

        // 3. destination match.
        if env.lock.dest_chain != self.my_genesis_hash {
            return Err(BridgeError::WrongDestination {
                expected: self.my_genesis_hash,
                got: env.lock.dest_chain,
            });
        }

        // 4. replay.
        if self
            .consumed
            .contains(&(self.source_genesis_hash, env.lock_id))
        {
            return Err(BridgeError::AlreadyConsumed {
                source_chain: self.source_genesis_hash,
                lock_id: env.lock_id,
            });
        }

        Ok(VerifiedLock {
            source_chain: self.source_genesis_hash,
            lock_id: env.lock_id,
            dest_account: env.lock.dest_account,
            amount: env.lock.amount,
        })
    }

    /// Consume a verified lock: insert into the dedup set and credit the
    /// destination account. Idempotent-guarded — a second `consume` of the
    /// same `(source_chain, lock_id)` returns `AlreadyConsumed` (so a caller
    /// that skips `verify_lock` cannot double-credit either).
    pub fn consume(&mut self, v: &VerifiedLock) -> Result<(), BridgeError> {
        let key = (v.source_chain, v.lock_id);
        if self.consumed.contains(&key) {
            return Err(BridgeError::AlreadyConsumed {
                source_chain: v.source_chain,
                lock_id: v.lock_id,
            });
        }
        self.consumed.insert(key);
        *self.minted.entry(v.dest_account).or_insert(0) += v.amount;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::{commit_block, Commit};
    use crate::{Block, Chain, Genesis, Keypair, DeltaKParams, MICRO};

    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
    }

    fn genesis_a() -> Genesis {
        Genesis {
            accounts: vec![
                (1, 100 * MICRO, kp(1).public()),
                (2, 100 * MICRO, kp(2).public()),
            ],
            reviewers: vec![(10, 1.0)],
            seed_nodes: vec![],
            params: DeltaKParams::default(),
            base_emission_micro: 8 * MICRO,
            slash_bps: 10_000,
            timestamp_days: 0.0,
            validators: vec![
                (21, kp(21).public(), 1),
                (22, kp(22).public(), 1),
                (23, kp(23).public(), 1),
            ],
            bridge_sources: vec![],
        }
    }

    /// Chain B: distinct genesis (different timestamp → different genesis hash).
    fn genesis_b() -> Genesis {
        let mut g = genesis_a();
        g.timestamp_days = 1.0;
        g
    }

    /// Chain C: a third, distinct genesis (for the wrong-destination test).
    fn genesis_c() -> Genesis {
        let mut g = genesis_a();
        g.timestamp_days = 2.0;
        g
    }

    /// The set of validators that sign, as a keypair list (matching genesis).
    fn validator_kps() -> std::collections::BTreeMap<u64, Keypair> {
        vec![(21, kp(21)), (22, kp(22)), (23, kp(23))].into_iter().collect()
    }

    fn all_voters() -> Vec<u64> {
        vec![21, 22, 23]
    }

    /// Produce, seal, and certify a block on `chain` carrying `locks`. Returns
    /// the sealed block and its finality certificate.
    fn certify_block_with_locks(chain: &mut Chain, locks: Vec<BridgeLock>) -> (Block, Commit) {
        let mut b = Block {
            height: chain.state.height + 1,
            prev_hash: chain.head,
            timestamp_days: (chain.state.height + 1) as f32,
            next_validators_root: [0u8; 32],
            state_root: [0u8; 32],
            accounts_root: [0u8; 32],
            graph_root: [0u8; 32],
            bridge_root: [0u8; 32],
            txs: Vec::new(),
            validator_updates: Vec::new(),
            stake_ops: Vec::new(),
            slashing_evidence: Vec::new(),
            bridge_locks: locks,
            bridge_headers: Vec::new(),
            bridge_redeems: Vec::new(),
        };
        chain.seal(&mut b).expect("seal");
        // Certify the SEALED hash under the active set.
        let set = chain.state.validators.clone();
        let cert = commit_block(&set, &validator_kps(), &b, 0, &all_voters())
            .expect("certify");
        chain.commit(&mut b).expect("commit");
        (b, cert)
    }

    /// End-to-end: A locks → relay → B verifies + credits.
    #[test]
    fn valid_cross_chain_lock_is_verified_and_credited() {
        let mut chain_a = Chain::new(genesis_a());
        let ga = genesis_a();
        let gb = genesis_b();
        let b_genesis_hash = ChainState::genesis(gb.clone()).1;

        // A: account 1 locks 10 for account 5 on chain B.
        let lock = BridgeLock {
            account: 1,
            amount: 10 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let (block, cert) = certify_block_with_locks(&mut chain_a, vec![lock.clone()]);

        // Relay envelope: header + cert + set + proof.
        let header = block.header();
        let set = ChainState::genesis(ga.clone()).0.validators.clone();
        let proof = chain_a.state.bridge_lock_proof(0).expect("lock proof");
        let env = LockEnvelope {
            source_header: header.clone(),
            source_cert: cert.clone(),
            source_tracked_set: set.clone(),
            lock_id: 0,
            lock: lock.clone(),
            proof,
        };

        // B endpoint follows A, then verifies + consumes.
        let mut endpoint = BridgeEndpoint::new(&gb, &ga);
        // Follow height 1: next set equals current set (no validator changes).
        endpoint
            .follow_source(&header, &cert, &set)
            .expect("follow height 1");
        let verified = endpoint.verify_lock(&env).expect("verify");
        assert_eq!(verified.dest_account, 5);
        assert_eq!(verified.amount, 10 * MICRO);
        endpoint.consume(&verified).expect("consume");
        assert_eq!(endpoint.minted(5), 10 * MICRO);
    }

    fn setup_relay() -> (BridgeEndpoint, LockEnvelope) {
        let mut chain_a = Chain::new(genesis_a());
        let ga = genesis_a();
        let gb = genesis_b();
        let b_genesis_hash = ChainState::genesis(gb.clone()).1;
        let lock = BridgeLock {
            account: 1,
            amount: 7 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 9,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let (block, cert) = certify_block_with_locks(&mut chain_a, vec![lock.clone()]);
        let header = block.header();
        let set = ChainState::genesis(ga.clone()).0.validators.clone();
        let proof = chain_a.state.bridge_lock_proof(0).expect("lock proof");
        let env = LockEnvelope {
            source_header: header.clone(),
            source_cert: cert.clone(),
            source_tracked_set: set.clone(),
            lock_id: 0,
            lock,
            proof,
        };
        let mut endpoint = BridgeEndpoint::new(&gb, &ga);
        endpoint.follow_source(&header, &cert, &set).expect("follow");
        (endpoint, env)
    }

    #[test]
    fn tampered_amount_fails_inclusion() {
        let (endpoint, mut env) = setup_relay();
        env.lock.amount += 1; // leaf no longer matches the committed root
        let err = endpoint.verify_lock(&env).unwrap_err();
        assert!(matches!(
            err,
            BridgeError::Cert(LightError::MembershipProofInvalid { .. })
        ));
    }

    #[test]
    fn tampered_bridge_root_fails_cert_binding() {
        let (endpoint, mut env) = setup_relay();
        env.source_header.bridge_root = [0xFF; 32]; // changes header.hash()
        let err = endpoint.verify_lock(&env).unwrap_err();
        assert!(matches!(
            err,
            BridgeError::Cert(LightError::CertificateMismatch { .. })
        ));
    }

    #[test]
    fn wrong_destination_is_rejected() {
        // Build a relay whose lock is destined for chain C, verified at B.
        let mut chain_a = Chain::new(genesis_a());
        let ga = genesis_a();
        let gb = genesis_b();
        let gc = genesis_c();
        let c_genesis_hash = ChainState::genesis(gc.clone()).1;
        let lock = BridgeLock {
            account: 1,
            amount: 3 * MICRO,
            dest_chain: c_genesis_hash, // NOT chain B
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let (block, cert) = certify_block_with_locks(&mut chain_a, vec![lock.clone()]);
        let header = block.header();
        let set = ChainState::genesis(ga.clone()).0.validators.clone();
        let proof = chain_a.state.bridge_lock_proof(0).expect("proof");
        let env = LockEnvelope {
            source_header: header.clone(),
            source_cert: cert.clone(),
            source_tracked_set: set.clone(),
            lock_id: 0,
            lock,
            proof,
        };
        let mut endpoint = BridgeEndpoint::new(&gb, &ga);
        endpoint.follow_source(&header, &cert, &set).expect("follow");
        let err = endpoint.verify_lock(&env).unwrap_err();
        assert!(matches!(err, BridgeError::WrongDestination { .. }));
    }

    #[test]
    fn replay_via_consume_is_rejected() {
        let (mut endpoint, env) = setup_relay();
        let v = endpoint.verify_lock(&env).expect("first verify");
        endpoint.consume(&v).expect("first consume");
        // Second verify now sees it in the dedup set.
        let err = endpoint.verify_lock(&env).unwrap_err();
        assert!(matches!(err, BridgeError::AlreadyConsumed { .. }));
        // And a direct second consume is rejected too.
        let err2 = endpoint.consume(&v).unwrap_err();
        assert!(matches!(err2, BridgeError::AlreadyConsumed { .. }));
    }

    #[test]
    fn source_not_followed_is_rejected() {
        // Build the envelope but do NOT follow the source chain.
        let mut chain_a = Chain::new(genesis_a());
        let ga = genesis_a();
        let gb = genesis_b();
        let b_genesis_hash = ChainState::genesis(gb.clone()).1;
        let lock = BridgeLock {
            account: 1,
            amount: 2 * MICRO,
            dest_chain: b_genesis_hash,
            dest_account: 5,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let (block, cert) = certify_block_with_locks(&mut chain_a, vec![lock.clone()]);
        let header = block.header();
        let set = ChainState::genesis(ga.clone()).0.validators.clone();
        let proof = chain_a.state.bridge_lock_proof(0).expect("proof");
        let env = LockEnvelope {
            source_header: header,
            source_cert: cert,
            source_tracked_set: set,
            lock_id: 0,
            lock,
            proof,
        };
        // endpoint at source height 0; lock is at height 1.
        let endpoint = BridgeEndpoint::new(&gb, &ga);
        let err = endpoint.verify_lock(&env).unwrap_err();
        assert!(matches!(err, BridgeError::SourceNotFollowed { height: 1 }));
    }

    /// Symmetry: B locks for A, and an A-side endpoint verifies it. The exact
    /// same code path, roles swapped.
    #[test]
    fn two_way_symmetry_b_to_a() {
        let ga = genesis_a();
        let gb = genesis_b();
        let a_genesis_hash = ChainState::genesis(ga.clone()).1;
        let mut chain_b = Chain::new(genesis_b());
        let lock = BridgeLock {
            account: 1,
            amount: 4 * MICRO,
            dest_chain: a_genesis_hash,
            dest_account: 2,
            nonce: 0,
            signature: [0u8; 64],
        }
        .signed(&kp(1));
        let (block, cert) = certify_block_with_locks(&mut chain_b, vec![lock.clone()]);
        let header = block.header();
        let set = ChainState::genesis(gb.clone()).0.validators.clone();
        let proof = chain_b.state.bridge_lock_proof(0).expect("proof");
        let env = LockEnvelope {
            source_header: header.clone(),
            source_cert: cert.clone(),
            source_tracked_set: set.clone(),
            lock_id: 0,
            lock,
            proof,
        };
        // A-side endpoint follows B.
        let mut endpoint = BridgeEndpoint::new(&ga, &gb);
        endpoint.follow_source(&header, &cert, &set).expect("follow");
        let v = endpoint.verify_lock(&env).expect("verify");
        endpoint.consume(&v).expect("consume");
        assert_eq!(endpoint.minted(2), 4 * MICRO);
    }
}
