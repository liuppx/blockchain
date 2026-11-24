//! 知识经济上链 v1 的链上状态与规则(`docs/design/knowledge-economy-v1.md`)。
//!
//! 只在升级 `knowledge-economy-v1` 激活(高度 ≥ H\*)以后才有内容、才进 `state_root`;
//! H\* 之前它始终为空,整条链与升级前逐字节相同(见 `upgrade` 模块)。
//!
//! 这一步(S2a)落两件后面所有规则都依赖的事:
//!
//! 1. **不可逆的确立事件**:一个版本第一次达到"至少 2 位**被指派且过了见习期**的核验人通过,
//!    且通过多于需修改"时,记下确立高度与促成确立的核验人。之后结论再怎么变,确立都不撤销
//!    (设计稿 §4 决定 #4:否则结算会被反复重算)。
//! 2. **协议随机指派核验人**:版本发布时,用 `sha256(上一块哈希 ‖ 单元 ‖ 版本)` 作种子,从
//!    **发布那一刻**过了见习期、且不是发布者的成员里抽 4 位;凑不够 2 位净通过时每 3 天追加 1 位,
//!    最多 8 位。只有被指派者的结论计入确立(设计稿 §2.4)。
//!
//! 诚实边界:随机源是上一块的哈希,出块人能在一定范围内左右它。指派**降低**串通,不能**消除**串通。
use std::collections::{BTreeMap, BTreeSet};

use crate::hash::sha256;
use crate::Hash;

/// 每个版本首次指派的核验人数。
pub const ASSIGN_INITIAL: usize = 4;
/// 每个版本最多指派的核验人数(含追加)。
pub const ASSIGN_MAX: usize = 8;
/// 确立所需的、被指派且计入的"通过"人数下限。
pub const ESTABLISH_MIN_PASSES: u32 = 2;
// 追加指派的间隔、结算周期、锁定期都在 `upgrade::EconomyCadence` 里(可在 [[upgrades]] 覆盖),
// 那里是唯一的来源;这里不再重复定义,免得两处数字漂移。

// ---- 重要性(S2b)--------------------------------------------------------------

/// 重要性的定点表示:Q32(1.0 = 2^32)。
pub type Importance = u64;

/// 各层级的半衰期(区块数)。测试网约 1 块/秒:
/// 基础 3600 天、解释 1440 天、场景 720 天、应用 360 天。
///
/// **为什么是这些数**:仿真的期 = 月,链上的期 = 天。设计稿 §4.1 的 hybrid 读法把时长按真实时间换算,
/// 所以基础层仍是 10 年(120 个月),而不是字面读法的 120 天。字面读法实测有可测量的坏处
/// (货币偏离目标 72%、作恶单元放大 1.48 倍),见设计稿 §5.1。
pub const IMPORTANCE_HALF_LIFE_BLOCKS: [u64; 4] = [
    3600 * 86_400, // 基础:10 年
    1440 * 86_400, // 解释:4 年
    720 * 86_400,  // 场景:2 年
    360 * 86_400,  // 应用:1 年
];

/// 一次"参见"引用加的重要性:0.25(Q32)。
pub const SEE_ALSO_WEIGHT: Importance = 1 << 30;

/// 2^(-1/2^i),Q32,i = 1..=24。**用整数算分数次幂**:共识规则不能用 `powf`——
/// 浮点结果不保证在不同机器上逐位相同,那会让不同节点算出不同的重要性。
/// 表由 60 位精度的十进制运算四舍五入到 Q32 得到(见 `pow_neg_frac_table_is_exact` 测试)。
const POW_NEG_FRAC: [u64; 24] = [
    3037000500, 3611622603, 3938502376, 4112874773, 4202935003, 4248701965, 4271771996, 4283353945,
    4289156690, 4292061010, 4293513907, 4294240540, 4294603903, 4294785595, 4294876445, 4294921870,
    4294944583, 4294955939, 4294961618, 4294964457, 4294965876, 4294966586, 4294966941, 4294967119,
];

