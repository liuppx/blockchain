"""Parameter-sweep benchmark (whitepaper B.2.2 calibration workload).

Runs the SAME ΔK calibration sweep two ways and compares:
  1. pure Python  (sim/delta_k.py)
  2. Rust engine  (zhixing_engine, built via engine/build_python.sh)

For each parameter combination it measures acceptance rate + mean ΔK over a
fixed corpus, so this is exactly the kind of offline calibration sweep the
whitepaper calls for — the case where "sim/ calls the Rust engine for large
parameter sweeps" pays off.

Run:
  ./engine/build_python.sh          # build the module first
  python3 engine/sweep.py [N] [M]
"""

import math
import os
import random
import sys
import time

HERE = os.path.dirname(__file__)
sys.path.insert(0, os.path.join(HERE, "..", "sim"))
sys.path.insert(0, HERE)  # so `import zhixing_engine` finds the built .so

import delta_k as py  # pure-Python reference  # noqa: E402

try:
    import zhixing_engine as rs  # Rust extension
except ImportError:
    rs = None

DIM = 8

# Sweep grid (B.2.2: threshold / gate / bridging coefficient calibration).
TAU_DUP = [0.90, 0.93, 0.96]
DELTA_K_MIN = [0.03, 0.05, 0.08]
LAM = [0.2, 0.4]


def rand_unit(rng):
    v = [rng.gauss(0, 1) for _ in range(DIM)]
    n = math.sqrt(sum(x * x for x in v)) or 1e-9
    return [x / n for x in v]


def build_corpus(n, m, d, seed=42):
    """Deterministic corpus shared by both implementations."""
    rng = random.Random(seed)
    nodes = [(rand_unit(rng), rng.randrange(d)) for _ in range(n)]
    subs = [(rand_unit(rng), rng.randrange(d)) for _ in range(m)]
    reviews = [(1.0, 0.8) for _ in range(5)]
    return nodes, subs, reviews


def run_python(nodes, subs, reviews, d):
    graph = py.CognitiveGraph()
    for emb, dom in nodes:
        graph.add(py.GraphNode(emb, f"d{dom}"))
    checksum = 0.0
    combos = 0
    start = time.perf_counter()
    for tau in TAU_DUP:
        for dkm in DELTA_K_MIN:
            for lam in LAM:
                p = py.DeltaKParams(tau_dup=tau, delta_k_min=dkm, lam=lam)
                for emb, dom in subs:
                    sub = py.Submission(emb, f"d{dom}", 0.0)
                    dk = py.compute_delta_k(sub, graph, reviews, (2, 3), p,
                                            now_days=10.0)
                    checksum += dk
                combos += 1
    elapsed = time.perf_counter() - start
    return elapsed, checksum, combos


def run_rust(nodes, subs, reviews, d):
    graph = rs.PyGraph()
    for emb, dom in nodes:
        graph.add(emb, dom)
    checksum = 0.0
    combos = 0
    start = time.perf_counter()
    for tau in TAU_DUP:
        for dkm in DELTA_K_MIN:
            for lam in LAM:
                graph.set_params(tau_dup=tau, delta_k_min=dkm, lam=lam)
                for emb, dom in subs:
                    dk = graph.compute_delta_k(emb, dom, reviews, 2, 3,
                                               timestamp_days=0.0, now_days=10.0)
                    checksum += dk
                combos += 1
    elapsed = time.perf_counter() - start
    return elapsed, checksum, combos


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 4000
    m = int(sys.argv[2]) if len(sys.argv) > 2 else 400
    d = 8
    grid = len(TAU_DUP) * len(DELTA_K_MIN) * len(LAM)

    nodes, subs, reviews = build_corpus(n, m, d)
    print(f"parameter sweep: N={n} nodes, M={m} submissions, {grid} param combos "
          f"= {grid * m} ΔK calls\n")

    py_t, py_c, _ = run_python(nodes, subs, reviews, d)
    print(f"  python : {py_t:8.4f}s   checksum {py_c:.4f}")

    if rs is None:
        print("\n  Rust engine not built. Run ./engine/build_python.sh first.")
        return

    rs_t, rs_c, _ = run_rust(nodes, subs, reviews, d)
    print(f"  rust   : {rs_t:8.4f}s   checksum {rs_c:.4f}")

    speedup = py_t / rs_t if rs_t else float("inf")
    rel_err = abs(py_c - rs_c) / abs(py_c) if py_c else 0.0
    print(f"\n  speedup: {speedup:.1f}x   checksum rel-error: {rel_err:.2e}")
    ok = rel_err < 1e-3
    print("  contract match:", "OK" if ok else "MISMATCH")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
