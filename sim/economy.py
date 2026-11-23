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
    # 第四轮:弹性供应(锚定知识量)
    elastic: bool = False
    k_money: float = 200.0  # 每单位知识量对应的目标货币量
    k_imp_cap: float = 3.0  # 知识量中单元的 √重要性 上限
    kp: float = 0.10  # 比例增益(每期,按相对偏差)
    ki: float = 0.01  # 积分增益
    r_max: float = 0.005  # 每期供应量变化上限(相对)
    k_smooth: int = 3  # 知识量取最近几期的均值
    rent_mult_max: float = 3.0  # 收缩时租金最多提高到几倍
    feedforward: bool = True  # 增发直接跟随知识量的增长
    rent_tax: float = 0.0  # >0:租金改为收入的固定比例(税),休眠改由"使用"判定(重要性低于 i_alive)
    i_alive: float = 0.05
    treasury_floor: float = 0.0  # >0:知识奖励池每期至少 floor × 知识量,不足由国库补足
    # 第六轮:针对知识量的攻击
    mal_pump: bool = False  # 作恶者持续为已确立的自己人单元追加"通过",把可信度推到上限
    pump_actions: int = 3  # 每名作恶者每期额外的追加通过次数
    k_cred_cap: float = 0.9  # 知识量中可信度的计入上限(0.6 = 2 位净通过即饱和)
    # 冲击(用于检验死亡螺旋)
    shock_step: int = -1  # 这一期有 shock_frac 的已确立单元被证伪退出
    shock_frac: float = 0.0
    shrink_step: int = -1  # 从这一期起社区萎缩:不再有新人,行动数降为 shrink_actions
    shrink_actions: int = 1
    # 第七轮(上链 v1):结算期长、映射方式与 v1 的简化
    period_days: float = 30.0  # 一期多少天
    mapping: str = "time"  # time:所有按月标定的量换算成同样的**真实时间**(基线,仿真与已发布结果一致)
    # chain:按上链 v1 的字面读法——设计稿里的"期"就是结算期(天),不再折算成月
    # hybrid:天结算,时长(锁定期、半衰期、平滑窗口)按真实时间;追加指派每 3 天一次、见习期 7 天(链上的实际值)
    burn: bool = True  # 关:只增不缩(不销毁、不收缩),对应 v1 的"只前馈"
    slashing: bool = True  # 关:挑战成功只让错误单元退出,不罚没锁定收益、不限流放行者(v1 还没有挑战与罚没)


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


def per_period(p: Params) -> tuple[Params, float, bool]:
    """把参数换算到 period_days 的期长。

    返回(换算后的参数, 每月几期 M, 是否按链的字面读法换算)。

    两种映射:
    - **time**:所有按月标定的量换算成同样的**真实时间**。时长(期数)× M:步数、锁定期、增发半衰期、
      外部付费曲线、平滑窗口、冲击时刻;每期的量 ÷ M:行动数、新人数、增发、外部付费、租金、追加通过;
      每期的增益与上限按复利换算:r_max → (1+r)^(1/M) − 1;kp、ki ÷ M。
      核验的检出率、错误率、挑战比例是"每次行动"的属性,与期长无关,不换算。
      M = 1(按月结算)时**完全等价于原来的仿真**,所以已发布的结果仍然可比。
    - **chain**:按上链 v1 的字面读法——设计稿里的"期"就是结算期(天)。见 Sim 里的 lit()/mon()。

    诚实边界:两种映射回答的是同一个问题("换成天结算后结论还成立吗"),但它们对"期"的理解不同,
    所以结果不同是预期的;选哪个是产品决定,不是技术决定。
    """
    m = 30.0 / p.period_days
    if m == 1.0:
        return p, 1.0, p.mapping == "chain"
    q = replace(
        p,
        steps=round(p.steps * m), probation=max(1, round(p.probation * m)), vesting=round(p.vesting * m),
        emission_half_life=round(p.emission_half_life * m), fee_mid=round(p.fee_mid * m), fee_width=p.fee_width * m,
        shock_step=round(p.shock_step * m) if p.shock_step >= 0 else -1,
        shrink_step=round(p.shrink_step * m) if p.shrink_step >= 0 else -1,
        actions=p.actions / m, shrink_actions=p.shrink_actions / m, growth=p.growth / m,
        e0=p.e0 / m, fee_max=p.fee_max / m, rent=p.rent / m, pump_actions=p.pump_actions / m,
        unit_cap=p.unit_cap / m,
        r_max=(1 + p.r_max) ** (1 / m) - 1 if p.r_max < 1 else p.r_max, kp=p.kp / m, ki=p.ki / m,
    )
    if p.mapping == "chain":
        # 链的字面读法:设计稿里的数字就是"期"(天)
        q = replace(q, probation=7, vesting=6, k_smooth=3)
    else:
        q = replace(q, k_smooth=max(1, round(p.k_smooth * m)))
        if p.mapping == "hybrid":
            q = replace(q, probation=7)  # 链上的见习期是 7 天
    return q, m, p.mapping == "chain"


