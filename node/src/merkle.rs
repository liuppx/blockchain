//! Deterministic binary Merkle tree over the node's state — the authenticated
//! commitment that lets a light client verify a single fact (e.g. "account 7
//! has balance B") against the state root without holding the full state.
//!
//! Design choices that keep it deterministic and zero-dep:
//!
//!   * **Domain separation.** Leaves are hashed `sha256(0x00 ‖ data)` and
//!     internal nodes `sha256(0x01 ‖ left ‖ right)`, so a leaf can never be
//!     reinterpreted as an internal node (second-preimage safety).
//!   * **Odd nodes are promoted, not duplicated.** A lone node at the end of a
//!     level is carried up unchanged rather than hashed with a copy of itself
//!     (the classic CT duplication foot-gun). A proof simply records no sibling
//!     step for the levels where its leaf was promoted.
//!
//! This is a plain sorted Merkle tree, not a Merkle-Patricia trie: it gives
//! inclusion proofs (what a light client needs to trust a value) but rebuilds
//! from the full leaf set each block. That is fine for a reference node; a
//! production node with large state would switch to an incrementally-updated
//! trie. Non-membership proofs are out of scope here.

use crate::{sha256, Hash};

const LEAF: u8 = 0x00;
const NODE: u8 = 0x01;

/// Hash of a leaf's raw bytes, domain-separated from internal nodes.
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut buf = Vec::with_capacity(1 + data.len());
    buf.push(LEAF);
    buf.extend_from_slice(data);
    sha256(&buf)
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut buf = [0u8; 1 + 32 + 32];
    buf[0] = NODE;
    buf[1..33].copy_from_slice(left);
    buf[33..].copy_from_slice(right);
    sha256(&buf)
}

/// One sibling along an inclusion path, tagged with the side it sits on so the
/// verifier hashes in the correct order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Left(Hash),
    Right(Hash),
}

/// An inclusion proof: the sibling hashes from the leaf up to the root. Levels
/// where the leaf was the promoted odd node contribute no step.
///
/// `key` is the leaf's 0-based index in the tree, and `leaf_count` is the
/// tree's width. Both are **authenticated** by [`verify`] (M173): a promoted
/// (odd-width) level consumes no step and so leaves no trace in the hash
/// chain, which means `key` alone cannot be bound to the steps — a verifier
/// needs `leaf_count` to replay the same width-halving the prover used at
/// every level and know exactly when a step is required versus when the node
/// was promoted. With both fields, `verify` rejects any proof whose `key` (or
/// `leaf_count`) has been tampered with. This lets a consumer trust
/// `proof.key` as the real source-side position of the leaf (e.g. the M172
/// cross-chain unbonding mirror uses `(source_height, key)` for replay dedup)
/// rather than a sentinel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    /// The leaf's 0-based index in the tree (authenticated by [`verify`]).
    pub key: u64,
    /// The tree's leaf count at proof-generation time (authenticated by
    /// [`verify`] alongside `key` — see the struct doc for why both are
    /// needed).
    pub leaf_count: u64,
    pub steps: Vec<Step>,
}

/// A binary Merkle tree, kept as its levels bottom-up (`levels[0]` = leaves,
/// last level = the single root).
pub struct MerkleTree {
    levels: Vec<Vec<Hash>>,
}

impl MerkleTree {
    /// Build a tree from already-hashed leaves (see [`leaf_hash`]).
    pub fn from_leaf_hashes(leaves: Vec<Hash>) -> Self {
        let mut levels = vec![leaves];
        while levels.last().unwrap().len() > 1 {
            let cur = levels.last().unwrap();
            let mut next = Vec::with_capacity(cur.len().div_ceil(2));
            let mut i = 0;
            while i < cur.len() {
                if i + 1 < cur.len() {
                    next.push(node_hash(&cur[i], &cur[i + 1]));
                    i += 2;
                } else {
                    next.push(cur[i]); // promote lone node unchanged
                    i += 1;
                }
            }
            levels.push(next);
        }
        MerkleTree { levels }
    }

    /// Build from raw (un-hashed) leaf byte-strings.
    pub fn from_leaves<I, T>(leaves: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        Self::from_leaf_hashes(leaves.into_iter().map(|d| leaf_hash(d.as_ref())).collect())
    }

    /// The Merkle root; all-zero for an empty tree.
    pub fn root(&self) -> Hash {
        match self.levels.last() {
            Some(top) if !top.is_empty() => top[0],
            _ => [0u8; 32],
        }
    }

