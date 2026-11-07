#!/usr/bin/env python3
"""知识质量收敛性仿真(对应 docs/VISION.md §4b)。

问题:在"准入宽、信任靠挣"的机制下(挑战悬赏 + 内部信誉 + 责任机制 + 前期保守),
知识库的**影响力加权错误比例**会不会随时间下降?在什么条件下会失效?

纯 Python 标准库,无依赖:
    python3 sim/convergence.py              # 跑场景矩阵,打印结果
    python3 sim/convergence.py --md         # 同上,并写出 sim/CONVERGENCE.md
    python3 sim/convergence.py --seeds 3 --steps 150

模型要素(与 VISION §4b 的对应关系):
  - 准入宽:任何人可提交,经 3 名核验人(按信誉抽取)初审;新单元可信度从低起步。
  - C1 挑战有奖:审查者检查已收录单元,发现可检出的错误即挑战成功,获得悬赏。
  - C2 悬赏与影响力挂钩:审查者的注意力按影响力分配(关闭时为均匀分配)。
  - C3 内部信誉:人人从新人基线起步(创始社区起点高),信誉随时间向基线衰减。
  - C4 修正沿引用链传播:错误单元被移除后不再被引用(模型中的简化)。
  - C5 前期保守:单元可承载的影响力有上限,随网络规模放开,抑制早期"先到先得"的滚雪球。
  - C6 前期社区驱动:初始成员是小规模、以诚实为主的创始社区,无永久特权。
  - 责任机制:作恶重罚、失职(放过当时可检出的错误)受罚、诚实的错(范式之错)不罚。
  - 争议:范式之错在"证据出现"前无法检出;开拓者提交竞争单元。
    竞争模式(本设计)下竞争单元与旧共识并存;举报模式(对照)下由多数裁决,证据出现前开拓者
    多半被判错并受罚。

诚实边界:这是方向性模型,所有行为参数都是假设。它回答的是"在这些假设下机制是否收敛、
哪个开关最关键、在哪里失效",不是对真实网络的预测。
"""
from __future__ import annotations

import argparse
import random
import statistics
from dataclasses import dataclass, field, replace

# ---------------------------------------------------------------------------
# 参数
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Params:
    steps: int = 200
    # 参与者:创始社区起步,之后逐步增长(C6)
    founders: int = 30
    max_agents: int = 300
    growth_per_step: float = 2.0
    frac_malicious: float = 0.10  # 新加入者中的作恶者比例(创始社区全为诚实/普通)
    frac_lazy: float = 0.25  # 新加入者中"不认真"的比例
    # 提交
    submit_rate_honest: float = 0.15
    submit_rate_mal: float = 0.30
    honest_err: float = 0.06  # 诚实者的事实性错误率
    lazy_err: float = 0.15
    llm_share: float = 0.30  # 由大模型生成初稿的比例
    llm_err: float = 0.25  # 大模型初稿的事实性错误率(编造)
    subtlety: float = 1.0  # 错误隐蔽程度的强度:每个错误的发现概率 = 技能 × (1 − 隐蔽度^…);0 = 都不隐蔽
    llm_subtle: bool = True  # 大模型的编造更隐蔽(读起来像真的)
    p_paradigm: float = 0.02  # 诚实提交中属于"当时的共识、后来被推翻"的比例
    evidence_mean: float = 60.0  # 范式之错的推翻证据平均在多少期后出现
    p_pioneer: float = 0.03  # 每期有开拓者针对某个范式之错提交竞争单元的概率
    # 核验与审查
    verifiers: int = 3
    skill_honest: float = 0.80
    skill_lazy: float = 0.30
    false_flag: float = 0.03
    exam_rate_honest: float = 0.5  # 每名诚实者每期的审查次数
    exam_rate_lazy: float = 0.1
    attack_rate_mal: float = 0.3  # 作恶者每期对正确单元发起恶意挑战的次数
    jurors: int = 5
    juror_acc_honest: float = 0.90
    juror_acc_lazy: float = 0.65
    # 图与收益
    cites_per_unit: int = 3
    infl_decay: float = 0.99
    reward_pool: float = 10.0
    stake: float = 1.0
    # 机制开关
    bounty_weighted: bool = True  # C2
    early_cap: bool = True  # C5
    rep_decay: float = 0.01  # C3,0 = 关闭
    accountability: bool = True  # 责任机制
    dispute_mode: str = "compete"  # "compete"(本设计)| "report"(对照)
    # 第二轮推演新增的机制(默认关,以便与第一轮对照)
    review_obligation: float = 0.0  # M1 审查义务:每提交一条,须审查几条
    target_uncertainty: bool = False  # M2 审查对准"影响大而没被查透"的内容
    rational_attackers: bool = False  # M3 作恶者理性:持续亏损即停手
    attacker_quit: float = -5.0
    penalize_honest_error: bool = False  # True = 朴素规则:范式之错也罚(对照)


