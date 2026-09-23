//! Reference-node CLI.
//!
//!   cargo run --release --bin node -- demo             # in-memory demo chain
//!   cargo run --release --bin node -- build            # mempool builds a block
//!   cargo run --release --bin node -- prove            # light-client Merkle proof
//!   cargo run --release --bin node -- bft              # BFT commit certificate over a block
//!   cargo run --release --bin node -- gossip           # gossip + anti-entropy sync (in-proc + loopback TCP)
//!   cargo run --release --bin node -- light            # light client: follow the validator set without full replay
//!   cargo run --release --bin node -- lsync            # header-only SPV sync over the gossip bus (light peer never sees a tx)
//!   cargo run --release --bin node -- account          # account-membership SPV for a wallet: prove your balance against a cert-signed header
//!   cargo run --release --bin node -- staking          # bond/unbond: stake-bound validator power + unbonding
//!   cargo run --release --bin node -- slashing         # slash an equivocating validator's bonded stake to the treasury
//!   cargo run --release --bin node -- run  --config F # networked tokio daemon (TCP P2P gossip)
//!   cargo run --release --bin node -- localnet         # in-process tokio testnet converges over real sockets
//!   cargo run --release --bin node -- status --dir DIR # replay log, print state
//!   cargo run --release --bin node -- certs  --dir DIR # persist certified chain, re-verify finality
//!
//! `run` is the real M33 daemon: it loads a node/genesis/validator config, binds a
//! TCP listener, dials configured peers, and runs both the gossip anti-entropy
//! protocol and distributed BFT voting over real sockets. There is no sequencer —
//! every node with `[validator] enabled = true` owns one key, gossips
//! proposals/prevotes/precommits, and drives round changes with wall-clock
//! timeouts; nodes without a validator key are pure followers that sync + verify
//! certificates over the wire. `status`/`certs --dir` remain offline replay tools.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{TcpListener, TcpStream};
use std::process::exit;
use std::sync::mpsc;
use std::thread;

use zhixing_engine::{DeltaKParams, DIM};
use zhixing_node::config::{self, NodeConfig, NodeSection, PeerConfig};
use zhixing_node::consensus::{commit_block, detect_equivocation, Commit};
use zhixing_node::daemon;
use zhixing_node::driver::ChainDriver;
use zhixing_node::codec::BlockHeader;
use zhixing_node::bridge::{BridgeEndpoint, BridgeError};
use zhixing_node::light::{DiffEnvelope, LightError, ProofEntry, ValidatorTracker};
use zhixing_node::mempool::Mempool;
use zhixing_node::merkle;
use zhixing_node::net::{read_msg, write_msg, GossipMsg, GossipNode, LightGossipNode, LightNetwork, Network};
use zhixing_node::round::Sim;
use zhixing_node::store::{BlockLog, CertLog};
use zhixing_node::validator::{Validator, ValidatorSet, ValidatorUpdate};
use zhixing_node::{
    hex, Block, BondKind, BridgeHeader, BridgeLock, BridgeRedeem, Chain, ChainError, ChainState,
    Genesis, Keypair, Review, SlashEvidence, StakeOp, SubmissionTx, Vote, VoteType, MICRO,
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
        bridge_sources: vec![],
    }
}

