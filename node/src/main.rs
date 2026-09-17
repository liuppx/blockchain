//! Reference-node CLI.
//!
//!   cargo run --release --bin node -- demo             # in-memory demo chain
//!   cargo run --release --bin node -- build            # mempool builds a block
//!   cargo run --release --bin node -- prove            # light-client Merkle proof
//!   cargo run --release --bin node -- bft              # BFT commit certificate over a block
//!   cargo run --release --bin node -- gossip           # gossip + anti-entropy sync (in-proc + loopback TCP)
//!   cargo run --release --bin node -- light            # light client: follow the validator set without full replay
//!   cargo run --release --bin node -- staking          # bond/unbond: stake-bound validator power + unbonding
//!   cargo run --release --bin node -- slashing         # slash an equivocating validator's bonded stake to the treasury
//!   cargo run --release --bin node -- run  --dir DIR   # persistent chain (block log)
//!   cargo run --release --bin node -- status --dir DIR # replay log, print state
//!   cargo run --release --bin node -- certs  --dir DIR # persist certified chain, re-verify finality
//!
//! `run` is durable: the first invocation seeds a few demo blocks into
//! DIR/blocks.log; every later `run`/`status` replays that log and reconstructs
//! byte-identical state (same state_root) — the point of the persistence layer.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{TcpListener, TcpStream};
use std::process::exit;
use std::sync::mpsc;
use std::thread;

use zhixing_engine::{DeltaKParams, DIM};
use zhixing_node::consensus::{commit_block, detect_equivocation};
use zhixing_node::driver::ChainDriver;
use zhixing_node::mempool::Mempool;
use zhixing_node::merkle;
use zhixing_node::net::{read_msg, write_msg, GossipMsg, GossipNode, Network};
use zhixing_node::round::Sim;
use zhixing_node::store::{BlockLog, CertLog};
use zhixing_node::validator::{Validator, ValidatorSet, ValidatorUpdate};
use zhixing_node::{
    hex, Block, BondKind, Chain, Genesis, Keypair, Review, SlashEvidence, StakeOp, SubmissionTx,
    ValidatorTracker, Vote, VoteType, MICRO,
};

type Emb = [f32; DIM];

/// Deterministic keypair for account `id` (demo only; real keys come from a
/// CSPRNG / HSM). Both the writer and any replayer derive the same keys.
fn kp(id: u64) -> Keypair {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&id.to_le_bytes());
    Keypair::from_seed(seed)
}

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

/// Build and sign a submission with `author`'s key.
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
        signature: [0u8; 64],
    }
    .signed(&kp(author))
}

/// The fixed genesis of this reference network (a network constant: both writers
/// and replayers must reconstruct it identically).
fn demo_genesis() -> Genesis {
    let (vset, _) = demo_validators();
    Genesis {
        accounts: vec![
            (1, 30 * MICRO, kp(1).public()),
            (2, 30 * MICRO, kp(2).public()),
            (3, 30 * MICRO, kp(3).public()),
        ],
        reviewers: vec![(10, 1.0), (11, 1.0), (12, 1.0)],
        seed_nodes: vec![(unit(0), 0)],
        params: DeltaKParams::default(),
        base_emission_micro: 8 * MICRO,
        slash_bps: 10_000,
        timestamp_days: 0.0,
        validators: vset
            .validators()
            .iter()
            .map(|v| (v.id, v.pubkey, v.power))
            .collect(),
    }
}