# ---------------------------------------------------------------------------
# 实体
# ---------------------------------------------------------------------------

HONEST, LAZY, MAL = 0, 1, 2
OK, FACTUAL, FABRICATED, PARADIGM = 0, 1, 2, 3
REP_BASE = 0.2  # 新人信誉基线
REP_FOUNDER = 1.0


class Agent:
    __slots__ = ("kind", "rep", "net")

    def __init__(self, kind: int, rep: float):
        self.kind = kind
        self.rep = rep
        self.net = 0.0


class Unit:
    __slots__ = ("author", "kind", "born", "evidence_at", "llm", "verifiers", "infl",
                 "cred", "active", "pioneer_of", "has_pioneer", "difficulty")

    def __init__(self, author: int, kind: int, born: int, llm: bool):
        self.author = author
        self.kind = kind
        self.born = born
        self.evidence_at = 10**9
        self.llm = llm
        self.verifiers: list[int] = []
        self.infl = 0.0
        self.cred = 0.0
        self.active = False
        self.pioneer_of = -1  # >=0:这是针对某个范式之错的竞争单元(本身正确)
        self.has_pioneer = False
        self.difficulty = 0.0  # 错误的隐蔽程度 ∈ [0,1):越大越难发现

    @property
    def wrong(self) -> bool:
        return self.kind != OK


@dataclass
class Metrics:
    e_inf: dict = field(default_factory=dict)  # 影响力加权错误比例(全部错误)
    e_know: dict = field(default_factory=dict)  # 只算"当时可检出"的错误
    e_cnt_end: float = 0.0
    false_removals: int = 0
    removals: int = 0
    honest_penalized: int = 0  # 因"诚实的错"或作为开拓者而被罚的诚实参与者人次
    pioneers: int = 0
    pioneers_alive: int = 0
    hon_net: float = 0.0  # 诚实参与者人均净收益
    mal_net: float = 0.0  # 作恶者人均净收益
    mal_rep: float = 0.0  # 作恶者平均信誉(末期)
    units_end: int = 0


# ---------------------------------------------------------------------------
# 仿真
# ---------------------------------------------------------------------------


