#!/usr/bin/env python3
"""知识经济仿真 L0(对应 docs/design/token-economy.md;设计稿 knowledge-lifecycle-economy.md §12 的"先仿真,后编码")。

问题:把 $COG 的发放从"自报嵌入向量的 ΔK"改为建立在**知识单元**(版本、依据 / 参见、核验)之上以后,
  E1 钱是否流向正确的知识,而不是错误的知识、作恶者、互引小圈子?
  E2 核验有没有人做?不付核验报酬时会怎样?
  E3 很少被直接引用的基础原理,能否靠"沿依据上溯"持续获益?
  E4 增发逐年递减后,外部付费不到位时,正常的知识会不会被租金挤出?
  E5 早期参与者会不会永久占据收益(P6)?

纯 Python 标准库:
    python3 sim/economy.py              # 场景矩阵
    python3 sim/economy.py --md         # 同上,并写出 sim/ECONOMY.md
    python3 sim/economy.py --seeds 3 --steps 120

一期 = 一个月。模型要素:
  - 发放来源:协议增发 E(t) = E0·2^(−t/He)(总量收敛于 E0·He/ln2,即事实上的上限)+ 外部付费(检索费,逻辑斯蒂增长)。
  - 每期分成:知识奖励池 / 核验奖励池 / 国库。
  - **可信度门槛**:只有已确立(至少 2 位过了见习期的成员核验通过,且通过多于"需修改")的单元参与分配。
  - 重要性:确立时获得起始值 I0;此后每被一个**已确立**的他人单元以"依据"引用 +1(参见 +0.25),
    按层级半衰期衰减(基础 10 年、解释 4 年、场景 2 年、应用 1 年)。同作者引用不计。
  - 上溯:每个单元分到的奖励,按比例沿"依据"上溯给它所依据的单元。
  - 核验报酬:单元确立时,促成确立的核验人各得 1 分;两位以上核验人一致给"需修改"的版本,给"需修改"者各得 1 分。
    核验奖励池按当期得分分配。
  - **收益锁定期**:所有收益先锁定 V 期再释放;锁定期内若单元被挑战成功(含当时可检出的错误),
    作者与放行的核验人锁定中的收益被罚没,一半奖励挑战者,一半销毁。这替代了"先交押金"——新人没有代币也能参与。
  - 存储租金:每个在册单元每期缴租,近 12 期收入不够缴租即休眠(不再分配、不再被引用)。
  - 行为:每人每期有固定行动数,在"写"与"核验"之间分配;理性者按近期每次行动的收益调整比例,诚实者保留最低核验比例。
    作恶者写错误内容、互相引用、互相放行;"走过场"者核验时一律通过。

诚实边界:方向性模型,所有行为参数都是假设。它回答"在这些假设下钱往哪里流、哪个开关最关键",不是收益预测。
"""
from __future__ import annotations

import argparse
import math
import random
import statistics
from dataclasses import dataclass, field, replace

LAYERS = ("foundation", "explanation", "scenario", "application")
LAYER_W = (0.15, 0.25, 0.20, 0.40)  # 新单元的层级分布
HALF_LIFE = {"foundation": 120, "explanation": 48, "scenario": 24, "application": 12}  # 期(月)
BASIS_FROM = {  # 每层可以依据哪些层
    "foundation": (),
    "explanation": ("foundation",),
    "scenario": ("foundation", "explanation"),
    "application": ("foundation", "explanation", "scenario"),
}


@dataclass(frozen=True)
class Params:
    steps: int = 120
    founders: int = 20
    max_agents: int = 300
    growth: float = 3.0
    frac_mal: float = 0.10
    frac_lazy: float = 0.25
    actions: int = 3
    honest_err: float = 0.08
    detectable: float = 0.7  # 错误中"当时就能查出"的比例
    detect_p: float = 0.7  # 认真核验者发现可检出错误的概率
    probation: int = 1  # 期;7 天约 0.25 期,取 1 偏保守
    # 发放
    e0: float = 10_000.0
    emission_half_life: int = 48  # 4 年
    fee_max: float = 6_000.0
    fee_mid: int = 48
    fee_width: float = 10.0
    split_pool: float = 0.60
    split_review: float = 0.25  # 余下进国库
    upstream: float = 0.20
    see_also_w: float = 0.25
    i0: float = 1.0
    importance_decay: bool = True
    cred_gate: bool = True  # 关:未确立的单元也参与分配,引用在创建时即计入
    vesting: int = 6
    rent: float = 0.10
    challenge_share: float = 0.3  # 诚实者的核验行动中用于挑战已确立单元的比例
    rational: bool = True
    # 对策(第二轮)
    assigned: bool = False  # 核验人由协议随机指派,只有被指派者的结论计入确立
    assign_k: int = 4
    share_exp: float = 1.0  # 分配权重 = 重要性^share_exp;0.5 即边际递减
    unit_cap: float = 0.0  # >0:每期增发不超过 unit_cap × 在册已确立单元数,未发出的额度顺延(总量不变)