/// The fixed validator set of this reference network (ids 21..=24, equal power)
/// and their signing-key seeds — a network constant both producers and replayers
/// reconstruct identically.
fn demo_validators() -> (ValidatorSet, BTreeMap<u64, [u8; 32]>) {
    let ids = [21u64, 22, 23, 24];
    let vset = ValidatorSet::new(
        ids.iter()
            .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
            .collect(),
    );
    let seeds = ids
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    (vset, seeds)
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
        validator_updates: Vec::new(),
        stake_ops: Vec::new(),
        slashing_evidence: Vec::new(),
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
        validator_updates: Vec::new(),
        stake_ops: Vec::new(),
        slashing_evidence: Vec::new(),
    };
    vec![b1, b2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("demo");
    match cmd {
        "demo" => cmd_demo(),
        "build" => cmd_build(),
        "prove" => cmd_prove(),
        "bft" => cmd_bft(),
        "live" => cmd_live(),
        "chain" => cmd_chain(),
        "validators" => cmd_validators(),
        "staking" => cmd_staking(),
        "slashing" => cmd_slashing(),
        "gossip" => cmd_gossip(),
        "light" => cmd_light(),
        "run" => cmd_run(dir_arg(&args)),
        "status" => cmd_status(dir_arg(&args)),
        "certs" => cmd_certs(dir_arg(&args)),
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
    eprintln!("  node build              feed a mempool (scrambled order) and build one block");
    eprintln!("  node prove              build+verify a light-client Merkle proof of an account");
    eprintln!("  node bft                4 validators certify a block; show fault tolerance + equivocation");
    eprintln!("  node live               drive the BFT round FSM to a commit (incl. a dead proposer)");
    eprintln!("  node chain              grow a BFT-certified chain height by height (mempool -> consensus -> commit)");
    eprintln!("  node validators         grow a chain across on-chain validator-set changes (add/remove)");
    eprintln!("  node staking            bond stake to gain validator power; unbond through a delayed withdrawal");
    eprintln!("  node slashing           slash an equivocating validator's bonded stake to the treasury");
    eprintln!("  node gossip             gossip + anti-entropy sync: fresh nodes catch up to a certified chain (in-proc + TCP)");
    eprintln!("  node light              light client: follow the validator set across heights without full replay");
    eprintln!("  node run    --dir DIR   persistent chain (seeds demo blocks once, then replays)");
    eprintln!("  node status --dir DIR   replay the block log and print state");
    eprintln!("  node certs  --dir DIR   persist a certified chain (blocks+certs) and re-verify finality on reload");
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

/// Demonstrate the deterministic mempool: submit transactions in a scrambled
/// order, let the builder lay them out canonically (by tx hash), and show that
/// the resulting block hash does not depend on arrival order.
fn cmd_build() {
    let chain = Chain::new(demo_genesis());
    // three novel submissions, offered to the pool in a deliberately odd order
    let candidates = vec![
        tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
    ];

    let mut mp = Mempool::new(16);
    println!("submitting {} txs to the mempool (arrival order):", candidates.len());
    for t in &candidates {
        let h = mp.insert(&chain, t.clone()).unwrap();
        println!("  author #{}  tx={}", t.author, short(&h));
    }

    let blk = mp.build_block(&chain, 1.0).expect("pool builds a block");
    println!("\nbuilder laid out block {} (canonical, hash-ordered):", blk.height);
    for t in &blk.txs {
        println!("  author #{}  tx={}", t.author, short(&t.hash()));
    }
    println!("block hash = {}", short(&blk.hash()));
    println!("(arrival order does not affect this hash — see mempool tests)\n");

    let mut chain = chain;
    commit_print(&mut chain, None, &format!("block {}", blk.height), blk);
    print_summary(&chain);
}

/// Demonstrate a light-client inclusion proof: run the demo chain, then verify
/// one account against the Merkle state root using only that account's contents
/// and a proof — no full state needed.
fn cmd_prove() {
    let mut chain = Chain::new(demo_genesis());
    for blk in demo_blocks(&chain) {
        chain.commit(&blk).unwrap();
    }
    let root = chain.state.merkle_root();
    println!("merkle_root = {}\n", short(&root));

    let id = 1u64;
    let acct = chain.state.accounts.get(&id).expect("account exists").clone();
    let proof = chain.state.account_proof(id).expect("proof exists");
    let leaf = merkle::leaf_hash(&acct.merkle_leaf(id));

    println!("light client is told: account #{id} balance={} COG", cog(acct.balance));
    println!("proof: {} sibling hash(es) up to the root", proof.steps.len());
    let ok = merkle::verify(&root, &leaf, &proof);
    println!("verify against merkle_root -> {ok}");

    // negative case: a lie about the balance must fail
    let mut lying = acct.clone();
    lying.balance += 1_000 * MICRO;
    let lie = merkle::verify(&root, &merkle::leaf_hash(&lying.merkle_leaf(id)), &proof);
    println!("verify an inflated balance -> {lie} (must be false)");
}

/// Demonstrate BFT finality: 4 equal-power validators certify a block. Shows
/// the deterministic proposer, a quorum commit that tolerates one crash, a
/// sub-quorum that fails to commit, and equivocation being caught.
fn cmd_bft() {
    // a fresh chain and one built block to certify
    let chain = Chain::new(demo_genesis());
    let mut mp = Mempool::new(16);
    for t in [
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
    ] {
        mp.insert(&chain, t).unwrap();
    }
    let blk = mp.build_block(&chain, 1.0).expect("a block to certify");

    // 4 equal-power validators (ids 21..=24)
    let ids = [21u64, 22, 23, 24];
    let keys: BTreeMap<u64, Keypair> = ids.iter().map(|&id| (id, kp(id))).collect();
    let vset = ValidatorSet::new(
        ids.iter()
            .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
            .collect(),
    );
    println!("validators   {ids:?}  (equal power)");
    println!("total power  {}   quorum {} (> 2/3)", vset.total_power(), vset.quorum());
    println!("proposer(h=1) #{}\n", vset.proposer_for(blk.height).unwrap());
    println!("certifying block {} ({})", blk.height, short(&blk.hash()));

    // happy path: one validator crashes (24), the other 3 still commit
    match commit_block(&vset, &keys, &blk, 0, &[21, 22, 23]) {
        Some(c) => {
            let power = c.verify(&vset).unwrap();
            println!(
                "  3 of 4 precommit (v24 crashed) -> COMMIT, power {}/{} ✓ finalized",
                power,
                vset.total_power()
            );
        }
        None => println!("  unexpected: quorum not reached"),
    }

    // sub-quorum: only 2 of 4 -> no commit
    match commit_block(&vset, &keys, &blk, 0, &[21, 22]) {
        Some(_) => println!("  2 of 4 -> unexpectedly committed"),
        None => println!("  2 of 4 precommit -> NO commit (below quorum) ✓ safe"),
    }

    // equivocation: a conflicting block certified by an overlapping quorum
    let blk2 = Block { prev_hash: [7u8; 32], ..blk.clone() }; // different hash, same height
    if let (Some(c1), Some(c2)) = (
        commit_block(&vset, &keys, &blk, 0, &[21, 22, 23]),
        commit_block(&vset, &keys, &blk2, 0, &[21, 22, 24]),
    ) {
        let guilty = detect_equivocation(&c1, &c2);
        println!("  two conflicting commits require double-signers -> equivocation by {guilty:?} (slashable)");
    }
}

/// Demonstrate BFT *liveness*: the round state machine drives a set of
/// validators to a commit over an in-process message bus — first with everyone
/// honest (commits at round 0), then with the round-0 proposer dead (a timeout
/// forces a round change and a live proposer finalizes the same block).
fn cmd_live() {
    let chain = Chain::new(demo_genesis());
    let mut mp = Mempool::new(16);
    for t in [
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
    ] {
        mp.insert(&chain, t).unwrap();
    }
    let blk = mp.build_block(&chain, 1.0).expect("a candidate block");

    let ids = [21u64, 22, 23, 24];
    let vset = ValidatorSet::new(
        ids.iter()
            .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
            .collect(),
    );
    let mkkeys = || -> BTreeMap<u64, Keypair> { ids.iter().map(|&id| (id, kp(id))).collect() };
    println!("validators {ids:?}  quorum {} (> 2/3)", vset.quorum());
    println!("candidate block {} ({})\n", blk.height, short(&blk.hash()));

    // scenario 1: everyone honest
    let mut sim = Sim::new(vset.clone(), mkkeys(), blk.height, blk.clone(), &BTreeSet::new());
    let dec = sim.run();
    report_live("all honest", &sim, &dec, &vset, &blk);

    // scenario 2: the round-0 proposer is offline
    let dead = vset.proposer_for_round(blk.height, 0).unwrap();
    let mut silent = BTreeSet::new();
    silent.insert(dead);
    let mut sim2 = Sim::new(vset.clone(), mkkeys(), blk.height, blk.clone(), &silent);
    let dec2 = sim2.run();
    println!("\nround-0 proposer #{dead} is offline:");
    report_live("dead proposer", &sim2, &dec2, &vset, &blk);
}

fn report_live(
    label: &str,
    sim: &Sim,
    dec: &BTreeMap<u64, zhixing_node::consensus::Commit>,
    vset: &ValidatorSet,
    blk: &Block,
) {
    let agreed = dec.values().all(|c| c.block_hash == blk.hash());
    let verified = dec.values().all(|c| c.verify(vset).is_ok());
    println!(
        "  {label}: {} validator(s) decided at round {} — agree={} certificate_valid={}",
        dec.len(),
        sim.max_round(),
        agreed,
        verified
    );
    if let Some(c) = dec.values().next() {
        println!("    finalized {} with a quorum certificate", short(&c.block_hash));
    }
}

/// Demonstrate the full pipeline as a growing, BFT-certified chain: submit
/// transactions to a mempool, then have a validator set finalize them one block
/// per height — each committed block backed by a verifiable > 2/3 certificate.
/// Then show the chain still advancing with a crashed validator, and stalling
/// (without ever forging a block) when a quorum is impossible.
fn cmd_chain() {
    let ids = [21u64, 22, 23, 24];
    let vset = ValidatorSet::new(
        ids.iter()
            .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
            .collect(),
    );
    let seeds: BTreeMap<u64, [u8; 32]> = ids
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();

    // one block per height so the chain visibly grows tx by tx
    let mut d = ChainDriver::new(demo_genesis(), seeds, 1);
    for t in [
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
    ] {
        d.submit(t).unwrap();
    }
    println!("validators {ids:?}  quorum {} (> 2/3)\n", vset.quorum());
    println!("growing the chain (all honest):");
    let mut day = 1.0;
    while let Some(c) = d.produce(day, &BTreeSet::new()).unwrap() {
        let power = c.verify(&vset).unwrap();
        println!(
            "  height {}  block {}  cert power {}/{}  state_root {}",
            c.height,
            short(&c.block_hash),
            power,
            vset.total_power(),
            short(&d.chain.state.state_root())
        );
        day += 1.0;
    }
    println!("head {} at height {}\n", short(&d.head()), d.height());

    // fault tolerance: submit one more, finalize it with a validator offline
    d.submit(tx(1, unit(4), 4, reviews(&[(10, 0.8), (11, 0.75), (12, 0.82)]), (0, 3), day)).unwrap();
    let mut silent = BTreeSet::new();
    silent.insert(24u64);
    match d.produce(day, &silent) {
        Ok(Some(c)) => println!(
            "with validator #24 offline: height {} still finalized (power {}/{}) ✓ liveness",
            c.height,
            c.verify(&vset).unwrap(),
            vset.total_power()
        ),
        other => println!("unexpected: {other:?}"),
    }

    // safety: with two offline, quorum is impossible -> stall, no block forged
    d.submit(tx(2, unit(5), 5, reviews(&[(10, 0.85), (11, 0.8), (12, 0.88)]), (3, 3), day + 1.0)).unwrap();
    let mut two_down = BTreeSet::new();
    two_down.insert(23u64);
    two_down.insert(24u64);
    let h_before = d.height();
    match d.produce(day + 1.0, &two_down) {
        Err(e) => println!(
            "with #23 and #24 offline: {e} — chain stays at height {} ✓ safety",
            d.height()
        ),
        Ok(_) => println!("unexpected: a block was produced below quorum"),
    }
    debug_assert_eq!(d.height(), h_before);
    println!("\nfinal head {} · height {} · {} certificates", short(&d.head()), d.height(), d.certificates().len());
}

/// Demonstrate a validator handoff on a live, BFT-certified chain: grow a few
/// heights under the genesis set, then admit and later remove a validator via
/// on-chain [`ValidatorUpdate`]s. Each change is certified by the set in force
/// *before* it and takes effect from the next height; a final replay re-verifies
/// finality following the very same handoffs.
fn cmd_validators() {
    // signing-key seeds are a superset (21..=25); the genesis set is 21..=24
    let seeds: BTreeMap<u64, [u8; 32]> = (21u64..=25)
        .map(|id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 1);
    for t in [
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
        tx(1, unit(4), 4, reviews(&[(10, 0.8), (11, 0.78), (12, 0.82)]), (3, 3), 1.0),
    ] {
        d.submit(t).unwrap();
    }

    let ids0: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!(
        "genesis validator set {ids0:?}  quorum {} (> 2/3)\n",
        d.chain.state.validators.quorum()
    );

    // height 1: a plain block under the genesis set of four
    produce_vreport(&mut d, 1.0);

    // admit validator #25: the change rides in the height-2 block but is
    // certified by the PRE-change set — the newcomer never votes on its arrival.
    println!("  staged: ADD validator #25 (takes effect next height)");
    d.stage_validator_update(ValidatorUpdate { id: 25, pubkey: kp(25).public(), power: 1 });
    produce_vreport(&mut d, 2.0);

    // remove validator #21, certified by the five-validator set now in force
    println!("  staged: REMOVE validator #21");
    d.stage_validator_update(ValidatorUpdate { id: 21, pubkey: kp(21).public(), power: 0 });
    produce_vreport(&mut d, 3.0);

    // one more plain height under the evolved set
    produce_vreport(&mut d, 4.0);

    // replay the whole certified chain, re-verifying finality height by height —
    // following the very same validator handoffs the live chain produced.
    let chain = Chain::replay_verified(demo_genesis(), d.blocks(), d.certificates())
        .unwrap_or_else(|e| fail_msg("verify finality across handoffs", &e));
    let final_ids: Vec<u64> = chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!(
        "\nreplay re-verified finality across every handoff ✓  final set {final_ids:?}  head {}",
        short(&chain.head)
    );
    debug_assert_eq!(chain.state.state_root(), d.chain.state.state_root());
}

/// Produce one height (all validators honest) and report which set certified it
/// and, if the set changed, what it becomes for the next height.
fn produce_vreport(d: &mut ChainDriver, day: f32) {
    let before = d.chain.state.validators.clone();
    let commit = d
        .produce(day, &BTreeSet::new())
        .unwrap_or_else(|e| fail_msg("produce height", &e))
        .expect("a block to produce");
    let power = commit.verify(&before).unwrap();
    let ids_before: Vec<u64> = before.validators().iter().map(|v| v.id).collect();
    println!(
        "  height {}  certified by {:?}  (power {}/{}, quorum {})",
        commit.height,
        ids_before,
        power,
        before.total_power(),
        before.quorum()
    );
    let ids_after: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    if ids_after != ids_before {
        println!(
            "    -> validator set for height {} is now {:?}  (quorum {})",
            commit.height + 1,
            ids_after,
            d.chain.state.validators.quorum()
        );
    }
}

/// Demonstrate staking-bound validator power: an account bonds $COG to become a
/// validator whose power equals its bonded stake, then unbonds through a
/// time-locked withdrawal (funds stay in the pool — still part of supply, still
/// slashable — until maturity, then return to the balance). Everything rides a
/// BFT-certified chain and is re-verified on replay.
fn cmd_staking() {
    use zhixing_node::UNBONDING_PERIOD;

    // signing-key seeds: genesis validators 21..=24 PLUS accounts 1..=3, so a
    // freshly-bonded account can sign consensus votes once its power is active.
    let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 2, 3, 21, 22, 23, 24]
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 1);

    let ids0: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!(
        "genesis validator set {ids0:?} (equal power); account #1 balance {} $COG, bonded pool {} $COG\n",
        cog(d.chain.state.accounts[&1].balance),
        cog(d.chain.state.bonded),
    );

    // height 1: account #1 bonds 6 $COG. The op rides a block certified by the
    // GENESIS set — the newcomer never votes on its own arrival. power == bond.
    println!("  height 1: account #1 BONDs 6 $COG  (power activates next height)");
    d.stage_stake_op(
        StakeOp { account: 1, kind: BondKind::Bond, amount: 6 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1)),
    );
    produce_stakereport(&mut d, 1.0);

    // height 2: #1 is now an active validator with power == its bond. It unbonds,
    // scheduling a delayed withdrawal that matures UNBONDING_PERIOD heights later.
    println!("\n  height 2: account #1 UNBONDs 6 $COG  (power removed next height; funds time-locked)");
    d.stage_stake_op(
        StakeOp { account: 1, kind: BondKind::Unbond, amount: 6 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1)),
    );
    produce_stakereport(&mut d, 2.0);
    let mature = 2 + UNBONDING_PERIOD;
    println!(
        "    scheduled withdrawal of 6 $COG matures at height {mature}; account #1 balance still {} $COG (locked in pool)",
        cog(d.chain.state.accounts[&1].balance),
    );

    // advance plain heights until the withdrawal matures; the released funds
    // return to the balance when the maturing height is applied.
    let mut day = 3.0;
    for h in 3..=mature {
        d.submit(tx(
            2,
            unit(h as usize + 1),
            h as u32 + 10,
            reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]),
            (3, 3),
            day,
        ))
        .unwrap();
        produce_stakereport(&mut d, day);
        day += 1.0;
    }
    println!(
        "\n  height {mature} applied: account #1 balance {} $COG, bonded pool {} $COG, unbonding queue {} entries",
        cog(d.chain.state.accounts[&1].balance),
        cog(d.chain.state.bonded),
        d.chain.state.unbonding.len(),
    );
    println!("  supply conserved throughout: {}", d.chain.state.supply_conserved());

    // replay the whole certified chain, re-verifying finality height by height.
    let chain = Chain::replay_verified(demo_genesis(), d.blocks(), d.certificates())
        .unwrap_or_else(|e| fail_msg("verify finality across staking", &e));
    println!(
        "\nreplay re-verified finality across the staking lifecycle ✓  head {}",
        short(&chain.head)
    );
    debug_assert_eq!(chain.state.state_root(), d.chain.state.state_root());
}

