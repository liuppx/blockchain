//! 按高度激活的协议升级(`docs/design/rolling-upgrade.md` §9 的 S3,最小形态)。
//!
//! 一条已在运行的链要换规则时,不能重开链,也不能让历史区块换一种算法重放——
//! 每个区块头都承诺了它的 `state_root`,重放时逐块核对(`Chain::apply_block`)。
//! 所以新规则只能**从某个高度 H\* 起**生效:H\* 之前的每一个字节都与升级前相同,H\* 起才换。
//!
//! ## 时刻表放在哪
//!
//! 时刻表写在创世文件的 `[[upgrades]]` 里,但**不进创世哈希**:它是"这条链从哪个高度起换规则"的约定,
//! 不是创世状态。这样已在运行的链加一条升级,链的身份(创世哈希)不变,已有的区块与证书照旧有效。
//!
//! 代价是:**创世哈希不能用来核对各节点的时刻表是否一致**。所以:
//! - `node genesis-hash` 把时刻表一起打印出来(运维带外核对,和创世哈希一样);
//! - 从 H\* 起,`state_root` 折入"哪些升级已生效"——时刻表不一致的节点在 H\* 当块就算出不同的状态根,
//!   拿不到 2/3 签名、停在 H\*,**而不是悄悄分叉**。
//!
//! ## 对旧版本节点
//!
//! 旧版本不认识 `[[upgrades]]`(创世文件解析不拒绝未知字段),会照旧规则执行,于是在 H\* 停下。
//! 这是升级流程的一部分:**H\* 之前所有验证节点都要升级**(rolling-upgrade §10 的风险条目)。
//! 新版本反过来**拒绝不认识的升级名**,避免将来再出现"静默忽略"。
use std::collections::BTreeMap;
use std::fmt;

/// 已知的协议升级。每个升级有一个**永不复用**的标签(折入状态根)与一个创世文件里用的名字。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Upgrade {
    /// 知识经济上链 v1(`docs/design/knowledge-economy-v1.md`)。
    KnowledgeEconomyV1,
    /// 新增领域:教育(`docs/design/knowledge-economy-v1.md` 之外的独立改动)。
    ///
    /// 领域登记在**创世**里(`[knowledge] domains`),而创世哈希是链的身份。给已在运行的链
    /// 加领域,只能按高度激活:从 H\* 起,领域集合里多一个编号。
    AddEducationDomain,
}

impl Upgrade {
    /// 全部已知升级(按标签递增)。
    pub const ALL: [Upgrade; 2] = [Upgrade::KnowledgeEconomyV1, Upgrade::AddEducationDomain];

    /// 创世文件 `[[upgrades]] name = "…"` 里用的名字。
    pub fn name(self) -> &'static str {
        match self {
            Upgrade::KnowledgeEconomyV1 => "knowledge-economy-v1",
            Upgrade::AddEducationDomain => "add-education-domain",
        }
    }

    /// 折入状态根的标签。**一旦发布就永不改、永不复用**。
    pub fn tag(self) -> u8 {
        match self {
            Upgrade::KnowledgeEconomyV1 => 1,
            Upgrade::AddEducationDomain => 2,
        }
    }

    pub fn from_name(name: &str) -> Option<Upgrade> {
        Self::ALL.iter().copied().find(|u| u.name() == name)
    }
}

impl fmt::Display for Upgrade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpgradeError {
    /// 创世文件里写了这个版本不认识的升级名。
    Unknown(String),
    /// 同一个升级写了两次。
    Duplicate(Upgrade),
    /// 节奏参数里有 0。
    BadCadence,
}

impl fmt::Display for UpgradeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpgradeError::Unknown(n) => {
                let known: Vec<&str> = Upgrade::ALL.iter().map(|u| u.name()).collect();
                write!(
                    f,
                    "unknown upgrade {n:?} (this build knows: {})",
                    known.join(", ")
                )
            }
            UpgradeError::Duplicate(u) => write!(f, "upgrade {u} is scheduled twice"),
            UpgradeError::BadCadence => {
                write!(
                    f,
                    "economy cadence (period/lock/topup blocks) must all be > 0"
                )
            }
        }
    }
}

