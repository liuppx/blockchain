//! 链上知识单元模型(测试网阶段 1)。设计见 `docs/design/knowledge-units-onchain.md`。
//!
//! 本模块定义知识单元的**操作**(账户签名的区块 op)与**状态**类型。编解码在 [`crate::codec`]
//! (与其他 op 一致,便于模糊测试覆盖);应用规则与可信度计算在后续分片(K2、K3)。
//!
//! 对应 VISION 的概念:
//! - 知识单元 = 只讲一件事、自身能读懂(§4a);版本化(U1);多语言算同一条(U2);
//! - 引用分两类:依据(传递可信度)与参见(不传递),并锁定被引用的版本(§4a);
//! - 正文不上链,只存每种语言正文的 SHA-256;内容许可 CC BY 4.0(§4d T2)。

use crate::{codec, sha256, Hash, Keypair, PubKey, Sig};
use std::collections::{BTreeMap, BTreeSet};

/// 内容许可(协议强制,VISION §4d T2)。所有版本均以此许可发布,提交即视为同意。
pub const CONTENT_LICENSE: &str = "CC-BY-4.0";

/// 单个版本最多引用的单元数(防"撒网引用",设计稿 §3)。
pub const MAX_CITES: usize = 32;
/// 单个版本最多的语言正文数。
pub const MAX_BODIES: usize = 16;
/// 语言标签的最大字节数(如 `zh`、`en`、`zh-Hant`)。
pub const MAX_LANG_LEN: usize = 16;
/// 模型标识的最大字节数(如 `claude-opus-5-5`)。
pub const MAX_MODEL_ID_LEN: usize = 64;

/// 单个区块最多携带的知识 op 数。共识规则:超过即整块拒绝(不只是诚实出块者的自我约束),
/// 限定单块的应用开销与体积。
pub const MAX_KNOWLEDGE_OPS_PER_BLOCK: usize = 256;

/// 签名域分隔标签:知识 op 的签名字节以此为前缀,保证它的签名不能被挪用为任何其他 op 的签名。
pub const SIGN_DOMAIN: &[u8] = b"zhixing-knowledge-op-v1";

/// 知识层级(VISION §3)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Foundation = 0,
    Explanation = 1,
    Scenario = 2,
    Application = 3,
}

impl Layer {
    pub fn from_u8(v: u8) -> Option<Layer> {
        match v {
            0 => Some(Layer::Foundation),
            1 => Some(Layer::Explanation),
            2 => Some(Layer::Scenario),
            3 => Some(Layer::Application),
            _ => None,
        }
    }
}

/// 内容来源(VISION P3)。由发布者声明,链上无法验证,但会影响初始可信度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// 人工撰写。
    Human,
    /// 模型生成(模型标识)。
    Model(String),
    /// 人工核验过的模型生成(模型标识)。
    ModelVerified(String),
}

/// 引用类型(VISION §4a)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiteKind {
    /// 依据:本版本建立在被引用版本之上;传递可信度。
    Basis = 0,
    /// 参见:相关但互不依赖;不传递可信度。
    SeeAlso = 1,
    /// 取代:本单元**取代**了被引用的单元——被取代的单元作废(生命周期进入"已取代"),
    /// 它的贡献者与核验记录保留,历史可追溯。
    ///
    /// 与"依据"相反:取代**不**传递可信度(新单元不继承旧单元的可信度),
    /// 也不要求双方有依赖关系——它们是两个独立的单元,只是前者废弃了后者。
    /// 取代关系不可成环(A 取代 B,B 不能再取代 A)。
    Supersedes = 2,
}

/// 一条引用,锁定被引用单元的具体版本(U1)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cite {
    pub unit: u64,
    pub version: u32,
    pub kind: CiteKind,
}

/// 一种语言的正文:语言标签 + 正文的 SHA-256(正文本身不上链)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub lang: String,
    pub hash: Hash,
}

/// 一个版本的内容:各语言正文哈希、来源、引用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionPayload {
    pub bodies: Vec<Body>,
    pub source: Source,
    pub cites: Vec<Cite>,
}

/// 核验结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass = 0,
    NeedsRevision = 1,
}

/// 知识 op 的具体动作(设计稿 §3)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnowledgeAction {
    /// 创建一个知识单元及其第一个版本。
    CreateUnit {
        layer: Layer,
        domain: u32,
        version: VersionPayload,
    },
    /// 为已有单元发布新版本(任何成员均可)。
    PublishVersion { unit: u64, version: VersionPayload },
    /// 对某单元的某个版本给出核验结论。`note_hash` 是核验说明正文的哈希(可全零表示无说明)。
    Verify {
        unit: u64,
        version: u32,
        verdict: Verdict,
        note_hash: Hash,
    },
    /// 邀请新成员加入(新账户 id 由链单调分配)。
    Invite { pubkey: PubKey },
}