@dataclass
class Agent:
    id: int
    kind: str  # honest | lazy | mal
    joined: int
    founder: bool = False
    f_review: float = 0.4
    locked: list = field(default_factory=list)  # [(release_step, amount, unit_id or None)]
    paid: float = 0.0  # 已释放
    slashed: float = 0.0
    earned_write: float = 0.0
    earned_review: float = 0.0
    ema_w: float = 0.0
    ema_r: float = 0.0
    banned_until: int = -1


@dataclass
class Unit:
    id: int
    author: int
    layer: str
    born: int
    correct: bool
    detectable: bool
    mal: bool
    basis: list
    see_also: list
    passes: list = field(default_factory=list)
    needs: list = field(default_factory=list)
    established: int = -1
    removed: bool = False
    dormant: bool = False
    i_last: float = 0.0
    t_last: int = 0
    income: list = field(default_factory=list)  # 近 12 期收入
    total_income: float = 0.0
    cited: bool = False
    assigned: set = field(default_factory=set)


class Sim:
    def __init__(self, p: Params, seed: int):
        self.p, self.r = p, random.Random(seed)
        self.members: list = []
        self.queue: dict = {}
        self.agents: list[Agent] = []
        self.units: list[Unit] = []
        self.minted = self.burned = self.treasury = self.fees = 0.0
        self.carry = 0.0
        self.minted_y1 = 0.0
        self.paid_flow: dict[int, tuple] = {}
        self.t = 0
        self.ring_units: set[int] = set()
        self.review_credits: dict[int, float] = {}
        self.est_delay: list[int] = []
        self.per_unit_reward: dict[int, float] = {}
        self.founder_share: dict[int, float] = {}
        self.dormant_good = 0
        for _ in range(p.founders):
            self.add_agent("honest", founder=True)

    # -- 参与者 ---------------------------------------------------------------
    def add_agent(self, kind, founder=False):
        a = Agent(len(self.agents), kind, self.t, founder=founder)
        if kind == "mal":
            a.f_review = 0.5
        self.agents.append(a)

    def established_member(self, a: Agent) -> bool:
        return (self.t - a.joined >= self.p.probation or a.founder) and self.t >= a.banned_until

    # -- 重要性 ---------------------------------------------------------------
    def importance(self, u: Unit) -> float:
        if not self.p.importance_decay:
            return u.i_last
        return u.i_last * 2 ** (-(self.t - u.t_last) / HALF_LIFE[u.layer])

    def bump(self, u: Unit, w: float):
        u.i_last = self.importance(u) + w
        u.t_last = self.t

    def live(self, u: Unit) -> bool:
        return not u.removed and not u.dormant and (u.established >= 0 or not self.p.cred_gate)

    # -- 写 -------------------------------------------------------------------
    def write(self, a: Agent):
        p, r = self.p, self.r
        layer = r.choices(LAYERS, LAYER_W)[0]
        mal = a.kind == "mal"
        correct = False if mal else r.random() >= p.honest_err
        detectable = (r.random() < 0.8) if mal else (r.random() < p.detectable)
        cands, cw, ring = self.live_layer[layer]
        basis, see = [], []
        if cands:
            k = r.randint(1, 3)
            if mal:
                pool = ring if ring else cands
                basis = r.sample(pool, min(k, len(pool)))
            else:
                basis = list({id(u): u for u in r.choices(cands, cum_weights=cw, k=k)}.values())
            basis = [b for b in basis if b.author != a.id]
        others = self.live_ring if (mal and self.live_ring) else self.live_all
        if others and r.random() < 0.5:
            c = r.choice(others)
            if c.author != a.id:
                see = [c]
        u = Unit(len(self.units), a.id, layer, self.t, correct, detectable, mal,
                 [b.id for b in basis], [s.id for s in see])
        self.units.append(u)
        if p.assigned:
            self.assign(u, p.assign_k)
        if mal:
            self.ring_units.add(u.id)
        if not p.cred_gate:
            self.count_citations(u)  # 无门槛:引用在创建时即计入

    def assign(self, u: Unit, k: int):
        pool = [a for a in self.members if a.id != u.author and a.id not in u.assigned]
        for a in self.r.sample(pool, min(k, len(pool))):
            u.assigned.add(a.id)
            self.queue.setdefault(a.id, []).append(u)

    def count_citations(self, u: Unit):
        for b in u.basis:
            t = self.units[b]
            if t.author != u.author:
                self.bump(t, 1.0)
                t.cited = True
        for s in u.see_also:
            t = self.units[s]
            if t.author != u.author:
                self.bump(t, self.p.see_also_w)

    # -- 核验与挑战 -------------------------------------------------------------
    def review(self, a: Agent):
        p, r = self.p, self.r
        if a.kind == "honest" and r.random() < p.challenge_share:
            return self.challenge(a)
        ok = lambda u: u.established < 0 and u.author != a.id and a.id not in u.passes and a.id not in u.needs
        if p.assigned:
            q = [u for u in self.queue.get(a.id, []) if ok(u)]
            if a.kind == "mal":
                q = [u for u in q if u.mal]
            if not q:
                return
            u = max(r.sample(q, min(8, len(q))), key=lambda c: len(c.passes))
            if a.kind == "mal":
                u.passes.append(a.id)
            else:
                caught = a.kind == "honest" and not u.correct and u.detectable and r.random() < p.detect_p
                (u.needs if caught else u.passes).append(a.id)
            return self.try_establish(u)
        if a.kind == "mal":
            ring = [u for u in self.pending if u.mal and ok(u)]
            if not ring:
                return
            u = r.choice(ring)
            u.passes.append(a.id)
        else:
            # 看 8 个候选,优先已有通过的(报酬在确立时发放,把快完成的做完更划算)
            if not self.pending:
                return
            cs = [c for c in (r.choice(self.pending) for _ in range(8)) if ok(c)]
            if not cs:
                return
            u = max(cs, key=lambda c: len(c.passes))
            caught = a.kind == "honest" and not u.correct and u.detectable and r.random() < p.detect_p
            (u.needs if caught else u.passes).append(a.id)
        self.try_establish(u)

    def counted(self, ids, u=None):
        return [i for i in ids if self.established_member(self.agents[i]) and (u is None or not self.p.assigned or i in u.assigned)]

    def try_establish(self, u: Unit):
        ps, ns = self.counted(u.passes, u), self.counted(u.needs, u)
        if len(ns) >= 2 and not getattr(u, "_needs_paid", False):
            u._needs_paid = True
            for i in ns:
                self.review_credits[i] = self.review_credits.get(i, 0) + 1
        if u.established < 0 and len(ps) >= 2 and len(ps) > len(ns):
            u.established = self.t
            u._approvers = ps
            self.est_delay.append(self.t - u.born)
            self.bump(u, self.p.i0)
            for i in ps:
                self.review_credits[i] = self.review_credits.get(i, 0) + 1
            if self.p.cred_gate:
                self.count_citations(u)

    def challenge(self, a: Agent):
        p, r = self.p, self.r
        est, cw = self.est_cache
        if not est:
            return
        u = r.choices(est, cum_weights=cw)[0]
        if u.removed or u.author == a.id or u.correct or not u.detectable or r.random() >= p.detect_p:
            return
        u.removed = True
        pot = self.slash(u.author, u.id)
        for i in getattr(u, "_approvers", []):
            pot += self.slash(i, u.id)
            self.agents[i].banned_until = self.t + 6  # 放行了可检出错误:暂停计入 6 期(M6 式限流)
        self.agents[u.author].banned_until = self.t + 6
        self.lock(a, pot * 0.5, None, review=True)
        self.burned += pot * 0.5

    def slash(self, aid: int, uid: int) -> float:
        a = self.agents[aid]
        # 锁定中的全部收益都可罚没:锁定期担保的是"这段时间里没有放过当时可检出的错误"
        cut = sum(amt for _, amt, _ in a.locked)
        a.locked = []
        a.slashed += cut
        return cut

    # -- 发放 -----------------------------------------------------------------
    def lock(self, a: Agent, amt: float, uid, review=False):
        if amt <= 0:
            return
        a.locked.append((self.t + self.p.vesting, amt, uid))
        if review:
            a.earned_review += amt
        else:
            a.earned_write += amt

    def distribute(self):
        p, t = self.p, self.t
        live = [u for u in self.units if self.live(u)]
        emission = p.e0 * 2 ** (-t / p.emission_half_life)
        if p.unit_cap > 0:
            avail = emission + self.carry
            emission = min(avail, p.unit_cap * len(live))
            self.carry = avail - emission
        fee = p.fee_max / (1 + math.exp(-(t - p.fee_mid) / p.fee_width)) if p.fee_max else 0.0
        self.minted += emission
        self.fees += fee
        total = emission + fee
        pool, rpool = total * p.split_pool, total * p.split_review
        self.treasury += total - pool - rpool
        imps = {u.id: self.importance(u) ** p.share_exp for u in live}
        s = sum(imps.values())
        got: dict[int, float] = {}
        if s > 0:
            for u in live:
                share = pool * imps[u.id] / s
                ups = [b for b in u.basis if self.live(self.units[b])]
                up = share * p.upstream if ups else 0.0
                got[u.id] = got.get(u.id, 0) + share - up
                for b in ups:
                    got[b] = got.get(b, 0) + up / len(ups)
        else:
            self.treasury += pool
        for u in self.units:
            if u.removed:
                continue
            inc = got.get(u.id, 0.0)
            u.income = (u.income + [inc])[-12:]
            u.total_income += inc
            net = inc - p.rent if not u.dormant else 0.0
            if not u.dormant:
                self.burned += min(inc, p.rent) * 0.5
                self.treasury += min(inc, p.rent) * 0.5
            if net > 0:
                self.lock(self.agents[u.author], net, u.id)
            if not u.dormant and t - u.born >= 12 and sum(u.income) < 12 * p.rent:
                u.dormant = True
                if u.correct and u.established >= 0 and u.cited:
                    self.dormant_good += 1
        cs = sum(self.review_credits.values())
        if cs > 0:
            for i, c in self.review_credits.items():
                self.lock(self.agents[i], rpool * c / cs, None, review=True)
        else:
            self.treasury += rpool
        self.review_credits = {}
        if live:
            self.per_unit_reward[t] = pool / len(live)
        # 释放
        fpaid = allflow = 0.0
        for a in self.agents:
            keep = []
            for rel, amt, src in a.locked:
                if rel <= t:
                    a.paid += amt
                    allflow += amt
                    if a.founder:
                        fpaid += amt
                else:
                    keep.append((rel, amt, src))
            a.locked = keep
        self.paid_flow[t] = (fpaid, allflow)
        if t == 11:
            self.minted_y1 = self.minted
        founders = sum(a.paid for a in self.agents if a.founder)
        allp = sum(a.paid for a in self.agents)
        self.founder_share[t] = founders / allp if allp else 0.0

    # -- 主循环 ---------------------------------------------------------------
    def step(self):
        p, r = self.p, self.r
        if len(self.agents) < p.max_agents:
            n = int(p.growth) + (1 if r.random() < p.growth % 1 else 0)
            for _ in range(n):
                x = r.random()
                self.add_agent("mal" if x < p.frac_mal else "lazy" if x < p.frac_mal + p.frac_lazy else "honest")
        self.pending = [u for u in self.units if u.established < 0 and not u.removed and self.t - u.born < 24]
        self.members = [a for a in self.agents if self.established_member(a)]
        if p.assigned:
            if not hasattr(self, "queue"):
                self.queue = {}
            for aid in list(self.queue):
                self.queue[aid] = [u for u in self.queue[aid] if u.established < 0 and not u.removed and self.t - u.born < 24]
            for u in self.pending:  # 迟迟凑不够通过的,追加指派(上限 8 人)
                if self.t - u.born >= 3 and len(u.assigned) < 8 and len(self.counted(u.passes, u)) < 2:
                    self.assign(u, 1)
        # 每期开始时缓存候选集(期内的变化下一期才可见,对月度粒度的模型影响可忽略)
        live = [u for u in self.units if self.live(u)]
        self.live_all = live
        self.live_ring = [u for u in live if u.id in self.ring_units]
        self.live_layer = {}
        for L in LAYERS:
            us = [u for u in live if u.layer in BASIS_FROM[L]]
            ws, acc = [], 0.0
            for u in us:
                acc += 1.0 + self.importance(u)
                ws.append(acc)
            self.live_layer[L] = (us, ws, [u for u in us if u.id in self.ring_units])
        est = [u for u in self.units if u.established >= 0 and not u.removed]
        acc, cw = 0.0, []
        for u in est:
            acc += 1.0 + self.importance(u)
            cw.append(acc)
        self.est_cache = (est, cw)
        wrote = {a.id: 0 for a in self.agents}
        reviewed = {a.id: 0 for a in self.agents}
        before_w = {a.id: a.earned_write for a in self.agents}
        before_r = {a.id: a.earned_review for a in self.agents}
        for a in self.agents:
            for _ in range(p.actions):
                if r.random() < a.f_review:
                    reviewed[a.id] += 1
                    self.review(a)
                else:
                    wrote[a.id] += 1
                    self.write(a)
        self.distribute()
        if p.rational:
            for a in self.agents:
                if a.kind == "mal":
                    continue
                gw = (a.earned_write - before_w[a.id]) / max(1, wrote[a.id])
                gr = (a.earned_review - before_r[a.id]) / max(1, reviewed[a.id])
                a.ema_w = 0.8 * a.ema_w + 0.2 * gw
                a.ema_r = 0.8 * a.ema_r + 0.2 * gr
                a.f_review += 0.03 if a.ema_r > a.ema_w else -0.03
                a.f_review = min(0.9, max(0.15 if a.kind == "honest" else 0.05, a.f_review))
        self.t += 1

    def run(self) -> dict:
        for _ in range(self.p.steps):
            self.step()
        return self.metrics()

    def metrics(self) -> dict:
        p = self.p
        units = self.units
        inc_correct = sum(u.total_income for u in units if u.correct)
        inc_all = sum(u.total_income for u in units) or 1.0
        paid = {k: sum(a.paid for a in self.agents if a.kind == k) for k in ("honest", "lazy", "mal")}
        tot_paid = sum(paid.values()) or 1.0
        n_mal = sum(1 for a in self.agents if a.kind == "mal") or 1
        n_all = len(self.agents)
        ring_inc = sum(u.total_income for u in units if u.mal)
        est = [u for u in units if u.established >= 0]
        found = [u for u in units if u.layer == "foundation"]
        found_inc = sum(u.total_income for u in found)
        honest = sorted(a.paid for a in self.agents if a.kind != "mal")
        rev = sum(a.earned_review for a in self.agents)
        wri = sum(a.earned_write for a in self.agents)
        fr = [a.f_review for a in self.agents if a.kind == "honest"]
        good_live = [u for u in units if u.correct and u.established >= 0 and not u.removed]

        def pur(year):
            ts = [v for k, v in self.per_unit_reward.items() if year * 12 - 12 <= k < year * 12]
            return statistics.mean(ts) if ts else 0.0

        def fs(year):  # 当年流量中创始者的占比
            ks = [k for k in self.paid_flow if year * 12 - 12 <= k < year * 12]
            f = sum(self.paid_flow[k][0] for k in ks)
            a = sum(self.paid_flow[k][1] for k in ks)
            return f / a if a else 0.0

        def fs_stock():
            allp = sum(x.paid for x in self.agents)
            return sum(x.paid for x in self.agents if x.founder) / allp if allp else 0.0

        cap = p.e0 * p.emission_half_life / math.log(2)
        return {
            "correct_share": inc_correct / inc_all,
            "mal_share": paid["mal"] / tot_paid,
            "mal_pop": n_mal / n_all,
            "ring_share": ring_inc / inc_all,
            "established_frac": len(est) / max(1, len(units)),
            "est_delay": statistics.mean(self.est_delay) if self.est_delay else float("nan"),
            "err_in_est": sum(1 for u in est if not u.correct and not u.removed) / max(1, sum(1 for u in est if not u.removed)),
            "review_share": rev / max(1e-9, rev + wri),
            "f_review": statistics.mean(fr) if fr else 0.0,
            "found_share": found_inc / inc_all,
            "found_unit_share": len(found) / max(1, len(units)),
            "pur1": pur(1), "pur5": pur(5), "pur10": pur(10),
            "dormant_good": self.dormant_good / max(1, len(good_live)),
            "founder_y1": fs(1), "founder_y10": fs(10),
            "founder_pop": p.founders / n_all,
            "founder_stock": fs_stock(),
            "minted_y1": sum(p.e0 * 0 for _ in ()) or self.minted_y1 / max(1e-9, cap),
            "gini": gini(honest),
            "minted_cap": self.minted / cap,
            "burned_minted": self.burned / max(1e-9, self.minted),
            "units": len(units),
        }


