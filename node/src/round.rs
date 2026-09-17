//! The BFT round state machine — the *liveness* half of consensus.
//!
//! `consensus` gives the safety half: a verifiable [`Commit`] certificate. But
//! nothing there *drives* validators to produce one under faults. This module is
//! that driver: a per-validator, per-height state machine that ingests proposals
//! and votes, fires step timeouts, changes rounds when a proposer is silent, and
//! *locks* on a value so two rounds can never finalize conflicting blocks. It is
//! a close transcription of the Tendermint algorithm (Buchman–Kwon–Milosevic
//! 2018): propose → prevote → precommit, with `lockedValue`/`validValue` and the
//! `upon` rules that guard safety across round changes.
//!
//! Determinism, again, is the whole point: every honest validator runs the same
//! [`RoundState`] transitions on the same messages, so they lock and decide
//! identically. Timeouts are modeled as explicit events (no wall clock), which
//! keeps the machine fully deterministic and testable. [`Sim`] wires N of these
//! together over an in-process message bus — a stand-in for the P2P gossip layer
//! (a later milestone) — so the mechanism runs end to end offline.
//!
//! Scope: this is single-height consensus (deciding one block at height H). The
//! chain loop that strings heights together, and real networking, sit above it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::consensus::{vote_signing_bytes, Commit, Vote, VoteType};
use crate::validator::ValidatorSet;
use crate::{codec, crypto, Block, Hash, Keypair, PubKey, Sig};

/// The vote target meaning "no value" (a prevote/precommit for nil). A real
/// block hash colliding with this is cryptographically negligible.
pub const NIL: Hash = [0u8; 32];

/// The three steps within a round. Ordered so `step >= Step::Prevote` reads
/// naturally in the `upon` guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Propose,
    Prevote,
    Precommit,
}

fn step_tag(s: Step) -> u8 {
    match s {
        Step::Propose => 0,
        Step::Prevote => 1,
        Step::Precommit => 2,
    }
}

fn tag_step(t: u8) -> Step {
    match t {
        0 => Step::Propose,
        1 => Step::Prevote,
        _ => Step::Precommit,
    }
}

/// A proposer's signed proposal for (`height`, `round`). `valid_round` is the
/// round at which the proposer last saw a prevote-quorum for this block (its
/// "proof of lock"), or `-1` when proposing a fresh value.
#[derive(Clone, Debug)]
pub struct Proposal {
    pub height: u64,
    pub round: u32,
    pub block: Block,
    pub valid_round: i64,
    pub proposer: u64,
    pub signature: Sig,
}

/// Bytes a proposer signs: the identity of the proposal, binding the block by
/// its hash (not the full block — the block travels alongside and is hashed).
pub fn proposal_signing_bytes(
    height: u64,
    round: u32,
    block_hash: &Hash,
    valid_round: i64,
    proposer: u64,
) -> Vec<u8> {
    let mut e = codec::Enc(Vec::new());
    e.u64(height);
    e.u32(round);
    e.raw(block_hash);
    e.u64(valid_round as u64); // two's-complement; -1 -> u64::MAX, deterministic
    e.u64(proposer);
    e.0
}

impl Proposal {
    pub fn signed(
        height: u64,
        round: u32,
        block: Block,
        valid_round: i64,
        proposer: u64,
        kp: &Keypair,
    ) -> Self {
        let msg = proposal_signing_bytes(height, round, &block.hash(), valid_round, proposer);
        let signature = kp.sign(&msg);
        Proposal { height, round, block, valid_round, proposer, signature }
    }

    fn verify_sig(&self, pk: &PubKey) -> bool {
        let msg = proposal_signing_bytes(
            self.height,
            self.round,
            &self.block.hash(),
            self.valid_round,
            self.proposer,
        );
        crypto::verify(pk, &msg, &self.signature)
    }
}

/// A consensus message on the wire.
#[derive(Clone, Debug)]
pub enum Msg {
    Proposal(Proposal),
    Vote(Vote),
}

/// A side effect the FSM asks its host to perform.
#[derive(Clone, Debug)]
pub enum Action {
    /// Send this message to every validator.
    Broadcast(Msg),
    /// Arm a timeout for (step, round); the host calls [`RoundState::on_timeout`]
    /// when it elapses.
    Schedule(Step, u32),
    /// Consensus finalized a block: here is the verifiable certificate.
    Decided(Commit),
}