class Sim:
    def __init__(self, p: Params, seed: int):
        self.p = p
        self.r = random.Random(seed)
        self.t = 0
        self.agents: list[Agent] = []
        for i in range(p.founders):
            # 创始社区:以诚实为主,有少数不认真者,无作恶者(C6)
            self.agents.append(Agent(LAZY if i % 6 == 0 else HONEST, REP_FOUNDER))
        self.units: list[Unit] = []
        self.active: list[int] = []
        self.submitters: list[int] = []
        self.m = Metrics()

    # -- helpers ------------------------------------------------------------

    def detectable(self, u: Unit) -> bool:
        return u.wrong and (u.kind != PARADIGM or self.t >= u.evidence_at)

    def finds(self, u: Unit, skill: float) -> bool:
        """一次检查能否发现该单元的错误:可检出 且 掷中 技能 ×(1 − 隐蔽度)。"""
        return self.detectable(u) and self.r.random() < skill * (1.0 - u.difficulty)

    def infl_cap(self) -> float:
        if not self.p.early_cap:
            return float("inf")
        # 影响力上限随网络规模放开:创始期很低,满规模时实际不再约束
        frac = len(self.agents) / self.p.max_agents
        return 2.0 + 200.0 * frac**2

    def pick_agents(self, k: int, exclude: int = -1) -> list[int]:
        idx = [i for i in range(len(self.agents)) if i != exclude]
        w = [max(self.agents[i].rep, 0.01) for i in idx]
        return self.r.choices(idx, weights=w, k=k)

    def penalize(self, ai: int, rep_mult: float, rep_sub: float, money: float,
                 honest_error: bool = False):
        if not self.p.accountability:
            return
        a = self.agents[ai]
        a.rep = max(0.0, a.rep * rep_mult - rep_sub)
        a.net -= money
        if honest_error and a.kind != MAL:
            self.m.honest_penalized += 1

    def remove(self, ui: int):
        u = self.units[ui]
        if u.active:
            u.active = False
            self.m.removals += 1

    # -- 一期 ---------------------------------------------------------------

    def step(self):
        p, r = self.p, self.r
        # 1. 增长
        target = min(p.max_agents, p.founders + int(p.growth_per_step * self.t))
        while len(self.agents) < target:
            x = r.random()
            kind = MAL if x < p.frac_malicious else (LAZY if x < p.frac_malicious + p.frac_lazy else HONEST)
            self.agents.append(Agent(kind, REP_BASE))
        # 2. 衰减
        self.active = [i for i in self.active if self.units[i].active]
        for i in self.active:
            self.units[i].infl *= p.infl_decay
        if p.rep_decay > 0:
            for a in self.agents:
                a.rep = REP_BASE + (a.rep - REP_BASE) * (1 - p.rep_decay)
        # 3. 提交
        new: list[int] = []
        self.submitters: list[int] = []
        for ai, a in enumerate(self.agents):
            if a.kind == MAL and p.rational_attackers and a.net < p.attacker_quit:
                continue  # 理性作恶者:已经亏损,停手
            rate = p.submit_rate_mal if a.kind == MAL else p.submit_rate_honest
            if r.random() >= rate:
                continue
            if a.kind == MAL:
                u = Unit(ai, FABRICATED, self.t, False)
                u.difficulty = min(r.random() ** (1.0 / p.subtlety), 0.97) if p.subtlety > 0 else 0.0
            else:
                llm = r.random() < p.llm_share
                err = p.llm_err if llm else (p.honest_err if a.kind == HONEST else p.lazy_err)
                x = r.random()
                if x < err:
                    kind = FACTUAL
                elif x < err + p.p_paradigm:
                    kind = PARADIGM
                else:
                    kind = OK
                u = Unit(ai, kind, self.t, llm)
                if kind == FACTUAL:
                    d = r.random() ** (1.0 / p.subtlety) if p.subtlety > 0 else 0.0
                    if llm and p.llm_subtle:
                        d = 1.0 - (1.0 - d) * 0.6  # 编造更像真的
                    u.difficulty = min(d, 0.97)
                if kind == PARADIGM:
                    u.evidence_at = self.t + int(r.expovariate(1.0 / p.evidence_mean))
            self.units.append(u)
            new.append(len(self.units) - 1)
            if a.kind != MAL:
                self.submitters.append(ai)
        # 3b. 开拓者:针对仍在册的范式之错提交竞争单元(本身正确)
        for ui in self.active:
            u = self.units[ui]
            if u.kind == PARADIGM and not u.has_pioneer and r.random() < p.p_pioneer:
                honest = [i for i, a in enumerate(self.agents) if a.kind == HONEST]
                if honest:
                    pu = Unit(r.choice(honest), OK, self.t, False)
                    pu.pioneer_of = ui
                    u.has_pioneer = True
                    self.units.append(pu)
                    new.append(len(self.units) - 1)
                    self.m.pioneers += 1
        # 4. 核验
        for ui in new:
            self.verify(ui)
        # 5. 引用
        self.cite([ui for ui in new if self.units[ui].active])
        # 6. 审查与挑战
        self.examine()
        if p.dispute_mode == "report":
            self.report_pioneers()
        # 7. 收益:奖励池按有效影响力分给在册单元的作者
        self.pay()
        self.t += 1

    def verify(self, ui: int):
        p, r = self.p, self.r
        u = self.units[ui]
        vs = self.pick_agents(p.verifiers, exclude=u.author)
        flags = 0
        for vi in vs:
            v = self.agents[vi]
            if v.kind == MAL:
                continue  # 作恶者一律放行
            skill = p.skill_honest if v.kind == HONEST else p.skill_lazy
            if u.pioneer_of >= 0 and p.dispute_mode == "report" and self.t < self.units[u.pioneer_of].evidence_at:
                # 举报模式下,核验人按当时共识判断开拓者:多半认为它错了
                if r.random() < 0.6:
                    flags += 1
            elif self.detectable(u):
                if self.finds(u, skill):
                    flags += 1
            elif r.random() < p.false_flag:
                flags += 1
        if flags * 2 > len(vs):  # 多数标记 → 拒收
            if u.wrong:
                for vi in vs:
                    if self.agents[vi].kind != MAL:
                        self.agents[vi].rep += 0.05
                        self.agents[vi].net += 0.1
                if u.kind == FABRICATED:
                    self.penalize(u.author, 0.1, 0.0, p.stake)
                else:
                    self.penalize(u.author, 1.0, 0.05, p.stake * 0.3)
            elif u.pioneer_of >= 0:
                # 正确的开拓者在初审被多数否决并受罚
                self.penalize(u.author, 1.0, 0.1, p.stake * 0.5, honest_error=True)
            return
        u.active = True
        u.verifiers = vs
        u.cred = 0.1 if u.llm else 0.2
        self.active.append(ui)

    def cite(self, citing: list[int]):
        p, r = self.p, self.r
        if not citing:
            return
        pool = [i for i in self.active if i not in set(citing)]
        if not pool:
            return
        cap = self.infl_cap()
        w = []
        for i in pool:
            u = self.units[i]
            wi = (min(u.infl, cap) + 1.0) * max(u.cred, 0.05)
            if u.kind == PARADIGM and self.t >= u.evidence_at:
                wi *= 0.2  # 证据出现后,旧共识越来越少被引用
            if u.pioneer_of >= 0 and self.t >= self.units[u.pioneer_of].evidence_at:
                wi *= 3.0  # 新认识越来越多被引用
            w.append(wi)
        targets = r.choices(pool, weights=w, k=p.cites_per_unit * len(citing))
        for ti in targets:
            t = self.units[ti]
            t.infl += 1.0
            t.cred = min(1.0, t.cred + 0.01)

    def examine(self):
        p, r = self.p, self.r
        if not self.active:
            return
        honest = [i for i, a in enumerate(self.agents) if a.kind == HONEST]
        lazy = [i for i, a in enumerate(self.agents) if a.kind == LAZY]
        n_h = int(len(honest) * p.exam_rate_honest)
        n_l = int(len(lazy) * p.exam_rate_lazy)
        n_o = int(len(self.submitters) * p.review_obligation)  # M1 审查义务
        n = n_h + n_l + n_o
        if n == 0:
            return
        if p.target_uncertainty:  # M2:影响力 × 未经检验程度
            w = [(self.units[i].infl + 0.05) * (1.05 - self.units[i].cred) for i in self.active]
        elif p.bounty_weighted:
            w = [self.units[i].infl + 0.05 for i in self.active]
        else:
            w = [1.0] * len(self.active)
        targets = r.choices(self.active, weights=w, k=n)
        for j, ui in enumerate(targets):
            u = self.units[ui]
            if not u.active:
                continue
            if j < n_h:
                ei = r.choice(honest)
            elif j < n_h + n_l:
                ei = r.choice(lazy)
            else:
                ei = r.choice(self.submitters)
            skill = p.skill_honest if self.agents[ei].kind == HONEST else p.skill_lazy
            if self.finds(u, skill):
                self.challenge_succeeds(ui, ei)
            elif not u.wrong and r.random() < 0.01:
                self.jury(ui, ei, truth_remove=False)  # 诚实者偶尔的误判挑战
            else:
                u.cred = min(1.0, u.cred + 0.05)  # 经受住一次检查
        # 恶意挑战:作恶者攻击有价值的正确单元
        mal = [i for i, a in enumerate(self.agents) if a.kind == MAL
               and not (p.rational_attackers and a.net < p.attacker_quit)]
        n_m = int(len(mal) * p.attack_rate_mal)
        good = [i for i in self.active if not self.units[i].wrong and self.units[i].active]
        if n_m and good:
            gw = [self.units[i].infl + 0.05 for i in good]
            for ui in r.choices(good, weights=gw, k=n_m):
                if self.units[ui].active:
                    self.jury(ui, r.choice(mal), truth_remove=False)

    def challenge_succeeds(self, ui: int, ei: int):
        p = self.p
        u = self.units[ui]
        bounty = p.stake + (u.infl if p.bounty_weighted else 0.0) * 0.1
        self.agents[ei].net += bounty
        self.agents[ei].rep += 0.2
        self.remove(ui)
        honest_err = u.kind == PARADIGM
        if honest_err and not p.penalize_honest_error:
            return  # 诚实的错:当时无从知道,不罚(推翻证据晚于提交与核验)
        if u.kind == FABRICATED:
            self.penalize(u.author, 0.1, 0.0, p.stake)
            neg = (1.0, 0.3, p.stake * 0.5)
        else:
            self.penalize(u.author, 1.0, 0.1, p.stake * 0.3, honest_error=honest_err)
            neg = (1.0, 0.2, p.stake * 0.3)
        for vi in u.verifiers:  # 失职:放过了当时可检出的错误
            self.penalize(vi, *neg, honest_error=honest_err)

    def jury(self, ui: int, challenger: int, truth_remove: bool):
        """对一次挑战做多数裁决。truth_remove:正确答案是否应移除该单元。"""
        p, r = self.p, self.r
        u = self.units[ui]
        challenger_mal = self.agents[challenger].kind == MAL
        js = self.pick_agents(p.jurors, exclude=challenger)
        votes: list[tuple[int, bool]] = []
        for ji in js:
            j = self.agents[ji]
            if j.kind == MAL:
                vote = challenger_mal  # 作恶裁决者站在作恶挑战者一边
            else:
                acc = p.juror_acc_honest if j.kind == HONEST else p.juror_acc_lazy
                vote = truth_remove if r.random() < acc else (not truth_remove)
            votes.append((ji, vote))
        remove = sum(v for _, v in votes) * 2 > len(js)
        # 裁决者与多数一致得奖,偏离受罚(谢林点激励;这也是"多数压制少数"风险的来源)
        for ji, vote in votes:
            if vote == remove:
                self.agents[ji].rep += 0.02
            else:
                self.penalize(ji, 1.0, 0.05, 0.0)
        if remove != truth_remove:
            # 裁决出错
            if remove:
                self.m.false_removals += 1
                self.remove(ui)
                self.agents[challenger].net += p.stake
            else:
                self.penalize(challenger, 0.5, 0.0, p.stake)
        else:
            if remove:
                self.remove(ui)
                self.agents[challenger].net += p.stake
            else:
                self.penalize(challenger, 0.5, 0.0, p.stake)
                u.cred = min(1.0, u.cred + 0.05)

    def report_pioneers(self):
        """举报模式(对照):旧共识的拥护者举报开拓者,由多数裁决。"""
        p, r = self.p, self.r
        for ui in list(self.active):
            u = self.units[ui]
            if u.pioneer_of < 0 or not u.active or r.random() >= 0.1:
                continue
            before = self.t < self.units[u.pioneer_of].evidence_at
            js = self.pick_agents(p.jurors, exclude=u.author)
            # 证据出现前,多数裁决者按当时共识判开拓者为错
            votes = sum(1 for ji in js if r.random() < (0.7 if before else 0.1))
            if votes * 2 > len(js):
                self.remove(ui)
                self.penalize(u.author, 1.0, 0.1, p.stake * 0.5, honest_error=True)

    def pay(self):
        cap = self.infl_cap()
        eff = [(i, min(self.units[i].infl, cap)) for i in self.active if self.units[i].active]
        total = sum(e for _, e in eff)
        if total <= 0:
            return
        for i, e in eff:
            self.agents[self.units[i].author].net += self.p.reward_pool * e / total

    # -- 指标 ---------------------------------------------------------------

    def error_ratios(self) -> tuple[float, float]:
        act = [self.units[i] for i in self.active if self.units[i].active]
        tot = sum(u.infl for u in act)
        if tot <= 0:
            return 0.0, 0.0
        wrong = sum(u.infl for u in act if u.wrong)
        know = sum(u.infl for u in act if self.detectable(u))
        return wrong / tot, know / tot

    def run(self) -> Metrics:
        p = self.p
        marks = {int(p.steps * f) - 1: f for f in (0.1, 0.25, 0.5, 0.75, 1.0)}
        for _ in range(p.steps):
            self.step()
            if self.t - 1 in marks:
                e, k = self.error_ratios()
                f = marks[self.t - 1]
                self.m.e_inf[f] = e
                self.m.e_know[f] = k
        act = [self.units[i] for i in self.active if self.units[i].active]
        self.m.units_end = len(act)
        self.m.e_cnt_end = sum(1 for u in act if u.wrong) / max(1, len(act))
        self.m.pioneers_alive = sum(1 for u in act if u.pioneer_of >= 0)
        hon = [a for a in self.agents if a.kind == HONEST]
        mal = [a for a in self.agents if a.kind == MAL]
        self.m.hon_net = statistics.mean(a.net for a in hon) if hon else 0.0
        self.m.mal_net = statistics.mean(a.net for a in mal) if mal else 0.0
        self.m.mal_rep = statistics.mean(a.rep for a in mal) if mal else 0.0
        return self.m