def gini(xs):
    if not xs or sum(xs) == 0:
        return 0.0
    n, s = len(xs), sum(xs)
    return sum((2 * (i + 1) - n - 1) * x for i, x in enumerate(sorted(xs))) / (n * s)


SCENARIOS = [
    ("base", "本设计:可信度门槛 + 核验报酬 25% + 上溯 20% + 锁定期 6 期 + 外部付费", {}),
    ("no_review_pay", "核验报酬关(分成并入知识奖励池)", {"split_review": 0.0, "split_pool": 0.85}),
    ("no_cred_gate", "可信度门槛关:未经核验的单元也分钱,引用创建即计入", {"cred_gate": False}),
    ("no_vesting", "锁定期关:收益立即到手,挑战成功也罚不到", {"vesting": 0}),
    ("no_upstream", "上溯关:基础原理只靠被直接引用获益", {"upstream": 0.0}),
    ("no_fees", "外部付费始终没有到来", {"fee_max": 0.0}),
    ("high_rent", "租金 ×5", {"rent": 0.5}),
    ("no_imp_decay", "重要性不衰减", {"importance_decay": False}),
    ("mal_20", "作恶者占新加入者 20%", {"frac_mal": 0.20}),
    ("mal_35", "作恶者占新加入者 35%", {"frac_mal": 0.35}),
    ("lazy_50", "走过场的核验者占 50%", {"frac_lazy": 0.50}),
    # 第二轮:对策
    ("assigned", "对策 A:核验人由协议随机指派(4 人,凑不够时追加)", {"assigned": True}),
    ("sqrt_share", "对策 B:分配权重取重要性的平方根(边际递减)", {"share_exp": 0.5}),
    ("v2", "A + B", {"assigned": True, "share_exp": 0.5}),
    ("v2_no_review_pay", "A + B,核验报酬关", {"assigned": True, "share_exp": 0.5, "split_review": 0.0, "split_pool": 0.85}),
    ("v2_no_vesting", "A + B,锁定期关", {"assigned": True, "share_exp": 0.5, "vesting": 0}),
    ("v2_no_fees", "A + B,外部付费不到来", {"assigned": True, "share_exp": 0.5, "fee_max": 0.0}),
    ("v2_mal_20", "A + B,作恶者 20%", {"assigned": True, "share_exp": 0.5, "frac_mal": 0.20}),
    ("v2_mal_35", "A + B,作恶者 35%", {"assigned": True, "share_exp": 0.5, "frac_mal": 0.35}),
    ("v2_lazy_50", "A + B,走过场 50%", {"assigned": True, "share_exp": 0.5, "frac_lazy": 0.50}),
    # 第三轮:增发与活动挂钩
    ("activity_cap", "对策 C:每期增发 ≤ 5 × 在册已确立单元数,余额顺延", {"unit_cap": 5.0}),
    ("v3", "A + B + C", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0}),
    ("v3_no_fees", "A + B + C,外部付费不到来", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0, "fee_max": 0.0}),
    ("v3_mal_20", "A + B + C,作恶者 20%", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0, "frac_mal": 0.20}),
    ("v3_mal_35", "A + B + C,作恶者 35%", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0, "frac_mal": 0.35}),
    ("v3_no_review_pay", "A + B + C,核验报酬关", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0, "split_review": 0.0, "split_pool": 0.85}),
    ("v3_no_vesting", "A + B + C,锁定期关", {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0, "vesting": 0}),
]