/// One validator's consensus state machine for a single height.
pub struct RoundState {
    vset: ValidatorSet,
    id: u64,
    height: u64,
    round: u32,
    step: Step,
    /// The value+round this validator is locked on (set at precommit).
    locked: Option<(Block, u32)>,
    /// The latest value+round with a prevote-quorum this validator has seen.
    valid: Option<(Block, u32)>,
    decided: Option<Commit>,
    proposals: BTreeMap<u32, Proposal>,
    prevotes: BTreeMap<u32, BTreeMap<u64, Vote>>,
    precommits: BTreeMap<u32, BTreeMap<u64, Vote>>,
    /// One-shot guards for rules that must fire at most once per round.
    fired: BTreeSet<(u8, u32)>,
    /// The value this validator proposes when it is the proposer with nothing
    /// locked (`getValue()` in the paper). Fixed for a reference node.
    candidate: Block,
}

impl RoundState {
    pub fn new(vset: ValidatorSet, id: u64, height: u64, candidate: Block) -> Self {
        RoundState {
            vset,
            id,
            height,
            round: 0,
            step: Step::Propose,
            locked: None,
            valid: None,
            decided: None,
            proposals: BTreeMap::new(),
            prevotes: BTreeMap::new(),
            precommits: BTreeMap::new(),
            fired: BTreeSet::new(),
            candidate,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn round(&self) -> u32 {
        self.round
    }
    pub fn decided(&self) -> Option<&Commit> {
        self.decided.as_ref()
    }

    /// Enter the machine at round 0.
    pub fn start(&mut self, kp: &Keypair) -> Vec<Action> {
        let mut out = Vec::new();
        self.start_round(0, kp, &mut out);
        self.evaluate(kp, &mut out);
        out
    }

    /// Feed one received message, then re-evaluate.
    pub fn on_message(&mut self, kp: &Keypair, msg: Msg) -> Vec<Action> {
        let mut out = Vec::new();
        self.ingest(msg);
        self.evaluate(kp, &mut out);
        out
    }

    /// A previously-scheduled timeout for (step, round) elapsed.
    pub fn on_timeout(&mut self, kp: &Keypair, step: Step, round: u32) -> Vec<Action> {
        let mut out = Vec::new();
        if round == self.round {
            match step {
                Step::Propose if self.step == Step::Propose => {
                    // no valid proposal arrived in time -> prevote nil
                    self.cast_prevote(round, NIL, kp, &mut out);
                    self.step = Step::Prevote;
                }
                Step::Prevote if self.step == Step::Prevote => {
                    // saw a prevote-quorum but not for one value -> precommit nil
                    self.cast_precommit(round, NIL, kp, &mut out);
                    self.step = Step::Precommit;
                }
                Step::Precommit => {
                    // round failed to decide -> move on
                    self.start_round(round + 1, kp, &mut out);
                }
                _ => {}
            }
        }
        self.evaluate(kp, &mut out);
        out
    }

    // --- internals ----------------------------------------------------------

    fn proposer(&self, round: u32) -> u64 {
        self.vset
            .proposer_for_round(self.height, round)
            .expect("non-empty validator set")
    }

    fn power_of(&self, id: u64) -> u64 {
        self.vset.get(id).map(|v| v.power).unwrap_or(0)
    }

    fn quorum(&self) -> u64 {
        self.vset.quorum()
    }

    /// `f+1` worth of power: strictly more than 1/3, enough to prove at least one
    /// honest validator is at a higher round (the round-skip trigger).
    fn f_plus_one(&self) -> u64 {
        self.vset.total_power() - self.vset.quorum() + 1
    }

    fn valid_block(&self, b: &Block) -> bool {
        // Block-level validity for consensus: it must be for this height. Tx-level
        // validity is enforced separately by the state machine on commit.
        b.height == self.height
    }

    fn prevote_power(&self, round: u32, target: Option<Hash>) -> u64 {
        self.votes_power(&self.prevotes, round, target)
    }

    fn precommit_power(&self, round: u32, target: Option<Hash>) -> u64 {
        self.votes_power(&self.precommits, round, target)
    }

    fn votes_power(
        &self,
        map: &BTreeMap<u32, BTreeMap<u64, Vote>>,
        round: u32,
        target: Option<Hash>,
    ) -> u64 {
        map.get(&round)
            .map(|m| {
                m.values()
                    .filter(|v| target.map(|h| v.block_hash == h).unwrap_or(true))
                    .map(|v| self.power_of(v.validator))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Distinct voting power that sent *any* message at `round` (for round-skip).
    fn participation(&self, round: u32) -> u64 {
        let mut ids = BTreeSet::new();
        if let Some(m) = self.prevotes.get(&round) {
            ids.extend(m.keys().copied());
        }
        if let Some(m) = self.precommits.get(&round) {
            ids.extend(m.keys().copied());
        }
        ids.iter().map(|&id| self.power_of(id)).sum()
    }

    fn ingest(&mut self, msg: Msg) {
        match msg {
            Msg::Proposal(p) => {
                if p.height != self.height {
                    return;
                }
                if self.proposer(p.round) != p.proposer {
                    return; // only the round's proposer may propose
                }
                let Some(v) = self.vset.get(p.proposer) else { return };
                let pk = v.pubkey;
                if !p.verify_sig(&pk) {
                    return;
                }
                self.proposals.entry(p.round).or_insert(p);
            }
            Msg::Vote(v) => {
                if v.height != self.height {
                    return;
                }
                let Some(val) = self.vset.get(v.validator) else { return };
                let pk = val.pubkey;
                let bytes = vote_signing_bytes(v.validator, v.height, v.round, &v.block_hash, v.vote_type);
                if !crypto::verify(&pk, &bytes, &v.signature) {
                    return;
                }
                let map = match v.vote_type {
                    VoteType::Prevote => &mut self.prevotes,
                    VoteType::Precommit => &mut self.precommits,
                };
                // first vote per (round, validator) wins; a conflicting later one
                // is ignored here (equivocation is caught by detect_equivocation).
                map.entry(v.round).or_default().entry(v.validator).or_insert(v);
            }
        }
    }

    fn cast_prevote(&mut self, round: u32, hash: Hash, kp: &Keypair, out: &mut Vec<Action>) {
        let v = Vote::signed(self.id, self.height, round, hash, VoteType::Prevote, kp);
        self.ingest(Msg::Vote(v.clone()));
        out.push(Action::Broadcast(Msg::Vote(v)));
    }

    fn cast_precommit(&mut self, round: u32, hash: Hash, kp: &Keypair, out: &mut Vec<Action>) {
        let v = Vote::signed(self.id, self.height, round, hash, VoteType::Precommit, kp);
        self.ingest(Msg::Vote(v.clone()));
        out.push(Action::Broadcast(Msg::Vote(v)));
    }

    fn start_round(&mut self, round: u32, kp: &Keypair, out: &mut Vec<Action>) {
        self.round = round;
        self.step = Step::Propose;
        if self.proposer(round) == self.id {
            // re-propose the valid value if we have one (carries proof of lock),
            // otherwise propose a fresh candidate.
            let (block, vr) = match &self.valid {
                Some((b, r)) => (b.clone(), *r as i64),
                None => (self.candidate.clone(), -1),
            };
            let prop = Proposal::signed(self.height, round, block, vr, self.id, kp);
            self.ingest(Msg::Proposal(prop.clone()));
            out.push(Action::Broadcast(Msg::Proposal(prop)));
        } else {
            out.push(Action::Schedule(Step::Propose, round));
        }
    }

    fn evaluate(&mut self, kp: &Keypair, out: &mut Vec<Action>) {
        let mut guard = 0;
        while self.eval_once(kp, out) {
            guard += 1;
            if guard > 10_000 {
                break; // defensive: no rule should loop, but never hang
            }
        }
    }

    /// Apply the first applicable `upon` rule; return whether one fired.
    fn eval_once(&mut self, kp: &Keypair, out: &mut Vec<Action>) -> bool {
        let q = self.quorum();
        let r = self.round;

        // L49: decide on any round with a proposal and a precommit-quorum for it.
        if self.decided.is_none() {
            let rounds: Vec<u32> = self.proposals.keys().copied().collect();
            for pr in rounds {
                let bh = self.proposals[&pr].block.hash();
                if self.precommit_power(pr, Some(bh)) >= q {
                    let precommits: Vec<Vote> = self.precommits[&pr]
                        .values()
                        .filter(|v| v.block_hash == bh)
                        .cloned()
                        .collect();
                    let commit = Commit { height: self.height, round: pr, block_hash: bh, precommits };
                    self.decided = Some(commit.clone());
                    out.push(Action::Decided(commit));
                    return true;
                }
            }
        }
        if self.decided.is_some() {
            return false; // finalized; nothing more to drive
        }

        // L22 / L28: on the proposal for the current round, cast our prevote.
        if self.step == Step::Propose {
            if let Some(p) = self.proposals.get(&r).cloned() {
                let bh = p.block.hash();
                if p.valid_round < 0 {
                    // L22: fresh proposal
                    let target = if self.valid_block(&p.block) && self.locked_ok_fresh(&bh) {
                        bh
                    } else {
                        NIL
                    };
                    self.cast_prevote(r, target, kp, out);
                    self.step = Step::Prevote;
                    return true;
                } else if p.valid_round < r as i64
                    && self.prevote_power(p.valid_round as u32, Some(bh)) >= q
                {
                    // L28: proposal re-proposing a value locked at valid_round,
                    // backed by a prevote-quorum from that round
                    let target = if self.valid_block(&p.block) && self.locked_ok_pol(&bh, p.valid_round) {
                        bh
                    } else {
                        NIL
                    };
                    self.cast_prevote(r, target, kp, out);
                    self.step = Step::Prevote;
                    return true;
                }
            }
        }

        // L34: first prevote-quorum (any value) at this round -> arm prevote timeout.
        if self.step == Step::Prevote && !self.fired.contains(&(34, r)) && self.prevote_power(r, None) >= q {
            self.fired.insert((34, r));
            out.push(Action::Schedule(Step::Prevote, r));
            return true;
        }

        // L36: proposal + prevote-quorum for it -> lock and precommit (once/round).
        if self.step >= Step::Prevote && !self.fired.contains(&(36, r)) {
            if let Some(p) = self.proposals.get(&r).cloned() {
                let bh = p.block.hash();
                if self.valid_block(&p.block) && self.prevote_power(r, Some(bh)) >= q {
                    self.fired.insert((36, r));
                    if self.step == Step::Prevote {
                        self.locked = Some((p.block.clone(), r));
                        self.cast_precommit(r, bh, kp, out);
                        self.step = Step::Precommit;
                    }
                    self.valid = Some((p.block.clone(), r));
                    return true;
                }
            }
        }

        // L44: prevote-quorum for nil -> precommit nil.
        if self.step == Step::Prevote && self.prevote_power(r, Some(NIL)) >= q {
            self.cast_precommit(r, NIL, kp, out);
            self.step = Step::Precommit;
            return true;
        }

        // L47: first precommit-quorum (any value) at this round -> arm precommit timeout.
        if !self.fired.contains(&(47, r)) && self.precommit_power(r, None) >= q {
            self.fired.insert((47, r));
            out.push(Action::Schedule(Step::Precommit, r));
            return true;
        }

        // L55: f+1 power already at a higher round -> skip ahead to catch up.
        let f1 = self.f_plus_one();
        let mut higher: Vec<u32> = self
            .prevotes
            .keys()
            .chain(self.precommits.keys())
            .copied()
            .filter(|&rr| rr > r)
            .collect();
        higher.sort_unstable();
        higher.dedup();
        for rr in higher {
            if self.participation(rr) >= f1 {
                self.start_round(rr, kp, out);
                return true;
            }
        }

        false
    }

    /// L22 guard: prevote the proposal unless we are locked on a different value.
    fn locked_ok_fresh(&self, bh: &Hash) -> bool {
        match &self.locked {
            None => true,
            Some((b, _)) => b.hash() == *bh,
        }
    }

    /// L28 guard: prevote the re-proposed value if our lock is no newer than its
    /// proof-of-lock round, or is the same value.
    fn locked_ok_pol(&self, bh: &Hash, valid_round: i64) -> bool {
        match &self.locked {
            None => true,
            Some((b, lr)) => (*lr as i64) <= valid_round || b.hash() == *bh,
        }
    }
}

// --- in-process simulator ----------------------------------------------------

/// A deterministic, in-process network of [`RoundState`]s for one height — a
/// stand-in for the (future) P2P gossip layer. Broadcasts reach every live
/// validator; scheduled timeouts fire once the message bus quiesces, in a fixed
/// order, so the whole run is reproducible. `silent` validators (crashed /
/// offline) are simply never stepped and emit nothing.
pub struct Sim {
    vset: ValidatorSet,
    keys: BTreeMap<u64, Keypair>,
    nodes: BTreeMap<u64, RoundState>,
    queue: VecDeque<Msg>,
    timeouts: BTreeSet<(u64, u8, u32)>,
}

impl Sim {
    pub fn new(
        vset: ValidatorSet,
        keys: BTreeMap<u64, Keypair>,
        height: u64,
        candidate: Block,
        silent: &BTreeSet<u64>,
    ) -> Self {
        let mut nodes = BTreeMap::new();
        for v in vset.validators() {
            if !silent.contains(&v.id) {
                nodes.insert(v.id, RoundState::new(vset.clone(), v.id, height, candidate.clone()));
            }
        }
        Sim { vset, keys, nodes, queue: VecDeque::new(), timeouts: BTreeSet::new() }
    }

    fn apply(&mut self, id: u64, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Broadcast(m) => self.queue.push_back(m),
                Action::Schedule(step, round) => {
                    self.timeouts.insert((id, step_tag(step), round));
                }
                Action::Decided(_) => {} // recorded inside the node; read via decided()
            }
        }
    }

    fn all_decided(&self, ids: &[u64]) -> bool {
        ids.iter().all(|id| self.nodes[id].decided().is_some())
    }

    /// Drive to completion; returns each live validator's finality certificate.
    /// The highest round any node reached is available via [`Sim::max_round`].
    pub fn run(&mut self) -> BTreeMap<u64, Commit> {
        let ids: Vec<u64> = self.nodes.keys().copied().collect();

        for id in &ids {
            let acts = {
                let kp = &self.keys[id];
                self.nodes.get_mut(id).unwrap().start(kp)
            };
            self.apply(*id, acts);
        }

        let mut iters = 0;
        loop {
            // deliver every queued broadcast to every live node
            while let Some(msg) = self.queue.pop_front() {
                for id in &ids {
                    let acts = {
                        let kp = &self.keys[id];
                        self.nodes.get_mut(id).unwrap().on_message(kp, msg.clone())
                    };
                    self.apply(*id, acts);
                }
            }
            if self.all_decided(&ids) || self.timeouts.is_empty() {
                break;
            }
            // bus is quiet but not everyone decided: fire the armed timeouts.
            // Stale ones (wrong step/round) are ignored by the node itself.
            let fire: Vec<(u64, u8, u32)> = self.timeouts.iter().copied().collect();
            self.timeouts.clear();
            for (id, tag, round) in fire {
                if self.nodes.contains_key(&id) {
                    let acts = {
                        let kp = &self.keys[&id];
                        self.nodes.get_mut(&id).unwrap().on_timeout(kp, tag_step(tag), round)
                    };
                    self.apply(id, acts);
                }
            }
            iters += 1;
            if iters > 1000 {
                break; // defensive bound
            }
        }

        self.nodes
            .iter()
            .filter_map(|(id, n)| n.decided().cloned().map(|c| (*id, c)))
            .collect()
    }

    pub fn max_round(&self) -> u32 {
        self.nodes.values().map(|n| n.round()).max().unwrap_or(0)
    }

    pub fn vset(&self) -> &ValidatorSet {
        &self.vset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::Validator;

    fn kp(id: u64) -> Keypair {
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&id.to_le_bytes());
        Keypair::from_seed(seed)
    }

    fn keys(ids: &[u64]) -> BTreeMap<u64, Keypair> {
        ids.iter().map(|&id| (id, kp(id))).collect()
    }

    fn vset(ids: &[u64]) -> ValidatorSet {
        ValidatorSet::new(
            ids.iter()
                .map(|&id| Validator { id, pubkey: kp(id).public(), power: 1 })
                .collect(),
        )
    }

    fn block(height: u64) -> Block {
        Block { height, prev_hash: [9u8; 32], timestamp_days: height as f32, txs: Vec::new(), validator_updates: Vec::new(), stake_ops: Vec::new(), slashing_evidence: Vec::new() }
    }

    #[test]
    fn all_honest_commit_in_round_zero() {
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1);
        let mut sim = Sim::new(vs.clone(), keys(&ids), 1, b.clone(), &BTreeSet::new());
        let dec = sim.run();
        assert_eq!(dec.len(), 4, "all validators decide");
        assert_eq!(sim.max_round(), 0, "no round change needed");
        for c in dec.values() {
            assert_eq!(c.block_hash, b.hash());
            assert!(c.verify(&vs).is_ok(), "the FSM's certificate verifies");
        }
    }

    #[test]
    fn all_honest_agree_on_the_same_block() {
        let ids = [1, 2, 3, 4];
        let b = block(1);
        let mut sim = Sim::new(vset(&ids), keys(&ids), 1, b.clone(), &BTreeSet::new());
        let dec = sim.run();
        let hashes: BTreeSet<Hash> = dec.values().map(|c| c.block_hash).collect();
        assert_eq!(hashes.len(), 1, "one agreed value");
        assert!(hashes.contains(&b.hash()));
    }

    #[test]
    fn one_crash_still_commits() {
        // 4 validators, quorum 3; one non-proposer crashes -> still finalizes.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1);
        let proposer = vs.proposer_for_round(1, 0).unwrap();
        let victim = ids.iter().copied().find(|&x| x != proposer).unwrap();
        let mut silent = BTreeSet::new();
        silent.insert(victim);
        let mut sim = Sim::new(vs.clone(), keys(&ids), 1, b.clone(), &silent);
        let dec = sim.run();
        assert_eq!(dec.len(), 3, "the three live validators decide");
        for c in dec.values() {
            assert_eq!(c.block_hash, b.hash());
            assert!(c.verify(&vs).is_ok());
        }
    }

    #[test]
    fn silent_proposer_triggers_round_change_and_still_commits() {
        // The round-0 proposer is dead. Consensus must time out, change rounds,
        // and finalize under a live proposer — this is liveness.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1);
        let dead_proposer = vs.proposer_for_round(1, 0).unwrap();
        let mut silent = BTreeSet::new();
        silent.insert(dead_proposer);
        let mut sim = Sim::new(vs.clone(), keys(&ids), 1, b.clone(), &silent);
        let dec = sim.run();
        assert_eq!(dec.len(), 3, "the three live validators decide");
        assert!(sim.max_round() >= 1, "at least one round change happened");
        for c in dec.values() {
            assert_eq!(c.block_hash, b.hash());
            assert!(c.verify(&vs).is_ok());
        }
    }

    #[test]
    fn too_many_crashes_stalls_without_forging_a_commit() {
        // 2 of 4 crash: quorum 3 is unreachable, so NO commit forms. Liveness is
        // lost (as BFT allows past 1/3 faults) but safety holds — nobody decides.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1);
        let mut silent = BTreeSet::new();
        silent.insert(3);
        silent.insert(4);
        let mut sim = Sim::new(vs, keys(&ids), 1, b, &silent);
        let dec = sim.run();
        assert!(dec.is_empty(), "no certificate can form below quorum");
    }

    #[test]
    fn a_proposal_from_a_non_proposer_is_ignored() {
        // Safety of admission: only the round's designated proposer is heard.
        let ids = [1, 2, 3, 4];
        let vs = vset(&ids);
        let b = block(1);
        let proposer = vs.proposer_for_round(1, 0).unwrap();
        let impostor = ids.iter().copied().find(|&x| x != proposer).unwrap();
        let mut node = RoundState::new(vs, impostor, 1, b.clone());
        let _ = node.start(&kp(impostor)); // a non-proposer just arms a timeout
        // impostor forges a proposal for round 0 signed with its own key
        let forged = Proposal::signed(1, 0, b.clone(), -1, impostor, &kp(impostor));
        let acts = node.on_message(&kp(impostor), Msg::Proposal(forged));
        // it must NOT have prevoted the forged proposal (no broadcast produced)
        assert!(
            !acts.iter().any(|a| matches!(a, Action::Broadcast(Msg::Vote(_)))),
            "a proposal from the wrong proposer must be dropped"
        );
    }

    #[test]
    fn run_is_deterministic() {
        let ids = [1, 2, 3, 4];
        let b = block(1);
        let mut a = Sim::new(vset(&ids), keys(&ids), 1, b.clone(), &BTreeSet::new());
        let mut c = Sim::new(vset(&ids), keys(&ids), 1, b.clone(), &BTreeSet::new());
        let da = a.run();
        let dc = c.run();
        let ha: Vec<Hash> = da.values().map(|x| x.block_hash).collect();
        let hc: Vec<Hash> = dc.values().map(|x| x.block_hash).collect();
        assert_eq!(ha, hc);
        assert_eq!(a.max_round(), c.max_round());
    }
}