# ---------------------------------------------------------------------------
# 场景矩阵
# ---------------------------------------------------------------------------

BASE = Params()
SCENARIOS: list[tuple[str, str, Params]] = [
    ("baseline", "本设计:C1–C6 + 责任机制 + 竞争单元", BASE),
    ("easy_errors", "对照:错误都不隐蔽(上一版模型)", replace(BASE, subtlety=0.0, llm_subtle=False)),
    ("no_bounty_weight", "C2 关:审查注意力均匀分配,不按影响力", replace(BASE, bounty_weighted=False)),
    ("no_early_cap", "C5 关:无前期影响力上限", replace(BASE, early_cap=False)),
    ("no_rep_decay", "C3 衰减关:信誉不衰减", replace(BASE, rep_decay=0.0)),
    ("no_accountability", "责任机制关:不罚任何人", replace(BASE, accountability=False)),
    ("mal_20", "作恶者占新加入者 20%", replace(BASE, frac_malicious=0.20)),
    ("mal_35", "作恶者占新加入者 35%", replace(BASE, frac_malicious=0.35)),
    ("mal_50", "作恶者占新加入者 50%", replace(BASE, frac_malicious=0.50)),
    ("llm_heavy", "80% 内容由大模型生成初稿", replace(BASE, llm_share=0.80)),
    ("low_scrutiny", "审查积极性低:诚实者每期只查 0.1 次", replace(BASE, exam_rate_honest=0.1)),
    ("naive_dispute", "对照:举报模式 + 诚实的错也罚", replace(BASE, dispute_mode="report", penalize_honest_error=True)),
]