/// 2^(-x) 的 Q32 值,其中 x = num/den ∈ [0, ∞)。**全部整数运算**,所有节点结果逐位相同。
///
/// 做法:整数部分 q = ⌊x⌋ 用移位;小数部分按二进制位展开 x 的前 24 位,
/// 第 i 位为 1 就乘上 2^(-1/2^i)(查表)。截断在第 24 位,相对误差 < 2^-24·ln2 ≈ 4e-8,
/// 对"重要性"这个量绰绰有余。
pub fn pow2_neg_q32(num: u64, den: u64) -> u64 {
    const ONE: u64 = 1 << 32;
    if num == 0 || den == 0 {
        return ONE;
    }
    let q = num / den;
    if q >= 32 {
        return 0; // 2^-32 以下,Q32 里就是 0
    }
    let mut out: u64 = ONE >> q;
    let mut rem = num % den;
    for &f in POW_NEG_FRAC.iter() {
        if rem == 0 {
            break;
        }
        // 小数的下一位:rem·2 / den
        let twice = (rem as u128) * 2;
        if twice >= den as u128 {
            out = ((out as u128 * f as u128) >> 32) as u64;
            rem = (twice - den as u128) as u64;
        } else {
            rem = twice as u64;
        }
    }
    out
}

/// 一个单元的重要性累计。**只记最后一次被更新时的值与高度**,衰减在读取时惰性计算
/// (设计稿 §3:链上不存全量重算的结果)。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ImportanceAcc {
    /// 累计的重要性(Q32),截至 `last_height`。
    pub value: Importance,
    /// 上次更新时的高度。
    pub last_height: u64,
}

/// 重要性计算:在 `height` 时刻的累计值(惰性衰减)。
pub fn importance_at(acc: &ImportanceAcc, half_life_blocks: u64, height: u64) -> Importance {
    if height <= acc.last_height {
        return acc.value;
    }
    let d = pow2_neg_q32(height - acc.last_height, half_life_blocks);
    ((acc.value as u128 * d as u128) >> 32) as u64
}

/// 给 `acc` 加 `w`:先把旧值衰减到 `height`,再加上 `w`,记下新的高度。
pub fn bump(acc: &mut ImportanceAcc, w: Importance, half_life_blocks: u64, height: u64) {
    acc.value = importance_at(acc, half_life_blocks, height).saturating_add(w);
    acc.last_height = height;
}

/// 一次"依据"引用加的重要性:1.0(Q32)。
pub const BASIS_WEIGHT: Importance = 1 << 32;
/// 确立时的起始重要性 I0 = 1.0(`sim/economy.py` `i0=1.0`)。
pub const ESTABLISH_IMPORTANCE: Importance = 1 << 32;

/// 一个版本的指派与确立状态。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct VersionEconomy {
    /// 被指派的核验人(按指派顺序;追加的排在后面)。
    pub assigned: Vec<u64>,
    /// 最近一次指派(首次或追加)的高度,用来决定何时再追加。
    pub last_assigned_height: u64,
    /// 确立高度;`None` 表示尚未确立。**一旦写入永不清除。**
    pub established_height: Option<u64>,
    /// 促成确立的核验人(确立那一刻计入的"通过"者),之后不变。
    pub approvers: Vec<u64>,
    /// 「需修改」一致认可(≥2 位)的报酬是否已记过(只记一次)。
    pub needs_paid: bool,
}

/// 知识经济的全部链上状态。H\* 之前为空。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EconomyState {
    /// 每个 (单元, 版本) 的指派与确立。只记 H\* 及以后发布的版本。
    pub versions: BTreeMap<(u64, u32), VersionEconomy>,
    /// 每个单元的重要性(按单元,不按版本:被引用的是"这条知识",版本只是它的修订)。
    /// 只在单元有过一次确立、或被已确立的他人单元引用时才出现。
    pub importance: BTreeMap<u64, ImportanceAcc>,
    /// 每个账户的核验得分(累计)。见 [`EconomyState::grant_credits`]。
    pub review_scores: BTreeMap<u64, u64>,
    /// 每个账户的锁定账本:已锁定的收益(还没到释放期)、释放期。
    /// 测试网阶段只记积分、不发币(设计稿决定 #7),但账本先跑起来,主网创世时按主网规则重算。
    pub locks: BTreeMap<u64, LockLedger>,
    /// 结算状态(期号、知识量历史、累计增发、国库)。
    pub period: PeriodState,
}

// ---- 结算(S3)------------------------------------------------------------------