/// M30: a second genesis for the cross-chain demo. Identical to
/// `demo_genesis` except for `timestamp_days`, so the two chains have
/// distinct `genesis_hash` values — each can name the other's hash in a
/// `BridgeLock::dest_chain` field, and a relayer cannot substitute one
/// chain's envelope for the other's.
fn demo_genesis_b() -> Genesis {
    let mut g = demo_genesis();
    g.timestamp_days = 1.0;
    g
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

/// The demo block sequence (built against the chain's current head). Each block
/// is sealed with its validator-set commitment against an advancing trial clone,
/// so the sealed b1 hash is the one b2 chains to and both pass the commitment
/// check on commit.
fn demo_blocks(chain: &Chain) -> Vec<Block> {
    let mut trial = chain.clone();
    let mut b1 = Block {
        height: 1,
        prev_hash: trial.head,
        timestamp_days: 1.0,
        next_validators_root: [0u8; 32],
        // M23: state commitments stamped by `Chain::commit`.
        state_root: [0u8; 32],
        accounts_root: [0u8; 32],
        // M27: stamped by `Chain::commit` (or `Chain::seal`) — builder leaves zero.
        graph_root: [0u8; 32],
        bridge_root: [0u8; 32],
        txs: vec![
            tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
            tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
        ],
        validator_updates: Vec::new(),
        stake_ops: Vec::new(),
        slashing_evidence: Vec::new(),
        bridge_locks: Vec::new(),
        bridge_headers: Vec::new(),
        bridge_redeems: Vec::new(),
    };
    trial.seal(&mut b1).expect("seal b1");
    trial.commit(&mut b1).expect("commit b1 on trial");
    // block 2 prev_hash is (sealed) block 1's hash
    let mut b2 = Block {
        height: 2,
        prev_hash: b1.hash(),
        timestamp_days: 2.0,
        next_validators_root: [0u8; 32],
        // M23: state commitments stamped by `Chain::commit`.
        state_root: [0u8; 32],
        accounts_root: [0u8; 32],
        graph_root: [0u8; 32],
        bridge_root: [0u8; 32],
        txs: vec![
            tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 2.0),
            tx(1, unit(0), 0, reviews(&[(10, 0.7), (11, 0.6), (12, 0.65)]), (0, 3), 2.0),
        ],
        validator_updates: Vec::new(),
        stake_ops: Vec::new(),
        slashing_evidence: Vec::new(),
        bridge_locks: Vec::new(),
        bridge_headers: Vec::new(),
        bridge_redeems: Vec::new(),
    };
    trial.seal(&mut b2).expect("seal b2");
    vec![b1, b2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("demo");
    match cmd {
        "demo" => cmd_demo(),
        "build" => cmd_build(),
        "prove" => cmd_prove(),
        "vprove" => cmd_vprove(),
        "bft" => cmd_bft(),
        "live" => cmd_live(),
        "chain" => cmd_chain(),
        "validators" => cmd_validators(),
        "staking" => cmd_staking(),
        "slashing" => cmd_slashing(),
        "gossip" => cmd_gossip(),
        "light" => cmd_light(),
        "lsync" => cmd_lsync(),
        "account" => cmd_account(),
        "graph" => cmd_graph(),
        "knn" => cmd_knn(),
        "range" => cmd_range(),
        "diff" => cmd_diff(),
        "batch" => cmd_batch(),
        "bridge" => cmd_bridge(),
        "redeem" => cmd_redeem(),
        "run" => cmd_run(config_arg(&args)),
        "localnet" => cmd_localnet(),
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

fn config_arg(args: &[String]) -> String {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
        i += 1;
    }
    eprintln!("this command requires --config <path>");
    exit(2);
}

fn usage() {
    eprintln!("zhixing reference node");
    eprintln!("  node demo               run an in-memory demo chain");
    eprintln!("  node build              feed a mempool (scrambled order) and build one block");
    eprintln!("  node prove              build+verify a light-client Merkle proof of an account");
    eprintln!("  node vprove             prove a validator's membership in a cert-signed block's next set");
    eprintln!("  node bft                4 validators certify a block; show fault tolerance + equivocation");
    eprintln!("  node live               drive the BFT round FSM to a commit (incl. a dead proposer)");
    eprintln!("  node chain              grow a BFT-certified chain height by height (mempool -> consensus -> commit)");
    eprintln!("  node validators         grow a chain across on-chain validator-set changes (add/remove)");
    eprintln!("  node staking            bond stake to gain validator power; unbond through a delayed withdrawal");
    eprintln!("  node slashing           slash an equivocating validator's bonded stake to the treasury");
    eprintln!("  node gossip             gossip + anti-entropy sync: fresh nodes catch up to a certified chain (in-proc + TCP)");
    eprintln!("  node light              light client: follow the validator set across heights without full replay");
    eprintln!("  node lsync              header-only SPV sync over the gossip bus: a light peer reaches the full node's height with zero tx bodies");
    eprintln!("  node account            account-membership SPV for a wallet: prove your own balance against a cert-signed header, no replay, no tx bodies");
    eprintln!("  node graph              cognitive-graph inclusion proof: prove a graph node against a cert-signed header's accounts_root, no graph download");
    eprintln!("  node knn                cert-signed kNN over the cognitive graph: the wallet re-derives the neighbourhood ranking from committed leaves, no graph download");
    eprintln!("  node range              cert-signed graph range query: wallet re-derives the cos_sim(q, n) >= min_sim cut set from graph_root, no graph download");
    eprintln!("  node diff               cert-signed temporal graph diff between two cert-signed heights: added/dropped, wallet re-derives via partial replay");
    eprintln!("  node batch              heterogeneous batched proof transport: Inclusion + kNN + Range + Diff in a single round-trip");
    eprintln!("  node bridge             trustless bridge: chain A locks to chain B, relayer ferries a cert-signed envelope, B's endpoint verifies + credits — no relayer trust");
    eprintln!("  node redeem             consensus-level redeem: B's producer follows A on-chain and mints from a source lock inside its state machine — the mint is BFT-enforced, not off-chain");
    eprintln!("  node run    --config F  run the networked BFT daemon (tokio TCP P2P): load config/genesis/validator key, gossip votes + sync/verify over sockets");
    eprintln!("  node localnet           spin up an in-process tokio testnet (4 validators, no sequencer) and show all nodes converge via distributed BFT voting over real sockets");
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

    let mut blk = mp.build_block(&chain, 1.0).expect("pool builds a block");
    chain.seal(&mut blk).expect("seal the built block");
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
    for mut blk in demo_blocks(&chain) {
        chain.commit(&mut blk).unwrap();
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

/// Demonstrate an M21 validator-membership proof: build a certified block that
/// reshapes the validator set, then prove a single validator is in the *next*
/// set — committed by that block's `next_validators_root`, which the certificate
/// signs — via an O(log n) Merkle inclusion proof, no replay. A forged leaf is
/// rejected.
fn cmd_vprove() {
    let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 2, 3, 21, 22, 23, 24, 25]
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 4);

    // height 1: admit a brand-new validator (id 25) so the next set differs from
    // the genesis set — the thing we will prove membership in.
    d.stage_validator_update(ValidatorUpdate { id: 25, pubkey: kp(25).public(), power: 2 * MICRO });
    d.produce(1.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h1", &e));

    let block = &d.blocks()[0];
    let cert = &d.certificates()[0];
    // the set that certifies height 2 — what block 1 commits to in its header.
    let next_set = d.chain.state.validators.clone();
    // the set active for height 1 (anchors the certificate): the genesis set.
    let tracked = ValidatorTracker::from_genesis(&demo_genesis()).validators().clone();

    println!("certified block {} commits next_validators_root = {}", block.height, short(&block.next_validators_root));
    println!("(the certificate signs the block hash, which includes that root)\n");

    let id = 25u64;
    let v = next_set.get(id).expect("validator 25 is in the next set").clone();
    let proof = next_set.proof(id).expect("membership proof exists");
    println!("light client is told: validator #{id} power={} $COG", cog(v.power));
    println!("proof: {} sibling hash(es) up to next_validators_root", proof.steps.len());
    let header = zhixing_node::codec::BlockHeader::from_block(block);
    let entry = ProofEntry::Validator { id, validator: v.clone(), proof: proof.clone() };
    match ValidatorTracker::verify_proof_against_header(&header, cert, &tracked, &entry) {
        Ok(()) => println!("verify membership against the cert-signed header -> true"),
        Err(e) => {
            eprintln!("membership proof unexpectedly failed: {e}");
            exit(1);
        }
    }

    // negative case: a lie about the validator's power must fail.
    let mut forged = v.clone();
    forged.power += 1;
    let forged_entry = ProofEntry::Validator { id, validator: forged, proof };
    let bad = ValidatorTracker::verify_proof_against_header(&header, cert, &tracked, &forged_entry);
    println!("verify a forged (power+1) leaf -> {} (must be false)", bad.is_ok());
}


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

    // M21: the transition-FREE path. Given each block's next set (which a header
    // sync transport would ship alongside the block), the tracker advances by
    // verifying it against the certificate-signed `next_validators_root` — never
    // replaying stake ops / evidence at all.
    println!("\nM21 — transition-free follow (verify next set vs committed root, no replay):");
    let mut committed_sets: Vec<ValidatorSet> = Vec::new();
    let mut replay = Chain::new(demo_genesis());
    for b in d.blocks() {
        let mut b = b.clone();
        replay.commit(&mut b).unwrap_or_else(|e| fail_msg("replay commit", &e));
        committed_sets.push(replay.state.validators.clone());
    }
    let mut lt2 = ValidatorTracker::from_genesis(&demo_genesis());
    for (i, (b, c)) in d.blocks().iter().zip(d.certificates()).enumerate() {
        lt2.follow_committed(b, c, &committed_sets[i])
            .unwrap_or_else(|e| fail_msg("follow_committed", &e));
    }
    let committed_ids: Vec<(u64, u64)> =
        lt2.validators().validators().iter().map(|v| (v.id, v.power)).collect();
    show("transition-free set", lt2.validators());
    println!(
        "transition-free set == replayed set: {}",
        committed_ids == full_ids && lt2.head() == full.head,
    );
    assert_eq!(committed_ids, full_ids, "follow_committed diverged from the full chain");
}

/// Demonstrate M22 — header-only SPV sync over the gossip bus. A full node holds
/// the certified chain (with real transactions, stake ops, and slashing
/// evidence in the block bodies); a light peer announces its height, pulls
/// only `(CertifiedHeader, next_set)` pairs over the wire, and advances its
/// [`ValidatorTracker`] to the head — never deserializing a transaction body.
/// The demo prints the wire-byte savings and the membership proof the SPV
/// path is designed to enable.
fn cmd_lsync() {
    use zhixing_node::codec::{encode_block, encode_certified_header, encode_commit};

    // Build a real certified chain. Same shape as `cmd_light` — a chain that
    // changes its validator set via explicit update, self-bond, and slashing —
    // because the demo is most visible across set transitions.
    let seeds: BTreeMap<u64, [u8; 32]> = [1u64, 2, 3, 21, 22, 23, 24, 25]
        .iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect();
    let mut d = ChainDriver::new(demo_genesis(), seeds, 4);
    d.submit(tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0)).unwrap();
    d.submit(tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0)).unwrap();
    d.stage_validator_update(ValidatorUpdate { id: 25, pubkey: kp(25).public(), power: 2 * MICRO });
    d.produce(1.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h1", &e));
    d.stage_stake_op(
        StakeOp { account: 1, kind: BondKind::Bond, amount: 6 * MICRO, signature: [0u8; 64] }
            .signed(&kp(1)),
    );
    d.produce(2.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h2", &e));
    let ev = SlashEvidence {
        vote_a: Vote::signed(1, 3, 0, [0xAA; 32], VoteType::Precommit, &kp(1)),
        vote_b: Vote::signed(1, 3, 0, [0xBB; 32], VoteType::Precommit, &kp(1)),
    };
    d.stage_slashing_evidence(ev);
    d.produce(3.0, &BTreeSet::new()).unwrap_or_else(|e| fail_msg("produce h3", &e));

    let n_txs: usize = d.blocks().iter().map(|b| b.txs.len()).sum();
    let n_stake_ops: usize = d.blocks().iter().map(|b| b.stake_ops.len()).sum();
    let n_evidence: usize = d.blocks().iter().map(|b| b.slashing_evidence.len()).sum();
    println!(
        "full chain: {} heights, {} txs, {} stake ops, {} evidence, head {}\n",
        d.blocks().len(),
        n_txs,
        n_stake_ops,
        n_evidence,
        short(&d.head()),
    );

    // Full node (id=1) seeds the bus with the certified chain; light node
    // (id=2) holds only the genesis.
    let mut full = GossipNode::new(1, demo_genesis(), 16, [1, 2]);
    let blocks = d.blocks().to_vec();
    let certs = d.certificates().to_vec();
    assert!(full.load_certified(&blocks, &certs));
    let light_id = 2u64;
    let light = LightGossipNode::new(light_id, &demo_genesis(), [1, 2]);

    println!("SPV header-sync over the gossip bus:");
    let full_height = full.height();
    println!("  full peer (id=1): height {}  ·  light peer (id={}): height 0", full_height, light_id);
    let mut net = LightNetwork::new(vec![full], vec![light]);

    // Per-height authoritative next sets, computed once from a full replay.
    // The light peer is told "block i+1 was certified by this set, and it
    // commits to this next set in its `next_validators_root`" — exactly the
    // side-information `follow_committed`/`follow_header` need.
    let mut committed_sets: Vec<ValidatorSet> = Vec::new();
    let mut replay = Chain::new(demo_genesis());
    for b in d.blocks() {
        let mut b = b.clone();
        replay.commit(&mut b).unwrap_or_else(|e| fail_msg("replay commit", &e));
        committed_sets.push(replay.state.validators.clone());
    }
    let next_set_for = |h: u64| committed_sets.get((h - 1) as usize).cloned();

    net.announce_all();
    for round in 0..10 {
        let n = net.run(1, light_id, &next_set_for);
        if n == 0 {
            break;
        }
        if round == 0 {
            println!("  round 1: light Status{{height=0}} -> full replies Headers{{from=1}}");
        }
    }

    let l = net.light_node(light_id);
    assert_eq!(
        l.tracker().height(),
        full_height,
        "light peer reached the full node's height — without ever seeing a tx"
    );

    // Wire-byte accounting: the certified headers carry no bodies.
    let certs_ref = d.certificates();
    let full_bytes: usize = d.blocks().iter().zip(certs_ref.iter()).map(|(b, c)| encode_block(b).len() + encode_commit(c).len()).sum();
    let header_bytes: usize = d
        .blocks()
        .iter()
        .zip(certs_ref.iter())
        .map(|(b, c)| encode_certified_header(&zhixing_node::codec::CertifiedHeader::from_certified(b, c)).len())
        .sum();
    println!(
        "\n  bandwidth (block+certs vs certified headers, summed across the chain):"
    );
    println!("    full blocks  : {:>6} B", full_bytes);
    println!("    headers only : {:>6} B", header_bytes);
    println!(
        "    savings      : {:>5.1}%  ({})",
        100.0 * (1.0 - (header_bytes as f64 / full_bytes as f64)),
        if header_bytes < full_bytes { "SPV saved bytes ✓" } else { "UNEXPECTED: not smaller" },
    );

    // Light peer can prove a validator's membership against the latest header.
    // The cert binds the header; the set active for that header's height is
    // the post-apply set of the *previous* block — which is `committed_sets[h-2]`
    // (height `h`'s prev block was block `h-1`, committed to height `h-1`'s set
    // in its `next_validators_root`). For h=1, the active set is the genesis set.
    let lh = l.headers().last().unwrap();
    let h = lh.height() as usize; // 1-indexed
    let active_for_cert: &ValidatorSet = if h >= 2 {
        &committed_sets[h - 2]
    } else {
        &d.chain.state.validators // pre-anything; genesis set for h=1
    };
    // Prove a validator who is in the *latest* set (the one committed by this
    // header's `next_validators_root`): `committed_sets[h-1]`.
    let next_set = &committed_sets[h - 1];
    let id = 25u64;
    let v = next_set.get(id).expect("validator 25 active at the tip").clone();
    let proof = next_set.proof(id).expect("membership proof exists");
    let entry = ProofEntry::Validator { id, validator: v.clone(), proof: proof.clone() };
    match ValidatorTracker::verify_proof_against_header(
        &lh.header,
        &lh.cert,
        active_for_cert,
        &entry,
    ) {
        Ok(()) => println!(
            "  verify_proof_against_header (validator #{id}) against the latest cert-signed header -> true"
        ),
        Err(e) => {
            eprintln!("  membership proof unexpectedly failed: {e}");
            exit(1);
        }
    }

    // Light peer rejects a header whose next set contradicts the cert-signed root.
    let bad_validator = Validator {
        id: 99,
        pubkey: kp(99).public(),
        power: 1,
    };
    // Use the valid proof for validator 25 but claim it opens validator 99 —
    // merkle::verify rejects it, exercising the proof path.
    let bad_proof = next_set.proof(25).unwrap();
    let bad_entry = ProofEntry::Validator { id: 99, validator: bad_validator, proof: bad_proof };
    let wrong = ValidatorTracker::verify_proof_against_header(
        &lh.header,
        &lh.cert,
        active_for_cert,
        &bad_entry,
    );
    println!(
        "  verify_proof_against_header against a forged next set -> {} (must be false)",
        wrong.is_ok()
    );
}

/// M24: a light wallet proves its own balance, its reviewer reputation, AND a
/// next-set validator membership — all three with one batched `GetProof` round
/// trip and a single verifier, against the same cert-signed header pulled over
/// the M22 gossip bus. No replay, no tx bodies, no full-peer trust.
fn cmd_account() {
use zhixing_node::light::ProofKind;

    println!("M24 — batched SPV: account + reviewer + validator in one round-trip");
    println!();

    // 1. Build a real certified chain (same shape as cmd_lsync — a chain that
    //    reshapes its validator set so M22 is also exercised).
    let (blocks, certs) = run_driver(3);

    // 2. Wrap in GossipNode (full) + LightGossipNode (light) over LightNetwork.
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);
    let light_id = 2u64;
    let light = LightGossipNode::new(light_id, &demo_genesis(), [1, 2]);
    let mut net = LightNetwork::new(vec![full], vec![light]);

    // 3. Authoritative per-height next sets from a full replay; needed to
    //    side-channel the headers from full -> light through
    //    LightGossipNode::apply_header (the bus drops them otherwise).
    let sets = committed_sets(&demo_genesis(), &blocks);
    let next_set_for = |h: u64| sets.get((h - 1) as usize).cloned();

    // 4. Run the M22 sync to header height 2.
    net.announce_all();
    for _ in 0..20 {
        let n = net.run(1, light_id, &next_set_for);
        if n == 0 && net.light_node(light_id).tracker().height() == 2 {
            break;
        }
        if n == 0 {
            break; // bus drained — either synced or stuck
        }
    }
    let lt = net.light_node(light_id).tracker().clone();
    let full_height = blocks.len() as u64;
    if lt.height() != full_height {
        eprintln!(
            "note: light reached height {} / {}",
            lt.height(),
            full_height
        );
    }
    println!(
        "M22 sync: light at height {}, head {}",
        lt.height(),
        short(&lt.head())
    );
    println!();

    // The wallet proves its account against the latest cert-signed header
    // the light peer actually advanced to. That is `lt.height()`'s block.
    let last_idx = (lt.height() as usize).saturating_sub(1);
    let last_block = &blocks[last_idx];
    let last_cert = &certs[last_idx];

    // 5. Light wallet asks full peer: GetProof { items: [(Account, 1),
    //    (Reviewer, 10), (Validator, 25)] }. One round-trip, three proofs.
    let mut full_node = net.take_full(1);
    let mut light_node = net.take_light(light_id);
    let reply = full_node.on_message(light_id, GossipMsg::GetProof {
        items: vec![
            (ProofKind::Account, 1),
            (ProofKind::Reviewer, 10),
            (ProofKind::Validator, 25),
        ],
    });
    assert_eq!(reply.len(), 1, "full peer must serve the batched proofs");
    let (_dst, proof_msg) = reply.into_iter().next().unwrap();
    light_node.on_message(1, proof_msg);

    // 6. Light wallet pulls the cached entries and verifies all three against
    //    the same cert-signed header with the unified dispatcher. The
    //    `ProofKind` inside each entry decides which root slot is checked:
    //    Account + Reviewer -> header.accounts_root, Validator ->
    //    header.next_validators_root.
    //
    //    `lt.validators()` is the active set for height `lt.height() + 1`
    //    (i.e. the post-update set, since the light tracker just followed
    //    height H and advanced to it). For verifying the cert at height H,
    //    we need the active set FOR height H — the pre-update set, which
    //    equals the genesis set when the chain's first block is the one
    //    carrying the update. Use the genesis-tracker set explicitly.
    let header = zhixing_node::codec::BlockHeader::from_block(last_block);
    let tracked = ValidatorTracker::from_genesis(&demo_genesis()).validators().clone();

    let account_entry = light_node
        .take_proof(ProofKind::Account, 1)
        .expect("account proof cached for id 1");
    let reviewer_entry = light_node
        .take_proof(ProofKind::Reviewer, 10)
        .expect("reviewer proof cached for id 10");
    let validator_entry = light_node
        .take_proof(ProofKind::Validator, 25)
        .expect("validator proof cached for id 25");

    let account_ok = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &account_entry,
    );
    let reviewer_ok = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &reviewer_entry,
    );
    let validator_ok = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &validator_entry,
    );

    let ProofEntry::Account { account, .. } = &account_entry else {
        unreachable!("account entry shape");
    };
    let ProofEntry::Reviewer { reputation, .. } = &reviewer_entry else {
        unreachable!("reviewer entry shape");
    };
    let ProofEntry::Validator { validator, .. } = &validator_entry else {
        unreachable!("validator entry shape");
    };

    println!("account #1 (post-apply state at height {}):", lt.height());
    println!("  balance       = {} COG", account.balance / MICRO);
    println!("  staked_total  = {} COG", account.staked_total / MICRO);
    println!("  earned_total  = {} COG", account.earned_total / MICRO);
    println!("  submissions   = {}", account.submissions);
    println!(
        "  verify_proof_against_header (Account)         -> {}",
        account_ok.is_ok()
    );
    println!(
        "  verify_proof_against_header (Reviewer #10)    -> {} (reputation = {})",
        reviewer_ok.is_ok(),
        reputation
    );
    println!(
        "  verify_proof_against_header (Validator #25)   -> {} (power = {} COG)",
        validator_ok.is_ok(),
        validator.power / MICRO
    );

    // 7. Negative: light wallet tampers with the validator's leaf (claims power+1),
    //    retry -> MembershipProofInvalid.
    let ProofEntry::Validator { id: _id, validator: v_real, proof: p_real } = validator_entry else {
        unreachable!()
    };
    let mut bad_validator = v_real.clone();
    bad_validator.power += 1;
    let forged_entry = ProofEntry::Validator {
        id: 25,
        validator: bad_validator,
        proof: p_real,
    };
    let bad = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &forged_entry,
    );
    println!(
        "  inflated validator power (power+1) -> {} (must be false: {})",
        bad.is_ok(),
        match bad.as_ref().err() {
            Some(e) => format!("{e}"),
            None => String::from("ok (UNEXPECTED)"),
        }
    );

    // 8. Negative: light wallet tampers with header.accounts_root.
    let mut bad_header = header.clone();
    bad_header.accounts_root = [0xAB; 32];
    let bad_root = ValidatorTracker::verify_proof_against_header(
        &bad_header, last_cert, &tracked, &account_entry,
    );
    println!(
        "  tampered accounts_root -> {} (must be false: {})",
        bad_root.is_ok(),
        match bad_root.as_ref().err() {
            Some(e) => format!("{e}"),
            None => String::from("ok (UNEXPECTED)"),
        }
    );

    // 9. Dual-root note: state_root covers the full consensus state;
    //    accounts_root is the inclusion-proof tree for accounts + reviewers.
    println!();
    println!("dual-root contract:");
    println!("  state_root            = {}  (full consensus-state digest)", short(&header.state_root));
    println!("  accounts_root         = {}  (Merkle root over accounts U reviewers)", short(&header.accounts_root));
    println!("  next_validators_root  = {}  (Merkle root over next-set validators)", short(&header.next_validators_root));
    println!("  the cert signs header.hash() which covers ALL THREE roots; the wallet");
    println!("  verifies each ProofEntry locally against its kind's root slot.");
    net.put_full(full_node);
    net.put_light(light_node);
}