/// Produce one height (all validators honest) and report the set that certified
/// it, the bonded pool, and account #1's live validator power (== its bond).
fn produce_stakereport(d: &mut ChainDriver, day: f32) {
    let before: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    d.produce(day, &BTreeSet::new())
        .unwrap_or_else(|e| fail_msg("produce height", &e))
        .expect("a block to produce");
    let after: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!(
        "    height {} certified by {:?}; bonded pool {} $COG; account #1 power {} $COG",
        d.height(),
        before,
        cog(d.chain.state.bonded),
        cog(d.chain.state.validators.get(1).map(|v| v.power).unwrap_or(0)),
    );
    if after != before {
        println!("      -> validator set for the next height is now {after:?}");
    }
}

/// Demonstrate on-chain equivocation slashing (M18): a validator self-bonds,
/// then double-signs at a height. The cryptographic proof (two conflicting
/// precommits) is submitted on-chain; the chain seizes the offender's bonded
/// stake into the treasury (supply-neutral) and removes it from the validator
/// set at the next height. The whole certified chain then replays and re-verifies
/// finality to the same state root.
fn cmd_slashing() {
    // signing-key seeds: genesis validators 21..=24 PLUS accounts 1..=3, so a
    // freshly-bonded account can sign consensus votes once its power is active.
    let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 2, 3, 21, 22, 23, 24]
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 1);

    let supply0 = d.chain.state.supply;
    let ids0: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!(
        "genesis validator set {ids0:?}; treasury {} $COG, bonded pool {} $COG\n",
        cog(d.chain.state.treasury),
        cog(d.chain.state.bonded),
    );

    // height 1: account #1 bonds 6 $COG and becomes an active validator with
    // power == its bond, effective from height 2 (certified by the genesis set).
    println!("  height 1: account #1 BONDs 6 $COG  (becomes a validator next height)");
    d.stage_stake_op(
        StakeOp { account: 1, kind: BondKind::Bond, amount: 6 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1)),
    );
    produce_stakereport(&mut d, 1.0);

    // height 2: validator #1 equivocates — it precommits TWO different block
    // hashes at the same (height, round). Each vote is validly signed by #1's
    // key, so together they are un-forgeable proof of a double-sign. (In a live
    // network these are gathered from two conflicting commits via
    // `consensus::detect_equivocation`.)
    let bad_a = Vote::signed(1, 2, 0, [0xAAu8; 32], VoteType::Precommit, &kp(1));
    let bad_b = Vote::signed(1, 2, 0, [0xBBu8; 32], VoteType::Precommit, &kp(1));
    let evidence = SlashEvidence { vote_a: bad_a, vote_b: bad_b };
    println!(
        "\n  height 2: validator #1 DOUBLE-SIGNs (precommits {} and {} at h2/r0)",
        short(&[0xAAu8; 32]),
        short(&[0xBBu8; 32]),
    );
    println!("    -> submitting the proof on-chain; power {} $COG at stake", cog(6 * MICRO));

    d.stage_slashing_evidence(evidence);
    let commit = d
        .produce(2.0, &BTreeSet::new())
        .unwrap_or_else(|e| fail_msg("produce slashing block", &e))
        .expect("a slashing block is produced");

    println!(
        "    height 2 certified (commit binds {}); offender slashed",
        short(&commit.block_hash)
    );
    println!(
        "    treasury {} $COG (+{} seized), bonded pool {} $COG, validator #1 present: {}",
        cog(d.chain.state.treasury),
        cog(d.chain.state.treasury),
        cog(d.chain.state.bonded),
        d.chain.state.validators.get(1).is_some(),
    );
    let ids_after: Vec<u64> = d.chain.state.validators.validators().iter().map(|v| v.id).collect();
    println!("    validator set for the next height is now {ids_after:?}");
    println!(
        "\n  slash is supply-neutral (bonded -> treasury): supply {} unchanged: {}",
        cog(d.chain.state.supply),
        d.chain.state.supply == supply0 && d.chain.state.supply_conserved(),
    );

    // replay the whole certified chain, re-verifying finality height by height.
    let chain = Chain::replay_verified(demo_genesis(), d.blocks(), d.certificates())
        .unwrap_or_else(|e| fail_msg("verify finality across slashing", &e));
    println!(
        "\nreplay re-verified finality across the slash ✓  head {}",
        short(&chain.head)
    );
    debug_assert_eq!(chain.state.state_root(), d.chain.state.state_root());
}

