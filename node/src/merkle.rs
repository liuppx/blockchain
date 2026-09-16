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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
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
    pub fn proof(&self, mut index: usize) -> Option<Proof> {
        if index >= self.leaf_count() {
            return None;
        }
        let mut steps = Vec::new();
        for level in &self.levels[..self.levels.len().saturating_sub(1)] {
            if index % 2 == 1 {
                steps.push(Step::Left(level[index - 1]));
            } else if index + 1 < level.len() {
                steps.push(Step::Right(level[index + 1]));
            }
            // else: promoted odd node at this level — no sibling to record.
            index /= 2;
        }
        Some(Proof { steps })
    }
}

/// Verify that `leaf` (already hashed) is committed by `root` via `proof`.
pub fn verify(root: &Hash, leaf: &Hash, proof: &Proof) -> bool {
    let mut h = *leaf;
    for step in &proof.steps {
        h = match step {
            Step::Left(s) => node_hash(s, &h),
            Step::Right(s) => node_hash(&h, s),
        };
    }
    h == *root
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
}