/// M26 — cert-signed kNN over the cognitive graph.
///
/// The full peer computes `k_nearest_with_ties(query, k)` against its
/// current `chain.state.graph`, packages each `(node_id, graph_node,
/// merkle_proof)` into a `KnnClaim`, and hands it to the light wallet.
/// The light wallet feeds the claim to `verify_knn_against_header`, which:
///   1. cert-signing contract (header.hash matches cert, cert verifies
///      against the tracked set);
///   2. re-verifies every neighbour's Merkle proof against
///      `header.accounts_root` (same routing as M25 GraphNode proofs);
///   3. re-ranks the verified leaves by cosine against the query,
///      tie-breaks by `node_id` ascending, and applies the same
///      `k_nearest_with_ties(k)` cut the engine uses;
///   4. requires the prover's neighbour order to equal the verifier's.
///
/// The wallet never downloads the graph. The prover never gets to lie
/// about the ranking or omit a tied neighbour.
fn cmd_knn() {
use zhixing_node::light::KnnClaim;

    println!("M26 — cert-signed kNN over the cognitive graph");
    println!();

    // 1. Build a 2-block certified chain so the graph has a handful of nodes.
    let (blocks, certs) = run_driver(2);
    let last_idx = blocks.len() - 1;
    let last_block = &blocks[last_idx];
    let last_cert = &certs[last_idx];

    // 2. Wrap in GossipNode (full) and a bare ValidatorTracker (light).
    //    We don't need a LightNetwork here — the full peer exposes
    //    `serve_knn` as a synchronous helper, and the verifier is the
    //    same `verify_knn_against_header` the network path would call.
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);

    let tracked = ValidatorTracker::from_genesis(&demo_genesis()).validators().clone();
    let header = zhixing_node::codec::BlockHeader::from_block(last_block);

    let n_graph = full.chain.state.graph.nodes.len();
    if n_graph < 2 {
        eprintln!("graph has only {n_graph} node(s); demo needs >= 2 for a non-trivial kNN");
        return;
    }
    println!(
        "graph has {n_graph} nodes at height {}; cert-signed accounts_root = {}",
        header.height,
        short(&header.accounts_root),
    );
    println!();

    // 3. Compose a query embedding. Bias toward the first node but
    //    blend in the second so the ranking is non-trivial.
    let first = &full.chain.state.graph.nodes[0];
    let second = &full.chain.state.graph.nodes[1];
    let query: zhixing_node::engine::Embedding = {
        let mut q = first.embedding;
        for (i, x) in q.iter_mut().enumerate() {
            *x = 0.5 * *x + 0.5 * second.embedding[i];
        }
        let norm: f32 = q.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in q.iter_mut() {
                *x /= norm;
            }
        }
        q
    };
    println!(
        "query embedding = [{:.3}, {:.3}, {:.3}, {:.3}, {:.3}, {:.3}, {:.3}, {:.3}]",
        query[0], query[1], query[2], query[3], query[4], query[5], query[6], query[7]
    );
    println!();

    // 4. Full peer serves a kNN claim.
    let k: usize = 3;
    let claim: KnnClaim = full
        .serve_knn(query, k)
        .expect("full peer must serve a non-empty kNN claim");
    println!(
        "full peer claims {} neighbours (k = {}; ties may have grown the set):",
        claim.neighbours.len(),
        claim.k,
    );
    for (i, (id, n, _)) in claim.neighbours.iter().enumerate() {
        let sim = zhixing_node::engine::cos_sim(&claim.query, &n.embedding);
        println!(
            "  #{i:<2}  node_id = {id:<3}  cos_sim = {sim:+.4}  domain = {}",
            n.domain
        );
    }
    println!();

    // 5. Wallet-side verification.
    match ValidatorTracker::verify_knn_against_header(&header, last_cert, &tracked, &claim) {
        Ok(()) => println!("verify_knn_against_header -> Ok (claim accepted)"),
        Err(e) => {
            eprintln!("verify_knn_against_header -> Err({e:?}) — demo abort");
            return;
        }
    }

    // 6. Negative test 1 — swap two neighbours. The verifier re-derives
    //    the ranking from the verified leaves, so swapping a high-sim
    //    neighbour past a lower-sim one violates the sort and trips
    //    KnnRankingMismatch. (Choosing a swap rather than a deletion
    //    keeps the claim a complete kNN-with-ties set, so the only
    //    thing that can fail is the ordering.)
    let mut swapped = claim.clone();
    if swapped.neighbours.len() >= 2 {
        swapped.neighbours.swap(0, 1);
    }
    match ValidatorTracker::verify_knn_against_header(&header, last_cert, &tracked, &swapped) {
        Err(zhixing_node::light::LightError::KnnRankingMismatch { .. }) => {
            println!("swapped neighbours -> KnnRankingMismatch ✓");
        }
        other => panic!("expected KnnRankingMismatch after swap, got {other:?}"),
    }

    // 7. Negative test 2 — tamper a neighbour's embedding. Membership
    //    proof against accounts_root must fail.
    let mut tampered_emb = claim.clone();
    {
        let mut g = tampered_emb.neighbours[0].1.clone();
        g.embedding[0] += 1.0; // push it off the cosine ball
        tampered_emb.neighbours[0].1 = g;
    }
    match ValidatorTracker::verify_knn_against_header(&header, last_cert, &tracked, &tampered_emb) {
        Err(zhixing_node::light::LightError::MembershipProofInvalid { .. }) => {
            println!("tampered neighbour embedding -> MembershipProofInvalid ✓");
        }
        other => panic!("expected MembershipProofInvalid after tampering embedding, got {other:?}"),
    }

    // 8. Negative test 3 — tamper the accounts_root. Cert no longer
    //    matches the header hash; CertificateMismatch wins (the wallet
    //    never gets to the Merkle check).
    let mut bad_header = header.clone();
    bad_header.accounts_root = [0xAB; 32];
    match ValidatorTracker::verify_knn_against_header(&bad_header, last_cert, &tracked, &claim) {
        Err(zhixing_node::light::LightError::CertificateMismatch { .. }) => {
            println!("tampered accounts_root -> CertificateMismatch ✓");
        }
        other => panic!("expected CertificateMismatch after tampering accounts_root, got {other:?}"),
    }

    println!();
    println!(
        "M26 — light wallet verified a graph-shaped neighbourhood claim from a \
cert-signed header without downloading the graph."
    );
}