/// 账户签名的知识 op。`actor` 为发起账户,`nonce` 严格等于该账户当前序号(防重放,与其他 op 一致)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeOp {
    pub actor: u64,
    pub action: KnowledgeAction,
    pub nonce: u64,
    /// ed25519 签名,签在 [`codec::knowledge_op_signing_bytes`] 上(带 [`SIGN_DOMAIN`] 前缀)。
    pub signature: Sig,
}

impl KnowledgeOp {
    pub fn signed(mut self, kp: &Keypair) -> Self {
        self.signature = kp.sign(&codec::knowledge_op_signing_bytes(&self));
        self
    }

    /// 基于完整签名编码的内容哈希(gossip 去重键)。
    pub fn hash(&self) -> Hash {
        sha256(&codec::encode_knowledge_op(self))
    }

    /// 结构检查(不验签、不查状态):正文数量与语言标签、引用数量、模型标识长度等。
    /// 解码器已对数量与长度设上限;此处补充解码器不检查的语义约束。
    pub fn is_well_formed(&self) -> Result<(), &'static str> {
        match &self.action {
            KnowledgeAction::CreateUnit { version, .. }
            | KnowledgeAction::PublishVersion { version, .. } => version.check(),
            KnowledgeAction::Verify { version, .. } => {
                if *version == 0 {
                    Err("version numbers start at 1")
                } else {
                    Ok(())
                }
            }
            KnowledgeAction::Invite { .. } => Ok(()),
        }
    }
}

impl VersionPayload {
    fn check(&self) -> Result<(), &'static str> {
        if self.bodies.is_empty() {
            return Err("a version needs at least one body");
        }
        if self.bodies.len() > MAX_BODIES {
            return Err("too many bodies");
        }
        let mut langs: Vec<&str> = Vec::with_capacity(self.bodies.len());
        for b in &self.bodies {
            if b.lang.is_empty() || b.lang.len() > MAX_LANG_LEN {
                return Err("language tag must be 1..=16 bytes");
            }
            if !b
                .lang
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                return Err("language tag must be ASCII letters, digits or '-'");
            }
            if langs.contains(&b.lang.as_str()) {
                return Err("duplicate language");
            }
            langs.push(&b.lang);
        }
        match &self.source {
            Source::Human => {}
            Source::Model(m) | Source::ModelVerified(m) => {
                if m.is_empty() || m.len() > MAX_MODEL_ID_LEN {
                    return Err("model id must be 1..=64 bytes");
                }
            }
        }
        if self.cites.len() > MAX_CITES {
            return Err("too many cites");
        }
        let mut seen: Vec<(u64, u32)> = Vec::with_capacity(self.cites.len());
        for c in &self.cites {
            if c.version == 0 {
                return Err("version numbers start at 1");
            }
            if seen.contains(&(c.unit, c.version)) {
                return Err("duplicate cite");
            }
            seen.push((c.unit, c.version));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 状态类型(应用规则见 K2)
// ---------------------------------------------------------------------------

/// 一个已入链的版本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitVersion {
    pub n: u32,
    pub publisher: u64,
    pub payload: VersionPayload,
    pub published_height: u64,
}

/// 一个已入链的知识单元。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeUnit {
    pub id: u64,
    pub author: u64,
    pub layer: Layer,
    pub domain: u32,
    pub created_height: u64,
    /// `versions[i].n == i + 1`。
    pub versions: Vec<UnitVersion>,
}

/// 一条核验记录。同一核验人对同一版本的多次核验,以最新一条为准。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub unit: u64,
    pub version: u32,
    pub verifier: u64,
    pub verdict: Verdict,
    pub note_hash: Hash,
    pub height: u64,
}

/// 成员信息:加入高度、邀请人(创世成员的邀请人为 0)、最近一次邀请他人的高度(0 表示从未邀请)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub joined_height: u64,
    pub inviter: u64,
    pub last_invite_height: u64,
}