# 第二轮推演:针对第一轮"不收敛"的发现,加入 M1–M3
V2 = replace(BASE, review_obligation=2.0, target_uncertainty=True, rational_attackers=True)
SCENARIOS_V2: list[tuple[str, str, Params]] = [
    ("v2_obligation", "仅 M1:每提交一条须审查 2 条", replace(BASE, review_obligation=2.0)),
    ("v2_targeting", "仅 M2:审查对准影响大而没查透的", replace(BASE, target_uncertainty=True)),
    ("v2_rational", "仅 M3:作恶者亏损即停手", replace(BASE, rational_attackers=True)),
    ("v2_all", "M1 + M2 + M3", V2),
    ("v2_all_no_acc", "M1–M3,但关掉责任机制", replace(V2, accountability=False)),
    ("v2_mal_35", "M1–M3,作恶者 35%", replace(V2, frac_malicious=0.35)),
    ("v2_mal_50", "M1–M3,作恶者 50%", replace(V2, frac_malicious=0.50)),
    ("v2_llm_heavy", "M1–M3,80% 大模型初稿", replace(V2, llm_share=0.80)),
    ("v2_low_scrutiny", "M1–M3,自愿审查积极性低", replace(V2, exam_rate_honest=0.1)),
    ("v2_naive_dispute", "M1–M3,但用举报模式 + 诚实的错也罚", replace(V2, dispute_mode="report", penalize_honest_error=True)),
]