COLS = [
    ("correct_share", "收入流向正确单元", "{:.3f}"),
    ("mal_share", "作恶者收入占比", "{:.3f}"),
    ("mal_pop", "作恶者人数占比", "{:.2f}"),
    ("ring_share", "互引圈单元收入占比", "{:.3f}"),
    ("err_in_est", "已确立单元中的错误", "{:.3f}"),
    ("established_frac", "单元确立比例", "{:.2f}"),
    ("est_delay", "确立耗时(期)", "{:.1f}"),
    ("f_review", "诚实者核验比例", "{:.2f}"),
    ("review_share", "核验收入占比", "{:.2f}"),
    ("found_share", "基础层收入占比", "{:.3f}"),
    ("found_unit_share", "基础层单元占比", "{:.2f}"),
    ("pur1", "每单元奖励 第1年", "{:.2f}"),
    ("pur5", "第5年", "{:.2f}"),
    ("pur10", "第10年", "{:.2f}"),
    ("dormant_good", "被租金挤出的好单元", "{:.3f}"),
    ("founder_y1", "创始者占当年收入 第1年", "{:.2f}"),
    ("founder_y10", "第10年", "{:.2f}"),
    ("founder_stock", "创始者持有累计收入", "{:.2f}"),
    ("founder_pop", "创始者人数占比", "{:.2f}"),
    ("minted_y1", "第1年增发/上限", "{:.2f}"),
    ("gini", "诚实者收入基尼", "{:.2f}"),
    ("minted_cap", "已增发/上限", "{:.2f}"),
    ("burned_minted", "销毁/增发", "{:.2f}"),
]


