//! Validator set and deterministic proposer selection for BFT consensus.
//!
//! A validator is an identity (id + ed25519 pubkey) with integer *voting power*
//! (its stake weight). Consensus is by voting power, not head-count: a decision
//! needs strictly more than 2/3 of total power (see [`ValidatorSet::quorum`]),
//! the classic BFT threshold that tolerates < 1/3 Byzantine power while keeping
//! safety (any two quorums intersect in > 1/3 power, so they cannot certify
//! conflicting blocks without some validator equivocating).
//!
//! Proposer selection is Tendermint's proposer-priority accumulator: over
//! successive heights each validator accrues priority equal to its power, the
//! highest-priority validator proposes and then has the total power subtracted.
//! This yields a deterministic, stake-proportional, drift-free rotation that
//! every node computes identically.

use std::collections::BTreeMap;

use crate::PubKey;

#[derive(Clone, Debug, PartialEq)]
pub struct Validator {
    pub id: u64,
    pub pubkey: PubKey,
    pub power: u64,
}

impl Validator {
    /// Canonical leaf bytes for this validator — the exact preimage a light
    /// client hashes (via [`crate::merkle::leaf_hash`]) to check an inclusion
    /// proof against [`ValidatorSet::merkle_root`]. Byte-identical to the
    /// per-validator triple folded into [`crate::ChainState::state_root`]
    /// (`u64 id ‖ raw pubkey ‖ u64 power`), so the two commitments move in
    /// lockstep and a verifier needs only the validator it was told.
    pub fn merkle_leaf(&self) -> Vec<u8> {
        let mut e = crate::codec::Enc(Vec::new());
        e.u64(self.id);
        e.raw(&self.pubkey);
        e.u64(self.power);
        e.0
    }
}

/// An on-chain change to the validator set, carried in a [`crate::Block`] and
/// applied *after* that block's transactions. `power == 0` removes the validator
/// (a no-op if absent); `power > 0` inserts a new validator or updates an
/// existing one's power (and rotates in the given pubkey). Changes take effect
/// from the *next* height — the block carrying an update is still certified by
/// the set in force before it (see [`crate::ChainState::apply_block`] and
/// [`crate::Chain::replay_verified`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorUpdate {
    pub id: u64,
    pub pubkey: PubKey,
    pub power: u64,
}

/// An ordered validator set (sorted by id for deterministic iteration/tie-break).
#[derive(Clone, Debug)]
pub struct ValidatorSet {
    validators: Vec<Validator>,
}

impl ValidatorSet {
    pub fn new(mut validators: Vec<Validator>) -> Self {
        validators.sort_by_key(|v| v.id);
        ValidatorSet { validators }
    }

