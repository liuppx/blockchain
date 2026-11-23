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
/// 追加指派的间隔:3 天(测试网约 1 块/秒)。设计稿 §4.1 的 hybrid 读法——链上的实际规则。
pub const TOPUP_INTERVAL_BLOCKS: u64 = 3 * 86_400;

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
}

/// 知识经济的全部链上状态。H\* 之前为空。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EconomyState {
    /// 每个 (单元, 版本) 的指派与确立。只记 H\* 及以后发布的版本。
    pub versions: BTreeMap<(u64, u32), VersionEconomy>,
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

    #[test]
    fn establishment_needs_two_passes_and_a_majority() {
        assert!(!meets_establishment(1, 0), "1 位不够");
        assert!(meets_establishment(2, 0));
        assert!(meets_establishment(2, 1));
        assert!(!meets_establishment(2, 2), "通过必须多于需修改");
        assert!(meets_establishment(3, 2));
    }
}