/// Demonstrate the P2P network layer: fresh/lagging nodes converge to a
/// certified chain by anti-entropy sync, and a transaction floods to every node
/// by epidemic gossip — first over the deterministic in-process bus, then over
/// real loopback TCP sockets (every certificate re-verified on arrival).
fn cmd_gossip() {
    // a real certified chain to disseminate (produced exactly as `certs` does)
    let (_, seeds) = demo_validators();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 1);
    for t in [
        tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
        tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
    ] {
        d.submit(t).unwrap();
    }
    d.produce_until_drained(1.0, 16).unwrap_or_else(|e| fail_msg("produce chain", &e));
    let blocks = d.blocks().to_vec();
    let certs = d.certificates().to_vec();
    println!(
        "seed produced a certified chain: height {} · head {}\n",
        blocks.len(),
        short(&d.head())
    );

    // --- in-process gossip: node 1 seeded, nodes 2..4 fresh, all connected ----
    let ids = [1u64, 2, 3, 4];
    let mut seed_node = GossipNode::new(1, demo_genesis(), 16, ids);
    assert!(seed_node.load_certified(&blocks, &certs));
    let others: Vec<GossipNode> =
        [2u64, 3, 4].iter().map(|&id| GossipNode::new(id, demo_genesis(), 16, ids)).collect();
    let mut net = Network::new(std::iter::once(seed_node).chain(others).collect());

    println!("in-process anti-entropy sync (node 1 seeded, nodes 2–4 fresh):");
    net.announce_all();
    let delivered = net.run();
    for id in ids {
        let n = net.node(id);
        println!("  node {id}  height {}  head {}", n.height(), short(&n.head()));
    }
    println!(
        "  -> converged={} after {delivered} messages; state_root {}\n",
        net.converged(),
        short(&net.node(4).chain.state.state_root())
    );

    // epidemic tx gossip: inject one tx at node 3, watch it reach every mempool
    let t = tx(1, unit(5), 5, reviews(&[(10, 0.8), (11, 0.78), (12, 0.82)]), (3, 3), 4.0);
    let h = t.hash();
    net.submit(3, t);
    net.run();
    let reached: Vec<u64> = ids.iter().copied().filter(|&id| net.node(id).mempool.contains(&h)).collect();
    println!("epidemic tx gossip: tx {} injected at node 3", short(&h));
    println!("  -> present in mempools of nodes {reached:?}\n");

    // M19: equivocation-evidence gossip — any node that observes a double-sign
    // floods the proof to every peer; the next proposer drains the pending
    // pool into its driver and admits the evidence into a slashing block.
    // The gossip layer is purely shape-based: well-formed evidence floods
    // regardless. Full cryptographic validation (offender is active, both
    // signatures verify) happens at apply_evidence in chain.commit; the unit
    // test `gossiped_evidence_lands_in_the_next_proposed_block` exercises the
    // end-to-end slash.
    let offender = 1u64;
    let ev = SlashEvidence {
        vote_a: Vote::signed(offender, 2, 0, [0xAAu8; 32], VoteType::Precommit, &kp(offender)),
        vote_b: Vote::signed(offender, 2, 0, [0xBBu8; 32], VoteType::Precommit, &kp(offender)),
    };
    let ev_hash = ev.hash();
    net.submit_evidence(2, ev);
    net.run();
    let pending_evidence_nodes: Vec<u64> = ids
        .iter()
        .copied()
        .filter(|&id| !net.node(id).pending_evidence().is_empty())
        .collect();
    println!(
        "M19 block-level op gossip: evidence {} injected at node 2",
        short(&ev_hash)
    );
    println!(
        "  -> present in pending_evidence pools of nodes {pending_evidence_nodes:?} (flooded, deduped by content hash; full cryptographic validation happens at apply_evidence)\n"
    );

    // --- real loopback TCP: three followers pull the chain over sockets --------
    println!("loopback TCP sync (three followers pull from a seed over sockets):");
    let n_followers = 3usize;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| fail("bind seed", e));
    let addr = listener.local_addr().unwrap();

    // seed server: serve `n_followers` connections, each a GetBlocks -> Blocks
    let (sblocks, scerts) = (blocks.clone(), certs.clone());
    let server = thread::spawn(move || {
        let mut seed = GossipNode::new(1, demo_genesis(), 16, [1u64]);
        assert!(seed.load_certified(&sblocks, &scerts));
        for _ in 0..n_followers {
            let (mut stream, _) = listener.accept().expect("accept");
            if let Ok(GossipMsg::GetBlocks { from }) = read_msg(&mut stream) {
                // reuse the real protocol to build the response
                for (_, msg) in seed.on_message(0, GossipMsg::GetBlocks { from }) {
                    write_msg(&mut stream, &msg).expect("serve blocks");
                }
            }
        }
    });

    // followers: each dials the seed, requests from height 1, verifies + applies
    let (tx_done, rx_done) = mpsc::channel();
    for id in 2u64..2 + n_followers as u64 {
        let tx_done = tx_done.clone();
        thread::spawn(move || {
            let mut node = GossipNode::new(id, demo_genesis(), 16, [1u64]);
            let mut stream = TcpStream::connect(addr).expect("dial seed");
            write_msg(&mut stream, &GossipMsg::GetBlocks { from: 1 }).expect("request");
            if let Ok(GossipMsg::Blocks(batch)) = read_msg(&mut stream) {
                for (b, c) in batch {
                    node.apply_certified(b, c); // re-verifies each cert on arrival
                }
            }
            tx_done.send((id, node.height(), node.head())).expect("report");
        });
    }
    drop(tx_done);
    server.join().expect("seed server");

    let mut results: Vec<(u64, u64, String)> =
        rx_done.iter().map(|(id, h, head)| (id, h, short(&head))).collect();
    results.sort();
    let expected = short(&d.head());
    for (id, h, head) in &results {
        let ok = if *head == expected { "✓" } else { "✗" };
        println!("  follower {id}  synced to height {h}  head {head}  {ok}");
    }
    let all_ok = results.iter().all(|(_, h, head)| *h == blocks.len() as u64 && *head == expected);
    println!(
        "  -> {} follower(s) synced to the certified head over TCP, every certificate re-verified: {}",
        results.len(),
        all_ok
    );
}