/// 创世时的知识模块参数。默认值保持现有链的行为不变:旧 ΔK 提交路径开启、无见习期、无邀请冷却、
/// 未登记领域(即不能创建知识单元)。测试网创世显式设置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeGenesis {
    /// 已登记的领域命名空间(如 1 = 大模型应用开发,2 = 运动健身)。
    pub domains: Vec<u32>,
    /// 是否接受旧的 ΔK 知识提交(`SubmissionTx`)。测试网设为 false(VISION Q7)。
    pub legacy_submissions: bool,
    /// 见习期(区块数):新成员在此期间的核验不计入可信度,也不能邀请他人(M7)。
    pub probation_blocks: u64,
    /// 邀请冷却(区块数):同一成员两次邀请之间至少间隔的区块数。
    pub invite_cooldown_blocks: u64,
}

impl Default for KnowledgeGenesis {
    fn default() -> Self {
        KnowledgeGenesis {
            domains: Vec::new(),
            legacy_submissions: true,
            probation_blocks: 0,
            invite_cooldown_blocks: 0,
        }
    }
}

/// 链上知识模块的全部状态,作为 `ChainState::knowledge` 一个字段,便于整体折入状态根、快照与主网迁移。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnowledgeState {
    pub units: BTreeMap<u64, KnowledgeUnit>,
    /// 键为 (单元, 版本, 核验人);同一核验人对同一版本以最新一条为准。
    pub verifications: BTreeMap<(u64, u32, u64), Verification>,
    pub members: BTreeMap<u64, Member>,
    pub domains: BTreeSet<u32>,
    pub next_unit_id: u64,
    pub next_account_id: u64,
    pub legacy_submissions: bool,
    pub probation_blocks: u64,
    pub invite_cooldown_blocks: u64,
}

impl KnowledgeState {
    /// 由创世账户与参数初始化:所有创世账户都是成员(邀请人 0,加入高度 0)。
    pub fn from_genesis<'a>(
        g: &KnowledgeGenesis,
        account_ids: impl Iterator<Item = &'a u64>,
    ) -> Self {
        let mut members = BTreeMap::new();
        let mut max_id = 0u64;
        for id in account_ids {
            max_id = max_id.max(*id);
            members.insert(
                *id,
                Member {
                    joined_height: 0,
                    inviter: 0,
                    last_invite_height: 0,
                },
            );
        }
        KnowledgeState {
            units: BTreeMap::new(),
            verifications: BTreeMap::new(),
            members,
            domains: g.domains.iter().copied().collect(),
            next_unit_id: 1,
            next_account_id: max_id + 1,
            legacy_submissions: g.legacy_submissions,
            probation_blocks: g.probation_blocks,
            invite_cooldown_blocks: g.invite_cooldown_blocks,
        }
    }

    /// 成员在 `height` 时是否已过见习期。创世成员始终视为已过见习期。
    pub fn is_established(&self, id: u64, height: u64) -> bool {
        match self.members.get(&id) {
            Some(m) if m.inviter == 0 => true,
            Some(m) => height >= m.joined_height.saturating_add(self.probation_blocks),
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// 可信度(K3,试运行规则 v0,与 tools/kgraph.py 一致)
// ---------------------------------------------------------------------------

/// 模型生成(含自报"人工核验过的模型生成")且无人核验通过时的自身可信度(万分比)。
pub const CRED_MODEL: u32 = 1000;
/// 人工撰写,或已有至少一人核验通过时的基础值。
pub const CRED_HUMAN: u32 = 2000;
/// 每多一位计入的核验通过者增加的值。
pub const CRED_STEP: u32 = 2000;
/// 自身可信度上限。
pub const CRED_MAX: u32 = 9000;

/// 一个版本的可信度计算结果。所有值为万分比(10000 = 1.0)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credibility {
    /// 计入的核验通过人数(已过见习期、不是发布者、对该版本的最新结论为"通过")。
    pub passes: u32,
    /// 计入的"需修改"人数(条件同上)。每一位抵消一位"通过"。
    pub needs: u32,
    /// 自身可信度。
    pub self_bps: u32,
    /// 有效可信度 = min(自身可信度, 所依据的每个版本的有效可信度)。
    pub effective_bps: u32,
    /// 决定有效可信度的那个依据(单元, 版本);为 None 表示取决于自身。
    pub bottleneck: Option<(u64, u32)>,
    /// 引用已过时的依据:(被引用单元, 引用的版本, 当前版本)。
    pub stale_cites: Vec<(u64, u32, u32)>,
}

/// 知识单元的生命周期状态(§4a)。**由链上已有的关系派生**,不是新增的状态字段,
/// 所以推导它不会改变 `state_root`,已有链不需要重开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    /// 活跃:正常参与可信度计算与奖励分配。
    Active,
    /// 休眠:长期无人引用、无人核验;仍在链上,仍可被引用与修订。
    Dormant,
    /// 已被另一个单元取代(见 `CiteKind::Supersedes`);保留历史,不再参与奖励分配。
    Superseded,
    /// 竞争单元:与被引用的单元**并存**(§4b"挑战共识不用举报,而用提交竞争单元")。
    /// 链上记录为一处"断层",由证据、引用与使用随时间分出高下;**不强行裁决**。
    Contested,
}