def run_scenario(p: Params, seeds: int) -> dict:
    ms = [Sim(p, s).run() for s in range(seeds)]
    return {k: statistics.mean(m[k] for m in ms) for k in ms[0]}


def table(results) -> str:
    head = "| 场景 | 说明 | " + " | ".join(c[1] for c in COLS) + " |"
    sep = "|---|---|" + "---|" * len(COLS)
    rows = [f"| `{n}` | {d} | " + " | ".join(f.format(m[k]) for k, _, f in COLS) + " |" for n, d, m in results]
    return "\n".join([head, sep, *rows])


READING = """## 怎么读这张表

- **收入流向正确单元**:知识奖励池里流向"内容正确"的单元的比例。越高说明钱跟知识质量走。
- **已确立单元中的错误**:已通过核验、但内容其实有错的单元比例。这是质量的核心指标。
- **核验收入占比**:核验报酬在总发放里的份额。它高,审查有人做;它低(见 `no_review_pay`),错误率上升。
- **基础层收入占比**:很少被直接引用的基础原理(占单元数 15%)拿到多少收入。靠上溯,应当高于其单元占比。
- **每单元奖励**:平均每个在册单元分到多少。第 1 年高、第 10 年低,反映增发逐年递减。
- **被租金挤出的好单元**:正确的、确立过的、被引用过的单元里,因付不起租金而休眠的比例。
- **创始者占当年收入**:当年流出的钱里创始社区拿到的比例。它逐年下降,说明 P6(不永久占据话语权)成立。
- **已增发/上限**:累计增发 ÷ 理论上限(E0·半衰期/ln2)。

**诚实边界**:方向性模型。所有行为参数(错误率、检出率、行动数、外部付费曲线)都是假设。
它回答的是"在这些假设下钱往哪里流、哪个开关最关键、在哪里失效",**不是收益预测**。
参数未经真实网络校准;真实社区的行为可能与假设差别很大。
"""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", type=int, default=4)
    ap.add_argument("--steps", type=int, default=120)
    ap.add_argument("--md", action="store_true")
    ap.add_argument("--only", help="逗号分隔的场景名")
    a = ap.parse_args()
    base = Params(steps=a.steps)
    only = set(a.only.split(",")) if a.only else None
    out = []
    for name, desc, over in SCENARIOS:
        if only and name not in only:
            continue
        out.append((name, desc, run_scenario(replace(base, **over), a.seeds)))
        print(f"done {name}", flush=True)
    t = table(out)
    print(t)
    if a.md:
        with open("sim/ECONOMY.md", "w", encoding="utf-8") as f:
            f.write("<!-- 由 sim/economy.py --md 自动生成,勿手改 -->\n")
            f.write(f"# 知识经济仿真结果({a.seeds} 种子均值,{a.steps} 期 = {a.steps // 12} 年)\n\n")
            f.write(READING + "\n\n")
            f.write(t + "\n")


if __name__ == "__main__":
    main()