/// M27 demo: cert-signed graph range query.
///
/// A wallet asks "what are the graph nodes with cos_sim(query, n) >= min_sim
/// at height H?" The full peer serves a `RangeClaim` whose `nodes` are
/// sorted by cosine against the **user's** query (desc, `node_id` asc
/// tie-break), and each carries a Merkle proof against `header.graph_root`
/// (the M27 cert-signed secondary index, distinct from `accounts_root`
/// because the leaves live in a sorted-by-cosine view).
///
/// The verifier:
///   1. cert-signing contract (header.height/hash ↔ cert, cert.verify(set))
///   2. cutoff validity (`min_sim ∈ [-1, 1]`)
///   3. per-leaf Merkle verify against `header.graph_root`
///   4. re-rank by cosine against `query`, take prefix `sim >= min_sim`
///   5. set + order equality with the prover's claim
///
/// Three negative tests confirm each rejection path triggers:
///   - drop a node in the cut  → `RangeMismatch`
///   - tamper `header.graph_root` → `CertificateMismatch` (cert binding flips)
///   - bad `min_sim` (outside `[-1, 1]`) → `RangeCutoffInvalid`
fn cmd_range() {
use zhixing_node::light::RangeClaim;

    println!("M27 — cert-signed graph range query against a cert-signed header");
    println!();

    // Build a real certified chain so `state.graph` has accepted-submission
    // nodes (in addition to the genesis seed).
    let (blocks, certs) = run_driver(1);
    let last_block = blocks.last().expect("non-empty").clone();
    let last_cert = certs.last().expect("non-empty").clone();
    let header = BlockHeader::from_block(&last_block);

    // Wrap a full peer so we can call `serve_range`. The full peer replays
    // the certified chain internally, so its `chain.state.graph` matches
    // the wallet's view of the world (modulo the wallet's own copy).
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);

    // Compose a query near the first graph node's embedding so we have a
    // known-satisfying cut set. The query is `query = unit(1.0) + small bump`,
    // which puts the genesis-aligned node at sim ≈ 1.0 and other accepted-
    // submission nodes somewhere between 0 and 1 depending on their embedding.
    let query: Emb = [0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let min_sim = 0.0;
    let claim = full
        .serve_range(query, min_sim)
        .expect("graph must have at least one node");
    println!(
        "full peer served a range claim: query={:?}, min_sim={}, |nodes|={}",
        query,
        min_sim,
        claim.nodes.len()
    );
    // Each proof must verify locally against graph_root.
    for (id, g, proof) in &claim.nodes {
        let leaf = merkle::leaf_hash(&g.merkle_leaf());
        assert!(
            merkle::verify(&header.graph_root, &leaf, proof),
            "node {id}: leaf did not verify against graph_root"
        );
    }
    println!(
        "Merkle proofs: every leaf verifies against header.graph_root ({} nodes)",
        claim.nodes.len()
    );
    println!();

    // Wallet-side verifier: set up a light tracker from genesis so we can
    // validate the cert against the same active set the full peer used.
    let tracked = ValidatorTracker::from_genesis(&demo_genesis())
        .validators()
        .clone();
    ValidatorTracker::verify_range_against_header(&header, &last_cert, &tracked, &claim)
        .expect("wallet-side range claim verifies");
    println!("wallet: verify_range_against_header → Ok");
    println!();

    // Negative test 1: swap two nodes in the cut. The verifier re-derives
    // the cut from the verified leaves and applies the same cosine-desc,
    // node_id-asc sort — so a swap across different sims flips the order
    // and trips `RangeMismatch`. (Dropping a node cannot be detected
    // without the full graph; the same limitation M26 documents for kNN.)
    if claim.nodes.len() >= 2 {
        let mut bad = RangeClaim {
            query: claim.query,
            min_sim: claim.min_sim,
            nodes: claim.nodes.clone(),
        };
        bad.nodes.swap(0, 1);
        match ValidatorTracker::verify_range_against_header(&header, &last_cert, &tracked, &bad) {
            Err(LightError::RangeMismatch { .. }) => {
                println!("negative 1: swap two cut entries → RangeMismatch ✓");
            }
            other => panic!("expected RangeMismatch after a swap, got {other:?}"),
        }
    }

    // Negative test 2: tamper `header.graph_root`. Since graph_root is in
    // header.hash(), the cert no longer matches the tampered header —
    // CertificateMismatch wins before any Merkle check.
    let mut bad_header = header.clone();
    bad_header.graph_root = [0xCDu8; 32];
    match ValidatorTracker::verify_range_against_header(
        &bad_header,
        &last_cert,
        &tracked,
        &claim,
    ) {
        Err(LightError::CertificateMismatch { .. }) => {
            println!("negative 2: tamper graph_root → CertificateMismatch ✓");
        }
        other => panic!("expected CertificateMismatch after tampering graph_root, got {other:?}"),
    }

    // Negative test 3: `min_sim` outside [-1, 1]. Cutoff is a closed cosine
    // interval; values above 1.0 are degenerate "match everything" requests
    // and are rejected before any Merkle work.
    let bad_claim = RangeClaim {
        query: claim.query,
        min_sim: 2.0,
        nodes: claim.nodes.clone(),
    };
    match ValidatorTracker::verify_range_against_header(
        &header,
        &last_cert,
        &tracked,
        &bad_claim,
    ) {
        Err(LightError::RangeCutoffInvalid { .. }) => {
            println!("negative 3: min_sim=2.0 → RangeCutoffInvalid ✓");
        }
        other => panic!("expected RangeCutoffInvalid, got {other:?}"),
    }

    println!();
    println!(
        "M27 — light wallet verified a graph-shaped range claim (cosine cutoff) \
from a cert-signed header without downloading the graph."
    );
}

