//! Performance-critical cognitive-graph + ΔK engine for ZhixingGraph.
//!
//! This is the Rust port of the hot path defined in `sim/delta_k.py` and
//! whitepaper appendix B.2.3. It is the same contract, re-implemented for the
//! throughput needed at graph scale (kNN over embeddings runs on every submit
//! and every simulation epoch). Dependency-free (std only) so it builds offline.
//!
//! `compute_delta_k` is a pure function; its output is in `[0, bonus_max]`.
//! Returning `0.0` is the enforcement point of the §3.2 hard constraint
//! "ΔK <= 0 -> no minting".

/// Embedding dimension (kept small and fixed for cache-friendly SIMD-able loops).
pub const DIM: usize = 8;

pub type Embedding = [f32; DIM];

/// Unified governance parameter table (mirrors B.2.3 defaults).
#[derive(Clone, Copy, Debug)]
pub struct DeltaKParams {
    pub tau_dup: f32,      // near-duplicate threshold
    pub n_review_min: usize,
    pub c_cap: f32,        // correctness cap when reviews insufficient
    pub n_min: usize,      // minimum replication attempts
    pub lam: f32,          // cross-domain bridging coefficient
    pub bonus_max: f32,
    pub decay: f32,        // time decay per day
    pub fresh_min: f32,
    pub delta_k_min: f32,  // ΔK gate: below this -> 0
}

impl Default for DeltaKParams {
    fn default() -> Self {
        DeltaKParams {
            tau_dup: 0.95,
            n_review_min: 3,
            c_cap: 0.5,
            n_min: 2,
            lam: 0.3,
            bonus_max: 2.0,
            decay: 0.01,
            fresh_min: 0.5,
            delta_k_min: 0.05,
        }
    }
}

pub struct Submission {
    pub embedding: Embedding,
    pub domain: u32,
    pub timestamp_days: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    /// M25: monotonic id assigned at insertion (0, 1, 2, ...). Stable for
    /// inclusion proofs and external references — distinct from the
    /// node's position in `CognitiveGraph::nodes`, which is the same
    /// value, but the id is what the chain commits to in the per-node
    /// leaf preimage.
    pub node_id: u64,
    pub embedding: Embedding,
    pub domain: u32,
}

impl GraphNode {
    /// M25: canonical leaf preimage for inclusion proofs. Mirrors the
    /// per-node layout used by `ChainState::state_root` for graph nodes,
    /// but prepends `node_id` so the leaf identifies a specific node
    /// independent of position. Total width: 8 + 32 + 4 = 44 bytes.
    pub fn merkle_leaf(&self) -> Vec<u8> {
        let mut e = Enc(Vec::new());
        e.u64(self.node_id);
        e.emb(&self.embedding);
        e.u32(self.domain);
        e.0
    }
}

/// Minimal endian-aware encoder so the engine can produce a leaf
/// preimage without depending on any external crate. Mirrors
/// `node::codec::Enc` semantics byte-for-byte.
struct Enc(Vec<u8>);

impl Enc {
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn emb(&mut self, e: &Embedding) {
        for &x in e { self.f32(x); }
    }
}

/// Minimal cognitive graph with same-domain kNN and a cross-domain bridge probe.
/// Nodes are stored in a flat contiguous vector for cache-friendly scans.
#[derive(Clone)]
pub struct CognitiveGraph {
    pub nodes: Vec<GraphNode>,
}

impl CognitiveGraph {
    pub fn new() -> Self {
        CognitiveGraph { nodes: Vec::new() }
    }

    pub fn with_capacity(cap: usize) -> Self {
        CognitiveGraph { nodes: Vec::with_capacity(cap) }
    }

