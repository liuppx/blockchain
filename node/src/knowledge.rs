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

/// 成员信息:加入高度与邀请人(创世成员的邀请人为 0)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub joined_height: u64,
    pub inviter: u64,
}
