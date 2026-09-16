"""Python-side benchmark mirroring engine/src/bench.rs on the SAME workload,
so the Rust vs Python speedup is apples-to-apples. Reuses sim/delta_k.py (the
real Python implementation of the ΔK contract).

Run: python3 engine/bench.py [N] [M] [D]
"""

import math
import os
import random
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sim"))
from delta_k import (  # noqa: E402
    CognitiveGraph,
    DeltaKParams,
    GraphNode,
    Submission,
    compute_delta_k,
)

DIM = 8


def rand_unit(rng):
    v = [rng.gauss(0, 1) for _ in range(DIM)]
    n = math.sqrt(sum(x * x for x in v)) or 1e-9
    return [x / n for x in v]


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 20000
    m = int(sys.argv[2]) if len(sys.argv) > 2 else 2000
    d = int(sys.argv[3]) if len(sys.argv) > 3 else 8

    rng = random.Random(42)
    p = DeltaKParams()

    graph = CognitiveGraph()
    for _ in range(n):
        graph.add(GraphNode(rand_unit(rng), f"d{rng.randrange(d)}"))

    reviews = [(1.0, 0.8) for _ in range(5)]
    subs = [Submission(rand_unit(rng), f"d{rng.randrange(d)}", 0.0) for _ in range(m)]

    start = time.perf_counter()
    acc = 0.0
    for s in subs:
        acc += compute_delta_k(s, graph, reviews, (2, 3), p, now_days=10.0)
    elapsed = time.perf_counter() - start

    per_sec = m / elapsed if elapsed else float("inf")
    print(f"python engine: N={n} nodes, M={m} submissions, D={d} domains")
    print(f"  elapsed: {elapsed:.4f}s   throughput: {per_sec:.0f} submissions/sec")
    print(f"  (checksum {acc:.4f})")


if __name__ == "__main__":
    main()
