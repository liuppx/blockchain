//! Throughput benchmark for the ΔK / cognitive-graph hot path.
//!
//! Builds a graph of N nodes across D domains, then computes ΔK for M
//! submissions (each triggering a same-domain kNN scan + cross-domain probe).
//! Prints wall-clock time and submissions/sec. Deterministic (seeded RNG).
//!
//! Run: cargo run --release --bin bench -- [N] [M] [D]

use std::time::Instant;
use zhixing_engine::{
    compute_delta_k, CognitiveGraph, DeltaKParams, Embedding, GraphNode, Submission, DIM,
};

/// Tiny deterministic xorshift64* RNG (std-only, no crates).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    #[inline]
    fn next_f32(&mut self) -> f32 {
        // uniform in [0,1)
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    #[inline]
    fn gauss(&mut self) -> f32 {
        // Box-Muller (one value)
        let u1 = self.next_f32().max(1e-7);
        let u2 = self.next_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

fn rand_unit(rng: &mut Rng) -> Embedding {
    let mut e = [0.0f32; DIM];
    let mut norm = 0.0f32;
    for v in e.iter_mut() {
        *v = rng.gauss();
        norm += *v * *v;
    }
    let norm = norm.sqrt().max(1e-9);
    for v in e.iter_mut() {
        *v /= norm;
    }
    e
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let m: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let d: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(8);

    let mut rng = Rng::new(42);
    let p = DeltaKParams::default();

    // build graph
    let mut graph = CognitiveGraph::with_capacity(n);
    for _ in 0..n {
        graph.add(GraphNode {
            embedding: rand_unit(&mut rng),
            domain: (rng.next_u64() % d as u64) as u32,
        });
    }

    // pre-generate submissions + reviews so timing isolates the ΔK hot path
    let reviews: Vec<(f32, f32)> = (0..5).map(|_| (1.0, 0.8)).collect();
    let subs: Vec<Submission> = (0..m)
        .map(|_| Submission {
            embedding: rand_unit(&mut rng),
            domain: (rng.next_u64() % d as u64) as u32,
            timestamp_days: 0.0,
        })
        .collect();

    let start = Instant::now();
    let mut acc = 0.0f64; // consume result so the loop isn't optimized away
    for s in &subs {
        acc += compute_delta_k(s, &graph, &reviews, (2, 3), &p, 10.0) as f64;
    }
    let elapsed = start.elapsed();

    let secs = elapsed.as_secs_f64();
    let per_sec = m as f64 / secs;
    println!("rust engine: N={n} nodes, M={m} submissions, D={d} domains");
    println!("  elapsed: {secs:.4}s   throughput: {per_sec:.0} submissions/sec");
    println!("  (checksum {:.4})", acc);
}
