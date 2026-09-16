//! Reference-node CLI.
//!
//!   cargo run --release --bin node -- demo             # in-memory demo chain
//!   cargo run --release --bin node -- run  --dir DIR   # persistent chain (block log)
//!   cargo run --release --bin node -- status --dir DIR # replay log, print state
//!
//! `run` is durable: the first invocation seeds a few demo blocks into
//! DIR/blocks.log; every later `run`/`status` replays that log and reconstructs
//! byte-identical state (same state_root) — the point of the persistence layer.

use std::process::exit;

use zhixing_engine::{DeltaKParams, DIM};
use zhixing_node::store::BlockLog;
use zhixing_node::{hex, Block, Chain, Genesis, Review, SubmissionTx, MICRO};

type Emb = [f32; DIM];

fn unit(dim: usize) -> Emb {
    let mut e = [0.0f32; DIM];
    e[dim % DIM] = 1.0;
    e
}

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

/// The fixed genesis of this reference network (a network constant: both writers
/// and replayers must reconstruct it identically).
fn demo_genesis() -> Genesis {
    Genesis {
        accounts: vec![(1, 30 * MICRO), (2, 30 * MICRO), (3, 30 * MICRO)],
        reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
        seed_nodes: vec![(unit(0), 0)],
        params: DeltaKParams::default(),
        base_emission_micro: 8 * MICRO,
        slash_bps: 10_000,
        timestamp_days: 0.0,
    }
}

/// The demo block sequence (built against the chain's current head).
fn demo_blocks(chain: &Chain) -> Vec<Block> {
    let b1 = Block {
        height: 1,
        prev_hash: chain.head,
        timestamp_days: 1.0,
        txs: vec![
            tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
            tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        ],
    };
    // block 2 prev_hash is block 1's hash
    let b2 = Block {
        height: 2,
        prev_hash: b1.hash(),
        timestamp_days: 2.0,
        txs: vec![
            tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 2.0),
            tx(1, unit(0), 0, reviews(&[(10, 0.7), (11, 0.6), (12, 0.65)]), (0, 3), 2.0),
        ],
    };
    vec![b1, b2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("demo");
    match cmd {
        "demo" => cmd_demo(),
        "run" => cmd_run(dir_arg(&args)),
        "status" => cmd_status(dir_arg(&args)),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("unknown command: {other}\n");
            usage();
            exit(2);
        }
    }
}

fn dir_arg(args: &[String]) -> String {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dir" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        i += 1;
    }
    eprintln!("this command requires --dir <path>");
    exit(2);
}

fn usage() {
    eprintln!("zhixing reference node");
    eprintln!("  node demo               run an in-memory demo chain");
    eprintln!("  node run    --dir DIR   persistent chain (seeds demo blocks once, then replays)");
    eprintln!("  node status --dir DIR   replay the block log and print state");
}

fn cmd_demo() {
    let mut chain = Chain::new(demo_genesis());
    println!("genesis  head={}  supply={} COG", short(&chain.head), cog(chain.state.supply));
    for blk in demo_blocks(&chain) {
        let label = format!("block {}", blk.height);
        commit_print(&mut chain, None, &label, blk);
    }
    print_summary(&chain);
}

fn cmd_run(dir: String) {
    let path = format!("{dir}/blocks.log");
    let log = BlockLog::open(&path).unwrap_or_else(|e| fail("open log", e));
    let blocks = log.read_all().unwrap_or_else(|e| fail("read log", e));

    let mut chain = Chain::replay(demo_genesis(), &blocks)
        .unwrap_or_else(|e| fail_chain("replay log", e));

    if chain.state.height == 0 {
        println!("empty log at {path} — seeding demo blocks\n");
        for blk in demo_blocks(&chain) {
            let label = format!("block {}", blk.height);
            commit_print(&mut chain, Some(&log), &label, blk);
        }
    } else {
        println!(
            "replayed {} block(s) from {path} (head={})\n",
            chain.state.height,
            short(&chain.head)
        );
    }
    print_summary(&chain);
}

fn cmd_status(dir: String) {
    let path = format!("{dir}/blocks.log");
    let log = BlockLog::open(&path).unwrap_or_else(|e| fail("open log", e));
    let blocks = log.read_all().unwrap_or_else(|e| fail("read log", e));
    let chain = Chain::replay(demo_genesis(), &blocks)
        .unwrap_or_else(|e| fail_chain("replay log", e));
    println!("replayed {} block(s) from {path}", chain.state.height);
    print_summary(&chain);
}

fn commit_print(chain: &mut Chain, log: Option<&BlockLog>, label: &str, blk: Block) {
    match chain.commit(&blk) {
        Ok(r) => {
            if let Some(l) = log {
                l.append(&blk).unwrap_or_else(|e| fail("append block", e));
            }
            println!(
                "{label}: h={} accepted={} rejected={} minted={} COG slashed={} COG",
                r.height, r.accepted, r.rejected, cog(r.minted), cog(r.slashed)
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
            println!();
        }
        Err(e) => println!("{label}: REJECTED — {e}\n"),
    }
}

fn print_summary(chain: &Chain) {
    println!("--- chain summary ---");
    println!("height           {}", chain.state.height);
    println!("head             {}", short(&chain.head));
    println!("state_root       {}", short(&chain.state.state_root()));
    println!("supply           {} COG", cog(chain.state.supply));
    println!("treasury         {} COG", cog(chain.state.treasury));
    println!("graph nodes      {}", chain.state.graph.len());
    println!("supply conserved {}", chain.state.supply_conserved());
    println!("\naccounts:");
    for (id, a) in &chain.state.accounts {
        println!(
            "  #{id}  bal={:>10} COG  earned={:>8}  slashed={:>8}  acc={}/{}",
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

fn cog(micro: u64) -> String {
    format!("{}.{:06}", micro / MICRO, micro % MICRO)
}

fn short(h: &[u8]) -> String {
    let s = hex(h);
    format!("{}…{}", &s[..8], &s[s.len() - 6..])
}

fn fail(ctx: &str, e: std::io::Error) -> ! {
    eprintln!("error: {ctx}: {e}");
    exit(1);
}

fn fail_chain(ctx: &str, e: zhixing_node::ChainError) -> ! {
    eprintln!("error: {ctx}: {e}");
    exit(1);
}