/// Demonstrate the light-client validator-set follow protocol: build a
/// certified chain that changes its validator set every way it can — an
/// explicit validator update, a self-bond, and a slashing removal, with real
/// transactions mixed in — then have a [`ValidatorTracker`] follow the same
/// `(block, certificate)` pairs a full node gossips and arrive at the exact
/// same active set at every height, WITHOUT applying a single transaction or
/// tracking any account balance.
fn cmd_light() {
    // seeds cover the genesis validators (21..=24), a mid-chain addition (25)
    // and the accounts that bond into the set (1..=3).
    let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 2, 3, 21, 22, 23, 24, 25]
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 4);

    let show = |label: &str, vs: &ValidatorSet| {
        let ids: Vec<String> = vs
            .validators()
            .iter()
            .map(|v| format!("{}={}", v.id, cog(v.power)))
            .collect();
        println!("    {label}: {{ {} }}", ids.join(", "));
    };

    println!("building a certified chain that reshapes its validator set 3 ways:\n");

    // height 1: two real submissions PLUS admit a brand-new validator (id 25).
    d.submit(tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0))
        .unwrap_or_else(|e| fail_chain("submit tx", e));
    d.submit(tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0))
        .unwrap_or_else(|e| fail_chain("submit tx", e));
    d.stage_validator_update(ValidatorUpdate { id: 25, pubkey: kp(25).public(), power: 2 * MICRO });
    println!("  height 1: 2 submissions + ADD validator 25 (power 2)");
    d.produce(1.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h1", &e));

    // height 2: account #1 self-bonds and becomes a validator.
    d.stage_stake_op(
        StakeOp { account: 1, kind: BondKind::Bond, amount: 6 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1)),
    );
    println!("  height 2: account #1 BONDs 6 $COG (becomes validator #1)");
    d.produce(2.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h2", &e));

    // height 3: validator #1 double-signs; evidence slashes and removes it.
    let ev = SlashEvidence {
        vote_a: Vote::signed(1, 3, 0, [0xAA; 32], VoteType::Precommit, &kp(1)),
        vote_b: Vote::signed(1, 3, 0, [0xBB; 32], VoteType::Precommit, &kp(1)),
    };
    d.stage_slashing_evidence(ev);
    println!("  height 3: validator #1 DOUBLE-SIGNs -> slashed + removed\n");
    d.produce(3.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h3", &e));

    let n_txs: usize = d.blocks().iter().map(|b| b.txs.len()).sum();
    println!(
        "full chain: {} certified heights, {} transactions applied, head {}\n",
        d.blocks().len(),
        n_txs,
        short(&d.head()),
    );

    // The light client. Bootstrapped from ONLY the trusted genesis, it consumes
    // the same (block, certificate) pairs a full node gossips (GossipMsg::Blocks)
    // and follows the validator set height by height.
    println!("light client follows the SAME (block, cert) pairs — no tx execution:");
    let mut lt = ValidatorTracker::from_genesis(&demo_genesis());
    show("genesis set", lt.validators());
    for (i, (b, c)) in d.blocks().iter().zip(d.certificates()).enumerate() {
        let power = lt
            .follow(b, c)
            .unwrap_or_else(|e| fail_msg("light follow", &e));
        println!(
            "  height {} certified by {} $COG of voting power; set for the next height ->",
            i + 1,
            cog(power),
        );
        show("tracked set", lt.validators());
    }

    // The payoff: the light-followed set equals the authoritative full-replay
    // set at the tip — proven, not trusted (every cert was re-verified).
    let full = Chain::replay_verified(demo_genesis(), d.blocks(), d.certificates())
        .unwrap_or_else(|e| fail_msg("replay_verified", &e));
    let light_ids: Vec<(u64, u64)> =
        lt.validators().validators().iter().map(|v| (v.id, v.power)).collect();
    let full_ids: Vec<(u64, u64)> =
        full.state.validators.validators().iter().map(|v| (v.id, v.power)).collect();
    println!(
        "\nlight-followed set == authoritative replayed set: {}  (light applied 0 txs)",
        light_ids == full_ids && lt.head() == full.head,
    );
    assert_eq!(light_ids, full_ids, "light client diverged from the full chain");
}

fn cmd_run(dir: String) {    let path = format!("{dir}/blocks.log");
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

/// Demonstrate certificate persistence and *replay-as-finality-verification*.
/// First run: produce a BFT-certified chain and persist both `blocks.log` and
/// `certs.log`. Every run: reload both logs and replay them re-verifying each
/// height's > 2/3 certificate — recovering *finality*, not just deterministic
/// state. Then show that dropping a certificate is caught here even though the
/// plain (state-only) replay still succeeds.
fn cmd_certs(dir: String) {
    let bpath = format!("{dir}/blocks.log");
    let cpath = format!("{dir}/certs.log");
    let blog = BlockLog::open(&bpath).unwrap_or_else(|e| fail("open block log", e));
    let clog = CertLog::open(&cpath).unwrap_or_else(|e| fail("open cert log", e));
    let (vset, seeds) = demo_validators();

    // seed once: if the logs are empty, produce a certified chain and persist it
    let existing = blog.read_all().unwrap_or_else(|e| fail("read block log", e));
    if existing.is_empty() {
        println!("empty logs at {dir} — producing a BFT-certified chain\n");
        let mut d = ChainDriver::new(demo_genesis(), seeds, 1);
        for t in [
            tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
            tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
            tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
        ] {
            d.submit(t).unwrap();
        }
        d.produce_until_drained(1.0, 16)
            .unwrap_or_else(|e| fail_msg("produce chain", &e));
        for b in d.blocks() {
            blog.append(b).unwrap_or_else(|e| fail("append block", e));
        }
        for c in d.certificates() {
            clog.append(c).unwrap_or_else(|e| fail("append certificate", e));
        }
        println!(
            "persisted {} block(s) + {} certificate(s) to {dir}\n",
            d.blocks().len(),
            d.certificates().len()
        );
    }

    // reload both logs and replay re-verifying finality at every height
    let blocks = blog.read_all().unwrap_or_else(|e| fail("read block log", e));
    let certs = clog.read_all().unwrap_or_else(|e| fail("read cert log", e));
    let chain = Chain::replay_verified(demo_genesis(), &blocks, &certs)
        .unwrap_or_else(|e| fail_msg("verify finality on replay", &e));
    println!(
        "reloaded {} block(s) + {} certificate(s); FINALITY re-verified height by height:",
        blocks.len(),
        certs.len()
    );
    for c in &certs {
        let power = c.verify(&vset).unwrap();
        println!(
            "  height {}  block {}  certificate power {}/{} (> 2/3) ✓",
            c.height,
            short(&c.block_hash),
            power,
            vset.total_power()
        );
    }
    println!();
    print_summary(&chain);

    // the point of certificates: state-only replay cannot tell a finalized chain
    // from an unfinalized one; finality replay can. Drop one cert and compare.
    if !certs.is_empty() {
        let dropped = &certs[..certs.len() - 1];
        let state_ok = Chain::replay(demo_genesis(), &blocks).is_ok();
        let finality = Chain::replay_verified(demo_genesis(), &blocks, dropped);
        println!("\ntamper check — drop the last certificate:");
        println!("  state-only replay still succeeds: {state_ok}");
        match finality {
            Err(e) => println!("  finality replay rejects it: true ({e})"),
            Ok(_) => println!("  finality replay rejects it: false (UNEXPECTED)"),
        }
    }
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
    println!("merkle_root      {}", short(&chain.state.merkle_root()));
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

fn fail_msg<E: std::fmt::Display>(ctx: &str, e: &E) -> ! {
    eprintln!("error: {ctx}: {e}");
    exit(1);
}
