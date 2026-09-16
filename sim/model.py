"""Agent-based simulation of the ZhixingGraph cognitive economy (whitepaper B.3).

Stdlib-only by default. Faithfully reuses compute_delta_k from delta_k.py so the
simulation and the whitepaper share one contract. Optionally offloads the ΔK
hot path to the Rust engine (engine/, whitepaper §7.3) when it is built and
`backend="rust"` (or "auto") — same contract, cross-validated by checksum.

Agents: honest contributors, spammers, colluding ring, reviewers, and a demand
side that burns $COG. Each epoch runs the PoK loop from §5.1:
    submit -> review -> challenge/replicate -> finalize (ΔK) -> mint/slash -> burn
"""

from __future__ import annotations

import math
import os
import random
import sys
from dataclasses import dataclass, field

from delta_k import (
    CognitiveGraph,
    DeltaKParams,
    GraphNode,
    Submission,
    compute_delta_k,
)

# Optional Rust acceleration (engine/zhixing_engine.*.so). Import is best-effort;
# the simulation runs pure-Python if the module is not built.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "engine"))
try:
    import zhixing_engine as _rust
except ImportError:
    _rust = None

DIM = 8


def rust_available() -> bool:
    return _rust is not None


# --- Config -------------------------------------------------------------------
@dataclass
class SimConfig:
    epochs: int = 200
    n_domains: int = 6
    n_honest: int = 40
    n_spammers: int = 0
    n_colluders: int = 0          # size of one colluding ring
    n_reviewers: int = 30
    base_emission: float = 8.0    # $COG minted per unit ΔK (governance knob)
    submit_stake: float = 2.0     # $COG staked per submission
    slash_ratio: float = 1.0      # fraction of stake burned when ΔK == 0
    reviewers_per_item: int = 5
    replication_attempts: int = 3
    initial_balance: float = 30.0    # $COG endowment per contributor (counted once)
    demand_base: float = 0.5         # baseline service demand per epoch
    demand_rate: float = 0.9         # share of minted $COG burned as service demand
    seed: int = 42
    backend: str = "auto"            # "python" | "rust" | "auto" (rust if built)


# --- Agents -------------------------------------------------------------------
@dataclass
class Contributor:
    aid: int
    kind: str                     # "honest" | "spammer" | "colluder"
    ring: int = -1                # colluding ring id, -1 if none
    balance: float = 0.0
    staked_total: float = 0.0
    earned_total: float = 0.0
    slashed_total: float = 0.0
    submissions: int = 0
    accepted: int = 0


@dataclass
class Reviewer:
    rid: int
    kind: str                     # "honest" | "lazy" | "colluder"
    ring: int = -1
    reputation: float = 1.0


# --- Helpers ------------------------------------------------------------------
def rand_unit_vec(rng: random.Random) -> list[float]:
    v = [rng.gauss(0, 1) for _ in range(DIM)]
    norm = math.sqrt(sum(x * x for x in v)) or 1.0
    return [x / norm for x in v]


def jitter(vec: list[float], rng: random.Random, scale: float) -> list[float]:
    v = [x + rng.gauss(0, scale) for x in vec]
    norm = math.sqrt(sum(x * x for x in v)) or 1.0
    return [x / norm for x in v]


def gini(values: list[float]) -> float:
    xs = sorted(v for v in values if v >= 0)
    n = len(xs)
    if n == 0 or sum(xs) == 0:
        return 0.0
    cum = 0.0
    for i, x in enumerate(xs, 1):
        cum += i * x
    return (2 * cum) / (n * sum(xs)) - (n + 1) / n