/// 知识经济 v1 的节奏参数(结算周期、锁定期、追加指派间隔),单位是区块数。
///
/// 默认值是设计稿的取值(测试网约 1 块/秒:结算 1 天、锁定 180 天、追加 3 天)。
/// 可以在 `[[upgrades]]` 里覆盖——本地演练用短周期,几分钟就能走完一轮结算。
/// 它和时刻表一样**不进创世哈希**,但从 H* 起折入 `state_root`:节奏不一致的节点在 H* 当块停下,不分叉。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EconomyCadence {
    pub period_blocks: u64,
    pub lock_blocks: u64,
    pub topup_blocks: u64,
}

impl Default for EconomyCadence {
    fn default() -> Self {
        EconomyCadence {
            period_blocks: 86_400,
            lock_blocks: 180 * 86_400,
            topup_blocks: 3 * 86_400,
        }
    }
}

/// 升级时刻表:每个升级从哪个高度起生效。空表 = 没有任何升级,行为与引入本模块之前逐字节相同。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpgradeSchedule {
    heights: BTreeMap<Upgrade, u64>,
    /// 知识经济 v1 的节奏(未设置时用默认值)。
    cadence: EconomyCadence,
}

impl UpgradeSchedule {
    /// 从 `(名字, 高度)` 列表建表;不认识的名字、重复的升级都报错。
    pub fn from_entries<'a>(
        entries: impl IntoIterator<Item = (&'a str, u64)>,
    ) -> Result<Self, UpgradeError> {
        let mut heights = BTreeMap::new();
        for (name, h) in entries {
            let u =
                Upgrade::from_name(name).ok_or_else(|| UpgradeError::Unknown(name.to_string()))?;
            if heights.insert(u, h).is_some() {
                return Err(UpgradeError::Duplicate(u));
            }
        }
        Ok(UpgradeSchedule {
            heights,
            cadence: EconomyCadence::default(),
        })
    }

    /// 覆盖知识经济的节奏。0 值无效(会让结算每块都跑、或锁定立即到期),拒绝。
    pub fn with_cadence(mut self, c: EconomyCadence) -> Result<Self, UpgradeError> {
        if c.period_blocks == 0 || c.lock_blocks == 0 || c.topup_blocks == 0 {
            return Err(UpgradeError::BadCadence);
        }
        self.cadence = c;
        Ok(self)
    }

    /// 知识经济的节奏。
    pub fn cadence(&self) -> EconomyCadence {
        self.cadence
    }

    /// 直接给定(测试与内部使用)。
    pub fn with(mut self, u: Upgrade, height: u64) -> Self {
        self.heights.insert(u, height);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// 某升级的激活高度;没排期则为 `None`。
    pub fn activation(&self, u: Upgrade) -> Option<u64> {
        self.heights.get(&u).copied()
    }

    /// 高度为 `height` 的区块是否按升级后的规则执行(`height >= H*`)。
    pub fn is_active(&self, u: Upgrade, height: u64) -> bool {
        self.activation(u).is_some_and(|h| height >= h)
    }

    /// 在 `height` 已生效的升级(按标签递增),附激活高度。
    pub fn active_at(&self, height: u64) -> impl Iterator<Item = (Upgrade, u64)> + '_ {
        self.heights
            .iter()
            .filter(move |(_, h)| height >= **h)
            .map(|(u, h)| (*u, *h))
    }