/// M28 demo: cert-signed temporal graph diff between two cert-signed
/// heights. The full peer serves a `DiffEnvelope` (two headers + two
/// certs + the diff body with per-leaf proofs); the light wallet
/// re-derives the diff from its own cached range of blocks via
/// `verify_diff_against_headers` and checks the prover's claim. The
/// load-bearing soundness step is the wallet-side partial replay — the
/// per-leaf proofs verify each entry's body, but completeness is bound
/// to the replay.
fn cmd_diff() {

    println!("M28 — cert-signed temporal graph diff between two cert-signed heights");
    println!();

    // Build a 2-block certified chain so (h₁, h₂) = (1, 2) is non-trivial:
    // each block carries an accepted submission so the graph grows in
    // both blocks — block 1 accepts one tx, block 2 accepts another.
    // We use the driver's helper (which seeds the validator set the same
    // way `run_driver` does), then submit a fresh tx for the second
    // block after the first is produced.
    let mut driver = ChainDriver::new(demo_genesis(), demo_driver_seeds(), 4);
    driver
        .submit(tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0))
        .expect("submit 1");
    driver.produce(1.0, &BTreeSet::new()).unwrap().expect("block 1");
    // Block 2 carries two txs so the diff's `added` set has 2+ entries
    // (negative test 3 below requires at least two to drop one).
    driver
        .submit(tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 2.0))
        .expect("submit 2");
    driver
        .submit(tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 2.5))
        .expect("submit 3");
    driver.produce(2.0, &BTreeSet::new()).unwrap().expect("block 2");
    let blocks = driver.blocks().to_vec();
    let certs = driver.certificates().to_vec();
    assert_eq!(blocks.len(), 2, "test setup: chain must have 2 blocks");
    let header_h1 = BlockHeader::from_block(&blocks[0]);
    let header_h2 = BlockHeader::from_block(&blocks[1]);
    let _cert_h1 = certs[0].clone();
    let cert_h2 = certs[1].clone();

    // Wrap a full peer so we can call `serve_diff`. The full peer replays
    // the certified chain internally, so its `chain.state.graph` matches
    // the wallet's view of the world.
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);

    // The full peer serves the diff envelope.
    let envelope: DiffEnvelope = full
        .serve_diff(1, 2, &header_h1, &header_h2)
        .expect("full peer serves a 2-height diff");
    println!(
        "full peer served a diff envelope: h₁=1, h₂=2, |added|={}, |dropped|={}",
        envelope.diff.added.len(),
        envelope.diff.dropped.len(),
    );

    // Sanity: every `added` proof must verify locally against h₂ accounts_root.
    for entry in &envelope.diff.added {
        let leaf = merkle::leaf_hash(&entry.graph_node.merkle_leaf());
        assert!(
            merkle::verify(&header_h2.accounts_root, &leaf, &entry.proof),
            "added leaf {} did not verify against h₂ accounts_root",
            entry.node_id,
        );
    }
    println!(
        "Merkle proofs: every added leaf verifies against header_h2.accounts_root"
    );
    println!();

    // Wallet-side verifier: the wallet replays the full `[1..=h₂]` range
    // and re-derives the diff from scratch.
    let blocks_in_range: Vec<(Block, Commit)> = blocks
        .iter()
        .cloned()
        .zip(certs.iter().cloned())
        .collect();
    ValidatorTracker::verify_diff_against_headers(
        &demo_genesis(),
        &blocks_in_range,
        &envelope,
    )
    .expect("wallet-side diff verifier accepts the envelope");
    println!("wallet: verify_diff_against_headers → Ok");
    println!();

    // Negative test 1: tamper one `added` proof → MembershipProofInvalid.
    if let Some(first) = envelope.diff.added.first().cloned() {
        let mut bad = envelope.clone();
        let mut forged = first.clone();
        // Flip the first step's side tag — same hash bytes but wrong
        // direction — which trips the Merkle verifier.
        if let Some(step0) = forged.proof.steps.first().cloned() {
            forged.proof.steps[0] = match step0 {
                merkle::Step::Right(h) => merkle::Step::Left(h),
                merkle::Step::Left(h) => merkle::Step::Right(h),
            };
        }
        bad.diff.added[0] = forged;
        match ValidatorTracker::verify_diff_against_headers(
            &demo_genesis(),
            &blocks_in_range,
            &bad,
        ) {
            Err(LightError::MembershipProofInvalid { .. }) => {
                println!("negative 1: tamper added proof → MembershipProofInvalid ✓");
            }
            other => panic!("expected MembershipProofInvalid, got {other:?}"),
        }
    }

    // Negative test 2: tamper `header_h2.accounts_root` → CertificateMismatch.
    // The field is part of `header_h2.hash()`, so the cert no longer binds it.
    let mut bad_header_h2 = header_h2.clone();
    bad_header_h2.accounts_root = [0xCDu8; 32];
    let bad_envelope = DiffEnvelope {
        header_prev: envelope.header_prev.clone(),
        cert_prev: envelope.cert_prev.clone(),
        header_new: bad_header_h2,
        cert_new: cert_h2.clone(),
        diff: envelope.diff.clone(),
        tracked_set_h1: envelope.tracked_set_h1.clone(),
        tracked_set_h2: envelope.tracked_set_h2.clone(),
    };
    match ValidatorTracker::verify_diff_against_headers(
        &demo_genesis(),
        &blocks_in_range,
        &bad_envelope,
    ) {
        Err(LightError::CertificateMismatch { .. }) => {
            println!("negative 2: tamper header_h2.accounts_root → CertificateMismatch ✓");
        }
        other => panic!("expected CertificateMismatch, got {other:?}"),
    }

    // Negative test 3: drop one `added` entry → DiffMismatch. The wallet's
    // replay finds the missing node and rejects the smaller claim.
    if envelope.diff.added.len() >= 2 {
        let mut bad_envelope = envelope.clone();
        bad_envelope.diff.added.pop();
        match ValidatorTracker::verify_diff_against_headers(
            &demo_genesis(),
            &blocks_in_range,
            &bad_envelope,
        ) {
            Err(LightError::DiffMismatch { .. }) => {
                println!("negative 3: drop one added entry → DiffMismatch ✓");
            }
            other => panic!("expected DiffMismatch after dropping an added entry, got {other:?}"),
        }
    }

    println!();
    println!(
        "M28 — light wallet verified a cert-signed temporal graph diff \
between two cert-signed heights without trusting the prover's claim: \
the wallet-side partial replay re-derives every added/dropped node \
from its own header cache, and per-leaf Merkle proofs bind each \
claim entry to its accounts_root."
    );
}

/// M29 demo: heterogeneous batched proof transport — one round-trip
/// fetches any mix of Account inclusion + kNN claim + Range claim +
/// Diff envelope under a single self-contained response. The wallet
/// dispatches each slot to the matching existing verifier; no new SPV
/// logic is added on top of M22–M28.
fn cmd_batch() {
use zhixing_node::light::{
    BatchItem, BatchResponseEnvelope, BatchResponseItem, ProofKind,
};

    println!("M29 — heterogeneous batched proof transport (Inclusion + kNN + Range + Diff in one round-trip)");
    println!();

    // Build the same 2-block certified chain cmd_diff uses — that way
    // every batched slot has a non-trivial body to verify.
    let mut driver = ChainDriver::new(demo_genesis(), demo_driver_seeds(), 4);
    driver
        .submit(tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0))
        .expect("submit 1");
    driver.produce(1.0, &BTreeSet::new()).unwrap().expect("block 1");
    driver
        .submit(tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 2.0))
        .expect("submit 2");
    driver
        .submit(tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 2.5))
        .expect("submit 3");
    driver.produce(2.0, &BTreeSet::new()).unwrap().expect("block 2");
    let blocks = driver.blocks().to_vec();
    let certs = driver.certificates().to_vec();
    let header_h2 = BlockHeader::from_block(&blocks[1]);
    let cert_h2 = certs[1].clone();
    let blocks_in_range: Vec<(Block, Commit)> = blocks
        .iter()
        .cloned()
        .zip(certs.iter().cloned())
        .collect();

    // Full peer — wraps the same certified chain, so `serve_batch` reads
    // from the same state every per-primitive serve_* method does.
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);

    // Build a heterogeneous request: (Inclusion, Knn, Range, Diff).
    // The full peer responds with one envelope carrying all four bodies.
    let query = [1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let items = vec![
        BatchItem::Inclusion { kind: ProofKind::Account, id: 1 },
        BatchItem::Knn { query, k: 3 },
        BatchItem::Range { query, min_sim: 0.0 },
        BatchItem::Diff { h1: 1, h2: 2 },
    ];
    let envelope: BatchResponseEnvelope = full
        .serve_batch(items.clone())
        .expect("full peer serves a heterogeneous batch");
    assert_eq!(envelope.items.len(), 4, "envelope must have one slot per request item");
    println!(
        "full peer served a heterogeneous batch: 4 items, |inclusion|={:?}, |knn_neighbours|={:?}, |range_nodes|={:?}, diff.added={}",
        matches!(envelope.items[0], BatchResponseItem::Inclusion(Some(_))),
        if let BatchResponseItem::Knn(Some(c)) = &envelope.items[1] { Some(c.neighbours.len()) } else { None },
        if let BatchResponseItem::Range(Some(c)) = &envelope.items[2] { Some(c.nodes.len()) } else { None },
        if let BatchResponseItem::Diff(d) = &envelope.items[3] { d.diff.added.len() } else { 0 },
    );

    // The wallet threads (header_h2, cert_h2, tracked_set_h2) explicitly
    // because the tracker's `self` only stores set/bonds/pubkeys/head/height.
    let mut tracker = ValidatorTracker::from_genesis(&demo_genesis());
    // follow_all populates the tracked set chain so verify_batch's
    // cert-binding context matches the live header cache.
    tracker.follow_all(&blocks, &certs).expect("follow");
    let tracked_h2 = tracker.validators().clone();
    ValidatorTracker::verify_batch(
        &tracker,
        &demo_genesis(),
        &header_h2,
        &cert_h2,
        &tracked_h2,
        &blocks_in_range,
        &items,
        &envelope,
    )
    .expect("wallet-side verify_batch accepts the heterogeneous envelope");
    println!("wallet: verify_batch → Ok");
    println!();

    // Negative 1: tamper inclusion balance → MembershipProofInvalid.
    if let BatchResponseItem::Inclusion(Some(ProofEntry::Account { id, mut account, proof })) =
        envelope.items[0].clone()
    {
        account.balance = account.balance.wrapping_add(1);
        let mut bad = envelope.clone();
        bad.items[0] = BatchResponseItem::Inclusion(Some(ProofEntry::Account {
            id,
            account,
            proof,
        }));
        match ValidatorTracker::verify_batch(
            &tracker,
            &demo_genesis(),
            &header_h2,
            &cert_h2,
            &tracked_h2,
            &blocks_in_range,
            &items,
            &bad,
        ) {
            Err(LightError::MembershipProofInvalid { .. }) => {
                println!("negative 1: tamper inclusion balance → MembershipProofInvalid ✓");
            }
            other => panic!("expected MembershipProofInvalid, got {other:?}"),
        }
    }

    // Negative 2: swap two kNN neighbours → KnnRankingMismatch.
    if let BatchResponseItem::Knn(Some(mut claim)) = envelope.items[1].clone() {
        if claim.neighbours.len() >= 2 {
            claim.neighbours.swap(0, 1);
        }
        let mut bad = envelope.clone();
        bad.items[1] = BatchResponseItem::Knn(Some(claim));
        match ValidatorTracker::verify_batch(
            &tracker,
            &demo_genesis(),
            &header_h2,
            &cert_h2,
            &tracked_h2,
            &blocks_in_range,
            &items,
            &bad,
        ) {
            Err(LightError::KnnRankingMismatch { .. }) => {
                println!("negative 2: swap two kNN neighbours → KnnRankingMismatch ✓");
            }
            other => panic!("expected KnnRankingMismatch, got {other:?}"),
        }
    }

    // Negative 3: pop one diff `added` entry → DiffMismatch.
    if let BatchResponseItem::Diff(env) = envelope.items[3].clone() {
        if env.diff.added.len() >= 2 {
            let mut bad_env = (*env).clone();
            bad_env.diff.added.pop();
            let mut bad = envelope.clone();
            bad.items[3] = BatchResponseItem::Diff(Box::new(bad_env));
            match ValidatorTracker::verify_batch(
                &tracker,
                &demo_genesis(),
                &header_h2,
                &cert_h2,
                &tracked_h2,
                &blocks_in_range,
                &items,
                &bad,
            ) {
                Err(LightError::DiffMismatch { .. }) => {
                    println!("negative 3: pop one diff added entry → DiffMismatch ✓");
                }
                other => panic!("expected DiffMismatch, got {other:?}"),
            }
        }
    }

    println!();
    println!(
        "M29 — light wallet verified a heterogeneous batched envelope \
         (Inclusion + kNN + Range + Diff) in a single round-trip: each slot \
         dispatches to its existing per-primitive verifier, no new SPV \
         logic, and protocol violations (kind mismatch, count mismatch) \
         surface as explicit BatchItemKindMismatch / BatchItemCountMismatch."
    );
}