class Sim:
    def __init__(self, p: Params, seed: int):
        p, self.M, self.chain = per_period(p)
        self.hybrid = p.mapping == "hybrid" and self.M != 1.0
        self.Y = round(12 * self.M)  # 每年几期
        self.p, self.r = p, random.Random(seed)
        self.members: list = []
        self.queue: dict = {}
        self.agents: list[Agent] = []
        self.units: list[Unit] = []
        self.minted = self.burned = self.treasury = self.fees = 0.0
        self.carry = 0.0
        self.minted_y1 = 0.0
        self.k_hist: list[float] = []
        self.m_hist: list[float] = []
        self.kmal_hist: list[float] = []
        self.integ = 0.0
        self.last_burn = 0.0
        self.dormant_good_post = 0
        self.good_at_shock = 0
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

    def n_of(self, x: float) -> int:
        """期望为 x 的随机整数(期长变短后每期行动数会小于 1)。

        x 是整数时不消耗随机数——否则随机序列会变,基线(M=1)就对不上原来的仿真了。"""
        frac = x % 1
        return int(x) + (1 if frac > 1e-12 and self.r.random() < frac else 0)

    def months(self, n: float) -> int:
        """n 个月对应几期(真实时间)。"""
        return max(1, round(n * self.M))

    def lit(self, n: int) -> int:
        """设计稿里的"n 期":time 映射下是 n 个月(真实时间),chain 映射下就是 n 个结算期(天)。"""
        return n if self.chain else self.months(n)

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
        hl = HALF_LIFE[u.layer] * (1.0 if self.chain else self.M)
        return u.i_last * 2 ** (-(self.t - u.t_last) / hl)

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
        if not p.slashing:  # v1:错误单元退出,但没有罚没与限流
            return
        pot = self.slash(u.author, u.id)
        for i in getattr(u, "_approvers", []):
            pot += self.slash(i, u.id)
            self.agents[i].banned_until = self.t + self.lit(6)  # 放行了可检出错误:暂停计入 6 期(M6 式限流)
        self.agents[u.author].banned_until = self.t + self.lit(6)
        self.lock(a, pot * 0.5, None, review=True)
        self.burned += pot * 0.5

    def slash(self, aid: int, uid: int) -> float:
        a = self.agents[aid]
        # 锁定中的全部收益都可罚没:锁定期担保的是"这段时间里没有放过当时可检出的错误"
        cut = sum(amt for _, amt, _ in a.locked)
        a.locked = []
        a.slashed += cut
        return cut

    # -- 知识量 ---------------------------------------------------------------
    def knowledge_volume(self, live) -> tuple[float, float]:
        """K = Σ 有效可信度 × min(√重要性, 上限);有效可信度 = min(自身, 所依据单元的有效可信度)。"""
        eff: dict[int, float] = {}
        k = kmal = 0.0
        for u in live:  # live 按 id 递增,依据的单元 id 更小,一遍即可
            if u.established < 0:
                continue
            c = min(self.p.k_cred_cap, 0.2 + 0.2 * len(self.counted(u.passes, u)))
            for b in u.basis:
                if b in eff:
                    c = min(c, eff[b])
            eff[u.id] = c
            v = c * min(math.sqrt(max(0.0, self.importance(u))), self.p.k_imp_cap)
            k += v
            if u.mal:
                kmal += v
        return k, kmal

    def k_amp(self) -> float:
        """作恶单元在知识量中的份额 ÷ 其在已确立在册单元中的份额。1 = 与诚实单元同等分量;>1 = 被放大。"""
        live = [u for u in self.units if self.live(u) and u.established >= 0]
        if not live or not self.k_hist or not self.k_hist[-1]:
            return float("nan")
        ms = sum(1 for u in live if u.mal) / len(live)
        return (self.kmal_hist[-1] / self.k_hist[-1]) / ms if ms else float("nan")

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
        kv, kmal = self.knowledge_volume(live)
        self.k_hist.append(kv)
        self.kmal_hist.append(kmal)
        fee = p.fee_max / (1 + math.exp(-(t - p.fee_mid) / p.fee_width)) if p.fee_max else 0.0
        burned_before = self.burned
        rent_mult, rent_burn, fee_burn = 1.0, 0.5, 0.0
        if p.elastic:
            ks = statistics.mean(self.k_hist[-p.k_smooth:])
            ks_prev = statistics.mean(self.k_hist[-p.k_smooth - 1:-1]) if len(self.k_hist) > 1 else 0.0
            target = p.k_money * ks
            m = self.minted - self.burned
            base = max(m, target, p.k_money)
            e = (target - m) / base
            self.integ = max(-5.0, min(5.0, self.integ + e))
            correction = max(-p.r_max, min(p.r_max, p.kp * e + p.ki * self.integ)) * base
            # 前馈:知识量增长多少,货币就跟着增发多少(不受速度上限约束);
            # 知识量减少时同样跟着收缩,但收缩与纠偏合计每期不超过 r_max,避免一次冲击引发剧烈通缩。
            ff = p.k_money * (ks - ks_prev) if p.feedforward else 0.0
            desired = ff + correction
            if not p.burn:  # v1:只增不缩——知识量下降时停止增发,但不销毁
                desired = max(0.0, desired)
            if desired < 0:
                desired = max(desired, -p.r_max * base)
            if desired >= 0:
                emission = desired + self.last_burn  # 抵消上期销毁,使净变化≈期望值
            else:
                emission = 0.0
                need = -desired
                fee_burn = min(fee, need)
                need -= fee_burn
                rent_base = (p.rent * len(live) if p.rent_tax <= 0 else p.rent_tax * max(1e-9, fee + self.last_burn)) or 1e-9
                rent_mult = 1.0 + min(p.rent_mult_max - 1.0, need / rent_base)
                rent_burn = 1.0
        else:
            emission = p.e0 * 2 ** (-t / p.emission_half_life)
            if p.unit_cap > 0:
                avail = emission + self.carry
                emission = min(avail, p.unit_cap * len(live))
                self.carry = avail - emission
        self.minted += emission
        self.burned += fee_burn
        self.fees += fee
        total = emission + fee - fee_burn
        pool, rpool = total * p.split_pool, total * p.split_review
        self.treasury += total - pool - rpool
        if p.treasury_floor > 0:
            floor = p.treasury_floor * kv
            if pool < floor:
                draw = min(self.treasury, floor - pool)
                self.treasury -= draw
                pool += draw
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
            u.income = (u.income + [inc])[-self.lit(12):]
            u.total_income += inc
            if p.rent_tax > 0:
                rent = min(inc, inc * p.rent_tax * rent_mult)
            else:
                rent = p.rent * rent_mult
            net = inc - rent if not u.dormant else 0.0
            if not u.dormant:
                self.burned += min(inc, rent) * rent_burn
                self.treasury += min(inc, rent) * (1 - rent_burn)
            if net > 0:
                self.lock(self.agents[u.author], net, u.id)
            if p.rent_tax > 0:
                unused = u.established >= 0 and t - u.established >= self.lit(12) and self.importance(u) < p.i_alive
                dies = not u.dormant and unused
            else:
                dies = not u.dormant and t - u.born >= self.lit(12) and sum(u.income) < self.lit(12) * rent
            if dies:
                u.dormant = True
                if u.correct and u.established >= 0 and u.cited:
                    self.dormant_good += 1
                    if 0 <= p.shock_step <= t:
                        self.dormant_good_post += 1
        cs = sum(self.review_credits.values())
        if cs > 0:
            for i, c in self.review_credits.items():
                self.lock(self.agents[i], rpool * c / cs, None, review=True)
        else:
            self.treasury += rpool
        self.review_credits = {}
        if live:
            self.per_unit_reward[t] = pool / len(live) * self.M  # 折算成每月,不同期长可比
        self.m_hist.append(self.minted - self.burned)
        self.last_burn = self.burned - burned_before
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
        if t == self.Y - 1:
            self.minted_y1 = self.minted
        founders = sum(a.paid for a in self.agents if a.founder)
        allp = sum(a.paid for a in self.agents)
        self.founder_share[t] = founders / allp if allp else 0.0

    # -- 主循环 ---------------------------------------------------------------
    def step(self):
        p, r = self.p, self.r
        if self.t == p.shock_step:  # 外部冲击:一批已确立单元被证伪退出(如一个领域的认识被推翻)
            est = [u for u in self.units if u.established >= 0 and not u.removed and not u.dormant]
            self.good_at_shock = sum(1 for u in est if u.correct and u.cited)
            for u in r.sample(est, int(len(est) * p.shock_frac)):
                u.removed = True
        shrinking = 0 <= p.shrink_step <= self.t
        actions_mean = p.shrink_actions if shrinking else p.actions
        if len(self.agents) < p.max_agents and not shrinking:
            n = int(p.growth) + (1 if r.random() < p.growth % 1 else 0)
            for _ in range(n):
                x = r.random()
                self.add_agent("mal" if x < p.frac_mal else "lazy" if x < p.frac_mal + p.frac_lazy else "honest")
        self.pending = [u for u in self.units if u.established < 0 and not u.removed and self.t - u.born < self.lit(24)]
        self.members = [a for a in self.agents if self.established_member(a)]
        if p.assigned:
            if not hasattr(self, "queue"):
                self.queue = {}
            for aid in list(self.queue):
                self.queue[aid] = [u for u in self.queue[aid] if u.established < 0 and not u.removed and self.t - u.born < self.lit(24)]
            topup = 3 if self.hybrid else self.lit(3)  # hybrid:每 3 天追加一次(链上的规则)
            for u in self.pending:  # 迟迟凑不够通过的,追加指派 1 人(上限 8 人)
                if self.t - u.born >= topup and len(u.assigned) < 8 and len(self.counted(u.passes, u)) < 2:
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
            for _ in range(self.n_of(actions_mean)):
                if r.random() < a.f_review:
                    reviewed[a.id] += 1
                    self.review(a)
                else:
                    wrote[a.id] += 1
                    self.write(a)
        if p.mal_pump:  # 额外行动(不占用写/核验预算):为已确立的自己人单元追加"通过";指派模式下只有被指派者计入
            est = [u for u in self.live_ring if u.established >= 0]
            for a in self.agents:
                if a.kind != "mal" or not est:
                    continue
                for _ in range(self.n_of(p.pump_actions)):
                    u = r.choice(est)
                    if u.author != a.id and a.id not in u.passes:
                        u.passes.append(a.id)
        self.distribute()
        if p.rational:
            for a in self.agents:
                if a.kind == "mal":
                    continue
                gw = (a.earned_write - before_w[a.id]) / max(1, wrote[a.id])
                gr = (a.earned_review - before_r[a.id]) / max(1, reviewed[a.id])
                if not wrote[a.id] and not reviewed[a.id]:
                    continue  # 这一期没有行动:没有新信息
                keep = 0.8 ** (1 / self.M)
                a.ema_w = keep * a.ema_w + (1 - keep) * gw
                a.ema_r = keep * a.ema_r + (1 - keep) * gr
                step = 0.03 / self.M
                a.f_review += step if a.ema_r > a.ema_w else -step
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
            Y = self.Y
            ts = [v for k, v in self.per_unit_reward.items() if year * Y - Y <= k < year * Y]
            return statistics.mean(ts) if ts else 0.0

        def fs(year):  # 当年流量中创始者的占比
            Y = self.Y
            ks = [k for k in self.paid_flow if year * Y - Y <= k < year * Y]
            f = sum(self.paid_flow[k][0] for k in ks)
            a = sum(self.paid_flow[k][1] for k in ks)
            return f / a if a else 0.0

        def fs_stock():
            allp = sum(x.paid for x in self.agents)
            return sum(x.paid for x in self.agents if x.founder) / allp if allp else 0.0

        cap = p.e0 * p.emission_half_life / math.log(2)
        mk = [m / k for m, k in zip(self.m_hist, self.k_hist) if k > 0]
        Y = self.Y
        mk_tail = mk[3 * Y:] if len(mk) > 4 * Y else mk
        yearly = []
        for y in range(3, p.steps // Y):
            a0, a1 = self.m_hist[y * Y - 1], self.m_hist[min(len(self.m_hist) - 1, y * Y + Y - 1)]
            if a0 > 0:
                yearly.append((a1 - a0) / a0)
        kend = self.k_hist[-1] if self.k_hist else 0.0
        mon = {
            "mk_cv": statistics.pstdev(mk_tail) / statistics.mean(mk_tail) if mk_tail and statistics.mean(mk_tail) else float("nan"),
            "mk_end": (self.m_hist[-1] / kend) if kend else float("nan"),
            "infl_max": max((abs(x) for x in yearly), default=0.0),
            "infl_min": min(yearly, default=0.0),
            "k_end": kend,
            "k_peak": max(self.k_hist) if self.k_hist else 0.0,
            "m_end": self.m_hist[-1] if self.m_hist else 0.0,
            "kmal": (self.kmal_hist[-1] / kend) if kend else 0.0,
            "kmal_amp": self.k_amp(),
            "spiral": self.dormant_good_post / max(1, self.good_at_shock) if p.shock_step >= 0 else 0.0,
        }
        return mon | {
            "correct_share": inc_correct / inc_all,
            "mal_share": paid["mal"] / tot_paid,
            "mal_pop": n_mal / n_all,
            "ring_share": ring_inc / inc_all,
            "established_frac": len(est) / max(1, len(units)),
            "est_delay": statistics.mean(self.est_delay) / self.M if self.est_delay else float("nan"),  # 月
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


V3 = {"assigned": True, "share_exp": 0.5, "unit_cap": 5.0}
EL = {"assigned": True, "share_exp": 0.5, "elastic": True}
EL2 = EL | {"rent_tax": 0.10, "treasury_floor": 0.5}
ELASTIC = [
    ("fixed", "固定上限(上一版 v3:按半衰期增发 + 与活动挂钩)", V3),
    ("elastic", "弹性供应:锚定知识量,前馈跟随 + PI 纠偏,收缩每期 ≤0.5%", EL),
    ("elastic_noff", "弹性,无前馈(只靠 PI,纠偏每期 ≤0.5%)", EL | {"feedforward": False}),
    ("fixed_shock", "固定上限 + 第 5 年 30% 的已确立知识被证伪", V3 | {"shock_step": 60, "shock_frac": 0.3}),
    ("elastic_shock", "弹性 + 第 5 年 30% 的已确立知识被证伪", EL | {"shock_step": 60, "shock_frac": 0.3}),
    ("fixed_shrink", "固定上限 + 第 5 年起社区萎缩(无新人,活跃度降到 1/3)", V3 | {"shrink_step": 60, "shock_step": 60}),
    ("elastic_shrink", "弹性 + 第 5 年起社区萎缩", EL | {"shrink_step": 60, "shock_step": 60}),
    ("fixed_shock8", "固定上限 + 第 8 年(网络成熟后)50% 的已确立知识被证伪", V3 | {"shock_step": 96, "shock_frac": 0.5}),
    ("elastic_shock8", "弹性 + 第 8 年 50% 的已确立知识被证伪", EL | {"shock_step": 96, "shock_frac": 0.5}),
    ("elastic_shock8_hard", "弹性 + 第 8 年冲击,收缩不设上限(检验螺旋)", EL | {"shock_step": 96, "shock_frac": 0.5, "r_max": 1.0}),
    ("elastic_nofee", "弹性 + 外部付费不到来", EL | {"fee_max": 0.0}),
    ("elastic_mal20", "弹性 + 作恶者 20%(试图刷高知识量)", EL | {"frac_mal": 0.20}),
    ("fixed_mal20", "固定上限 + 作恶者 20%", V3 | {"frac_mal": 0.20}),
    ("elastic_fast", "弹性,调节过猛:增益 ×10、每期上限 5%", EL | {"kp": 1.0, "ki": 0.1, "r_max": 0.05}),
    ("elastic_nocap", "弹性,不设每期变化上限", EL | {"r_max": 1.0}),
    ("elastic_raw_k", "弹性,但知识量不平滑(取当期值)", EL | {"k_smooth": 1}),
    # 第五轮:存活看使用,不看钱;国库托底
    ("el2", "弹性 + 租金改为收入的 10%(税),休眠看使用 + 国库托底", EL | {"rent_tax": 0.10, "treasury_floor": 0.5}),
    ("el2_nofee", "el2 + 外部付费不到来", EL | {"rent_tax": 0.10, "treasury_floor": 0.5, "fee_max": 0.0}),
    ("el2_tax_only_nofee", "仅租金改税(无国库托底)+ 外部付费不到来", EL | {"rent_tax": 0.10, "fee_max": 0.0}),
    ("el2_shock8_hard", "el2 + 第 8 年 50% 知识被证伪,收缩不设上限", EL | {"rent_tax": 0.10, "treasury_floor": 0.5, "shock_step": 96, "shock_frac": 0.5, "r_max": 1.0}),
    ("el2_shock8", "el2 + 第 8 年 50% 知识被证伪", EL | {"rent_tax": 0.10, "treasury_floor": 0.5, "shock_step": 96, "shock_frac": 0.5}),
    ("el2_shrink", "el2 + 第 5 年起社区萎缩", EL | {"rent_tax": 0.10, "treasury_floor": 0.5, "shrink_step": 60, "shock_step": 60}),
    ("el2_mal20", "el2 + 作恶者 20%", EL | {"rent_tax": 0.10, "treasury_floor": 0.5, "frac_mal": 0.20}),
    ("fixed2_nofee", "固定上限 + 租金改税、休眠看使用 + 外部付费不到来", V3 | {"rent_tax": 0.10, "fee_max": 0.0}),
    # 第六轮:针对知识量的攻击(作恶者 20%)
    ("k_naive", "作恶者 20%,不针对知识量(对照)", EL2 | {"frac_mal": 0.20, "assigned": False}),
    ("k_pump", "作恶者 20% 追加通过,把自己人单元的可信度推到上限;无指派", EL2 | {"frac_mal": 0.20, "assigned": False, "mal_pump": True}),
    ("k_pump_cap", "同上 + 知识量中可信度 2 位净通过即饱和(上限 0.6)", EL2 | {"frac_mal": 0.20, "assigned": False, "mal_pump": True, "k_cred_cap": 0.6}),
    ("k_pump_assigned", "同上攻击 + 核验人随机指派(只有被指派者计入)", EL2 | {"frac_mal": 0.20, "mal_pump": True}),
    ("k_pump_both", "同上攻击 + 指派 + 饱和", EL2 | {"frac_mal": 0.20, "mal_pump": True, "k_cred_cap": 0.6}),
    ("k_pump_35", "作恶者 35% 追加通过,指派 + 饱和", EL2 | {"frac_mal": 0.35, "mal_pump": True, "k_cred_cap": 0.6}),
    ("k_honest_cap", "无攻击,仅饱和(检验饱和对诚实网络的代价)", EL2 | {"k_cred_cap": 0.6}),
]

# 第七轮:上链 v1 —— 换成天结算、只前馈、只锁不罚
V1 = {"assigned": True, "share_exp": 0.5, "elastic": True, "k_cred_cap": 0.6,
      "burn": False, "slashing": False, "rent": 0.0}  # v1 没有租金,也就没有"付不起租金就休眠"
V1_CHAIN = V1 | {"mapping": "chain"}
V1_TIME = V1 | {"mapping": "time"}
V1_HYB = V1 | {"mapping": "hybrid", "period_days": 1.0}
CHAIN = [
    # —— 推荐读法:天结算,时长按真实时间,追加指派每 3 天、见习期 7 天 ——
    ("v1", "上链 v1(推荐读法):天结算 + 只前馈 + 只锁不罚,无租金", V1_HYB),
    ("v1_slash", "v1 + 挑战成功时罚没锁定收益并限流(v1 缺的那块)", V1_HYB | {"slashing": True}),
    ("v1_nofee", "v1,外部付费不到来(测试网的实际情况)", V1_HYB | {"fee_max": 0.0}),
    ("v1_norevpay", "v1,核验报酬关", V1_HYB | {"split_review": 0.0, "split_pool": 0.85}),
    ("v1_noassign", "v1,核验人不由协议指派", V1_HYB | {"assigned": False}),
    ("v1_mal20", "v1 + 作恶者 20%", V1_HYB | {"frac_mal": 0.20}),
    ("v1_mal35", "v1 + 作恶者 35%", V1_HYB | {"frac_mal": 0.35}),
    ("v1_lazy50", "v1 + 走过场的核验者 50%", V1_HYB | {"frac_lazy": 0.50}),
    ("v1_pump", "v1 + 作恶者持续追加通过(刷知识量)", V1_HYB | {"frac_mal": 0.20, "mal_pump": True}),
    ("v1_slash_mal20", "v1 + 罚没 + 作恶者 20%", V1_HYB | {"slashing": True, "frac_mal": 0.20}),
    # —— 另外两种读法(对照) ——
    ("v1_literal", "字面读法:期就是天(锁定 6 天、基础层半衰期 120 天)", V1_CHAIN | {"period_days": 1.0}),
    ("v1_literal_slash", "字面读法 + 罚没", V1_CHAIN | {"period_days": 1.0, "slashing": True}),
    ("v1_time", "真实时间读法:全部按月标定的量折算(追加指派每 3 个月)", V1_TIME | {"period_days": 1.0}),
    # —— 月结算(与已发布结果可比) ——
    ("v1_month", "月结算 v1", V1_CHAIN | {"period_days": 30.0}),
    ("v1_month_slash", "月结算 v1 + 罚没", V1_CHAIN | {"period_days": 30.0, "slashing": True}),
]

# v1 关心的是行为:核验有没有人做、确立得了多少、钱有没有流向作恶者
V1COLS = [
    ("err_in_est", "已确立单元中的错误", "{:.3f}"),
    ("mal_share", "作恶者收入占比", "{:.3f}"),
    ("mal_pop", "作恶者人数占比", "{:.2f}"),
    ("established_frac", "单元确立比例", "{:.2f}"),
    ("est_delay", "确立耗时(月)", "{:.1f}"),
    ("f_review", "诚实者核验比例", "{:.2f}"),
    ("review_share", "核验收入占比", "{:.2f}"),
    ("mk_cv", "货币/知识量波动", "{:.3f}"),
    ("mk_end", "末期 货币/知识量(目标 200)", "{:.0f}"),
    ("k_end", "末期知识量", "{:.0f}"),
    ("kmal_amp", "作恶单元放大倍数", "{:.2f}"),
    ("dormant_good", "休眠的好单元", "{:.3f}"),
    ("founder_stock", "创始者持有累计收入", "{:.2f}"),
]

ECOLS = [
    ("mk_cv", "每单位知识的货币量波动(变异系数)", "{:.3f}"),
    ("mk_end", "末期 货币量/知识量", "{:.0f}"),
    ("infl_max", "年度供应变化最大幅度", "{:.3f}"),
    ("infl_min", "年度供应变化最低值", "{:+.3f}"),
    ("k_peak", "知识量峰值", "{:.0f}"),
    ("k_end", "末期知识量", "{:.0f}"),
    ("m_end", "末期货币量", "{:.0f}"),
    ("spiral", "冲击后被租金挤出的好单元", "{:.3f}"),
    ("kmal", "知识量中作恶单元占比", "{:.3f}"),
    ("kmal_amp", "作恶单元的知识量放大倍数", "{:.2f}"),
    ("mal_share", "作恶者收入占比", "{:.3f}"),
    ("mal_pop", "作恶者人数占比", "{:.2f}"),
    ("err_in_est", "已确立单元中的错误", "{:.3f}"),
    ("pur5", "第5年每单元奖励", "{:.2f}"),
    ("pur10", "第10年每单元奖励", "{:.2f}"),
    ("dormant_good", "休眠的好单元(全程)", "{:.3f}"),
    ("founder_y1", "创始者占当年收入 第1年", "{:.2f}"),
    ("founder_stock", "创始者持有累计收入", "{:.2f}"),
]


def _one(args):
    p, s = args
    return Sim(p, s).run()


def mean_metrics(ms: list) -> dict:
    out = {}
    for k in ms[0]:
        xs = [m[k] for m in ms if not (isinstance(m[k], float) and math.isnan(m[k]))]
        out[k] = statistics.mean(xs) if xs else float("nan")
    return out


def run_scenario(p: Params, seeds: int) -> dict:
    return mean_metrics([Sim(p, s).run() for s in range(seeds)])


def table(results, cols=None) -> str:
    cols = cols or COLS
    head = "| 场景 | 说明 | " + " | ".join(c[1] for c in cols) + " |"
    sep = "|---|---|" + "---|" * len(cols)
    rows = [f"| `{n}` | {d} | " + " | ".join(f.format(m[k]) for k, _, f in cols) + " |" for n, d, m in results]
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


ELASTIC_READING = """## 第四轮:弹性供应(锚定知识量)

知识量 K = Σ 已确立在册单元的 有效可信度 × min(√重要性, 3)。目标货币量 M* = k × K(取最近 3 期均值)。
每期按相对偏差做比例 + 积分调节:低于目标就增发;高于目标就停止增发,并依次销毁部分外部付费、提高租金(最多 3 倍)
且租金全部销毁。**只从资金流里销毁,不动任何账户余额。**每期供应变化不超过 0.5%。

- **每单位知识的货币量波动**:第 4 年起 M/K 的变异系数。越小,说明货币量越紧地跟着知识量走。
- **年度供应变化**:第 4 年起每年货币量的相对变化;负值即净销毁(通缩)。
- **冲击后被租金挤出的好单元**:冲击发生后,正确且被引用过的单元因付不起租金而休眠的比例。用来检验死亡螺旋:
  知识减少 → 收缩货币 → 租金升高 → 更多单元休眠 → 知识再减少。
- **知识量中作恶单元占比**:作恶者能把多少"刷出来的知识"算进锚定物。
- **作恶单元的知识量放大倍数**(第六轮):作恶单元在知识量中的份额 ÷ 其在已确立单元中的份额。
  1 表示每个作恶单元与诚实单元分量相同;大于 1 表示"可信度泵"(确立后继续互相追加通过)放大了分量。
  `k_*` 场景中追加通过是作恶者的**额外**行动,不占用写与核验的预算,以便单独衡量放大效应。"""


V1_READING = """## 第七轮:上链 v1

把结算周期从"月"换成"天"以后,仿真里所有按月标定的参数怎么读,有两种读法:

- **链的字面读法**(`mapping="chain"`):设计稿里的"期"就是结算期(天)。锁定期 6 期 = 6 天,
  重要性半衰期 基础 120 期 = 120 天。见习期 7 期(链上就是 7 天)。
- **真实时间读法**(`mapping="time"`):所有按月标定的量折算成同样的真实时间(锁定期 6 期 = 6 个月)。
  这是"换个周期但行为不变"的对照。

两种读法回答同一个问题("换成天结算后结论还成立吗"),但对"期"的理解不同,结果不同是预期的。
**选哪个是产品决定,不是技术决定**——见设计稿 `docs/design/knowledge-economy-v1.md` §4。

另外三个 v1 的简化:只前馈(不销毁、不收缩)、只锁不罚(没有挑战与罚没)、可信度计入上限 0.6。

- **每单位知识的货币量波动**:变异系数越小,货币量越紧地跟着知识量走。
- **已确立单元中的错误**:这是质量的核心指标。
- **核验收入占比**:核验报酬在总发放里的份额。`v1_day_norevpay` 是它的对照。
- **诚实者核验比例**:社区愿不愿意做核验。"""

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", type=int, default=4)
    ap.add_argument("--steps", type=int, default=120)
    ap.add_argument("--md", action="store_true")
    ap.add_argument("--only", help="逗号分隔的场景名")
    ap.add_argument("--jobs", type=int, default=1, help="并行进程数(结果与串行相同)")
    ap.add_argument("--suite", choices=("rules", "elastic", "v1", "all"), default="all")
    a = ap.parse_args()
    base = Params(steps=a.steps)
    only = set(a.only.split(",")) if a.only else None

    def run(scen):
        todo = [(n, d, replace(base, **o)) for n, d, o in scen if not only or n in only]
        if a.jobs <= 1:
            out = []
            for name, desc, p in todo:
                out.append((name, desc, run_scenario(p, a.seeds)))
                print(f"done {name}", flush=True)
            return out
        # 并行:每个(场景, 种子)是一个独立任务;各自有固定种子,结果与串行完全相同
        from concurrent.futures import ProcessPoolExecutor
        jobs = [(p, s) for _, _, p in todo for s in range(a.seeds)]
        with ProcessPoolExecutor(max_workers=a.jobs) as ex:
            res = list(ex.map(_one, jobs))
        out = []
        for i, (name, desc, _) in enumerate(todo):
            out.append((name, desc, mean_metrics(res[i * a.seeds:(i + 1) * a.seeds])))
            print(f"done {name}", flush=True)
        return out

    rules = run(SCENARIOS) if a.suite in ("rules", "all") else []
    el = run(ELASTIC) if a.suite in ("elastic", "all") else []
    v1 = run(CHAIN) if a.suite in ("v1", "all") else []
    t1 = table(rules) if rules else ""
    t2 = table(el, ECOLS) if el else ""
    t3 = table(v1, V1COLS) if v1 else ""
    print(t1 + "\n\n" + t2 + ("\n\n" + t3 if t3 else ""))
    if a.md:
        with open("sim/ECONOMY.md", "w", encoding="utf-8") as f:
            f.write("<!-- 由 sim/economy.py --md 自动生成,勿手改 -->\n")
            f.write(f"# 知识经济仿真结果({a.seeds} 种子均值,{a.steps} 期 = {a.steps // 12} 年)\n\n")
            f.write(READING + "\n\n")
            f.write("## 第一至三轮:分配规则\n\n" + t1 + "\n\n")
            f.write(ELASTIC_READING + "\n\n" + t2 + "\n")
            if v1:
                f.write("\n## 第七轮:上链 v1(天结算、只前馈、只锁不罚)\n\n" + V1_READING + "\n\n" + t3 + "\n")


if __name__ == "__main__":
    main()