/// 每单位知识量对应的增发(µCOG)。仿真里 `k_money=200`(抽象单位),这里按 1 COG = 10^6 µCOG。
pub const K_MONEY: u64 = 200 * 1_000_000;
/// 分成(万分比):知识池 60% / 核验池 25% / 其余 15% 进国库。
pub const SPLIT_POOL_BPS: u64 = 6000;
pub const SPLIT_REVIEW_BPS: u64 = 2500;
/// 知识池里沿"依据"上溯的比例(万分比):20%。
pub const UPSTREAM_BPS: u64 = 2000;
/// 知识量里可信度的计入上限(万分比):2 位净通过即饱和(0.6)。
pub const K_CRED_CAP_BPS: u64 = 6000;
/// 知识量里 √重要性的上限(Q32):3.0。
pub const K_IMP_CAP_Q32: u64 = 3 << 32;
/// 知识量的滑动窗口(期数):3。
pub const K_SMOOTH: usize = 3;

/// 结算状态。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PeriodState {
    /// 已完成的结算次数。
    pub settled: u64,
    /// 最近若干期的知识量 K(Q32);够 2×K_SMOOTH 期才发第一次。
    pub k_history: Vec<u64>,
    /// 累计增发(µCOG)。
    pub total_issued: u64,
    /// 累计进国库(µCOG)。
    pub treasury: u64,
}

/// 一个账户的积分账本(测试网只记积分、不发币,设计稿决定 #7)。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PointLedger {
    /// 累计获得。
    pub earned: u64,
    /// 锁定中(还没到释放期)。
    pub locked: u64,
    /// 已释放。
    pub released: u64,
    /// 释放高度。
    pub release_height: u64,
}

/// 按权重分配一个池子:返回每项的份额,以及没分完的零头(归国库)。
///
/// **全程向下取整**,中间用 u128——绝不凭空造钱:分出去的总数不会超过池子。
pub fn distribute_by_weight(pool: u64, weights: &[u64]) -> (Vec<u64>, u64) {
    let sum: u128 = weights.iter().map(|w| *w as u128).sum();
    if sum == 0 {
        return (vec![0; weights.len()], pool);
    }
    let mut out = Vec::with_capacity(weights.len());
    let mut used: u128 = 0;
    for w in weights {
        let share = ((pool as u128) * (*w as u128) / sum) as u64;
        used += share as u128;
        out.push(share);
    }
    (out, (pool as u128 - used) as u64)
}

/// Q32 开平方:√(x / 2^32) · 2^32 = isqrt(x << 32)。整数牛顿法,不用浮点。
pub fn sqrt_q32(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    isqrt_u128((x as u128) << 32) as u64
}

fn isqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// 一个账户的锁定账本。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LockLedger {
    /// 尚未释放的锁定额(以"分"为单位,定点)。
    pub locked: u64,
    /// 下一笔释放的高度。
    pub next_release_height: u64,
    /// 累计已释放。
    pub released: u64,
    /// 累计获得(含仍在锁定的)。
    pub earned: u64,
}

impl EconomyState {
    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    /// 已确立的版本数(给 RPC 与测试用)。
    pub fn established_count(&self) -> usize {
        self.versions
            .values()
            .filter(|v| v.established_height.is_some())
            .count()
    }

    /// 给核验人记 1 分。
    pub fn grant_credit(&mut self, verifier: u64) {
        *self.review_scores.entry(verifier).or_insert(0) += 1;
    }

    /// 把账户的得分清零(结算时把得分转成收益之后调用;仿真里每期结算后清空 `review_credits`)。
    pub fn take_credits(&mut self, verifier: u64) -> u64 {
        self.review_scores.get(&verifier).copied().unwrap_or(0)
    }

    /// 记一笔收益:累计 + 锁定,释放期取最晚的那一笔。
    pub fn credit_locked(&mut self, who: u64, amount: u64, release_height: u64) {
        if amount == 0 {
            return;
        }
        let led = self.locks.entry(who).or_default();
        led.earned = led.earned.saturating_add(amount);
        led.locked = led.locked.saturating_add(amount);
        led.next_release_height = led.next_release_height.max(release_height);
    }