impl KnowledgeState {
    /// 某个正文哈希已经属于哪个单元(U2 跨语言查重:同一份正文只能属于一个单元)。
    pub fn unit_with_body_hash(&self, hash: &[u8; 32]) -> Option<u64> {
        self.units
            .values()
            .find(|u| {
                u.versions
                    .iter()
                    .any(|v| v.payload.bodies.iter().any(|b| &b.hash == hash))
            })
            .map(|u| u.id)
    }

    /// 某单元**取代了谁**(当前版本里的 `Supersedes` 引用指向被取代的单元)。
    pub fn unit_supersedes(&self, unit: u64) -> Option<u64> {
        let unit_state = self.units.get(&unit)?;
        let v = unit_state.versions.last()?;
        v.payload
            .cites
            .iter()
            .find(|c| c.kind == CiteKind::Supersedes)
            .map(|c| c.unit)
    }

    /// 某单元**被谁取代**:找出所有当前版本以 `Supersedes` 引用它的单元。
    pub fn superseded_by(&self, unit: u64) -> Option<u64> {
        self.units
            .values()
            .filter(|other| other.id != unit)
            .find(|other| {
                other.versions.last().is_some_and(|v| {
                    v.payload
                        .cites
                        .iter()
                        .any(|c| c.unit == unit && c.kind == CiteKind::Supersedes)
                })
            })
            .map(|other| other.id)
    }

    /// 某单元的生命周期状态。
    ///
    /// - 被取代 ⇒ `Superseded`(优先于其他判定:取代是显式声明);
    /// - 本身是竞争单元(它取代了别人)⇒ `Contested`:新旧并存,等证据与使用分高下;
    /// - 否则:若已确立(有核验通过)或仍被别的单元依据 ⇒ `Active`;
    /// - 否则(既无人核验、也无人依据)⇒ `Dormant`。
    pub fn lifecycle(&self, unit: u64) -> Option<LifecycleState> {
        self.units.get(&unit)?;
        if self.superseded_by(unit).is_some() {
            return Some(LifecycleState::Superseded);
        }
        if self.unit_supersedes(unit).is_some() {
            // 竞争单元:它主张取代了旧单元,但旧单元同时也是"被取代"——两个方向都存在时,
            // 说明这是"竞争"而非"平稳取代"(§4b:不强行裁决,靠证据与使用分高下)。
            return Some(LifecycleState::Contested);
        }
        let cred = self.current_credibility(unit)?;
        if cred.passes > 0 {
            return Some(LifecycleState::Active);
        }
        // 被别人的当前版本依据 ⇒ 仍在使用
        let cited = self.units.values().any(|other| {
            other.id != unit
                && other.versions.last().is_some_and(|v| {
                    v.payload
                        .cites
                        .iter()
                        .any(|c| c.unit == unit && c.kind == CiteKind::Basis)
                })
        });
        if cited {
            return Some(LifecycleState::Active);
        }
        Some(LifecycleState::Dormant)
    }

    fn version(&self, unit: u64, version: u32) -> Option<&UnitVersion> {
        self.units
            .get(&unit)?
            .versions
            .get((version as usize).checked_sub(1)?)
    }

    /// 计入的(通过人数, 需修改人数)。
    ///
    /// 计入条件:对该版本的**最新**结论;核验发生时核验人已过见习期(按核验高度判断,之后不回溯);
    /// 核验人不是该版本的发布者(应用时已强制)。核验只对当时的版本有效。
    pub fn verdict_counts(&self, unit: u64, version: u32) -> Option<(u32, u32)> {
        let v = self.version(unit, version)?;
        let (mut passes, mut needs) = (0u32, 0u32);
        for (_, r) in self
            .verifications
            .range((unit, version, 0)..=(unit, version, u64::MAX))
        {
            if r.verifier == v.publisher || !self.is_established(r.verifier, r.height) {
                continue;
            }
            match r.verdict {
                Verdict::Pass => passes += 1,
                Verdict::NeedsRevision => needs += 1,
            }
        }
        Some((passes, needs))
    }