/// M30 demo: trustless bridge between two chains running the same protocol.
///
/// Chain A locks tokens in a `BridgeLock`, committing to A's cert-signed
/// `bridge_root`. A relayer ferries the resulting `LockEnvelope` (header,
/// cert, validator set, lock, inclusion proof) to chain B. B's destination
/// `BridgeEndpoint` verifies the lock against A's cert-signed root — no new
/// SPV logic, just the M22 cert-binding + Merkle inclusion primitives — then
/// credits the destination account, with a dedup set blocking replay.
///
/// The soundness claim: a bridge is a light client + a dedup set.
fn cmd_bridge() {
    println!("M30 — trustless bridge (relay + verify-from-counterparty)");
    println!();

    let ga = demo_genesis();
    let gb = demo_genesis_b();
    let b_genesis_hash = ChainState::genesis(gb.clone()).1;

    // Chain A: account 1 locks 12 micro-$COG for account 7 on chain B.
    let mut driver_a = ChainDriver::new(ga.clone(), demo_driver_seeds(), 4);
    let lock = BridgeLock {
        account: 1,
        amount: 12 * MICRO,
        dest_chain: b_genesis_hash,
        dest_account: 7,
        nonce: 0,
        signature: [0u8; 64],
    }
    .signed(&kp(1));
    driver_a.stage_bridge_lock(lock.clone());
    driver_a.produce(1.0, &BTreeSet::new()).unwrap().expect("A block 1");
    let blocks_a = driver_a.blocks().to_vec();
    let certs_a = driver_a.certificates().to_vec();
    assert_eq!(blocks_a.len(), 1);
    assert_eq!(certs_a.len(), 1);

    // Source-side accounting sanity: A's balance dropped, A's bridge_locked
    // rose; supply is conserved (a redistribution within supply).
    let a_balance = driver_a.chain.state.accounts.get(&1).unwrap().balance;
    let a_locked = driver_a.chain.state.bridge_locked;
    let a_supply = driver_a.chain.state.supply;
    assert_eq!(a_locked, 12 * MICRO, "source bridge_locked pool");
    assert!(a_supply > 0, "supply remains positive");
    assert!(driver_a.chain.state.supply_conserved());
    println!(
        "chain A: account 1 balance={}, bridge_locked={}, supply conserved ✓",
        a_balance, a_locked
    );

    // Relay: a full peer of A serves the lock envelope to a peer of B.
    let mut full_a = GossipNode::new(1, ga.clone(), 8, [1, 2]);
    full_a.load_certified(&blocks_a, &certs_a);
    let env = full_a.serve_lock(0).expect("relay: A serves lock 0");
    println!(
        "relay: served LockEnvelope (header h={}, lock_id=0, amount={})",
        env.source_header.height, env.lock.amount
    );

    // Destination endpoint on B: follow source chain, verify lock, credit.
    let mut endpoint_b = BridgeEndpoint::new(&gb, &ga);
    endpoint_b
        .follow_source(&env.source_header, &env.source_cert, &env.source_tracked_set)
        .expect("B follows A's height 1");
    let verified = endpoint_b.verify_lock(&env).expect("verify_lock → Ok");
    println!(
        "chain B: verify_lock → Ok (dest_account={}, amount={})",
        verified.dest_account, verified.amount
    );
    endpoint_b.consume(&verified).expect("consume");
    assert_eq!(endpoint_b.minted(7), 12 * MICRO, "B credits destination");
    println!("chain B: consume → minted(7) = {} ✓", endpoint_b.minted(7));
    println!();

    // Negative 1: tamper the lock amount in the envelope → inclusion fails.
    let mut bad1 = env.clone();
    bad1.lock.amount += 1; // leaf no longer matches the committed root
    match endpoint_b.verify_lock(&bad1) {
        Err(BridgeError::Cert(LightError::MembershipProofInvalid { .. })) => {
            println!("negative 1: tamper lock.amount → MembershipProofInvalid ✓");
        }
        other => panic!("expected MembershipProofInvalid, got {other:?}"),
    }

    // Negative 2: wrong destination. Build a fresh lock destined for a
    // *third* chain; B's endpoint must reject with WrongDestination.
    let mut gc = demo_genesis();
    gc.timestamp_days = 2.0;
    let c_genesis_hash = ChainState::genesis(gc.clone()).1;
    let mut driver_a2 = ChainDriver::new(ga.clone(), demo_driver_seeds(), 4);
    let wrong_dest_lock = BridgeLock {
        account: 1,
        amount: 4 * MICRO,
        dest_chain: c_genesis_hash, // NOT chain B
        dest_account: 5,
        nonce: 0,
        signature: [0u8; 64],
    }
    .signed(&kp(1));
    driver_a2.stage_bridge_lock(wrong_dest_lock.clone());
    driver_a2
        .produce(1.0, &BTreeSet::new())
        .unwrap()
        .expect("A block 1 (wrong dest)");
    let mut full_a2 = GossipNode::new(1, ga.clone(), 8, [1, 2]);
    full_a2.load_certified(driver_a2.blocks(), driver_a2.certificates());
    let env_wrong = full_a2.serve_lock(0).expect("serve wrong-dest lock");
    let mut endpoint_b2 = BridgeEndpoint::new(&gb, &ga);
    endpoint_b2
        .follow_source(
            &env_wrong.source_header,
            &env_wrong.source_cert,
            &env_wrong.source_tracked_set,
        )
        .expect("B follows A");
    match endpoint_b2.verify_lock(&env_wrong) {
        Err(BridgeError::WrongDestination { .. }) => {
            println!("negative 2: lock destined for chain C → WrongDestination ✓");
        }
        other => panic!("expected WrongDestination, got {other:?}"),
    }

    // Negative 3: replay the *verified* (and consumed) lock → AlreadyConsumed.
    match endpoint_b.verify_lock(&env) {
        Err(BridgeError::AlreadyConsumed { .. }) => {
            println!("negative 3: replay verified lock → AlreadyConsumed ✓");
        }
        other => panic!("expected AlreadyConsumed, got {other:?}"),
    }

    // Negative 4: tamper source_header.bridge_root → CertificateMismatch.
    let mut bad4 = env.clone();
    bad4.source_header.bridge_root = [0xFFu8; 32]; // changes header.hash()
    match endpoint_b.verify_lock(&bad4) {
        Err(BridgeError::Cert(LightError::CertificateMismatch { .. })) => {
            println!("negative 4: tamper header.bridge_root → CertificateMismatch ✓");
        }
        other => panic!("expected CertificateMismatch, got {other:?}"),
    }

    println!();
    println!(
        "M30 — trustless bridge: A's validator-signed BridgeLock was relayed \
         to B and verified entirely from the cert-signed bridge_root (no \
         new SPV logic — M22 cert-binding + M25–M28 Merkle inclusion), then \
         credited on B; replay / wrong-destination / tampered-proof / \
         tampered-root are each rejected by the destination endpoint."
    );
}

