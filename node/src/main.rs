//! Reference-node demo: build genesis, produce a few blocks with a mix of novel
//! and duplicate submissions, and print the resulting chain — a deterministic,
//! runnable PoK chain over the ΔK engine.
//!
//!   cargo run --release --bin node

use zhixing_engine::{DeltaKParams, DIM};
use zhixing_node::{hex, Block, Chain, Genesis, Review, SubmissionTx, MICRO};

type Emb = [f32; DIM];

fn unit(dim: usize) -> Emb {
    let mut e = [0.0f32; DIM];
    e[dim % DIM] = 1.0;
    e
}

/// A blended embedding pointing between two axes (used for cross-domain bridges).
fn blend(a: usize, b: usize) -> Emb {
    let mut e = [0.0f32; DIM];
    let s = std::f32::consts::FRAC_1_SQRT_2;
    e[a % DIM] = s;
    e[b % DIM] = s;
    e
}

fn reviews(scores: &[(u64, f32)]) -> Vec<Review> {
    scores
        .iter()
        .map(|(id, s)| Review { reviewer: *id, score: *s })
        .collect()
}

fn tx(author: u64, emb: Emb, domain: u32, revs: Vec<Review>, repl: (u32, u32), day: f32) -> SubmissionTx {
    SubmissionTx {
        author,
        embedding: emb,
        domain,
        stake: 2 * MICRO,
        reviews: revs,
        repl_success: repl.0,
        repl_total: repl.1,
        timestamp_days: day,
    }
}

fn main() {
    let g = Genesis {
        accounts: vec![(1, 30 * MICRO), (2, 30 * MICRO), (3, 30 * MICRO)],
        reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
        seed_nodes: vec![(unit(0), 0)], // domain 0 seeded, so a domain-0 dup is a duplicate
        params: DeltaKParams::default(),
        base_emission_micro: 8 * MICRO,
        slash_bps: 10_000,
        timestamp_days: 0.0,
    };
    let mut chain = Chain::new(g);
    println!("genesis  head={}  supply={} COG", short(&chain.head), cog(chain.state.supply));

    // Block 1: two genuine, novel submissions in fresh domains -> accepted.
    let b1 = Block {
        height: 1,
        prev_hash: chain.head,
        timestamp_days: 1.0,
        txs: vec![
            tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
            tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        ],
    };

    // Block 2: a cross-domain bridge (novel + bonus) and a low-quality/duplicate
    // spam submission in domain 0 -> slashed.
    let b2 = |chain: &Chain| Block {
        height: 2,
        prev_hash: chain.head,
        timestamp_days: 2.0,
        txs: vec![
            tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 2.0),
            tx(1, unit(0), 0, reviews(&[(10, 0.7), (11, 0.6), (12, 0.65)]), (0, 3), 2.0),
        ],
    };

    for (label, blk) in [("block 1", b1)].into_iter() {
        commit_and_print(&mut chain, label, blk);
    }
    let blk2 = b2(&chain);
    commit_and_print(&mut chain, "block 2", blk2);

    println!("\n--- chain summary ---");
    println!("height          {}", chain.state.height);
    println!("head            {}", short(&chain.head));
    println!("state_root      {}", short(&chain.state.state_root()));
    println!("supply          {} COG", cog(chain.state.supply));
    println!("treasury        {} COG", cog(chain.state.treasury));
    println!("graph nodes     {}", chain.state.graph.len());
    println!("supply conserved {}", chain.state.supply_conserved());
    println!("\naccounts:");
    for (id, a) in &chain.state.accounts {
        println!(
            "  #{id}  bal={:>7} COG  earned={:>5}  slashed={:>5}  acc={}/{}",
            cog(a.balance),
            cog(a.earned_total),
            cog(a.slashed_total),
            a.accepted,
            a.submissions
        );
    }
    println!("\nreviewer reputations:");
    for (id, rep) in &chain.state.reviewers {
        println!("  #{id}  {rep:.3}");
    }
}

fn commit_and_print(chain: &mut Chain, label: &str, blk: Block) {
    match chain.commit(&blk) {
        Ok(r) => {
            println!(
                "\n{label}: h={} accepted={} rejected={} minted={} COG slashed={} COG",
                r.height,
                r.accepted,
                r.rejected,
                cog(r.minted),
                cog(r.slashed)
            );
            for t in &r.txs {
                println!(
                    "  author #{}  {}  ΔK={:.4}  minted={} slashed={}",
                    t.author,
                    if t.accepted { "ACCEPT" } else { "REJECT" },
                    t.delta_k,
                    cog(t.minted),
                    cog(t.slashed)
                );
            }
        }
        Err(e) => println!("\n{label}: REJECTED — {e}"),
    }
}

fn cog(micro: u64) -> String {
    format!("{}.{:06}", micro / MICRO, micro % MICRO)
}

fn short(h: &[u8]) -> String {
    let s = hex(h);
    format!("{}…{}", &s[..8], &s[s.len() - 6..])
}