    /// 自身可信度与计入的核验通过人数。
    ///
    /// 每一位计入的"需修改"抵消一位"通过"(经济仿真 `sim/economy.py` 的确立规则:通过多于需修改):
    /// 可信度按**净通过人数** `max(0, 通过 − 需修改)` 计算。否则一个版本即使多数核验人认为有错,
    /// 可信度也照样按通过的人数上升。
    /// 自报的"人工核验过的模型生成"在链上无法验证,链上无人净通过前仍按模型生成计。
    pub fn self_credibility(&self, unit: u64, version: u32) -> Option<(u32, u32)> {
        let v = self.version(unit, version)?;
        let (passes, needs) = self.verdict_counts(unit, version)?;
        let net = passes.saturating_sub(needs);
        let model = !matches!(v.payload.source, Source::Human);
        let base = if model && net == 0 {
            CRED_MODEL
        } else {
            CRED_HUMAN
        };
        let bps = base
            .saturating_add(CRED_STEP.saturating_mul(net))
            .min(CRED_MAX);
        Some((bps, passes))
    }

    /// 某单元某版本的可信度。版本级引用图天然无环,递归必然终止;记忆化避免重复计算。
    pub fn credibility(&self, unit: u64, version: u32) -> Option<Credibility> {
        let mut memo = BTreeMap::new();
        self.credibility_memo(unit, version, &mut memo)
    }

    /// 单元当前(最新)版本的可信度。
    pub fn current_credibility(&self, unit: u64) -> Option<Credibility> {
        let n = self.units.get(&unit)?.versions.len() as u32;
        self.credibility(unit, n)
    }