# --- Simulation ---------------------------------------------------------------
class Simulation:
    def __init__(self, cfg: SimConfig) -> None:
        self.cfg = cfg
        self.rng = random.Random(cfg.seed)
        self.p = DeltaKParams()
        self.graph = CognitiveGraph()
        self.domains = list(range(cfg.n_domains))   # int ids (Rust engine uses u32)
        self.day = 0.0
        self.total_supply = 0.0
        self.treasury = 0.0          # slashed stake pool (redistributed, not burned)
        self.burned_total = 0.0
        self.history: list[dict] = []

        # choose ΔK backend: python graph always exists (storage/metrics/sampling);
        # rust graph mirrors it and serves the ΔK hot path when enabled.
        want_rust = cfg.backend in ("rust", "auto") and _rust is not None
        if cfg.backend == "rust" and _rust is None:
            raise RuntimeError("backend='rust' requested but zhixing_engine not "
                               "built; run engine/build_python.sh")
        self.use_rust = want_rust
        self.backend = "rust" if want_rust else "python"
        if self.use_rust:
            self.rgraph = _rust.PyGraph()
            p = self.p
            self.rgraph.set_params(
                tau_dup=p.tau_dup, n_review_min=p.n_review_min, c_cap=p.c_cap,
                n_min=p.n_min, lam=p.lam, bonus_max=p.bonus_max, decay=p.decay,
                fresh_min=p.fresh_min, delta_k_min=p.delta_k_min,
            )
        else:
            self.rgraph = None

        self.contributors: list[Contributor] = []
        aid = 0
        for _ in range(cfg.n_honest):
            self.contributors.append(Contributor(aid, "honest")); aid += 1
        for _ in range(cfg.n_spammers):
            self.contributors.append(Contributor(aid, "spammer")); aid += 1
        for _ in range(cfg.n_colluders):
            self.contributors.append(Contributor(aid, "colluder", ring=0)); aid += 1

        # one-time endowment: minted into supply exactly once at genesis
        for c in self.contributors:
            c.balance = cfg.initial_balance
            self.total_supply += cfg.initial_balance

        self.reviewers: list[Reviewer] = []
        rid = 0
        # a slice of reviewers belong to the colluding ring if colluders exist
        n_ring_rev = cfg.n_reviewers // 4 if cfg.n_colluders else 0
        for i in range(cfg.n_reviewers):
            if i < n_ring_rev:
                self.reviewers.append(Reviewer(rid, "colluder", ring=0))
            elif i < n_ring_rev + max(1, cfg.n_reviewers // 6):
                self.reviewers.append(Reviewer(rid, "lazy"))
            else:
                self.reviewers.append(Reviewer(rid, "honest"))
            rid += 1

        # seed each domain with a couple of nodes so novelty/knn is meaningful
        for dom in self.domains:
            base = rand_unit_vec(self.rng)
            for _ in range(2):
                self._add_node(jitter(base, self.rng, 0.3), dom)

    # -- add a node to the python graph and (if enabled) the rust mirror -------
    def _add_node(self, embedding: list[float], domain: int) -> None:
        self.graph.add(GraphNode(embedding, domain))
        if self.use_rust:
            self.rgraph.add(embedding, domain)

    # -- compute ΔK via the selected backend (same B.2.3 contract) -------------
    def _delta_k(self, sub: Submission, reviews, replications) -> float:
        if self.use_rust:
            success, total = replications
            return self.rgraph.compute_delta_k(
                sub.embedding, sub.domain, reviews, success, total,
                timestamp_days=sub.timestamp_days, now_days=self.day,
            )
        return compute_delta_k(sub, self.graph, reviews, replications, self.p,
                               now_days=self.day)

    # -- one contributor produces a submission + its "true quality" ------------
    def _make_submission(self, c: Contributor) -> tuple[Submission, float]:
        dom = self.rng.choice(self.domains)
        if c.kind == "honest":
            # genuine work: decent quality, sometimes explores a fresh region
            true_q = self.rng.betavariate(5, 2)          # skew high
            emb = rand_unit_vec(self.rng)
        elif c.kind == "spammer":
            # near-duplicate of an existing node, low quality
            true_q = self.rng.betavariate(2, 5)          # skew low
            same = [n for n in self.graph.nodes if n.domain == dom]
            emb = (jitter(self.rng.choice(same).embedding, self.rng, 0.02)
                   if same else rand_unit_vec(self.rng))
        else:  # colluder: mediocre work propped up by ring reviewers
            true_q = self.rng.betavariate(2, 3)
            emb = rand_unit_vec(self.rng)
        return Submission(emb, dom, timestamp_days=self.day), true_q

    # -- select reviewers, VRF-like reputation-weighted sampling ---------------
    def _select_reviewers(self, k: int) -> list[Reviewer]:
        pool = list(self.reviewers)
        chosen: list[Reviewer] = []
        weights = [max(r.reputation, 0.01) for r in pool]
        for _ in range(min(k, len(pool))):
            total = sum(weights)
            pick = self.rng.uniform(0, total)
            acc = 0.0
            for idx, w in enumerate(weights):
                acc += w
                if pick <= acc:
                    chosen.append(pool.pop(idx))
                    weights.pop(idx)
                    break
        return chosen

    def _score(self, rev: Reviewer, c: Contributor, true_q: float) -> float:
        if rev.kind == "lazy":
            return 0.5
        if rev.kind == "colluder" and c.ring == rev.ring and c.ring >= 0:
            return min(1.0, true_q + 0.4)            # boost ring members
        return min(1.0, max(0.0, true_q + self.rng.gauss(0, 0.08)))

    def step(self) -> dict:
        cfg, p = self.cfg, self.p
        self.day += 1.0
        minted = burned = slashed = 0.0
        fake_submits = fake_passed = 0

        # active contributors this epoch (honest act if they can afford stake;
        # attackers keep trying while they still have stake)
        for c in self.contributors:
            act_prob = 0.6 if c.kind == "honest" else 0.8
            if self.rng.random() > act_prob:
                continue
            if c.balance < cfg.submit_stake:            # can't afford to stake
                continue

            sub, true_q = self._make_submission(c)
            c.balance -= cfg.submit_stake               # stake -> escrow
            c.staked_total += cfg.submit_stake
            c.submissions += 1
            is_fake = c.kind in ("spammer", "colluder")
            if is_fake:
                fake_submits += 1

            revs = self._select_reviewers(cfg.reviewers_per_item)
            reviews = [(r.reputation, self._score(r, c, true_q)) for r in revs]

            # replication: success ~ true quality
            attempts = cfg.replication_attempts
            success = sum(1 for _ in range(attempts)
                          if self.rng.random() < true_q)

            dk = self._delta_k(sub, reviews, (success, attempts))

            if dk > 0:
                reward = cfg.base_emission * dk
                c.balance += cfg.submit_stake + reward   # escrow returned + reward
                c.earned_total += reward
                c.accepted += 1
                minted += reward
                self.total_supply += reward              # only reward is new supply
                self._add_node(sub.embedding, sub.domain)
                if is_fake:
                    fake_passed += 1
                # reviewers whose score matched outcome gain reputation
                for r, s in zip(revs, [sc for _, sc in reviews]):
                    err = abs(s - true_q)
                    r.reputation = max(0.05, r.reputation + (0.02 - err * 0.05))
            else:
                loss = cfg.submit_stake * cfg.slash_ratio
                slashed += loss
                c.slashed_total += loss
                self.treasury += loss                    # redistributed, not burned
                c.balance += cfg.submit_stake - loss     # remainder returned
                # reviewers who over-scored a rejected item lose reputation
                for r, s in zip(revs, [sc for _, sc in reviews]):
                    if s > 0.6:
                        r.reputation = max(0.05, r.reputation - 0.03)

        # demand side: services consumed track production flow (a share of what
        # was minted this epoch) plus a base. Burning $COG is the deflationary
        # force. demand_rate<1 -> mild inflation; >1 -> deflation.
        demand = cfg.demand_base + cfg.demand_rate * minted
        demand = min(demand, self.total_supply)          # can't burn what isn't there
        burned += demand
        self.total_supply -= demand
        self.burned_total += demand

        rec = self._metrics(minted, burned, slashed, fake_submits, fake_passed)
        self.history.append(rec)
        return rec

    def _metrics(self, minted, burned, slashed, fake_submits, fake_passed) -> dict:
        cfg = self.cfg
        earnings = [c.earned_total for c in self.contributors]
        # valley balance: how evenly work spreads across domains (min/max node
        # count). 1.0 = every valley filled evenly; ->0 = concentrated on peaks.
        counts = {d: 0 for d in self.domains}
        for n in self.graph.nodes:
            counts[n.domain] = counts.get(n.domain, 0) + 1
        cvals = list(counts.values())
        valley_balance = (min(cvals) / max(cvals)) if max(cvals) > 0 else 0.0
        attackers = [c for c in self.contributors
                     if c.kind in ("spammer", "colluder")]
        honest = [c for c in self.contributors if c.kind == "honest"]
        atk_earn = sum(c.earned_total for c in attackers)
        atk_stake = sum(c.staked_total for c in attackers)
        atk_slash = sum(c.slashed_total for c in attackers)
        hon_earn = sum(c.earned_total for c in honest)
        hon_stake = sum(c.staked_total for c in honest)
        hon_slash = sum(c.slashed_total for c in honest)
        return {
            "epoch": int(self.day),
            "minted": round(minted, 3),
            "burned": round(burned, 3),
            "slashed": round(slashed, 3),
            "net_emission": round(minted - burned, 3),
            "supply": round(self.total_supply, 2),
            "gini": round(gini(earnings), 4),
            "valley_balance": round(valley_balance, 3),
            "graph_nodes": len(self.graph.nodes),
            "fake_pass_rate": round(fake_passed / fake_submits, 4) if fake_submits else 0.0,
            "attacker_roi": round((atk_earn - atk_slash) / atk_stake, 4) if atk_stake else 0.0,
            "honest_roi": round((hon_earn - hon_slash) / hon_stake, 4) if hon_stake else 0.0,
        }

    def run(self) -> dict:
        for _ in range(self.cfg.epochs):
            self.step()
        return self.summary()

    def summary(self) -> dict:
        tail = self.history[-min(20, len(self.history)):]
        avg = lambda k: round(sum(r[k] for r in tail) / len(tail), 4)
        return {
            "epochs": self.cfg.epochs,
            "backend": self.backend,
            "genesis_supply": round(self.cfg.initial_balance * len(self.contributors), 2),
            "final_supply": round(self.total_supply, 2),
            "avg_net_emission": avg("net_emission"),
            "final_gini": self.history[-1]["gini"],
            "final_valley_balance": self.history[-1]["valley_balance"],
            "graph_nodes": self.history[-1]["graph_nodes"],
            "avg_fake_pass_rate": avg("fake_pass_rate"),
            "avg_attacker_roi": avg("attacker_roi"),
            "final_honest_roi": self.history[-1]["honest_roi"],
        }