    pub fn len(&self) -> usize {
        self.validators.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    pub fn validators(&self) -> &[Validator] {
        &self.validators
    }

    pub fn get(&self, id: u64) -> Option<&Validator> {
        self.validators
            .binary_search_by_key(&id, |v| v.id)
            .ok()
            .map(|i| &self.validators[i])
    }

    /// Merkle commitment to the whole set: a binary tree over each validator's
    /// [`Validator::merkle_leaf`] in canonical (id-sorted) order. Folding this
    /// root into the block header (`Block.next_validators_root`) lets a light
    /// client verify the set — or prove one member — against a cert-signed
    /// block hash without replaying the validator-set transition.
    pub fn merkle_root(&self) -> crate::Hash {
        let leaves = self
            .validators
            .iter()
            .map(|v| crate::merkle::leaf_hash(&v.merkle_leaf()))
            .collect();
        crate::merkle::MerkleTree::from_leaf_hashes(leaves).root()
    }

    /// Inclusion proof that validator `id` is committed by [`Self::merkle_root`],
    /// or `None` if `id` is absent. The leaf order matches `merkle_root` (sorted
    /// by id), so the proof index is the validator's position in that order.
    pub fn proof(&self, id: u64) -> Option<crate::merkle::Proof> {
        let index = self.validators.iter().position(|v| v.id == id)?;
        let leaves = self
            .validators
            .iter()
            .map(|v| crate::merkle::leaf_hash(&v.merkle_leaf()))
            .collect();
        crate::merkle::MerkleTree::from_leaf_hashes(leaves).proof(index)
    }

    pub fn total_power(&self) -> u64 {
        self.validators.iter().map(|v| v.power).sum()
    }

    /// Apply on-chain [`ValidatorUpdate`]s in order, returning the evolved set.
    /// `power == 0` removes `id` (no-op if absent); `power > 0` upserts
    /// `(id, pubkey, power)`. The result is re-sorted by id (via [`Self::new`]),
    /// so it stays canonical regardless of the update order.
    pub fn apply_updates(&self, updates: &[ValidatorUpdate]) -> ValidatorSet {
        let mut by_id: BTreeMap<u64, Validator> =
            self.validators.iter().map(|v| (v.id, v.clone())).collect();
        for u in updates {
            if u.power == 0 {
                by_id.remove(&u.id);
            } else {
                by_id.insert(
                    u.id,
                    Validator { id: u.id, pubkey: u.pubkey, power: u.power },
                );
            }
        }
        ValidatorSet::new(by_id.into_values().collect())
    }

    /// Minimum voting power for a decision: strictly more than 2/3 of total,
    /// i.e. `floor(2*total/3) + 1`.
    pub fn quorum(&self) -> u64 {
        self.total_power() * 2 / 3 + 1
    }

    /// Deterministic proposer for `height` via the proposer-priority accumulator.
    /// Every honest node returns the same id. Ties break to the lowest id
    /// (validators are kept sorted). Returns `None` for an empty set.
    pub fn proposer_for(&self, height: u64) -> Option<u64> {
        self.proposer_for_round(height, 0)
    }

    /// Deterministic proposer for a specific (`height`, `round`). When a round
    /// times out (a silent/faulty proposer), consensus advances to the next
    /// round and needs a *different* proposer to make progress; folding `round`
    /// into the accumulator sequence rotates the proposer deterministically
    /// while staying stake-proportional. Every honest node computes the same id.
    pub fn proposer_for_round(&self, height: u64, round: u32) -> Option<u64> {
        let n = self.validators.len();
        if n == 0 {
            return None;
        }
        let total = self.total_power() as i128;
        let steps = (height + round as u64).max(1);
        let mut prio = vec![0i128; n];
        let mut chosen = self.validators[0].id;
        for _ in 0..steps {
            for (i, v) in self.validators.iter().enumerate() {
                prio[i] += v.power as i128;
            }
            let mut best = 0usize;
            for i in 1..n {
                if prio[i] > prio[best] {
                    best = i;
                }
            }
            prio[best] -= total;
            chosen = self.validators[best].id;
        }
        Some(chosen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Keypair;

    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
    }

    fn vset(powers: &[(u64, u64)]) -> ValidatorSet {
        ValidatorSet::new(
            powers
                .iter()
                .map(|&(id, power)| Validator {
                    id,
                    pubkey: kp(id).public(),
                    power,
                })
                .collect(),
        )
    }

    #[test]
    fn quorum_is_strictly_more_than_two_thirds() {
        assert_eq!(vset(&[(1, 1), (2, 1), (3, 1)]).quorum(), 3); // 3 of 3
        assert_eq!(vset(&[(1, 1), (2, 1), (3, 1), (4, 1)]).quorum(), 3); // 3 of 4
        // stake-weighted: total 100, quorum 67
        assert_eq!(vset(&[(1, 50), (2, 30), (3, 20)]).quorum(), 67);
    }

    #[test]
    fn proposer_rotates_proportionally_to_power() {
        // equal power -> round-robin over a full cycle hits everyone once
        let vs = vset(&[(1, 1), (2, 1), (3, 1)]);
        let seq: Vec<u64> = (1..=3).map(|h| vs.proposer_for(h).unwrap()).collect();
        let mut sorted = seq.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, vec![1, 2, 3]); // each proposes once per cycle
    }

    #[test]
    fn higher_power_proposes_more_often() {
        let vs = vset(&[(1, 3), (2, 1)]); // 1 has 3x the stake
        let mut count = [0u32; 3];
        for h in 1..=8 {
            count[vs.proposer_for(h).unwrap() as usize] += 1;
        }
        assert!(count[1] > count[2], "id1={} id2={}", count[1], count[2]);
    }

    #[test]
    fn proposer_is_deterministic() {
        let vs = vset(&[(1, 2), (2, 5), (3, 3)]);
        for h in 0..20 {
            assert_eq!(vs.proposer_for(h), vs.proposer_for(h));
        }
    }

    #[test]
    fn round_changes_the_proposer() {
        // A silent proposer at round 0 must be replaced at round 1 for liveness.
        let vs = vset(&[(1, 1), (2, 1), (3, 1)]);
        let h = 5;
        assert_ne!(vs.proposer_for_round(h, 0), vs.proposer_for_round(h, 1));
        // round 0 is exactly the height-only proposer (back-compat)
        assert_eq!(vs.proposer_for_round(h, 0), vs.proposer_for(h));
    }

    fn upd(id: u64, power: u64) -> ValidatorUpdate {
        ValidatorUpdate { id, pubkey: kp(id).public(), power }
    }

    #[test]
    fn apply_updates_adds_removes_and_reweights() {
        let vs = vset(&[(1, 1), (2, 1), (3, 1)]);
        // add #4, drop #2, reweight #1 to power 5
        let next = vs.apply_updates(&[upd(4, 1), upd(2, 0), upd(1, 5)]);
        let ids: Vec<u64> = next.validators().iter().map(|v| v.id).collect();
        assert_eq!(ids, vec![1, 3, 4]); // sorted, #2 gone, #4 in
        assert_eq!(next.get(1).unwrap().power, 5); // reweighted
        assert_eq!(next.total_power(), 5 + 1 + 1);
        // original set is untouched (updates return a new set)
        assert_eq!(vs.len(), 3);
        assert_eq!(vs.get(1).unwrap().power, 1);
    }

    #[test]
    fn apply_updates_removing_absent_is_a_noop() {
        let vs = vset(&[(1, 1), (2, 1)]);
        let next = vs.apply_updates(&[upd(99, 0)]);
        let ids: Vec<u64> = next.validators().iter().map(|v| v.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn apply_updates_result_is_order_independent() {
        let vs = vset(&[(1, 1), (2, 1), (3, 1)]);
        let a = vs.apply_updates(&[upd(4, 2), upd(5, 3)]);
        let b = vs.apply_updates(&[upd(5, 3), upd(4, 2)]);
        let ia: Vec<u64> = a.validators().iter().map(|v| v.id).collect();
        let ib: Vec<u64> = b.validators().iter().map(|v| v.id).collect();
        assert_eq!(ia, ib);
        assert_eq!(a.total_power(), b.total_power());
    }

    #[test]
    fn merkle_root_is_order_independent_and_content_addressed() {
        // same members, different construction order -> identical root (the set
        // canonicalizes by id), and any field change flips the root.
        let a = ValidatorSet::new(vec![
            Validator { id: 3, pubkey: kp(3).public(), power: 3 },
            Validator { id: 1, pubkey: kp(1).public(), power: 1 },
            Validator { id: 2, pubkey: kp(2).public(), power: 2 },
        ]);
        let b = vset(&[(1, 1), (2, 2), (3, 3)]);
        assert_eq!(a.merkle_root(), b.merkle_root());
        // reweight one validator: root must change
        let c = vset(&[(1, 1), (2, 9), (3, 3)]);
        assert_ne!(a.merkle_root(), c.merkle_root());
        // empty set commits to the all-zero root
        assert_eq!(ValidatorSet::new(vec![]).merkle_root(), [0u8; 32]);
    }

    #[test]
    fn membership_proofs_verify_for_every_member() {
        let vs = vset(&[(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)]);
        let root = vs.merkle_root();
        for v in vs.validators() {
            let proof = vs.proof(v.id).expect("proof exists");
            let leaf = crate::merkle::leaf_hash(&v.merkle_leaf());
            assert!(crate::merkle::verify(&root, &leaf, &proof), "id={}", v.id);
        }
        // absent id -> no proof
        assert!(vs.proof(99).is_none());
    }

    #[test]
    fn forged_validator_leaf_fails_membership() {
        let vs = vset(&[(1, 10), (2, 20), (3, 30)]);
        let root = vs.merkle_root();
        let proof = vs.proof(2).expect("proof exists");
        // a validator with the right id but a tampered power must not verify.
        let forged = Validator { id: 2, pubkey: kp(2).public(), power: 21 };
        let leaf = crate::merkle::leaf_hash(&forged.merkle_leaf());
        assert!(!crate::merkle::verify(&root, &leaf, &proof));
    }
}