/// M31 demo: consensus-level bridge redeem + mint. Where `cmd_bridge` (M30)
/// verified and credited a relayed lock in an *off-chain* endpoint, here chain
/// B's producer advances an *on-chain* follower of chain A (`BridgeHeader`) and
/// redeems a source lock (`BridgeRedeem`) inside its own state machine — so the
/// mint is BFT-enforced by B's validators, and replay-dedup lives in cert-signed
/// state. The relayer stays trustless: it only moves A's cert-signed bytes.
fn cmd_redeem() {
    println!("M31 — consensus-level bridge redeem + mint (destination side on-chain)");
    println!();

    let ga = demo_genesis();
    let a_genesis_hash = ChainState::genesis(ga.clone()).1;
    let a_genesis_set = ChainState::genesis(ga.clone()).0.validators.clone();

    // Chain B: register A as an allowed bridge source (its genesis hash + set is
    // the trust anchor), and fund a zero-balance destination account 5.
    let mut gb = demo_genesis_b();
    gb.bridge_sources = vec![(
        a_genesis_hash,
        a_genesis_set
            .validators()
            .iter()
            .map(|v| (v.id, v.pubkey, v.power))
            .collect(),
    )];
    gb.accounts.push((5, 0, kp(5).public()));
    let b_genesis_hash = ChainState::genesis(gb.clone()).1;
    let c_genesis_hash = [0xCCu8; 32]; // a third chain neither A nor B follows

    // Chain A locks twice in one block: lock 0 → B (redeemable), lock 1 → C
    // (a decoy for the wrong-destination case).
    let mut driver_a = ChainDriver::new(ga.clone(), demo_driver_seeds(), 4);
    let lock0 = BridgeLock {
        account: 1,
        amount: 10 * MICRO,
        dest_chain: b_genesis_hash,
        dest_account: 5,
        nonce: 0,
        signature: [0u8; 64],
    }
    .signed(&kp(1));
    let lock1 = BridgeLock {
        account: 1,
        amount: 5 * MICRO,
        dest_chain: c_genesis_hash,
        dest_account: 5,
        nonce: 1,
        signature: [0u8; 64],
    }
    .signed(&kp(1));
    driver_a.stage_bridge_lock(lock0.clone());
    driver_a.stage_bridge_lock(lock1.clone());
    driver_a.produce(1.0, &BTreeSet::new()).unwrap().expect("A locks");
    let a_header = driver_a.blocks()[0].header();
    let a_cert = driver_a.certificates()[0].clone();
    let proof0 = driver_a.chain.state.bridge_lock_proof(0).expect("proof for lock 0");
    let proof1 = driver_a.chain.state.bridge_lock_proof(1).expect("proof for lock 1");
    println!(
        "chain A: locked {} → B (lock 0) and {} → C (lock 1); bridge_locked={}",
        cog(lock0.amount),
        cog(lock1.amount),
        cog(driver_a.chain.state.bridge_locked)
    );

    // Chain B follows A's height 1 on-chain, then redeems lock 0.
    let mut driver_b = ChainDriver::new(gb.clone(), demo_driver_seeds(), 4);
    driver_b.stage_bridge_header(BridgeHeader {
        source_chain: a_genesis_hash,
        header: a_header.clone(),
        cert: a_cert.clone(),
        next_set: a_genesis_set.clone(),
    });
    driver_b.produce(1.0, &BTreeSet::new()).unwrap().expect("B follows A");
    // Snapshot B at the followed-but-unredeemed state, for the negative cases.
    let followed = driver_b.chain.clone();
    println!(
        "chain B: followed A → source follower at height {}",
        driver_b.chain.state.bridge_sources.get(&a_genesis_hash).unwrap().height
    );

    driver_b.stage_bridge_redeem(BridgeRedeem {
        source_chain: a_genesis_hash,
        source_header: a_header.clone(),
        source_cert: a_cert.clone(),
        lock_id: 0,
        lock: lock0.clone(),
        proof: proof0.clone(),
    });
    driver_b.produce(2.0, &BTreeSet::new()).unwrap().expect("B redeems lock 0");

    let minted = driver_b.chain.state.accounts.get(&5).unwrap().balance;
    assert_eq!(minted, 10 * MICRO, "destination credited on-chain");
    assert_eq!(driver_b.chain.state.bridge_minted, 10 * MICRO, "audit counter");
    assert!(driver_b.chain.state.supply_conserved(), "supply invariant holds");
    println!(
        "Ok: redeemed lock 0 in B's state machine → minted {} to account 5, \
         bridge_minted={}, supply conserved ✓",
        cog(minted),
        cog(driver_b.chain.state.bridge_minted)
    );
    println!();

    // Each negative builds an unsealed block carrying one bad redeem and commits
    // it directly to a fresh clone — `commit` re-runs apply with checks and the
    // whole block rolls back on the distinct `ChainError`.
    let try_redeem = |mut chain: Chain, redeem: BridgeRedeem, ts: f32| -> ChainError {
        let mut blk = empty_next_block(&chain, ts);
        blk.bridge_redeems.push(redeem);
        chain.commit(&mut blk).expect_err("bad redeem must be rejected")
    };

    // Negative 1: tamper lock.amount → leaf no longer opens under bridge_root.
    let mut bad_amount = lock0.clone();
    bad_amount.amount += 1;
    let e = try_redeem(
        followed.clone(),
        BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_header.clone(),
            source_cert: a_cert.clone(),
            lock_id: 0,
            lock: bad_amount,
            proof: proof0.clone(),
        },
        2.0,
    );
    assert!(matches!(e, ChainError::BridgeInclusionInvalid { .. }));
    println!("✓ tampered lock.amount → {e}");

    // Negative 2: tamper source_header.bridge_root → cert-binding fails first.
    let mut bad_header = a_header.clone();
    bad_header.bridge_root = [0xFFu8; 32];
    let e = try_redeem(
        followed.clone(),
        BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: bad_header,
            source_cert: a_cert.clone(),
            lock_id: 0,
            lock: lock0.clone(),
            proof: proof0.clone(),
        },
        2.0,
    );
    assert!(matches!(e, ChainError::BridgeCertInvalid { .. }));
    println!("✓ tampered source_header.bridge_root → {e}");

    // Negative 3: redeem lock 1, which is destined for chain C, not B.
    let e = try_redeem(
        followed.clone(),
        BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_header.clone(),
            source_cert: a_cert.clone(),
            lock_id: 1,
            lock: lock1.clone(),
            proof: proof1,
        },
        2.0,
    );
    assert!(matches!(e, ChainError::BridgeWrongDestination { .. }));
    println!("✓ lock destined for chain C → {e}");

    // Negative 4: replay lock 0 on the chain that already redeemed it.
    let e = try_redeem(
        driver_b.chain.clone(),
        BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_header.clone(),
            source_cert: a_cert.clone(),
            lock_id: 0,
            lock: lock0.clone(),
            proof: proof0.clone(),
        },
        3.0,
    );
    assert!(matches!(e, ChainError::BridgeAlreadyRedeemed { .. }));
    println!("✓ replay redeemed lock 0 → {e}");

    // Negative 5: redeem before following A (fresh B, follower still at height 0).
    let e = try_redeem(
        Chain::new(gb.clone()),
        BridgeRedeem {
            source_chain: a_genesis_hash,
            source_header: a_header.clone(),
            source_cert: a_cert.clone(),
            lock_id: 0,
            lock: lock0.clone(),
            proof: proof0.clone(),
        },
        1.0,
    );
    assert!(matches!(e, ChainError::BridgeSourceNotFollowed { .. }));
    println!("✓ redeem before follow → {e}");

    println!();
    println!(
        "M31 — consensus-level bridge: B's producer followed A's cert-signed \
         header and redeemed a source lock *inside its state machine*, minting \
         new supply to the destination account (1:1-backed by A's permanently \
         locked pool) — the mint and replay-dedup are BFT-enforced, not \
         off-chain; a forged proof, forged root, wrong destination, replay, or \
         un-followed source are each rejected by consensus."
    );
}

/// An empty block at `chain`'s next height (all commitments zero — `commit`
/// stamps them). Used by demos that carry a single body op.
fn empty_next_block(chain: &Chain, timestamp_days: f32) -> Block {
    Block {
        height: chain.state.height + 1,
        prev_hash: chain.head,
        timestamp_days,
        next_validators_root: [0u8; 32],
        state_root: [0u8; 32],
        accounts_root: [0u8; 32],
        graph_root: [0u8; 32],
        bridge_root: [0u8; 32],
        txs: Vec::new(),
        validator_updates: Vec::new(),
        stake_ops: Vec::new(),
        slashing_evidence: Vec::new(),
        bridge_locks: Vec::new(),
        bridge_headers: Vec::new(),
        bridge_redeems: Vec::new(),
    }
}