def run_scenario(p: Params, seeds: int) -> dict:
    ms = [Sim(p, s).run() for s in range(seeds)]

    def avg(fn):
        return statistics.mean(fn(m) for m in ms)

    return {
        "e10": avg(lambda m: m.e_inf.get(0.1, 0)),
        "e25": avg(lambda m: m.e_inf.get(0.25, 0)),
        "e50": avg(lambda m: m.e_inf.get(0.5, 0)),
        "e100": avg(lambda m: m.e_inf.get(1.0, 0)),
        "e100_sd": statistics.pstdev([m.e_inf.get(1.0, 0) for m in ms]),
        "k25": avg(lambda m: m.e_know.get(0.25, 0)),
        "k100": avg(lambda m: m.e_know.get(1.0, 0)),
        "cnt": avg(lambda m: m.e_cnt_end),
        "false_rm": avg(lambda m: m.false_removals),
        "hon_pen": avg(lambda m: m.honest_penalized),
        "pioneer": avg(lambda m: m.pioneers_alive / m.pioneers if m.pioneers else 0.0),
        "hon_net": avg(lambda m: m.hon_net),
        "mal_net": avg(lambda m: m.mal_net),
        "mal_rep": avg(lambda m: m.mal_rep),
        "units": avg(lambda m: m.units_end),
    }


