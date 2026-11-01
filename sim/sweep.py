"""Parameter sensitivity sweep for mainnet calibration (production-readiness #7).

The scenario matrix in `run.py` shows the model works *directionally*; this sweep
asks the calibration question: **what parameter values are defensible for a mainnet
genesis?** It varies one governable knob at a time over a range, under both an
honest baseline and an adversarial (sybil+collusion) scenario, averaging each cell
over several seeds to damp the seed-sensitivity the README warns about.

    python3 sim/sweep.py            # print all sweep tables
    python3 sim/sweep.py --md       # also write sim/SWEEP.md

Maps sim knobs to node genesis params:
    base_emission  <-> base_emission_micro  (sim $COG == node micro-$COG / 1e6)
    slash_ratio    <-> slash_bps            (1.0 == 10000 bps == 100%)
    delta_k_min    <-> params.delta_k_min   (ΔK gate)
    tau_dup        <-> params.tau_dup       (near-duplicate threshold)

NOTE: the sim does NOT model M187's emission time-decay (decay_half_life_blocks);
that knob is discussed separately in CALIBRATION.md.
"""

from __future__ import annotations

import dataclasses
import statistics
import sys

from delta_k import DeltaKParams
from model import SimConfig, Simulation

SEEDS = [42, 1, 2, 3, 7]

# Adversarial scenario: 30 honest + 15 spammers + a 10-ring collusion.
ADV = dict(n_honest=30, n_spammers=15, n_colluders=10)


def avg_run(base: SimConfig, **overrides) -> dict:
    """Run `base` with `overrides` across SEEDS; return per-metric means."""
    runs = []
    for s in SEEDS:
        cfg = dataclasses.replace(base, seed=s, **overrides)
        runs.append(Simulation(cfg).run())
    keys = ["final_supply", "genesis_supply", "avg_net_emission", "final_gini",
            "avg_fake_pass_rate", "avg_attacker_roi", "final_honest_roi"]
    return {k: statistics.fmean(r[k] for r in runs) for k in keys}


def growth(r: dict) -> float:
    return r["final_supply"] / r["genesis_supply"] if r["genesis_supply"] else float("nan")


def sweep_base_emission() -> str:
    """Supply inflation vs honest incentive, honest baseline (no attackers)."""
    rows = []
    for be in [1.0, 2.0, 4.0, 8.0, 16.0, 32.0]:
        r = avg_run(SimConfig(), base_emission=be)
        rows.append((be, growth(r), r["final_honest_roi"], r["avg_net_emission"]))
    out = ["### base_emission sweep (honest baseline, 200 epochs, mean of %d seeds)" % len(SEEDS),
           "",
           "| base_emission | node micro | supply growth × | honest ROI | net emit/ep |",
           "|---|---|---|---|---|"]
    for be, g, roi, ne in rows:
        out.append(f"| {be:g} | {int(be*1e6):,} | {g:.2f}× | {roi:+.3f} | {ne:.2f} |")
    return "\n".join(out)


def sweep_slash_ratio() -> str:
    """Attacker deterrence vs honest cost, under sybil+collusion."""
    rows = []
    for sr in [0.0, 0.25, 0.5, 0.75, 1.0]:
        r = avg_run(SimConfig(**ADV), slash_ratio=sr)
        rows.append((sr, r["avg_attacker_roi"], r["final_honest_roi"],
                     r["avg_fake_pass_rate"], r["final_gini"]))
    out = ["### slash_ratio sweep (sybil+collusion, mean of %d seeds)" % len(SEEDS),
           "",
           "| slash_ratio | node bps | attacker ROI | honest ROI | fake pass | Gini |",
           "|---|---|---|---|---|---|"]
    for sr, aroi, hroi, fp, gini in rows:
        out.append(f"| {sr:g} | {int(sr*10000)} | {aroi:+.3f} | {hroi:+.3f} | {fp:.3f} | {gini:.3f} |")
    return "\n".join(out)


def sweep_delta_k_min() -> str:
    """ΔK quality gate vs mint rate, under sybil+collusion."""
    rows = []
    for dkm in [0.0, 0.05, 0.1, 0.2, 0.4]:
        r = avg_run(SimConfig(**ADV), deltak=DeltaKParams(delta_k_min=dkm))
        rows.append((dkm, r["avg_fake_pass_rate"], r["avg_attacker_roi"],
                     r["final_honest_roi"], growth(r)))
    out = ["### delta_k_min sweep (sybil+collusion, mean of %d seeds)" % len(SEEDS),
           "",
           "| delta_k_min | fake pass | attacker ROI | honest ROI | supply growth × |",
           "|---|---|---|---|---|"]
    for dkm, fp, aroi, hroi, g in rows:
        out.append(f"| {dkm:g} | {fp:.3f} | {aroi:+.3f} | {hroi:+.3f} | {g:.2f}× |")
    return "\n".join(out)


def sweep_tau_dup() -> str:
    """Near-duplicate threshold vs spam acceptance, under sybil+collusion."""
    rows = []
    for td in [0.90, 0.95, 0.98, 0.99]:
        r = avg_run(SimConfig(**ADV), deltak=DeltaKParams(tau_dup=td))
        rows.append((td, r["avg_fake_pass_rate"], r["final_honest_roi"], growth(r)))
    out = ["### tau_dup sweep (sybil+collusion, mean of %d seeds)" % len(SEEDS),
           "",
           "| tau_dup | fake pass | honest ROI | supply growth × |",
           "|---|---|---|---|"]
    for td, fp, hroi, g in rows:
        out.append(f"| {td:g} | {fp:.3f} | {hroi:+.3f} | {g:.2f}× |")
    return "\n".join(out)


def main() -> None:
    sections = [sweep_base_emission(), sweep_slash_ratio(),
                sweep_delta_k_min(), sweep_tau_dup()]
    body = "\n\n".join(sections)
    print("\nZhixingGraph parameter sensitivity sweep (calibration #7)\n")
    print(body)
    if "--md" in sys.argv:
        with open("sim/SWEEP.md", "w") as f:
            f.write("# Parameter sensitivity sweep (auto-generated by sim/sweep.py)\n\n")
            f.write("Mean over seeds %s. One knob varied at a time.\n\n" % SEEDS)
            f.write(body + "\n")
        print("\nWrote sim/SWEEP.md")


if __name__ == "__main__":
    main()