    /// Append a node to the graph and return its monotonic `node_id`.
    /// The id equals the insertion index; both are stable for the
    /// lifetime of the graph.
    #[inline]
    pub fn add(&mut self, embedding: Embedding, domain: u32) -> u64 {
        let id = self.nodes.len() as u64;
        self.nodes.push(GraphNode { node_id: id, embedding, domain });
        id
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// M25: index → node_id reverse lookup. Returns `None` if `idx` is
    /// out of range. Equivalent to `nodes[idx].node_id` but bounds-checked.
    pub fn id_at(&self, idx: usize) -> Option<u64> {
        self.nodes.get(idx).map(|n| n.node_id)
    }

    /// Max cosine similarity to any same-domain node, plus whether the domain has
    /// any node at all. One linear scan; avoids allocating a neighbour list.
    #[inline]
    pub fn max_sim_same_domain(&self, emb: &Embedding, domain: u32) -> (f32, bool) {
        let mut max_sim = f32::NEG_INFINITY;
        let mut found = false;
        for n in &self.nodes {
            if n.domain == domain {
                found = true;
                let s = cos_sim(emb, &n.embedding);
                if s > max_sim {
                    max_sim = s;
                }
            }
        }
        (max_sim, found)
    }

    /// Count of distinct domains that have at least one node within `radius`
    /// cosine similarity of `emb` (crude cross-domain bridge estimate).
    #[inline]
    pub fn domains_within(&self, emb: &Embedding, radius: f32) -> usize {
        // DIM-independent small bitset over domain ids; domains are dense u32.
        let mut seen: u64 = 0;
        let mut extra: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for n in &self.nodes {
            if cos_sim(emb, &n.embedding) >= radius {
                if (n.domain as usize) < 64 {
                    seen |= 1u64 << n.domain;
                } else {
                    extra.insert(n.domain);
                }
            }
        }
        (seen.count_ones() as usize) + extra.len()
    }

    /// M26: full sorted (cosine-desc, node_id-asc) ranking of every node
    /// against `query`. Cross-domain (no domain filter). No truncation —
    /// the caller decides whether to keep all ties or cut at k. Pure,
    /// append-only-safe; identical output on any client given the same
    /// committed graph. O(n) one-pass scan + O(n log n) sort.
    pub fn rank_by_cosine(&self, query: &Embedding) -> Vec<(u64, f32)> {
        let mut out: Vec<(u64, f32)> = self
            .nodes
            .iter()
            .map(|n| (n.node_id, cos_sim(query, &n.embedding)))
            .collect();
        // Stable sort: cosine desc, then node_id asc (final tie-breaker).
        // `partial_cmp` returns None for NaN; we treat NaN ties as Equal
        // and let node_id break them — defensive: `cos_sim` can only
        // produce NaN if an embedding contains NaN, which the chain
        // never admits, but the verifier must not panic on a malicious
        // prover who tampered at the Merkle-leaf level.
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        out
    }

    /// M26: top-k with all ties at the boundary. If the node at rank k-1
    /// shares cosine similarity with rank k (or any later), all matching
    /// nodes are returned — the result may have `len > k`. Pure; identical
    /// output on any client.
    ///
    /// Ties are broken by `node_id` ascending, the same tie-breaker used
    /// by `rank_by_cosine`. So if 5 nodes all share sim 0.95 and k=3, the
    /// 3 lowest-`node_id` ones are kept (and the higher-`node_id` 0.95
    /// nodes are dropped — they're truly indistinguishable by cosine, so
    /// dropping by node_id is the only fair deterministic cut).
    pub fn k_nearest_with_ties(&self, query: &Embedding, k: usize) -> Vec<(u64, f32)> {
        if k == 0 {
            return Vec::new();
        }
        let ranked = self.rank_by_cosine(query);
        if ranked.is_empty() {
            return Vec::new();
        }
        if ranked.len() <= k {
            return ranked;
        }
        let cutoff = ranked[k - 1].1;
        // Keep every node with sim >= cutoff. The sort is stable, so ties
        // before the k-th slot are already in (node_id asc) order; ties
        // past the k-th slot share the same sim and we include them too.
        // `ranked` is sorted cos-desc, so once we see one < cutoff we are
        // done — every later node has even lower sim.
        let out: Vec<(u64, f32)> = ranked
            .into_iter()
            .take_while(|(_, s)| *s >= cutoff)
            .collect();
        debug_assert!(out.len() >= k);
        out
    }
}

impl Default for CognitiveGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
pub fn cos_sim(a: &Embedding, b: &Embedding) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..DIM {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[inline]
fn clamp(x: f32, lo: f32, hi: f32) -> f32 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// Reputation-weighted mean of `(reputation, score)` pairs.
pub fn weighted_mean(reviews: &[(f32, f32)]) -> f32 {
    if reviews.is_empty() {
        return 0.0;
    }
    let num: f32 = reviews.iter().map(|(r, s)| r * s).sum();
    let den: f32 = reviews.iter().map(|(r, _)| *r).sum();
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

/// Consensus proxy for theory-type claims (no replication): 1 - normalized spread.
pub fn agreement(reviews: &[(f32, f32)]) -> f32 {
    if reviews.len() < 2 {
        return 0.0;
    }
    let n = reviews.len() as f32;
    let mean: f32 = reviews.iter().map(|(_, s)| *s).sum::<f32>() / n;
    let var: f32 = reviews.iter().map(|(_, s)| (s - mean) * (s - mean)).sum::<f32>() / n;
    clamp(1.0 - 2.0 * var.sqrt(), 0.0, 1.0)
}

/// Reference ΔK implementation (whitepaper B.2.3), Rust port.
///
/// `reviews`: `(reviewer_reputation, score in [0,1])`
/// `replications`: `(success_count, total_attempts)`
/// Returns ΔK in `[0, bonus_max]`; `0.0` means "no minting".
pub fn compute_delta_k(
    sub: &Submission,
    graph: &CognitiveGraph,
    reviews: &[(f32, f32)],
    replications: (u32, u32),
    p: &DeltaKParams,
    now_days: f32,
) -> f32 {
    // 1. novelty
    let (max_sim, has_neighbors) = graph.max_sim_same_domain(&sub.embedding, sub.domain);
    let novelty = if !has_neighbors {
        1.0
    } else if max_sim > p.tau_dup {
        0.0
    } else {
        1.0 - max_sim
    };

    // 2. correctness (reputation-weighted), capped when reviews insufficient
    let wm = weighted_mean(reviews);
    let correctness = if reviews.len() < p.n_review_min {
        wm.min(p.c_cap)
    } else {
        wm
    };

    // 3. reproducibility
    let (success, total) = replications;
    let reproducibility = if total == 0 {
        agreement(reviews)
    } else {
        let r = success as f32 / total as f32;
        if (total as usize) < p.n_min {
            r.min(0.7)
        } else {
            r
        }
    };

    // 4. cross-domain bonus
    let bridged = graph.domains_within(&sub.embedding, 0.5);
    let avg_gap = 1.0f32; // placeholder metric at engine scale; refined later
    let bonus = clamp(
        1.0 + p.lam * (bridged.saturating_sub(1)) as f32 * avg_gap,
        1.0,
        p.bonus_max,
    );

    // 5. freshness
    let age = (now_days - sub.timestamp_days).max(0.0);
    let fresh = clamp((-p.decay * age).exp(), p.fresh_min, 1.0);

    let dk = novelty * correctness * reproducibility * bonus * fresh;
    if dk >= p.delta_k_min {
        dk
    } else {
        0.0 // gate -> no minting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(x: f32) -> Embedding {
        let mut e = [0.0f32; DIM];
        e[0] = x;
        e
    }

    #[test]
    fn first_node_in_domain_is_fully_novel() {
        let g = CognitiveGraph::new();
        let sub = Submission { embedding: unit(1.0), domain: 0, timestamp_days: 0.0 };
        let reviews = vec![(1.0, 0.9); 5];
        let dk = compute_delta_k(&sub, &g, &reviews, (3, 3), &DeltaKParams::default(), 0.0);
        assert!(dk > 0.0);
    }

    #[test]
    fn near_duplicate_gets_zero() {
        let mut g = CognitiveGraph::new();
        g.add(unit(1.0), 0);
        let sub = Submission { embedding: unit(1.0), domain: 0, timestamp_days: 0.0 };
        let reviews = vec![(1.0, 0.9); 5];
        let dk = compute_delta_k(&sub, &g, &reviews, (3, 3), &DeltaKParams::default(), 0.0);
        assert_eq!(dk, 0.0); // cos_sim == 1 > tau_dup -> novelty 0 -> gated to 0
    }

    #[test]
    fn add_assigns_monotonic_node_ids() {
        let mut g = CognitiveGraph::new();
        assert_eq!(g.add(unit(1.0), 0), 0);
        assert_eq!(g.add(unit(0.5), 1), 1);
        assert_eq!(g.add(unit(0.0), 0), 2);
        assert_eq!(g.id_at(0), Some(0));
        assert_eq!(g.id_at(2), Some(2));
        assert_eq!(g.id_at(3), None);
    }

    #[test]
    fn merkle_leaf_is_stable_and_id_prefixed() {
        let n = GraphNode { node_id: 7, embedding: unit(1.0), domain: 3 };
        let leaf = n.merkle_leaf();
        assert_eq!(leaf.len(), 8 + 32 + 4); // 44 bytes
        // id is at the front in BE
        assert_eq!(&leaf[0..8], &7u64.to_be_bytes());
        // embedding follows: 8 × f32, first one is 1.0
        let e0 = f32::from_be_bytes(leaf[8..12].try_into().unwrap());
        assert!((e0 - 1.0).abs() < 1e-6);
        // domain is the trailing u32 in BE
        assert_eq!(&leaf[40..44], &3u32.to_be_bytes());
    }

    #[test]
    fn low_scores_gated_out() {
        let g = CognitiveGraph::new();
        let sub = Submission { embedding: unit(1.0), domain: 0, timestamp_days: 0.0 };
        let reviews = vec![(1.0, 0.05); 5];
        let dk = compute_delta_k(&sub, &g, &reviews, (0, 3), &DeltaKParams::default(), 0.0);
        assert_eq!(dk, 0.0);
    }

    // ----- M26: k_nearest_with_ties -----

    /// Build a graph with 5 nodes whose embeddings, vs query=[1,0,0,0,0,0,0,0],
    /// have cosine similarities [0.95, 0.80, 0.70, 0.70, 0.60] in insertion order.
    ///
    /// `cos_sim(query, n) = n[0]` when both are unit-length AND query is
    /// `unit(1.0)`. So `unit(x)` works only for x in {-1, 0, +1}; for in-between
    /// values we have to fill in the other dimensions to keep the vector unit.
    fn embed(c: f32) -> Embedding {
        // cos_sim(query=[1,0,...], n) = n[0] / |n|. We pick n = (c, s, 0, ...)
        // where s = sqrt(1 - c^2), so |n| = 1 and cos_sim = c.
        let s = (1.0 - c * c).max(0.0).sqrt();
        let mut e = [0.0f32; DIM];
        e[0] = c;
        e[1] = s;
        e
    }

    fn build_knn_graph() -> CognitiveGraph {
        let mut g = CognitiveGraph::new();
        g.add(embed(0.95), 0); // id=0
        g.add(embed(0.80), 0); // id=1
        g.add(embed(0.70), 0); // id=2  (tied with id=3)
        g.add(embed(0.70), 0); // id=3  (tied with id=2)
        g.add(embed(0.60), 0); // id=4
        g
    }

    #[test]
    fn rank_by_cosine_is_cosine_desc_then_id_asc() {
        let g = build_knn_graph();
        let ranked = g.rank_by_cosine(&unit(1.0));
        // Expected order: 0.95 (id 0), 0.80 (id 1), 0.70 (id 2), 0.70 (id 3), 0.60 (id 4)
        let ids: Vec<u64> = ranked.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
        // Sanity: sims match insertion values (within 1e-5).
        let sims: Vec<f32> = ranked.iter().map(|(_, s)| *s).collect();
        for (got, want) in sims.iter().zip([0.95f32, 0.80, 0.70, 0.70, 0.60].iter()) {
            assert!((got - want).abs() < 1e-5, "got {got}, want {want}");
        }
    }

    #[test]
    fn k_nearest_with_ties_keeps_all_nodes_at_the_boundary() {
        let g = build_knn_graph();
        // k=2: cutoff = ranked[1].1 = 0.80. Nodes with sim >= 0.80 are id=0 and id=1.
        // Tied-at-0.70 nodes (id=2, id=3) are NOT included because their sim (0.70)
        // is strictly less than the cutoff (0.80). Result: exactly 2 nodes.
        let r2 = g.k_nearest_with_ties(&unit(1.0), 2);
        assert_eq!(r2.len(), 2, "k=2 result: {:?}", r2);
        let ids: Vec<u64> = r2.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![0, 1]);

        // Now k=3: cutoff = ranked[2].1 = 0.70. Nodes with sim >= 0.70 are
        // id={0,1,2,3} — four nodes returned because both tied 0.70 nodes
        // are at the boundary. The tie-breaker is node_id asc, so id=2 comes
        // before id=3. Result: exactly 4 nodes (1 over k).
        let r3 = g.k_nearest_with_ties(&unit(1.0), 3);
        assert_eq!(r3.len(), 4, "k=3 result: {:?}", r3);
        let ids: Vec<u64> = r3.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
    }

    #[test]
    fn k_nearest_with_ties_returns_all_when_n_leq_k() {
        let g = build_knn_graph(); // n = 5
        let r = g.k_nearest_with_ties(&unit(1.0), 5);
        assert_eq!(r.len(), 5);
        let r10 = g.k_nearest_with_ties(&unit(1.0), 10);
        assert_eq!(r10.len(), 5);
    }

    #[test]
    fn k_nearest_with_ties_returns_empty_for_k_zero() {
        let g = build_knn_graph();
        let r = g.k_nearest_with_ties(&unit(1.0), 0);
        assert!(r.is_empty());
    }

    #[test]
    fn k_nearest_with_ties_on_empty_graph() {
        let g = CognitiveGraph::new();
        let r = g.k_nearest_with_ties(&unit(1.0), 5);
        assert!(r.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Python bindings (behind the `python` feature). Exposes the same ΔK contract
// to `sim/` so large parameter sweeps run at Rust speed. Build with:
//   cargo build --release --features python
// See engine/build_python.sh.
// ---------------------------------------------------------------------------
#[cfg(feature = "python")]
mod python {
    use super::*;
    use pyo3::exceptions::PyValueError;
    use pyo3::prelude::*;

    fn to_embedding(v: &[f32]) -> PyResult<Embedding> {
        if v.len() != DIM {
            return Err(PyValueError::new_err(format!(
                "embedding must have exactly {DIM} dimensions, got {}",
                v.len()
            )));
        }
        let mut e = [0.0f32; DIM];
        e.copy_from_slice(v);
        Ok(e)
    }

    /// A cognitive graph living on the Rust side. Nodes are kept in Rust so
    /// repeated ΔK calls (e.g. a parameter sweep) avoid re-marshalling the graph.
    #[pyclass]
    struct PyGraph {
        graph: CognitiveGraph,
        params: DeltaKParams,
    }

    #[pymethods]
    impl PyGraph {
        #[new]
        fn new() -> Self {
            PyGraph {
                graph: CognitiveGraph::new(),
                params: DeltaKParams::default(),
            }
        }

        fn add(&mut self, embedding: Vec<f32>, domain: u32) -> PyResult<()> {
            let emb = to_embedding(&embedding)?;
            let _id = self.graph.add(emb, domain);
            Ok(())
        }

        fn __len__(&self) -> usize {
            self.graph.len()
        }

        /// Override any subset of the governance parameters (B.2.3). Unspecified
        /// keyword args keep their current value, enabling cheap parameter sweeps.
        #[pyo3(signature = (tau_dup=None, n_review_min=None, c_cap=None, n_min=None,
                            lam=None, bonus_max=None, decay=None, fresh_min=None,
                            delta_k_min=None))]
        #[allow(clippy::too_many_arguments)]
        fn set_params(
            &mut self,
            tau_dup: Option<f32>,
            n_review_min: Option<usize>,
            c_cap: Option<f32>,
            n_min: Option<usize>,
            lam: Option<f32>,
            bonus_max: Option<f32>,
            decay: Option<f32>,
            fresh_min: Option<f32>,
            delta_k_min: Option<f32>,
        ) {
            if let Some(v) = tau_dup { self.params.tau_dup = v; }
            if let Some(v) = n_review_min { self.params.n_review_min = v; }
            if let Some(v) = c_cap { self.params.c_cap = v; }
            if let Some(v) = n_min { self.params.n_min = v; }
            if let Some(v) = lam { self.params.lam = v; }
            if let Some(v) = bonus_max { self.params.bonus_max = v; }
            if let Some(v) = decay { self.params.decay = v; }
            if let Some(v) = fresh_min { self.params.fresh_min = v; }
            if let Some(v) = delta_k_min { self.params.delta_k_min = v; }
        }

        /// Compute ΔK for one submission against the current graph and params.
        #[pyo3(signature = (embedding, domain, reviews, repl_success, repl_total,
                            timestamp_days=0.0, now_days=0.0))]
        #[allow(clippy::too_many_arguments)]
        fn compute_delta_k(
            &self,
            embedding: Vec<f32>,
            domain: u32,
            reviews: Vec<(f32, f32)>,
            repl_success: u32,
            repl_total: u32,
            timestamp_days: f32,
            now_days: f32,
        ) -> PyResult<f32> {
            let sub = Submission {
                embedding: to_embedding(&embedding)?,
                domain,
                timestamp_days,
            };
            Ok(super::compute_delta_k(
                &sub,
                &self.graph,
                &reviews,
                (repl_success, repl_total),
                &self.params,
                now_days,
            ))
        }
    }

    #[pymodule]
    fn zhixing_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<PyGraph>()?;
        m.add("DIM", DIM)?;
        Ok(())
    }
}

