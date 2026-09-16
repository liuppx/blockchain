"""ΔK computation — the shared contract between whitepaper §5.2 / appendix B.2.3
and the agent-based simulation (appendix B.3).

This is a faithful, dependency-free (stdlib only) implementation of the reference
pseudocode in docs/WHITEPAPER.md B.2.3. The simulation MUST call this exact
function so that the document and the simulation never diverge.

compute_delta_k is a PURE function; its output is in [0, bonus_max]. Returning
0.0 is the enforcement point of the §3.2 hard constraint "ΔK<=0 -> no minting".
"""

from __future__ import annotations

import math
from dataclasses import dataclass, field
from typing import Sequence

Vec = Sequence[float]


# --- Unified governance parameter table (mirrors B.2.3 defaults) --------------
@dataclass
class DeltaKParams:
    tau_dup: float = 0.95        # near-duplicate threshold
    n_review_min: int = 3        # minimum reviewers before full trust
    c_cap: float = 0.5           # correctness cap when reviews are insufficient
    n_min: int = 2               # minimum replication attempts
    lam: float = 0.3             # cross-domain bridging coefficient
    bonus_max: float = 2.0       # cap on domain_gap_bonus
    decay: float = 0.01          # time decay rate (per day)
    fresh_min: float = 0.5       # floor on the freshness factor
    delta_k_min: float = 0.05    # ΔK gate: below this -> 0 (no minting)


# --- Lightweight data carriers ------------------------------------------------
@dataclass
class Submission:
    embedding: Vec
    domain: str
    timestamp_days: float = 0.0      # age is computed relative to "now"


@dataclass
class GraphNode:
    embedding: Vec
    domain: str


class CognitiveGraph:
    """Minimal graph supporting same-domain nearest-neighbour lookup and a crude
    cross-domain bridge estimate. Brute force is fine at simulation scale."""

    def __init__(self) -> None:
        self.nodes: list[GraphNode] = []

    def add(self, node: GraphNode) -> None:
        self.nodes.append(node)

    def knn(self, embedding: Vec, domain: str, k: int = 32) -> list[GraphNode]:
        same = [n for n in self.nodes if n.domain == domain]
        same.sort(key=lambda n: cos_sim(embedding, n.embedding), reverse=True)
        return same[:k]

    def domains_within(self, embedding: Vec, radius: float) -> set[str]:
        """Domains that have at least one node semantically close to `embedding`."""
        found: set[str] = set()
        for n in self.nodes:
            if cos_sim(embedding, n.embedding) >= radius:
                found.add(n.domain)
        return found


# --- Math helpers -------------------------------------------------------------
def cos_sim(a: Vec, b: Vec) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(y * y for y in b))
    if na == 0 or nb == 0:
        return 0.0
    return dot / (na * nb)


def clamp(x: float, lo: float, hi: float) -> float:
    return lo if x < lo else hi if x > hi else x


def weighted_mean(reviews: Sequence[tuple[float, float]]) -> float:
    """reviews: list of (reviewer_reputation, score in [0,1]) -> Σrᵢsᵢ / Σrᵢ."""
    if not reviews:
        return 0.0
    num = sum(r * s for r, s in reviews)
    den = sum(r for r, _ in reviews)
    return num / den if den else 0.0


def agreement(reviews: Sequence[tuple[float, float]]) -> float:
    """Consensus proxy for theory-type claims (no replication): 1 - normalized
    spread of scores. Higher when reviewers agree."""
    if len(reviews) < 2:
        return 0.0
    scores = [s for _, s in reviews]
    mean = sum(scores) / len(scores)
    var = sum((s - mean) ** 2 for s in scores) / len(scores)
    return clamp(1.0 - 2.0 * math.sqrt(var), 0.0, 1.0)


# --- The contract -------------------------------------------------------------
def compute_delta_k(
    sub: Submission,
    graph: CognitiveGraph,
    reviews: Sequence[tuple[float, float]],
    replications: tuple[int, int],
    p: DeltaKParams,
    now_days: float = 0.0,
) -> float:
    """Reference implementation of ΔK (whitepaper B.2.3).

    reviews: [(reviewer_reputation, score)], score in [0,1]
    replications: (success_count, total_attempts)
    Returns ΔK in [0, bonus_max]; 0.0 means "no minting".
    """
    # 1. novelty
    nbrs = graph.knn(sub.embedding, sub.domain, k=32)
    if not nbrs:
        novelty = 1.0  # first node in the domain
    else:
        max_sim = max(cos_sim(sub.embedding, n.embedding) for n in nbrs)
        novelty = 0.0 if max_sim > p.tau_dup else (1.0 - max_sim)

    # 2. correctness (reputation-weighted), capped when reviews are insufficient
    wm = weighted_mean(reviews)
    if len(reviews) < p.n_review_min:
        correctness = min(wm, p.c_cap)
    else:
        correctness = wm

    # 3. reproducibility
    success, total = replications
    if total == 0:
        reproducibility = agreement(reviews)  # theory-type
    else:
        r = success / total
        reproducibility = min(r, 0.7) if total < p.n_min else r

    # 4. cross-domain bonus
    bridged = len(graph.domains_within(sub.embedding, radius=0.5))
    avg_gap = 1.0  # placeholder metric at sim scale; refined in later milestones
    bonus = clamp(1.0 + p.lam * max(bridged - 1, 0) * avg_gap, 1.0, p.bonus_max)

    # 5. freshness
    age = max(now_days - sub.timestamp_days, 0.0)
    fresh = clamp(math.exp(-p.decay * age), p.fresh_min, 1.0)

    dk = novelty * correctness * reproducibility * bonus * fresh
    return dk if dk >= p.delta_k_min else 0.0  # gate -> no minting