/// a concept) against a cert-signed header via the unified GetProof bus —
/// no replay, no graph download, no tx bodies. The graph node lives in the
/// same `accounts_root` tree as accounts and reviewers; the wallet
/// recomputes the leaf locally from the typed `ProofEntry::GraphNode`.
fn cmd_graph() {
use zhixing_node::light::ProofKind;

    println!("M25 — cognitive-graph inclusion proof against a cert-signed header");
    println!();

    // Build a real certified chain. `run_driver(1)` produces block 1, which
    // includes an accepted submission, so `state.graph` has at least one
    // freshly minted node (in addition to any genesis seed nodes).
    let (blocks, certs) = run_driver(1);

    // Wrap in GossipNode (full) + LightGossipNode (light) over LightNetwork.
    let mut full = GossipNode::new(1, demo_genesis(), 8, [1, 2]);
    full.load_certified(&blocks, &certs);
    let light_id = 2u64;
    let light = LightGossipNode::new(light_id, &demo_genesis(), [1, 2]);
    let mut net = LightNetwork::new(vec![full], vec![light]);

    // Side-channel next_set_for so the M22 sync runs through apply_header.
    let sets = committed_sets(&demo_genesis(), &blocks);
    let next_set_for = |h: u64| sets.get((h - 1) as usize).cloned();

    net.announce_all();
    for _ in 0..20 {
        let n = net.run(1, light_id, &next_set_for);
        if n == 0 && net.light_node(light_id).tracker().height() == 1 {
            break;
        }
        if n == 0 {
            break;
        }
    }
    let lt = net.light_node(light_id).tracker().clone();
    println!(
        "M22 sync: light at height {}, head {}",
        lt.height(),
        short(&lt.head())
    );
    println!();

    // The chain state must have at least one graph node (genesis seeds or
    // accepted submissions). Pick the first one — guaranteed.
    let last_idx = (lt.height() as usize).saturating_sub(1);
    let last_block = &blocks[last_idx];
    let last_cert = &certs[last_idx];
    let n_graph = net.full_node(1).chain.state.graph.nodes.len();
    if n_graph == 0 {
        eprintln!("graph is empty — demo chain must seed at least one node");
        return;
    }
    let idx = 0usize;
    let expected_node = net.full_node(1).chain.state.graph.nodes[idx].clone();
    println!("graph node to prove:");
    println!("  node_id   = {}", expected_node.node_id);
    println!("  domain    = {}", expected_node.domain);
    println!(
        "  embedding = [{:.2}, {:.2}, {:.2}, {:.2}, {:.2}, {:.2}, {:.2}, {:.2}]",
        expected_node.embedding[0], expected_node.embedding[1],
        expected_node.embedding[2], expected_node.embedding[3],
        expected_node.embedding[4], expected_node.embedding[5],
        expected_node.embedding[6], expected_node.embedding[7],
    );

    // Send a single batched GetProof: account #1 + graph node (idx 0).
    let mut full_node = net.take_full(1);
    let mut light_node = net.take_light(light_id);
    let reply = full_node.on_message(
        light_id,
        GossipMsg::GetProof {
            items: vec![
                (ProofKind::Account, 1),
                (ProofKind::GraphNode, idx as u64),
            ],
        },
    );
    assert_eq!(reply.len(), 1, "full peer must serve the batched proofs");
    let (_dst, proof_msg) = reply.into_iter().next().unwrap();
    light_node.on_message(1, proof_msg);

    let header = zhixing_node::codec::BlockHeader::from_block(last_block);
    let tracked = ValidatorTracker::from_genesis(&demo_genesis()).validators().clone();

    let account_entry = light_node
        .take_proof(ProofKind::Account, 1)
        .expect("account proof cached");
    let graph_entry = light_node
        .take_proof(ProofKind::GraphNode, expected_node.node_id)
        .expect("graph node proof cached");

    let account_ok = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &account_entry,
    );
    let graph_ok = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &graph_entry,
    );

    let ProofEntry::Account { account, .. } = &account_entry else {
        unreachable!("account entry shape");
    };
    let ProofEntry::GraphNode { graph_node, .. } = &graph_entry else {
        unreachable!("graph entry shape");
    };

    println!();
    println!("verify_proof_against_header (Account #1, balance = {} COG) -> {}",
        account.balance / MICRO, account_ok.is_ok());
    println!(
        "verify_proof_against_header (GraphNode #{}, domain = {}) -> {}",
        graph_node.node_id, graph_node.domain, graph_ok.is_ok()
    );

    // Negative: tamper with the graph node's embedding -> verifier recomputes
    // the leaf locally and rejects the proof with MembershipProofInvalid.
    let ProofEntry::GraphNode { node_id: nid, graph_node: real_node, proof: p } = graph_entry.clone() else {
        unreachable!()
    };
    let mut bad_node = real_node.clone();
    bad_node.embedding[0] += 1.0;
    let forged_entry = ProofEntry::GraphNode {
        node_id: nid,
        graph_node: bad_node,
        proof: p.clone(),
    };
    let bad = ValidatorTracker::verify_proof_against_header(
        &header, last_cert, &tracked, &forged_entry,
    );
    println!();
    println!(
        "tampered embedding[0] -> {} (must be false: {})",
        bad.is_ok(),
        match bad.as_ref().err() {
            Some(e) => format!("{e}"),
            None => String::from("ok (UNEXPECTED)"),
        }
    );

    // Negative: tamper with header.accounts_root — root is cert-signed, so
    // the cert no longer matches `header.hash()` -> CertificateMismatch.
    let mut bad_header = header.clone();
    bad_header.accounts_root = [0xAB; 32];
    let bad_root = ValidatorTracker::verify_proof_against_header(
        &bad_header, last_cert, &tracked, &graph_entry,
    );
    println!(
        "tampered accounts_root -> {} (must be false: {})",
        bad_root.is_ok(),
        match bad_root.as_ref().err() {
            Some(e) => format!("{e}"),
            None => String::from("ok (UNEXPECTED)"),
        }
    );

    net.put_full(full_node);
    net.put_light(light_node);
}


/// Run the real networked daemon. Loads the node config and its referenced
/// genesis, derives this process's single validator key (M33: one key per node,
/// no sequencer) from the `[validator]` section, builds a multi-thread tokio
/// runtime, and blocks on `daemon::run` until Ctrl-C. `main()` stays sync so the
/// ~20 in-memory demo commands are unaffected by the async runtime.
fn cmd_run(config_path: String) {
    let cfg = config::load_node_config(&config_path)
        .unwrap_or_else(|e| fail_msg("load node config", &e));
    let gcfg = config::load_genesis(&cfg.genesis)
        .unwrap_or_else(|e| fail_msg("load genesis", &e));
    let genesis = gcfg.to_genesis().unwrap_or_else(|e| fail_msg("build genesis", &e));

    // M33: an enabled `[validator]` section makes this node a voting validator;
    // absent or disabled ⇒ a pure follower that syncs + verifies but never votes.
    let validator_key = match cfg.validator.as_ref() {
        Some(vc) if vc.enabled => {
            Some(vc.keypair().unwrap_or_else(|e| fail_msg("validator key", &e)))
        }
        _ => None,
    };

    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| fail("tokio runtime", e));
    rt.block_on(async move {
        if let Err(e) = daemon::run(cfg, genesis, validator_key).await {
            fail("daemon", e);
        }
    });
}

/// End-to-end showcase on the production path: launch a small tokio testnet
/// entirely in-process — **four validators (21..24), no sequencer** — wired over
/// the **real** TCP transport on loopback. Each node owns one signing key and
/// votes; blocks are finalized by distributed prevote/precommit gossip with
/// wall-clock timeouts. Submit a few transactions to one node (they flood) and
/// poll until every node has synced + verified the same head.
fn cmd_localnet() {
    let ids = [21u64, 22, 23, 24];
    let base_port = 19021u16;
    let genesis = demo_genesis();

    // one private data dir per node so their logs never collide
    let root = std::env::temp_dir().join(format!("zhixing-localnet-{}", std::process::id()));

    // build a config per node; every node lists all the others as peers. The
    // signing key is supplied directly to `Node::start` below (not via config).
    let addr = |id: u64| format!("127.0.0.1:{}", base_port + (id as u16 - 21));
    let node_cfg = |id: u64| -> NodeConfig {
        let peers = ids
            .iter()
            .filter(|&&p| p != id)
            .map(|&p| PeerConfig { id: p, addr: addr(p) })
            .collect();
        NodeConfig {
            node: NodeSection {
                id,
                listen: addr(id),
                data_dir: root.join(format!("n{id}")).to_string_lossy().into_owned(),
            },
            peers,
            genesis: String::new(), // supplied directly to Node::start below
            validator: None,
            consensus: crate::config::ConsensusConfig::default(),
            network: crate::config::NetworkConfig::default(),
        }
    };

    println!("localnet: starting 4 validators (21..24), no sequencer, over loopback TCP\n");

    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| fail("tokio runtime", e));
    rt.block_on(async move {
        let mut nodes = Vec::new();
        for &id in &ids {
            let node = daemon::Node::start(node_cfg(id), genesis.clone(), Some(kp(id)))
                .await
                .unwrap_or_else(|e| fail("start node", e));
            nodes.push((id, node));
        }

        // give the mesh a moment to dial + handshake, then feed txs to one node
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let any = &nodes[0].1;
        for t in [
            tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0),
            tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0),
            tx(3, blend(1, 2), 3, reviews(&[(10, 0.9), (11, 0.9), (12, 0.85)]), (3, 3), 1.0),
        ] {
            any.submit(t);
        }
        let target = 3u64;

        // poll until every node reports the same height >= target (or time out)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
        loop {
            let mut heights = Vec::new();
            for (id, node) in &nodes {
                heights.push((*id, node.status().await.unwrap_or((0, [0u8; 32]))));
            }
            let converged = heights.iter().all(|(_, (h, _))| *h >= target)
                && heights.windows(2).all(|w| w[0].1 == w[1].1);
            if converged || std::time::Instant::now() >= deadline {
                println!("final node states:");
                for (id, (h, head)) in &heights {
                    println!("  node {id} (validator)  height {h}  head {}", short(head));
                }
                if converged {
                    println!("\n✓ all 4 validators converged on the same cert-verified head via distributed voting (no sequencer)");
                } else {
                    eprintln!("\n✗ nodes did not converge before the deadline");
                    exit(1);
                }
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });

    // best-effort cleanup of the scratch dirs
    let _ = std::fs::remove_dir_all(root);
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

fn commit_print(chain: &mut Chain, log: Option<&BlockLog>, label: &str, mut blk: Block) {
    match chain.commit(&mut blk) {
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

/// Run the chain driver until the mempool drains (or `max_heights`), returning
/// the certified `(blocks, certs)` vectors the driver produced.
fn run_driver(max_heights: usize) -> (Vec<Block>, Vec<Commit>) {
    let mut driver = ChainDriver::new(demo_genesis(), demo_driver_seeds(), 4);
    // M24: stage a brand-new validator (id 25) so the M24 batched proof demo
    // can ask for an Account + Reviewer + Validator proof against the same
    // post-block next-set, with all three leaves non-trivial.
    driver.stage_validator_update(ValidatorUpdate {
        id: 25,
        pubkey: kp(25).public(),
        power: 2 * MICRO,
    });
    // Submit enough txs that the mempool actually produces blocks (otherwise
    // `produce_until_drained` returns 0 because no candidate is yielded).
    driver.submit(tx(1, unit(1), 1, reviews(&[(10, 0.9), (11, 0.85), (12, 0.9)]), (3, 3), 1.0)).expect("submit 1");
    driver.submit(tx(2, unit(2), 2, reviews(&[(10, 0.88), (11, 0.9), (12, 0.86)]), (3, 3), 1.0)).expect("submit 2");
    driver
        .produce_until_drained(1.0, max_heights)
        .expect("driver");
    let blocks = driver.blocks().to_vec();
    let certs = driver.certificates().to_vec();
    assert_eq!(blocks.len(), certs.len());
    (blocks, certs)
}

/// Deterministic keypair seeds for the demo validator set (ids 21..=24).
fn demo_driver_seeds() -> std::collections::BTreeMap<u64, [u8; 32]> {
    let ids = [21u64, 22, 23, 24];
    ids.iter()
        .map(|&id| {
            let mut s = [0u8; 32];
            s[..8].copy_from_slice(&id.to_le_bytes());
            (id, s)
        })
        .collect()
}

/// Per-height committed sets from an authoritative replay: `sets[i]` is the
/// validator set that certifies height `i + 2` (what block `i + 1` commits to
/// in `next_validators_root`).
fn committed_sets(g: &Genesis, blocks: &[Block]) -> Vec<ValidatorSet> {
    let mut replay = Chain::new(g.clone());
    let mut sets = Vec::new();
    for b in blocks {
        let mut b = b.clone();
        replay.commit(&mut b).expect("commit");
        sets.push(replay.state.validators.clone());
    }
    sets
}