    fn credibility_memo(
        &self,
        unit: u64,
        version: u32,
        memo: &mut BTreeMap<(u64, u32), Credibility>,
    ) -> Option<Credibility> {
        if let Some(c) = memo.get(&(unit, version)) {
            return Some(c.clone());
        }
        let v = self.version(unit, version)?;
        let (self_bps, passes) = self.self_credibility(unit, version)?;
        let needs = self.verdict_counts(unit, version).map_or(0, |(_, n)| n);
        let mut effective_bps = self_bps;
        let mut bottleneck = None;
        let mut stale_cites = Vec::new();
        for c in v.payload.cites.iter().filter(|c| c.kind == CiteKind::Basis) {
            // 被取代的地基不再传递可信度:依据一个已作废的单元,上层同样不可信。
            // (取代关系是显式声明,不是"看起来没人用",所以不靠启发式判断。)
            if self.superseded_by(c.unit).is_some() {
                effective_bps = 0;
                bottleneck = Some((c.unit, c.version));
                break;
            }
            // 引用目标在应用时已校验存在;状态被手工破坏时按 0 处理(不可信)。
            let dep = self
                .credibility_memo(c.unit, c.version, memo)
                .map(|d| d.effective_bps)
                .unwrap_or(0);
            if dep < effective_bps {
                effective_bps = dep;
                bottleneck = Some((c.unit, c.version));
            }
            if let Some(u) = self.units.get(&c.unit) {
                let cur = u.versions.len() as u32;
                if cur > c.version {
                    stale_cites.push((c.unit, c.version, cur));
                }
            }
        }
        let out = Credibility {
            passes,
            needs,
            self_bps,
            effective_bps,
            bottleneck,
            stale_cites,
        };
        memo.insert((unit, version), out.clone());
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由创世成员 ids 构造知识状态(无见习期,除非另行设置)。
    fn state(members: &[u64], probation: u64) -> KnowledgeState {
        let g = KnowledgeGenesis {
            domains: vec![2],
            legacy_submissions: false,
            probation_blocks: probation,
            invite_cooldown_blocks: 0,
        };
        KnowledgeState::from_genesis(&g, members.iter())
    }

    fn add_unit(k: &mut KnowledgeState, id: u64, publisher: u64, source: Source, cites: Vec<Cite>) {
        k.units.insert(
            id,
            KnowledgeUnit {
                id,
                author: publisher,
                layer: if cites.is_empty() {
                    Layer::Foundation
                } else {
                    Layer::Application
                },
                domain: 2,
                created_height: 1,
                versions: vec![UnitVersion {
                    n: 1,
                    publisher,
                    payload: VersionPayload {
                        bodies: vec![Body {
                            lang: "zh".into(),
                            hash: [id as u8; 32],
                        }],
                        source,
                        cites,
                    },
                    published_height: 1,
                }],
            },
        );
    }

    fn verify(
        k: &mut KnowledgeState,
        unit: u64,
        version: u32,
        verifier: u64,
        verdict: Verdict,
        height: u64,
    ) {
        k.verifications.insert(
            (unit, version, verifier),
            Verification {
                unit,
                version,
                verifier,
                verdict,
                note_hash: [0u8; 32],
                height,
            },
        );
    }

    fn model() -> Source {
        Source::Model("claude-opus-5-5".into())
    }

    fn basis(unit: u64) -> Cite {
        Cite {
            unit,
            version: 1,
            kind: CiteKind::Basis,
        }
    }

    #[test]
    fn matches_the_kgraph_pilot_scenario() {
        // 与 docs/pilot/fitness 上 tools/kgraph.py 的演示场景逐项一致:
        // 五条基础单元 + 深蹲(依据全部五条);深蹲两人核验,基础单元中"运动前自我评估"无人核验。
        let mut k = state(&[1, 10, 11, 12, 13], 0);
        for id in 1..=5 {
            add_unit(&mut k, id, 1, model(), vec![]);
        }
        add_unit(&mut k, 6, 1, model(), (1..=5).map(basis).collect());
        for (u, vs) in [
            (6u64, vec![10u64, 11]),
            (1, vec![12, 13]),
            (2, vec![12, 13]),
            (3, vec![12, 13]),
            (4, vec![12]),
        ] {
            for v in vs {
                verify(&mut k, u, 1, v, Verdict::Pass, 2);
            }
        }
        let eff = |u| k.current_credibility(u).unwrap();
        assert_eq!(eff(1).effective_bps, 6000); // 渐进超负荷 0.6
        assert_eq!(eff(4).effective_bps, 4000); // 关节的稳定与灵活 0.4(一人核验)
        assert_eq!(eff(5).effective_bps, 1000); // 运动前自我评估 0.1(无人核验)
        let squat = eff(6);
        assert_eq!(squat.passes, 2);
        assert_eq!(squat.self_bps, 6000, "squat itself: 0.6");
        assert_eq!(
            squat.effective_bps, 1000,
            "capped by its weakest basis: 0.1"
        );
        assert_eq!(
            squat.bottleneck,
            Some((5, 1)),
            "bottleneck = 运动前自我评估"
        );
    }

    #[test]
    fn self_credibility_rules() {
        let mut k = state(&[1, 2, 3, 4, 5, 6, 7], 0);
        add_unit(&mut k, 1, 1, Source::Human, vec![]);
        add_unit(&mut k, 2, 1, model(), vec![]);
        add_unit(&mut k, 3, 1, Source::ModelVerified("m".into()), vec![]);
        assert_eq!(
            k.self_credibility(1, 1),
            Some((CRED_HUMAN, 0)),
            "human-written starts at 0.2"
        );
        assert_eq!(
            k.self_credibility(2, 1),
            Some((CRED_MODEL, 0)),
            "model-generated starts at 0.1"
        );
        assert_eq!(
            k.self_credibility(3, 1),
            Some((CRED_MODEL, 0)),
            "a self-declared 'model, human-verified' label is not an on-chain verification"
        );
        // 五人通过 → 0.2 + 5×0.2 = 1.2,封顶 0.9
        for v in 2..=6 {
            verify(&mut k, 2, 1, v, Verdict::Pass, 2);
        }
        assert_eq!(k.self_credibility(2, 1), Some((CRED_MAX, 5)));
        // 同一核验人的最新结论改为"需修改":不再计入通过,并抵消一位通过 → 净 3 → 0.2 + 0.6 = 0.8
        verify(&mut k, 2, 1, 6, Verdict::NeedsRevision, 3);
        assert_eq!(
            k.self_credibility(2, 1),
            Some((CRED_HUMAN + 3 * CRED_STEP, 4))
        );
        assert_eq!(k.verdict_counts(2, 1), Some((4, 1)));
        // 发布者本人的核验不计入(应用时已拒绝,这里防御性地确认)
        verify(&mut k, 1, 1, 1, Verdict::Pass, 2);
        assert_eq!(k.self_credibility(1, 1), Some((CRED_HUMAN, 0)));
        assert_eq!(k.self_credibility(9, 1), None);
        assert_eq!(k.self_credibility(1, 2), None);
    }

    #[test]
    fn supersedes_marks_the_old_unit_and_blocks_cycles() {
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, model(), vec![]); // 被取代的旧单元
        add_unit(&mut k, 2, 2, model(), vec![]); // 取代它的新单元
                                                 // 新单元引用旧单元,类型为"取代"
        k.units.get_mut(&2).unwrap().versions[0].payload.cites = vec![Cite {
            unit: 1,
            version: 1,
            kind: CiteKind::Supersedes,
        }];
        assert_eq!(k.superseded_by(1), Some(2), "旧单元应被标记为已取代");
        assert_eq!(k.unit_supersedes(2), Some(1));
        assert_eq!(k.lifecycle(1), Some(LifecycleState::Superseded));
        // 新单元是**竞争单元**:新旧并存,由证据与使用分高下(§4b,不强行裁决)
        assert_eq!(k.lifecycle(2), Some(LifecycleState::Contested));
        // 取代**不**传递可信度:新单元不继承旧单元的可信度
        verify(&mut k, 1, 1, 2, Verdict::Pass, 3);
        let c2 = k.credibility(2, 1).unwrap();
        assert_eq!(c2.passes, 0, "取代关系本身不应带来通过计数");
    }