    /// 释放到期锁定的收益,返回释放的数额。
    pub fn release_locks(&mut self, height: u64) -> u64 {
        let mut released = 0u64;
        for led in self.locks.values_mut() {
            if led.locked > 0 && height >= led.next_release_height {
                released = released.saturating_add(led.locked);
                led.released = led.released.saturating_add(led.locked);
                led.locked = 0;
            }
        }
        if released > 0 {
            // 清理已释放完的账户,免得账本无限增长
            self.locks.retain(|_, l| l.locked > 0 || l.released > 0);
        }
        released
    }
}

/// 从 `pool` 里按种子确定性地抽 `k` 位(不放回)。`pool` 必须已排序、无重复——
/// 所有节点拿同样的有序候选与同样的种子,得到同样的结果。
pub fn draw(pool: &[u64], seed: &Hash, k: usize) -> Vec<u64> {
    let mut rest: Vec<u64> = pool.to_vec();
    let mut out = Vec::with_capacity(k.min(rest.len()));
    let mut ctr: u64 = 0;
    while out.len() < k && !rest.is_empty() {
        let mut pre = Vec::with_capacity(40);
        pre.extend_from_slice(seed);
        pre.extend_from_slice(&ctr.to_be_bytes());
        let h = sha256(&pre);
        let r = u64::from_be_bytes(h[..8].try_into().expect("8 bytes"));
        // 取模偏差:候选只有几十到几千人,相对 2^64 可以忽略;确定性才是这里的要求。
        let i = (r % rest.len() as u64) as usize;
        out.push(rest.remove(i));
        ctr += 1;
    }
    out
}

/// 指派种子:上一块哈希 ‖ 单元 ‖ 版本 ‖ 第几次指派(0 = 首次)。
pub fn assign_seed(prev_hash: &Hash, unit: u64, version: u32, round: u32) -> Hash {
    let mut pre = Vec::with_capacity(32 + 8 + 4 + 4 + 16);
    pre.extend_from_slice(b"kdg-assign-v1");
    pre.extend_from_slice(prev_hash);
    pre.extend_from_slice(&unit.to_be_bytes());
    pre.extend_from_slice(&version.to_be_bytes());
    pre.extend_from_slice(&round.to_be_bytes());
    sha256(&pre)
}

/// 计入确立的"通过 / 需修改"人数:只算**被指派**的人,且核验时已过见习期、不是发布者。
/// `verdicts` 是 (核验人, 是否通过, 核验高度);`established_at(id, h)` 判断见习期。
pub fn counted<F>(
    assigned: &[u64],
    publisher: u64,
    verdicts: &[(u64, bool, u64)],
    established_at: F,
) -> (Vec<u64>, u32)
where
    F: Fn(u64, u64) -> bool,
{
    let assigned: BTreeSet<u64> = assigned.iter().copied().collect();
    let mut passes = Vec::new();
    let mut needs = 0u32;
    for &(who, pass, h) in verdicts {
        if who == publisher || !assigned.contains(&who) || !established_at(who, h) {
            continue;
        }
        if pass {
            passes.push(who);
        } else {
            needs += 1;
        }
    }
    (passes, needs)
}