def table(results: list[tuple[str, str, dict]]) -> str:
    h = ("| 场景 | 说明 | 加权错误率 10%→25%→50%→末期 | 可检出错误 25%→末期 | 末期错误占数量比 "
         "| 正确单元被误删 | 诚实者因诚实的错受罚 | 开拓者存活率 | 诚实者人均净收益 | 作恶者人均净收益 | 作恶者末期信誉 |")
    sep = "|" + "---|" * 11
    rows = [h, sep]
    for name, desc, x in results:
        rows.append(
            f"| `{name}` | {desc} | {x['e10']:.3f} → {x['e25']:.3f} → {x['e50']:.3f} → **{x['e100']:.3f}** ±{x['e100_sd']:.3f} "
            f"| {x['k25']:.3f} → **{x['k100']:.3f}** | {x['cnt']:.3f} | {x['false_rm']:.1f} | {x['hon_pen']:.1f} "
            f"| {x['pioneer']:.2f} | {x['hon_net']:+.2f} | {x['mal_net']:+.2f} | {x['mal_rep']:.2f} |"
        )
    return "\n".join(rows)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--md", action="store_true", help="写出 sim/CONVERGENCE.md")
    ap.add_argument("--seeds", type=int, default=5)
    ap.add_argument("--steps", type=int, default=BASE.steps)
    args = ap.parse_args()
    def run_all(scen):
        out = []
        for name, desc, p in scen:
            out.append((name, desc, run_scenario(replace(p, steps=args.steps), args.seeds)))
            print(f"  done {name}", flush=True)
        return out

    t1 = table(run_all(SCENARIOS))
    t2 = table(run_all(SCENARIOS_V2))
    t = "## 第一轮:VISION §4b 草案(C1–C6 + 责任机制)\n\n" + t1 + \
        "\n\n## 第二轮:加入 M1 审查义务、M2 精准审查、M3 理性作恶者\n\n" + t2
    print(t)
    if args.md:
        import os
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "CONVERGENCE.md")
        with open(path, "w", encoding="utf-8") as f:
            f.write("<!-- 由 sim/convergence.py --md 自动生成,勿手改 -->\n")
            f.write(f"# 收敛性仿真结果({args.seeds} 种子均值,{args.steps} 期)\n\n")
            f.write(t + "\n")
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