    #[test]
    fn competing_units_coexist_rather_than_being_retired() {
        // §4b:竞争单元与原单元**并存**,链上显示为一处"断层";旧单元不因被挑战就作废。
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, model(), vec![]);
        add_unit(
            &mut k,
            2,
            2,
            model(),
            vec![Cite {
                unit: 1,
                version: 1,
                kind: CiteKind::Supersedes,
            }],
        );
        assert_eq!(k.lifecycle(2), Some(LifecycleState::Contested));
        // 旧单元仍可被引用与修订(不因被挑战而消失)
        assert!(k.units.contains_key(&1));
        assert_eq!(k.lifecycle(1), Some(LifecycleState::Superseded));
        // 旧单元的历史(核验记录)保留
        verify(&mut k, 1, 1, 3, Verdict::Pass, 3);
        assert_eq!(k.verdict_counts(1, 1), Some((1, 0)));
    }

    #[test]
    fn a_superseded_foundation_stops_passing_credibility() {
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, model(), vec![]); // 地基
        add_unit(
            &mut k,
            2,
            2,
            model(),
            vec![Cite {
                unit: 1,
                version: 1,
                kind: CiteKind::Basis,
            }],
        ); // 依据它
        verify(&mut k, 1, 1, 3, Verdict::Pass, 3);
        verify(&mut k, 2, 1, 3, Verdict::Pass, 3);
        assert_eq!(
            k.credibility(2, 1).unwrap().effective_bps,
            CRED_HUMAN + CRED_STEP
        );
        // 单元 3 取代地基 1
        add_unit(
            &mut k,
            3,
            3,
            model(),
            vec![Cite {
                unit: 1,
                version: 1,
                kind: CiteKind::Supersedes,
            }],
        );
        let c2 = k.credibility(2, 1).unwrap();
        assert_eq!(c2.effective_bps, 0, "依据已作废地基的上层单元不再有可信度");
        assert_eq!(c2.bottleneck, Some((1, 1)), "应指出是哪个地基作废了它");
    }

    #[test]
    fn dormant_when_nobody_verified_or_cites_it() {
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, model(), vec![]);
        add_unit(&mut k, 2, 2, model(), vec![]);
        assert_eq!(
            k.lifecycle(1),
            Some(LifecycleState::Dormant),
            "无人核验也无人依据 ⇒ 休眠"
        );
        verify(&mut k, 1, 1, 2, Verdict::Pass, 3);
        assert_eq!(
            k.lifecycle(1),
            Some(LifecycleState::Active),
            "有核验通过 ⇒ 活跃"
        );
    }

    #[test]
    fn cited_units_stay_active() {
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, model(), vec![]);
        add_unit(
            &mut k,
            2,
            2,
            model(),
            vec![Cite {
                unit: 1,
                version: 1,
                kind: CiteKind::Basis,
            }],
        );
        assert_eq!(
            k.lifecycle(1),
            Some(LifecycleState::Active),
            "被依据 ⇒ 活跃"
        );
    }

    #[test]
    fn needs_revision_offsets_passes() {
        let mut k = state(&[1, 2, 3, 4, 5], 0);
        add_unit(&mut k, 1, 1, model(), vec![]);
        verify(&mut k, 1, 1, 2, Verdict::Pass, 2);
        verify(&mut k, 1, 1, 3, Verdict::Pass, 2);
        assert_eq!(
            k.self_credibility(1, 1),
            Some((CRED_HUMAN + 2 * CRED_STEP, 2))
        );
        // 两人认为需修改:与两位通过相互抵消,回到"无人净通过"的模型生成起点
        verify(&mut k, 1, 1, 4, Verdict::NeedsRevision, 3);
        verify(&mut k, 1, 1, 5, Verdict::NeedsRevision, 3);
        let c = k.credibility(1, 1).unwrap();
        assert_eq!((c.passes, c.needs, c.self_bps), (2, 2, CRED_MODEL));
        // 需修改多于通过:不会低于起点
        verify(&mut k, 1, 1, 2, Verdict::NeedsRevision, 4);
        assert_eq!(k.self_credibility(1, 1), Some((CRED_MODEL, 1)));
    }

    #[test]
    fn probation_is_judged_at_verification_time() {
        let mut k = state(&[1], 10);
        k.members.insert(
            50,
            Member {
                joined_height: 5,
                inviter: 1,
                last_invite_height: 0,
            },
        );
        add_unit(&mut k, 1, 1, Source::Human, vec![]);
        verify(&mut k, 1, 1, 50, Verdict::Pass, 14); // 加入后第 9 个区块:仍在见习期
        assert_eq!(
            k.self_credibility(1, 1),
            Some((CRED_HUMAN, 0)),
            "probation verification not counted"
        );
        verify(&mut k, 1, 1, 50, Verdict::Pass, 15); // 加入后第 10 个区块:已过见习期
        assert_eq!(k.self_credibility(1, 1), Some((CRED_HUMAN + CRED_STEP, 1)));
    }

    #[test]
    fn basis_cites_pin_versions_and_report_staleness() {
        let mut k = state(&[1, 2, 3], 0);
        add_unit(&mut k, 1, 1, Source::Human, vec![]);
        verify(&mut k, 1, 1, 2, Verdict::Pass, 2);
        verify(&mut k, 1, 1, 3, Verdict::Pass, 2); // 单元 1 v1:0.6
        add_unit(&mut k, 2, 1, Source::Human, vec![basis(1)]);
        verify(&mut k, 2, 1, 2, Verdict::Pass, 2);
        verify(&mut k, 2, 1, 3, Verdict::Pass, 2); // 单元 2 自身 0.6
        assert_eq!(k.current_credibility(2).unwrap().effective_bps, 6000);
        // 单元 1 发布 v2(无人核验):v1 的核验不带到 v2;单元 2 仍锁定 v1,不受影响,但提示过时
        let mut v2 = k.units[&1].versions[0].clone();
        v2.n = 2;
        k.units.get_mut(&1).unwrap().versions.push(v2);
        assert_eq!(
            k.current_credibility(1).unwrap().self_bps,
            CRED_HUMAN,
            "v1 verifications do not carry to v2"
        );
        let c2 = k.current_credibility(2).unwrap();
        assert_eq!(c2.effective_bps, 6000, "pinned to v1");
        assert_eq!(c2.stale_cites, vec![(1, 1, 2)]);
        // 参见不传递可信度
        add_unit(
            &mut k,
            3,
            1,
            Source::Human,
            vec![Cite {
                unit: 1,
                version: 2,
                kind: CiteKind::SeeAlso,
            }],
        );
        assert_eq!(k.current_credibility(3).unwrap().bottleneck, None);
    }

    #[test]
    fn effective_credibility_propagates_through_chains() {
        // 1 ← 2 ← 3(依据链):最弱的一环决定整条链,并逐级报告瓶颈
        let mut k = state(&[1, 2, 3, 4], 0);
        add_unit(&mut k, 1, 1, model(), vec![]); // 0.1
        add_unit(&mut k, 2, 1, Source::Human, vec![basis(1)]);
        add_unit(
            &mut k,
            3,
            1,
            Source::Human,
            vec![Cite {
                unit: 2,
                version: 1,
                kind: CiteKind::Basis,
            }],
        );
        for u in [2, 3] {
            for v in [2, 3, 4] {
                verify(&mut k, u, 1, v, Verdict::Pass, 2);
            }
        }
        let c3 = k.current_credibility(3).unwrap();
        assert_eq!(c3.self_bps, 8000);
        assert_eq!(
            c3.effective_bps, CRED_MODEL,
            "a weak foundation caps everything built on it"
        );
        assert_eq!(c3.bottleneck, Some((2, 1)));
        assert_eq!(k.current_credibility(2).unwrap().bottleneck, Some((1, 1)));
        // 打牢地基后,整条链随之上升
        for v in [2, 3, 4] {
            verify(&mut k, 1, 1, v, Verdict::Pass, 2);
        }
        assert_eq!(k.current_credibility(3).unwrap().effective_bps, 8000);
    }
}