/// 是否满足确立条件(至少 2 位计入的通过,且通过多于需修改)。
pub fn meets_establishment(passes: usize, needs: u32) -> bool {
    passes as u32 >= ESTABLISH_MIN_PASSES && passes as u32 > needs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_is_deterministic_and_without_replacement() {
        let pool: Vec<u64> = (1..=10).collect();
        let s = assign_seed(&[7u8; 32], 3, 1, 0);
        let a = draw(&pool, &s, 4);
        assert_eq!(a, draw(&pool, &s, 4), "同样的候选与种子 ⇒ 同样的结果");
        assert_eq!(a.len(), 4);
        let set: BTreeSet<_> = a.iter().collect();
        assert_eq!(set.len(), 4, "不放回");
        assert!(a.iter().all(|x| pool.contains(x)));
        // 不同的版本、不同的轮次 ⇒ 不同的种子
        assert_ne!(s, assign_seed(&[7u8; 32], 3, 2, 0));
        assert_ne!(s, assign_seed(&[7u8; 32], 3, 1, 1));
    }

    #[test]
    fn draw_takes_everyone_when_the_pool_is_small() {
        let pool = vec![2, 5, 9];
        let got = draw(&pool, &[1u8; 32], 4);
        assert_eq!(got.len(), 3, "候选不足 4 人就全员指派");
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(sorted, pool);
        assert!(draw(&[], &[1u8; 32], 4).is_empty());
    }

    #[test]
    fn draw_spreads_over_the_pool() {
        // 粗检均匀性:大量种子下,每个候选被抽中的次数不应偏得离谱。
        let pool: Vec<u64> = (1..=8).collect();
        let mut hits = [0u32; 8];
        for i in 0..4000u32 {
            for x in draw(&pool, &assign_seed(&[0u8; 32], i as u64, 1, 0), 4) {
                hits[(x - 1) as usize] += 1;
            }
        }
        // 期望每人 2000 次
        assert!(hits.iter().all(|&h| (1800..=2200).contains(&h)), "{hits:?}");
    }

    #[test]
    fn only_assigned_established_non_publishers_count() {
        let assigned = [11, 12, 13, 14];
        let verdicts = [
            (11, true, 100),
            (12, true, 100),
            (99, true, 100), // 没被指派
            (13, false, 100),
            (14, true, 5),  // 见习期内
            (7, true, 100), // 发布者
        ];
        let est = |who: u64, h: u64| !(who == 14 && h < 50);
        let (p, n) = counted(&assigned, 7, &verdicts, est);
        assert_eq!(p, vec![11, 12]);
        assert_eq!(n, 1);
        assert!(meets_establishment(p.len(), n));
    }

    const ONE: u64 = 1 << 32;

    #[test]
    fn pow_neg_frac_table_is_exact() {
        // 表的每一项都应是 2^(-1/2^i)·2^32 四舍五入的结果。用 f64 复核(f64 有 53 位精度,
        // 足够判断 Q32 的取整),**只在测试里用浮点**,共识代码不碰浮点。
        for (k, &v) in POW_NEG_FRAC.iter().enumerate() {
            let i = (k + 1) as i32;
            let exact = 2f64.powf(-1.0 / 2f64.powi(i)) * ONE as f64;
            assert!(
                (v as f64 - exact).abs() <= 0.5 + 1e-6,
                "i={i}: {v} vs {exact}"
            );
        }
    }

    #[test]
    fn integer_decay_matches_the_real_value() {
        let cases = [
            (0u64, 7u64),
            (1, 2),
            (1, 1),
            (3, 2),
            (5, 4),
            (7, 3),
            (100, 99),
            (12345, 100_000),
        ];
        for (num, den) in cases {
            let got = pow2_neg_q32(num, den) as f64 / ONE as f64;
            let want = 2f64.powf(-(num as f64) / den as f64);
            assert!(
                (got - want).abs() < 1e-6,
                "2^-({num}/{den}): {got} vs {want}"
            );
        }
        assert_eq!(pow2_neg_q32(0, 5), ONE);
        assert_eq!(pow2_neg_q32(5, 5), ONE / 2, "整半衰期精确");
        assert_eq!(pow2_neg_q32(10, 5), ONE / 4);
        assert_eq!(pow2_neg_q32(32 * 5, 5), 0, "衰减到 Q32 以下就是 0");
    }

    #[test]
    fn importance_decays_lazily_and_bumps_add_on_top() {
        let hl = 1000;
        let mut acc = ImportanceAcc::default();
        bump(&mut acc, BASIS_WEIGHT, hl, 100);
        assert_eq!(importance_at(&acc, hl, 100), ONE);
        assert_eq!(importance_at(&acc, hl, 1100), ONE / 2, "过一个半衰期减半");
        bump(&mut acc, SEE_ALSO_WEIGHT, hl, 1100);
        assert_eq!(acc.value, ONE / 2 + ONE / 4, "先衰减,再加");
        assert_eq!(acc.last_height, 1100);
        // 惰性:不读就不变
        assert_eq!(acc.value, ONE / 2 + ONE / 4);
    }

    #[test]
    fn half_lives_follow_the_hybrid_reading() {
        let day = 86_400;
        assert_eq!(
            IMPORTANCE_HALF_LIFE_BLOCKS,
            [3600 * day, 1440 * day, 720 * day, 360 * day]
        );
    }

    #[test]
    fn establishment_needs_two_passes_and_a_majority() {
        assert!(!meets_establishment(1, 0), "1 位不够");
        assert!(meets_establishment(2, 0));
        assert!(meets_establishment(2, 1));
        assert!(!meets_establishment(2, 2), "通过必须多于需修改");
        assert!(meets_establishment(3, 2));
    }
}