    /// 全部排期(按标签递增)。
    pub fn entries(&self) -> impl Iterator<Item = (Upgrade, u64)> + '_ {
        self.heights.iter().map(|(u, h)| (*u, *h))
    }

    /// 给运维核对用的一行文字,例如 `knowledge-economy-v1@1100000`;空表为 `none`。
    /// 节奏不是默认值时一并打印(`period=…,lock=…,topup=…`),便于带外核对。
    pub fn describe(&self) -> String {
        if self.is_empty() {
            return "none".to_string();
        }
        let mut out = self
            .entries()
            .map(|(u, h)| format!("{u}@{h}"))
            .collect::<Vec<_>>()
            .join(",");
        if self.cadence != EconomyCadence::default() {
            let c = self.cadence;
            out.push_str(&format!(
                " (period={},lock={},topup={})",
                c.period_blocks, c.lock_blocks, c.topup_blocks
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_tags_are_stable() {
        // 标签折入状态根:改了就等于换了一条链。这条测试防止误改。
        assert_eq!(Upgrade::KnowledgeEconomyV1.name(), "knowledge-economy-v1");
        assert_eq!(Upgrade::KnowledgeEconomyV1.tag(), 1);
        assert_eq!(
            Upgrade::from_name("knowledge-economy-v1"),
            Some(Upgrade::KnowledgeEconomyV1)
        );
    }

    #[test]
    fn tags_are_unique() {
        let mut tags: Vec<u8> = Upgrade::ALL.iter().map(|u| u.tag()).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), Upgrade::ALL.len());
    }

    #[test]
    fn activation_is_inclusive_of_the_scheduled_height() {
        let s = UpgradeSchedule::default().with(Upgrade::KnowledgeEconomyV1, 100);
        assert!(!s.is_active(Upgrade::KnowledgeEconomyV1, 99));
        assert!(
            s.is_active(Upgrade::KnowledgeEconomyV1, 100),
            "H* 当块就按新规则执行"
        );
        assert!(s.is_active(Upgrade::KnowledgeEconomyV1, 101));
        assert_eq!(s.active_at(99).count(), 0);
        assert_eq!(
            s.active_at(100).collect::<Vec<_>>(),
            vec![(Upgrade::KnowledgeEconomyV1, 100)]
        );
    }

    #[test]
    fn empty_schedule_activates_nothing() {
        let s = UpgradeSchedule::default();
        assert!(s.is_empty());
        assert!(!s.is_active(Upgrade::KnowledgeEconomyV1, u64::MAX));
        assert_eq!(s.describe(), "none");
    }

    #[test]
    fn cadence_defaults_to_the_design_values_and_can_be_overridden() {
        let s = UpgradeSchedule::default().with(Upgrade::KnowledgeEconomyV1, 9);
        assert_eq!(s.cadence(), EconomyCadence::default());
        assert_eq!(s.cadence().period_blocks, 86_400, "默认 1 天");
        assert_eq!(s.describe(), "knowledge-economy-v1@9", "默认节奏不打印");
        let fast = EconomyCadence {
            period_blocks: 10,
            lock_blocks: 30,
            topup_blocks: 5,
        };
        let s = s.with_cadence(fast).unwrap();
        assert_eq!(s.cadence(), fast);
        assert_eq!(
            s.describe(),
            "knowledge-economy-v1@9 (period=10,lock=30,topup=5)"
        );
        let zero = EconomyCadence {
            period_blocks: 0,
            ..fast
        };
        assert_eq!(
            UpgradeSchedule::default().with_cadence(zero),
            Err(UpgradeError::BadCadence)
        );
    }

    #[test]
    fn unknown_and_duplicate_entries_are_rejected() {
        assert_eq!(
            UpgradeSchedule::from_entries([("knowledge-economy-v2", 5)]),
            Err(UpgradeError::Unknown("knowledge-economy-v2".into()))
        );
        assert_eq!(
            UpgradeSchedule::from_entries([
                ("knowledge-economy-v1", 5),
                ("knowledge-economy-v1", 9)
            ]),
            Err(UpgradeError::Duplicate(Upgrade::KnowledgeEconomyV1))
        );
        let ok = UpgradeSchedule::from_entries([("knowledge-economy-v1", 7)]).unwrap();
        assert_eq!(ok.describe(), "knowledge-economy-v1@7");
    }
}