    pub fn leaf_count(&self) -> usize {
        self.levels.first().map(|l| l.len()).unwrap_or(0)
    }

    /// Inclusion proof for the leaf at `index`, or `None` if out of range.
    pub fn proof(&self, index: usize) -> Option<Proof> {
        let leaf_count = self.leaf_count();
        if index >= leaf_count {
            return None;
        }
        let key = index as u64;
        let mut i = index;
        let mut steps = Vec::new();
        for level in &self.levels[..self.levels.len().saturating_sub(1)] {
            if i % 2 == 1 {
                steps.push(Step::Left(level[i - 1]));
            } else if i + 1 < level.len() {
                steps.push(Step::Right(level[i + 1]));
            }
            // else: promoted odd node at this level — no sibling to record.
            i /= 2;
        }
        Some(Proof {
            key,
            leaf_count: leaf_count as u64,
            steps,
        })
    }
}

/// Verify that `leaf` (already hashed) is committed by `root` via `proof`,
/// **and** that `proof.key` is the leaf's real index.
///
/// M173: a promoted (odd-width) level contributes no step and so leaves no
/// trace in the hash chain — `key` alone can't be bound to the steps, because
/// a verifier can't tell "no step: promoted" apart from "no step: wrong key"
/// without also knowing the width at each level. So the walk replays
/// [`MerkleTree::proof`]'s own level-by-level logic, driven by `proof.key`
/// AND `proof.leaf_count`: at each level with current width `w` and index
/// `i`, a step is required (and must be `Left`/`Right` matching `i`'s parity)
/// whenever `i` has a sibling in that level (`i^1 < w`); otherwise the level
/// must have promoted `i` unchanged, and no step may be present. Mirroring
/// the prover's exact level-width sequence means a forged `key` or
/// `leaf_count` desyncs from the recorded steps and is rejected — the only
/// way to pass is to have walked the real path.
pub fn verify(root: &Hash, leaf: &Hash, proof: &Proof) -> bool {
    let mut h = *leaf;
    let mut idx = proof.key;
    let mut width = proof.leaf_count;
    if idx >= width {
        return false;
    }
    let mut pos = 0usize; // cursor into proof.steps
    while width > 1 {
        let has_sibling = (idx ^ 1) < width;
        if has_sibling {
            match proof.steps.get(pos) {
                Some(Step::Left(s)) if idx % 2 == 1 => h = node_hash(s, &h),
                Some(Step::Right(s)) if idx.is_multiple_of(2) => h = node_hash(&h, s),
                _ => return false, // missing sibling, or wrong side for idx's parity
            }
            pos += 1;
        } else {
            // No sibling at this width: `idx` must be the lone promoted node
            // (i.e. `idx` is the last index, which is only possible when
            // `idx` is even — the promoted node is always a left child).
            if !idx.is_multiple_of(2) {
                return false;
            }
        }
        idx /= 2;
        width = width.div_ceil(2);
    }
    // Every recorded step must have been consumed, and we must have reached
    // the root (width 1, idx 0 — guaranteed by the loop invariant once
    // width == 1, but idx == 0 falls out of idx < width at every level).
    pos == proof.steps.len() && h == *root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| format!("leaf-{i}").into_bytes()).collect()
    }

    #[test]
    fn single_leaf_root_is_the_leaf_hash() {
        let data = b"only".to_vec();
        let t = MerkleTree::from_leaves([data.clone()]);
        assert_eq!(t.root(), leaf_hash(&data));
    }

    #[test]
    fn empty_tree_root_is_zero() {
        let t = MerkleTree::from_leaf_hashes(Vec::new());
        assert_eq!(t.root(), [0u8; 32]);
    }

    #[test]
    fn proofs_roundtrip_for_all_sizes_and_indices() {
        // exercise even, odd, and promotion-heavy sizes
        for n in 1..=17 {
            let data = leaves(n);
            let t = MerkleTree::from_leaves(&data);
            let root = t.root();
            for (i, d) in data.iter().enumerate() {
                let p = t.proof(i).expect("proof exists");
                assert!(verify(&root, &leaf_hash(d), &p), "n={n} i={i}");
            }
            assert!(t.proof(n).is_none()); // out of range
        }
    }

    #[test]
    fn tampered_leaf_fails_verification() {
        let data = leaves(8);
        let t = MerkleTree::from_leaves(&data);
        let p = t.proof(3).unwrap();
        assert!(!verify(&t.root(), &leaf_hash(b"forged"), &p));
    }

    #[test]
    fn proof_from_one_index_does_not_verify_another_leaf() {
        let data = leaves(8);
        let t = MerkleTree::from_leaves(&data);
        let p = t.proof(3).unwrap();
        // leaf 4's data with leaf 3's path must not verify
        assert!(!verify(&t.root(), &leaf_hash(&data[4]), &p));
    }

    #[test]
    fn changing_any_leaf_changes_the_root() {
        let a = MerkleTree::from_leaves(leaves(6)).root();
        let mut data = leaves(6);
        data[2] = b"mutated".to_vec();
        let b = MerkleTree::from_leaves(&data).root();
        assert_ne!(a, b);
    }

    #[test]
    fn proof_key_is_the_leaf_index() {
        // M173: `MerkleTree::proof` stamps the leaf's 0-based index.
        for n in 1..=17 {
            let data = leaves(n);
            let t = MerkleTree::from_leaves(&data);
            for i in 0..n {
                assert_eq!(t.proof(i).unwrap().key, i as u64, "n={n} i={i}");
            }
        }
    }

    #[test]
    fn tampering_the_key_fails_verification() {
        // M173: `verify` is driven by `proof.key`, so any other key value
        // (one that is in range but wrong, or out of range) is rejected —
        // across every tree size including promotion-heavy ones.
        for n in 1..=33 {
            let data = leaves(n);
            let t = MerkleTree::from_leaves(&data);
            let root = t.root();
            // `i` indexes `data` AND feeds `t.proof(i)` / `bad_key == i`, so a bare
            // range loop is clearer than enumerate here.
            #[allow(clippy::needless_range_loop)]
            for i in 0..n {
                let good = t.proof(i).unwrap();
                assert!(verify(&root, &leaf_hash(&data[i]), &good), "n={n} i={i}");
                // Any other key under the same (leaf, steps) must fail.
                for bad_key in 0..(n as u64 + 2) {
                    if bad_key == i as u64 {
                        continue;
                    }
                    let forged = Proof {
                        key: bad_key,
                        leaf_count: good.leaf_count,
                        steps: good.steps.clone(),
                    };
                    assert!(
                        !verify(&root, &leaf_hash(&data[i]), &forged),
                        "n={n} i={i} bad_key={bad_key} must fail"
                    );
                }
            }
        }
    }

    #[test]
    fn tampering_a_sibling_fails_verification() {
        let data = leaves(9);
        let t = MerkleTree::from_leaves(&data);
        let root = t.root();
        let mut p = t.proof(5).unwrap();
        // Flip a byte in the first recorded sibling.
        if let Some(Step::Left(s) | Step::Right(s)) = p.steps.first_mut() {
            s[0] ^= 0xff;
        }
        assert!(!verify(&root, &leaf_hash(&data[5]), &p));
    }

    #[test]
    fn flipping_a_step_side_fails_verification() {
        // A proof whose step side contradicts its key is rejected even if the
        // sibling hash is genuine.
        let data = leaves(8);
        let t = MerkleTree::from_leaves(&data);
        let root = t.root();
        let good = t.proof(2).unwrap(); // index 2 ⇒ first step is a Right sibling
        let flipped: Vec<Step> = good
            .steps
            .iter()
            .enumerate()
            .map(|(k, s)| {
                if k == 0 {
                    match s {
                        Step::Left(h) => Step::Right(*h),
                        Step::Right(h) => Step::Left(*h),
                    }
                } else {
                    s.clone()
                }
            })
            .collect();
        let forged = Proof {
            key: good.key,
            leaf_count: good.leaf_count,
            steps: flipped,
        };
        assert!(!verify(&root, &leaf_hash(&data[2]), &forged));
    }

    #[test]
    fn tampering_leaf_count_fails_verification() {
        // M173: `leaf_count` is authenticated alongside `key` — a promoted
        // level leaves no trace in the hash chain, so without the true width
        // a verifier can't tell a promotion apart from a forged key. Here we
        // fix a valid (leaf, steps) pair from a promotion-heavy tree (n=5,
        // index 4 is promoted twice) and confirm every other `leaf_count`
        // value is rejected.
        let data = leaves(5);
        let t = MerkleTree::from_leaves(&data);
        let root = t.root();
        let good = t.proof(4).unwrap();
        for bad_count in 0..10u64 {
            if bad_count == good.leaf_count {
                continue;
            }
            let forged = Proof {
                key: good.key,
                leaf_count: bad_count,
                steps: good.steps.clone(),
            };
            assert!(
                !verify(&root, &leaf_hash(&data[4]), &forged),
                "bad_count={bad_count} must fail"
            );
        }
    }
}
