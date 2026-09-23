# 知行图 · ZhixingGraph 白皮书

> **算力即电力，认知即资产，人类共识即地图。**
>
> A cognition-anchored graph protocol that maps energy into compute, compute into knowledge, and knowledge into a shared topography of human understanding.

---

| 项目 | 内容 |
|---|---|
| 项目名称 | 知行图（ZhixingGraph） |
| 社区 | 夜莺社区（Nightingale Community） |
| 主代币 | $COG（Cognition） |
| 辅代币 | $WATT（电力凭证）、$FLOP（算力凭证） |
| 贡献凭证 | cNFT（Cognitive Contribution NFT） |
| 共识机制 | PoK — Proof of Knowledge（认知证明） |
| 版本 | Draft v0.10 |
| 日期 | 2026-09 |
| 状态 | 草稿 · 征求意见（RFC） |

> ⚠️ 本文为构想阶段的白皮书草稿，用于社区讨论。所有参数、公式、经济模型均为待验证的初始设计，不构成任何投资建议或最终技术承诺。

---

## 摘要 / Abstract

大模型的快速迭代让"算力"成为新的稀缺资源，而算力的背后是持续消耗的电力——**"算力即电力"** 已成为行业共识。然而，今天的区块链大多停留在"去中心化账本"层面，记录的是资金流动，与人类真实的价值创造脱节，催生了大量"空气币"。

知行图（ZhixingGraph）提出一个根本性重构：**区块链不应是一条线性的链（Chain），而应是一张反映人类认知全貌的图（Graph）**。这张图像一幅地形图，有代表核心共识的山峰，有代表未知的山谷，有连接不同领域的山脊，也有代表争议的断层。

知行图建立三层价值映射——**电力（Energy）→ 算力（Compute）→ 认知（Cognition）**——并规定：代币 $COG 只有在产生"可验证的认知增量（ΔK）"时才被增发。由此，每一枚代币背后都锚定真实流过的电力、真实运行的算力和真实新增的人类认知，从根本上区别于工作量证明式的"为挖矿而挖矿"，也区别于毫无价值支撑的空气币。

本白皮书阐述知行图的哲学基础、数据结构、三层映射机制、PoK 共识、代币经济学、技术架构与分阶段落地路线。

---

## 目录

1. [背景与问题](#1-背景与问题)
2. [核心理念：从链到图](#2-核心理念从链到图)
3. [三层价值映射：电力—算力—认知](#3-三层价值映射电力算力认知)
4. [认知图谱数据结构](#4-认知图谱数据结构)
5. [PoK 共识机制](#5-pok-共识机制认知证明)
6. [代币经济学](#6-代币经济学)
7. [技术架构](#7-技术架构)
8. [认知地形图](#8-认知地形图human-cognition-topography)
9. [治理](#9-治理)
10. [路线图](#10-路线图)
11. [风险与开放问题](#11-风险与开放问题)
12. [结语](#12-结语)

---

## 1. 背景与问题

### 1.1 算力即电力

AI 大模型的训练与推理消耗巨量电力。据行业估算，单次前沿模型训练的耗电量已达数十 GWh 量级，推理阶段的累计电力消耗更是持续增长。算力的竞争，本质上是能源的竞争——**谁掌握了廉价、绿色、稳定的电力，谁就掌握了算力，进而掌握了认知生产力。**

然而这条价值链条目前是**割裂且不透明**的：
- 电力的绿色属性难以追溯；
- 算力是否被用于"有效计算"无法验证（大量算力空转或用于无意义哈希）；
- 认知产出（模型、数据、知识）与其消耗的资源之间缺乏可信映射。

### 1.2 区块链的异化

区块链本应是价值互联网的基础设施，但今天多数项目呈现两个异化：

1. **脱实向虚**：工作量证明消耗大量电力却不产生任何认知价值，是"为了算而算"。
2. **空气币泛滥**：代币价值无锚定，与真实世界的价值创造无关，沦为投机工具。

### 1.3 我们要解决的问题

> 如何构建一个系统，使得**电力 → 算力 → 认知**的每一步转化都可验证、可度量、可激励，并让代币真正锚定"人类认知的提升"？

---

## 2. 核心理念：从链到图

传统区块链是**线性账本**（Block → Block → Block），记录"谁转了多少币"。知行图主张：**人类认知是一张图，不是一条链。**

认知有高峰（核心共识）、有低谷（未知领域）、有连接（跨学科桥梁）、有冲突（范式争议）。用一条线性的链无法表达这种拓扑结构。因此知行图采用**有向加权图（DAG + 多层拓扑）**作为核心数据结构。

| 维度 | 传统区块链 | 知行图 |
|---|---|---|
| 数据结构 | 线性链表 | 有向加权图（DAG） |
| 节点 | 交易 | 概念 / 命题 / 证据 / 贡献者 |
| 边 | 引用上一区块 | 蕴含 / 反驳 / 扩展 / 引用 |
| 共识 | 计算哈希（PoW/PoS） | 认知验证（PoK） |
| 价值锚定 | 无 / 算力 | 电力 + 算力 + 认知增量 |
| 可视化 | 区块浏览器 | **人类认知地形图** |
| 目的 | 转移价值 | **提升并共享人类认知** |

**任何人打开知行图，看到的不是"区块高度"，而是一张 3D 认知地形图——这是人类第一次能直观看见"我们已知什么、未知什么、正在突破什么"。**

---

## 3. 三层价值映射：电力—算力—认知

这是知行图最核心的机制。价值从物理世界流向认知世界，逐层锚定、逐层证明。

```
┌─────────────────────────────────────────────────────┐
│  Layer 1 · 物理层 (Physical / Energy)                │
│  绿色发电 / 储能 / 余电回收                            │
│  证明：智能电表 + Oracle + 零知识证明 (PoE)           │
└──────────────────┬──────────────────────────────────┘
                   ▼  E_audit (kWh, 绿色加权)
┌─────────────────────────────────────────────────────┐
│  Layer 2 · 计算层 (Compute)                          │
│  GPU 集群 / 推理节点 / 分布式训练                     │
│  度量：FLOPs·s / token·s / epoch·step                │
│  证明：TEE + 可验证计算 (PoF)，剔除无效算力            │
└──────────────────┬──────────────────────────────────┘
                   ▼  F_useful
┌─────────────────────────────────────────────────────┐
│  Layer 3 · 认知层 (Cognition)                        │
│  知识图谱增量 / 模型权重 / 推理路径 / 数据集           │
│  证明：同行评议 + 引用网络 + 复现挑战 (PoK)            │
└──────────────────┬──────────────────────────────────┘
                   ▼  ΔK （认知增量）
              ┌──────────┐
              │  $COG    │  ← 仅当 ΔK > 0 时增发
              └──────────┘
```

### 3.1 认知贡献值公式

```
C = α · E_audit + β · F_useful + γ · ΔK
```

| 符号 | 含义 | 度量 |
|---|---|---|
| `E_audit` | 经审计的有效电力 | kWh（绿色电力加权） |
| `F_useful` | 产生有效认知输出的算力 | FLOPs / tokens / 训练步数 |
| `ΔK` | 认知图谱中新增或强化的知识 | 新节点 / 新边 / 已验证命题 |
| `α, β, γ` | 动态权重系数 | 由治理调节 |

### 3.2 核心原则

> **电力消耗不直接换取代币，必须穿过"认知增量 ΔK"这面透镜。**

即：`ΔK = 0` 时，无论消耗多少电力和算力，都不增发 $COG。这是知行图区别于 PoW 的根本所在——它奖励的不是"消耗"，而是"消耗所产生的认知"。

### 3.3 三种证明

| 证明 | 全称 | 解决的问题 | 技术手段 |
|---|---|---|---|
| **PoE** | Proof of Energy | 电力确实来自绿色/合规来源 | 智能电表 IoT + Oracle + ZK + I-REC/GEC 绿证 |
| **PoF** | Proof of useful Compute | 算力确实用于有效任务 | TEE（SGX/TDX）+ 可验证计算（zkML/opML） |
| **PoK** | Proof of Knowledge | 产出确实是新增认知 | 同行评议 + 引用网络 + 复现挑战 |

---

## 4. 认知图谱数据结构

### 4.1 节点类型

| 节点 | 说明 | 示例 |
|---|---|---|
| `ConceptNode` | 概念 | "Transformer"、"意识"、"素数" |
| `PropositionNode` | 命题 | "P ≠ NP"、"意识可计算" |
| `EvidenceNode` | 证据 | 论文、数据集、实验记录 |
| `ContributorNode` | 贡献者 | 个人、机构、AI Agent |
| `ServiceNode` | 服务 | 提供推理/训练的接口 |

### 4.2 边类型与权重

| 边 | 语义 | 权重 |
|---|---|---|
| `ENTAILS` | 蕴含 | 置信度 w ∈ [0,1] |
| `REFUTES` | 反驳 | 反驳强度 w ∈ [0,1] |
| `CITES` | 引用 | 引用次数 |
| `EXTENDS` | 扩展 | 创新度 w ∈ [0,1] |
| `DISPUTES` | 争议 | 争议度 w ∈ [0,1] |
| `POWERED_BY` | 算力支撑 | FLOPs |
| `ENERGIZED_BY` | 电力支撑 | kWh |

### 4.3 节点数据结构（示意）

```json
{
  "id": "prop:0x8f3a...",
  "type": "PropositionNode",
  "content": "缩放定律在参数量 > 10^13 后出现边际递减",
  "domain": "ai/scaling-laws",
  "author": "contrib:0x1c4d...",
  "created_at": 1789000000,
  "evidence": ["ev:0xaa..", "ev:0xbb.."],
  "resource_proof": {
    "energy_kwh": 12840.5,
    "green_ratio": 0.92,
    "compute_flops": 3.1e21,
    "poe_proof": "zk:...",
    "pof_proof": "tee:..."
  },
  "review": {
    "status": "validated",
    "reviewers": 7,
    "score": 0.83,
    "replications": 2
  },
  "delta_k": 0.61
}
```

---

## 5. PoK 共识机制（认知证明）

传统共识回答"谁记账"，PoK 回答"哪些认知是可信的、值得被记入图谱并获得奖励的"。

### 5.1 流程

```
提交 (Submit)
   │  贡献者提交命题/证据 + 资源证明 (PoE + PoF)，质押 $COG
   ▼
评议 (Review)
   │  随机抽选具备领域声誉的评议人（防合谋、利益回避）
   │  评议人对新颖性、正确性、可复现性打分
   ▼
挑战 (Challenge)
   │  挑战期内任何人可提交反驳或复现失败证据
   │  挑战成功者获得质押罚没的一部分
   ▼
定稿 (Finalize)
   │  综合评议分 + 引用网络 + 复现结果，计算 ΔK
   ▼
铸造 (Mint)
      ΔK > 0 → 铸造 $COG 与 cNFT，写入图谱
      ΔK ≤ 0 → 罚没部分质押，不入图谱
```

### 5.2 ΔK 的计算

```
ΔK = novelty × correctness × reproducibility × domain_gap_bonus
```

- `novelty`：与既有图谱的距离（越是填补"山谷"越高）
- `correctness`：同行评议加权分
- `reproducibility`：独立复现比例
- `domain_gap_bonus`：跨领域桥接奖励（连接原本孤立的子图）

### 5.3 抗女巫与防合谋

- 评议人从**领域声誉图**中按权重随机抽取，且强制利益回避；
- 声誉不可转让，只能通过高质量评议与被验证的贡献累积；
- 引入**二次方评议（quadratic review）**，抑制大户操纵；
- 复现挑战机制提供事后纠错，声誉可因错误评议而衰减。

---

## 6. 代币经济学

### 6.1 代币体系

| 代币 | 名称 | 作用 | 供应 |
|---|---|---|---|
| $COG | 认知币（主币） | 价值结算、治理、激励 | 动态增发（无固定上限，受 ΔK 约束） |
| $WATT | 瓦特（辅币） | 电力凭证，1:1 锚定 kWh | 与实际绿色电力等量 |
| $FLOP | 算力凭证（辅币） | 锚定 FLOPs | 与实际有效算力等量 |
| cNFT | 认知贡献证书 | 记录具体贡献，不可分割 | 按贡献铸造 |

### 6.2 $COG 增发规则

```
mint($COG) = base_emission × impact_score(ΔK) × time_decay
```

- `base_emission`：基准增发率，由治理调节；
- `impact_score`：ΔK 的单调递增函数（图谱增量 + 评议分 + 引用分）；
- `time_decay`：越早填补认知空白，奖励越高（鼓励探索"山谷"）。

> **硬约束：$COG 不能仅凭电力或算力增发，必须存在可验证的 ΔK > 0。**

### 6.3 $COG 销毁场景（通缩压力）

1. 调用高价值认知服务（推理、咨询、模型 API）；
2. 在图谱中发布新命题需支付 Gas（防垃圾）；
3. 竞拍"优先验证权"；
4. 跨链桥结算费用。

### 6.4 现实价值闭环

```
绿色电力供应商 ──$WATT──▶ 算力提供者 ──训练/推理──▶ AI 开发者
                                                      │
                                              提交 ΔK │
                                                      ▼
使用者/投资者 ◀──消费认知服务/回购── $COG ◀── 认知贡献者
```

每一枚 $COG 背后都有：真实流过的绿色电力、真实运行的有效算力、真实新增的人类认知——**这就是"不是空气币"的物理与认知双重基础。**

---

## 7. 技术架构

### 7.1 分层

```
┌──────────────────────────────────────────────┐
│  应用层  Cognition IDE / Explorer / DAO       │  可视化认知地形
├──────────────────────────────────────────────┤
│  共识层  Epistemic Consensus (PoK)            │  同行评议 + 引用加权
├──────────────────────────────────────────────┤
│  状态层  Cognitive Graph (DAG)                │  节点与边的最终性
├──────────────────────────────────────────────┤
│  资源层  Compute & Energy Oracle              │  PoE + PoF
├──────────────────────────────────────────────┤
│  物理层  GPU / 电网 / 储能                     │
└──────────────────────────────────────────────┘
```

### 7.2 关键模块

| 模块 | 职责 | 候选技术 |
|---|---|---|
| Energy Oracle | 对接电网 IoT、绿证 | 智能电表 + I-REC/GEC + ZK |
| Compute Verifier | 验证有效算力 | TEE（SGX/TDX）、zkML、opML |
| Cognition Engine | 论文/模型/数据集 → 图谱节点 | LLM 抽取 + 本体标准化 |
| Peer Review DAO | 链上分布式同行评议 | 声誉图 + 二次方投票 |
| Graph Explorer | 3D 认知地形可视化 | WebGL / 图数据库（Neo4j 等） |

### 7.3 性能架构与技术选型（Rust）

系统的**热路径**集中在认知图谱引擎：kNN 近邻检索（用于 ΔK 的 novelty 与跨域桥接）在每次提交、每个共识轮次都被调用，随图谱规模增长成为吞吐瓶颈。据此采用**分层语言选型**——用合适的语言做合适的事：

| 层 | 关注点 | 选型 | 理由 |
|---|---|---|---|
| 认知图谱引擎 / ΔK / kNN | **吞吐、可预测延迟** | **Rust** | 紧凑数值循环、无 GC 抖动、可 SIMD、可嵌入 |
| PoK 共识 / 节点运行时 | 安全、并发 | Rust | 内存安全 + 高并发，适合链上关键路径 |
| 经济仿真 / 数据管线 | 迭代速度 | Python | 生态丰富、便于建模与参数扫描 |
| 认知地形图前端 | 可视化 | TS + WebGL | 交互式 3D 渲染 |
| 契约互通 | 一致性 | FFI / WASM / pyo3 | Rust 引擎导出给上层复用 |

**实证结果**：已实现 Rust 版认知图谱引擎（仓库 [`engine/`](../engine/)，纯 std、零依赖），与 Python 参考实现（`sim/delta_k.py`）**共享同一份 B.2.3 ΔK 契约**。同一工况（N=20000 节点、M=2000 次提交）基准：

| 实现 | 吞吐 | 相对 |
|---|---|---|
| **Rust 引擎** | **≈ 10,900 submissions/sec** | **≈ 230×** |
| Python 参考 | ≈ 47 submissions/sec | 1× |

> 两侧输出校验和一致（157.48 vs 157.40，差异仅 f32/f64 舍入），交叉验证 Rust 端口忠实于契约、无实现漂移。这为主网节点在图谱规模化后仍能实时计算 ΔK 提供了工程可行性证据。

**设计原则**：性能关键路径用 Rust 并以校验和与 Python 参考实现交叉验证；建模与仿真保留在 Python；两者绑定同一份形式化契约（B.2.3），避免"文档、仿真、实现"三者漂移。生产环境的 kNN 应从暴力扫描升级为 HNSW/IVF 等近似最近邻索引。详见 [`engine/README.md`](../engine/README.md)。

**认知图谱的轻客户端可证明查询**（参考节点 M25–M29）：钱包在不下载整个图谱的前提下，可对 cert-signed 区块头（来自 BFT 共识的最终性证书）发起四类图谱查询（单点、邻域、同高度范围、时序 diff）与任意混合——M29 的异构批 SPV 传输把四类查询放在**单次往返**里取齐——全部由 Rust 节点状态机按全量重放成本为零的方式核对：

| 原语 | 用途 | 承诺根 | 复杂度 |
|---|---|---|---|
| M25：图节点包含证明（`ProofEntry::GraphNode`） | 证明某 node_id 在某高度图谱中 | `accounts_root`（插入序） | O(log n) |
| M26：邻域证明（`KnnClaim`，`verify_knn_against_header`） | "embedding 附近的 k 个邻居是谁" | `accounts_root`（插入序） | O(k) |
| M27：范围查询（`RangeClaim`，`verify_range_against_header`） | "cos_sim(query, n) ≥ θ 的全部节点" | `graph_root`（按 `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)` 排序索引） | O(cut) |
| M28：时序 diff（`DiffClaim`，`verify_diff_against_headers`） | "H₁ 到 H₂ 之间图节点 added/dropped 是哪些" | `accounts_root`（两侧各自高度） | O(diff) |
| M29：异构批 SPV 传输（`BatchResponseEnvelope`，`verify_batch`） | 一次往返取齐任意 (Inclusion + KNN + Range + Diff) 混合 | 各 slot 自带承诺根（dispatch 回 M24/M26/M27/M28） | O(Σ 各 slot) |

四类查询的 `prover`（全节点）都**不可信**：前三类由钱包在本地重排/重切，M28 由钱包侧局部重放派生 added/dropped；仅信任 cert-signed header 与 BFT 证书的最终性。同形信任模型让"证图谱"与"证账户"走同一条 SPV 总线（`GetProof/Proof`，M24），M29 把总线扩展为 `GetBatch/Batch`（wire tags 12/13）——单一 envelope 内 dispatch 回 M24/M26/M27/M28 四个现有验证器，**无新 SPV 逻辑**，仅一层 dispatch；上限 `MAX_BATCH_ITEMS = 32` 复用 M24 容量，per-primitive 错误通过 dispatch 转发，仅协议违规（kind/count mismatch）作为新 `LightError` 变体出现。M28 闭合**时序轴**——M25 的 append-only 不变式使 `changed` 臂结构上不可达（`engine::CognitiveGraph::add` 是唯一 mutator，`node_id == insertion_index` 单调），删除亦被 M25 拒绝，故 diff 退化为 `{added, dropped}` 两臂；钱包只需 M22 的 `LightGossipNode.headers` 缓存中 `[H₁+1..=H₂]` 的认证头即可做部分重放，prover 的每条 leaf 仍走 per-height `accounts_root` 的 Merkle 证明（不像 M27 走 `graph_root`——cosine 排序不保 `node_id` 序，无法界定 diff 大小）。详见 [`node/README.md`](../node/README.md) §"图节点 cert-signed 包含证明/邻域证明/范围查询/时序 diff/异构批 SPV 传输"。

**Python 绑定（pyo3）**：同一个 Rust 引擎通过 pyo3（abi3，无需 maturin）导出为 Python 扩展模块 `zhixing_engine`，使经济仿真（`sim/`）在**不改变契约**的前提下把 ΔK 热路径交给 Rust——既加速离线参数扫描，也加速 ABM 仿真本身。实测：

| 工作负载 | 纯 Python | Rust（pyo3） | 加速比 | 一致性 |
|---|---|---|---|---|
| 参数扫描（18 组 × 400 提交） | ≈ 31.7s | ≈ 0.14s | **≈ 225×** | 校验和相对误差 1.5e-7 |
| 整条 ABM 仿真（baseline，200 轮） | ≈ 11.8s | ≈ 0.12s | **≈ 96×** | 逐指标完全一致 |

> ΔK 只是 ABM 每轮工作的一部分（评审抽样、复现、记账仍在 Python 侧），故整条仿真加速比低于纯 ΔK 基准，但结果零漂移——同一种子下 Python 与 Rust 后端产出逐字节相同的指标，印证"一份契约、多种运行时"的架构目标。`sim/run.py --compare` 可复现该对比。

**节点运行时（[`node/`](../node/)，M6–M36）**：ΔK 引擎之上的最小 PoK 共识状态机：确定性区块/交易/账户、ΔK 定稿铸造或罚没入 treasury、供应守恒、链上评审声誉；ed25519 签名交易；追加式块日志 + 崩溃安全重放（落盘与区块哈希共用一份编码，重放得到**逐字节相同**的 `state_root` `1a2ec34c…9935b0`）；确定性 mempool + 试算式 `build_block`（按 tx 哈希规范排序出块，到达顺序无法改变区块哈希）；**Merkle 认证状态**——accounts/reviewers 的二叉 Merkle 树（`merkle_root` `7a08962f…cccdd1`）支持**轻客户端单账户包含证明**；**BFT 最终性内核**——按投票权（质押）计票、需严格 > 2/3 总权的 ed25519 预提交组成**可验证的 `Commit` 最终性证书**，配合 Tendermint 提议人优先级选择与 `detect_equivocation` 双签问责；**驱动活性的 BFT 轮次状态机**——忠实转写 Tendermint（Buchman–Kwon–Milosevic 2018）的 `upon` 规则（propose/prevote/precommit + 超时 + `lockedValue`/`validValue` 锁定 + 换轮），提议人宕机时经超时**换轮**由确定性轮换出的新提议人接手并定稿同一区块，锁定规则保证跨轮永不最终化冲突区块；**逐高度生长的 BFT 认证链驱动**——`ChainDriver` 把 mempool 造块 → 共识定稿 → `Chain` 提交串成一条链，每个已提交区块都由**复验过的 > 2/3 证书**背书，低于 1/3 权重宕机仍生长（活性），达到 1/3 则**安全停摆**而非无证书出块，且相同输入的两台驱动器长出逐字节相同的链；**证书落盘 + 重放即最终性复验**——`Commit` 证书与区块同格式落盘（`certs.log` 与 `blocks.log` 逐高度对应），`replay_verified` 在应用每个区块前复验其证书**恰好绑定该区块**且构成*该高度生效验证人集*下的真正 > 2/3 法定人数，从而重启后恢复的是**最终性**而非仅确定性状态——丢弃、调换或伪造任一证书都会被拒（对照纯状态重放分辨不出"已最终化"与"未最终化"的链）；**链上/动态验证人集**——验证人集是折入 `state_root` 的链上共识状态，区块可携带增删/改权（`power==0` 删除、`power>0` upsert），由**变更前的集合**认证、**下一高度生效**（新加入者绝不为自己的加入投票），驱动与重放对称跟随交接，用错误的创世验证人集重放即被拒；以及**P2P 网络与反熵状态同步**——节点间用 gossip 传播两样跨网工件：**待处理交易**（epidemic 泛洪，内容哈希 `seen` 去重使泛洪一次即终止）与**认证块**（区块 + 其最终性证书，反熵拉取：落后节点凭 `Status` 得知差距后 `GetBlocks → Blocks` 追赶），且每个认证块**仅当**其证书构成*该高度生效验证人集*下真正的 > 2/3 法定人数并恰好绑定该块时才应用（与 `replay_verified` 同一道校验），伪造或掉包的证书让同步**停在缺口**而非污染状态；高度内的投票 gossip 仍属验证人内部、留在 `round::Sim`（进程内投票总线），跨网传播的是已最终化、可自证的结果——`GossipNode` 是不做 I/O 的**纯状态机**，确定性 `Network` 让 N 节点收敛到逐字节相同的 head/`state_root`，同一套 wire 消息之上再加薄薄的 `u32` 长度前缀分帧即可跑在真实 TCP 上；以及**质押绑定的验证人权重与解绑期**——验证人的**权重**由质押背书：账户**自绑定** $COG 即成为验证人、权重严格等于其绑定的 micro-$COG（恒等映射、无舍入），由区块携带的 `stake_ops`（`Bond`/`Unbond`）驱动，解绑经时间锁提款队列（`UNBONDING_PERIOD = 3` 个高度，资金留池、仍可罚没、到期返还）——`bonded`/`bonds`/`unbonding` 三者均折入 `state_root`，故绑定改变状态根、`merkle_root` 不含绑定保持不变，供应守恒升级为 `Σ余额 + treasury + bonded + Σ解绑中金额 == supply`，全程有测试守护；以及**按证据罚没等价双签**——把 M11 的双签*检测*接上真正的*经济惩罚*：区块携带 `slashing_evidence`（同验证人同 (height, round) 对两个不同 `block_hash` 的预提交、各带有效签名），`apply_evidence` 先校验证据良构、作恶者属*当前生效*验证人集、两签均由其公钥验证通过，再把其绑定池与仍在解绑中的金额**罚没入 treasury**（解绑期正是为此留的安全窗口）、下一高度经 `power==0` 派生更新**移出验证人集**——证据纳入区块哈希但只把其*效果*（减少的 `bonds`/`bonded`、增长的 `treasury`）折入 `state_root`，坏证据整块回滚，供应守恒不变；以及**P2P 传播块级 ops**——把"作恶可被任何人检举、检举必定落地"的口径从"对出块方而言"收紧到"对网络而言"：扩展 `GossipMsg` 加 `Evidence(SlashEvidence)` / `StakeOp(StakeOp)`，节点上各开一个内容哈希去重的**待打包池**（`pending_evidence` / `pending_stake_ops`，键即 `SlashEvidence::hash` / `StakeOp::hash`），任何节点观察到双签/签发质押变更后即可 flood，下一区块的出块方 `take_pending_*` 灌进 `ChainDriver.pending_*`，`produce` 出的下一个区块自然把它们带出去。**形状即足够**（仅做 `is_well_formed()` 与去重），真正的密码学校验（offender 是活跃验证人、两签有效、不是陈旧）依然只在 `chain.commit.apply_evidence` / `apply_stake_op`，与 M18 的纪律一致；确定性 FIFO `Network` 总线让 N 节点的待打包池逐字节相同，gossip determinism 不变；以及**验证人集变更的轻客户端跟随协议**——给钱包/轻客户端一条不必全量重放就能知道"某高度谁能定稿、各多少权重"的捷径：`ValidatorTracker` 只凭**创世**这一信任根，消费全节点已经经 `GossipMsg::Blocks` gossip 出去的 `(块, 证书)` 对，对每个高度 (1) 用**当前跟随到的集合**复验最终性证书（> 2/3 权重、真实签名），(2) 复刻该块引起的验证人集迁移（`validator_updates` + 由 `stake_ops`/`slashing_evidence` 派生的权重变化）——**不执行任何交易、不追踪账户余额**。因这些输入全在 `block_hash`（证书所签）之内，派生出的集合**就是**链上集合（可证明而非可信：不攻破 > 2/3 签名就无法引到假集合），`follow` 的结果与 `Chain::replay_verified` 在每个高度**逐字节一致**；严格镜像既完整又可靠——`bonds` 完全由认证块的 `stake_ops`+证据决定（创世为空、解绑到期不动集合），账户公钥创世后不可变故一次性从 `Genesis.accounts` 播种即永不缺失，罚没派生出的是 `power==0`（移除）此时不看公钥。以及**验证人集 Merkle 承诺入区块头**——把 M20 的跟随收紧成 SPV 原语：把"下一高度生效的验证人集"的 Merkle 根 `next_validators_root`（每个验证人的叶字节 `u64 id ‖ raw pubkey ‖ u64 power` 与 `state_root` 三元组逐字节相同）折进区块头、纳入证书所签的 `block_hash`，轻客户端遂能 (1) **免复刻迁移**地凭一份证书验证整套下一验证人集（`follow_committed`：验完证书后仅比对给定集合的 `merkle_root == block.next_validators_root`），或 (2) 用 O(log n) 包含证明对 cert 签名的头**证明单个验证人**属于认证 `H+1` 的集合（`verify_membership`，即 SPV 原语）；`follow` 亦逐高度把迁移导出的集合对该承诺根**交叉校验**，令推导被共识确认。承诺的是 **post-apply 的"下一"集合**（认证 `H+1` 那套），与既有跨高度规则一致；**无循环依赖**——迁移从不读 `next_validators_root`，故导出集合与该字段取值无关：出块方共识前 `Chain::seal`（在 trial 克隆上跑不带强制的迁移、取根写回），`apply_block` 提交时强制 `block.next_validators_root == self.validators.merkle_root()`（否则 `ValidatorRootMismatch`）。以及**头部的轻同步传输**——把 M21 的 SPV 原语搬到 gossip 总线上：`BlockHeader` 是 `Block` 的证书签名投影——除 `txs` / `stake_ops` / `slashing_evidence` 之外的全部字段、外加每份体的 SHA-256 承诺（`txs_commitment` / `stake_ops_commitment` / `evidence_commitment`，空体→全零承诺），`Block::hash` 现改为哈希同一份 header 投影，使**任意区块**都满足 `header.hash() == block.hash()`——证书签的 `block_hash` 即可与头哈希同源，`CertifiedHeader = (BlockHeader, Commit)` 比 `(Block, Commit)` 小一整个体字节；新增 gossip 变体 `GetHeaders { from }` / `Headers(Vec<CertifiedHeader>)`、新节点类 `LightGossipNode`（伴随 `GossipNode`，只保留头 + 一个 `ValidatorTracker`，**从不反序列化**一笔交易）+ 新总线 `LightNetwork`（全/光节点共处，`Headers` 批次由总线经一条 `next_set_for` 边带喂给光节点的 `apply_header(ch, &next_set)`）；`ValidatorTracker` 增 `follow_header` 与 `verify_membership_against_header` 两条 API：`follow_header` 走"对承诺根的过渡免迁移"路径（与 `follow_committed` 同结构，输入换成 `BlockHeader`），`verify_membership_against_header` 是 `verify_membership` 的"只对头"对偶——同一套 SPV 契约，**纯头**、`Block::hash` 与 `header.hash()` 逐字节同源、证书签的就是这个哈希。最后给钱包一步到位的"账户-成员 SPV"：把 M22 头里再加两条**对钱包至关重要的承诺根**——`state_root`（`ChainState::state_root()` 的完整共识状态 digest，accounts/reviewers/graph/validators/bonds/bonded/unbonding/treasury/supply 一锅 SHA-256；钱包不重算此根——证书签的是 `header.hash()`，而根就在 `header` 字段里，由签名它的 > 2/3 验证人集**替钱包**承担校验）与 `accounts_root`（accounts ∪ reviewers 二叉 Merkle 根，供 O(log n) 包含证明），让钱包只凭头与对端的账户证明，就能在**不下载任何交易体、不重放任何状态转移**的前提下验证自己的余额。新 gossip 变体 `GossipMsg::GetAccountProof { id }` / `AccountProof { id, account, proof }`（wire tag 8/9）——全节点从 `chain.state.account_proof(id)` 现取现发，光节点入 `account_proofs` 缓存由 `take_account_proof(id)` 取用；新 SPV `verify_account_membership_against_header(header, cert, tracked_set, id, account, proof)` 在本地**重算** `leaf = leaf_hash(account.merkle_leaf(id))` 并验证 `merkle::verify(&header.accounts_root, &leaf, proof)`——根本不必相信对端给的 leaf，`verify_state_root_against_header` 则把"完整状态 digest 由证书代验"的契约写明。`Chain::commit` 走既有的"克隆 trial → apply → 拿根写回"路径，把两条根一并盖到 `block` 上；`apply_block_inner` 多两条强制度——`StateRootMismatch` / `AccountsRootMismatch`，与 M21 的 `ValidatorRootMismatch` 同形同序；`Block::hash` 哈希新的头投影，故 `header.hash() == block.hash()` 仍恒成立，证书绑定的 `block_hash` 与头哈希同源。最终性证书 + Merkle 状态根让轻客户端能**离线**验证"某区块已被 > 2/3 权重最终确定，且该账户确属此状态"。当前共识的**安全性与活性**内核、端到端的**链生长**、**最终性的持久化与复验**、**链上动态验证人集**、**P2P gossip 与反熵同步**、**质押绑定权重 + 解绑期**、**按证据罚没等价双签**、**块级 ops 走 gossip**、**轻客户端跟随验证人集**、**验证人集 Merkle 承诺入区块头**、**头部的轻同步传输**、以及**钱包的账户-成员 SPV**均已就绪；以及 **M24：批量化、类型化的统一 SPV 原语**——把 M23 单账户单 trick 的 `GetAccountProof/AccountProof` 传输**完全替换**为**一对**通用 `GossipMsg::GetProof { items }` / `Proof { items }`（wire tags 8/9 复用给新对），承载任意混合 `[(Account|Reviewer|Validator, id), ...]` 列表，**单次往返上限 `MAX_PROOF_BATCH = 32`**；新 `Reviewer::merkle_leaf()` + `ChainState::reviewer_proof(id)` 闭合审阅人包含证明的路径（审阅人叶一直就在 `accounts_root` 二叉树里，只是没有 typed producer）；新 typed `ProofEntry` 枚举（Account/Reviewer/Validator 三变体）携带 typed leaf + proof，wallet 端 M22/M23 的 `verify_membership_against_header` / `verify_account_membership_against_header` **全部删除**，只剩**一个** `ValidatorTracker::verify_proof_against_header(header, cert, tracked_set, entry)` 调度器，按 `entry.kind()` 选根——Account/Reviewer 对 `accounts_root`，Validator 对 `next_validators_root`，本地重算 leaf、零信任 prover；`cmd_account` demo 把三证明（账户余额 + 审阅人声誉 + 验证人集合成员）放在一次 GetProof 里并发证，全部对同一 cert-signed 头成立。以及 **M25–M28：认知图谱的 cert-signed 轻客户端可证明查询**——把图谱节点本身也搬上 M24 的 SPV 总线：`engine::GraphNode` 加单调 `node_id` 字段（`node_id == insertion_index` 不变式），`ChainState::merkle_leaves` 增第三段承载 graph 节点（插入序），`BlockHeader.accounts_root` 自动扩展覆盖 accounts ∪ reviewers ∪ graph 三集合的同一 Merkle 根；新 `ProofKind::GraphNode = 3` 与 `ProofEntry::GraphNode { node_id, graph_node, proof }` 走 M24 同一 `GetProof`/`Proof` 总线；`verify_proof_against_header` 加 `GraphNode → accounts_root` 一支闭合单点包含（M25）。邻域证明（M26）以 `engine::CognitiveGraph::k_nearest_with_ties` 按 cosine 降序+`node_id` 升序稳定排序，边界 ties 全留（`len ≥ k`）；`KnnClaim { query, k, neighbours }` + `verify_knn_against_header` 把单一 cert-signed header 拆为 cert 绑定 + 每个 neighbour leaf 对 `accounts_root` 的 Merkle 验证 + 本地 `cos_sim` 重排+同 cut + 与 prover 序列等比。同高度范围查询（M27）在 `BlockHeader` 新增 32-byte `graph_root` 槽位、`graph_merkle_root()` 按 `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)` 排序索引（`CANONICAL_PIVOT = [1,0,0,0,0,0,0,0]`，确定且 query-无关），`ChainState::graph_range_proof(a, b)` 返回 `(sub_root, entries)`，`RangeClaim { query, min_sim, nodes }` + `GossipNode::serve_range` 把 cosine-cutoff 派给全节点，`verify_range_against_header` 验 `min_sim ∈ [-1, 1]` + 每 node 对 `graph_root` 的 Merkle 验证 + 本地 `cos_sim` 重排+`sim >= min_sim` 的 prefix cut + 与 prover 序列等比。时序 diff（M28）闭合**时序轴**——钱包问"H₁ 到 H₂ 之间图节点 added/dropped 是哪些"无需下载两套图：新 `GraphLeafAtHeight { node_id, graph_node, proof }` + `DiffClaim { added, dropped }`（M25 append-only 下 `changed` 臂结构上不可达，删除被 M25 拒绝，故只两臂）；`ChainState::graph_diff(&prev_state)` 在 producer 侧构造 `{added, dropped}`，每个 leaf 的 `proof` 对**各自高度**的 `accounts_root`；`DiffEnvelope { header_prev, cert_prev, header_new, cert_new, diff, tracked_set_h1, tracked_set_h2 }` + `GossipNode::serve_diff(h1, h2, …)` 缓存 genesis 字段可重放 h₁ 状态；wallet 端 `verify_diff_against_headers` 拆为两边 header/cert 绑定+各自 tracked set 验签（动态验证人集下两个高度用不同集合）+ **wallet 端局部重放** `[1..=h₂]` 取 `state_at_h2`+`[1..=h₁]` 取 `state_at_h1`（与 prover 同重放路径）+ 每 leaf 对各自 `accounts_root` 的 Merkle 验证 + prover 与重放派生的 added/dropped 集合等比——**完整性由重放保障、单 leaf 证明只验身体**；M28 不走 `graph_root`（cosine 排序不保 `node_id` 序、无法界定 diff 大小），一律走 accounts_root（插入序）；wire tags `TAG_GETDIFF=10` / `TAG_DIFF=11`。四类图谱查询的 prover 全部**不可信**：M25/M26/M27 由钱包本地重排/重切，M28 由钱包局部重放派生 diff；仅信任 cert-signed header 与 BFT 证书的最终性。M29 把这一组 SPV 原语装入**单条总线**——`GossipMsg::GetBatch { items }` / `Batch { envelope }`（wire tags 12/13）承载任意混合 `Vec<BatchItem>`（`Inclusion{Kind,id} | Knn{query,k} | Range{query,min_sim} | Diff{h1,h2}`），`BatchResponseEnvelope { items: Vec<BatchResponseItem> }` 每 slot 由 `GossipNode::serve_batch` dispatch 回 M24/M26/M27/M28 四个现有 serve_*（无新 SPV 逻辑），wallet 端 `ValidatorTracker::verify_batch` 把每 slot 派回四个现有验证器；上限 `MAX_BATCH_ITEMS = 32`，新 `LightError::{BatchTooManyItems, BatchItemCountMismatch, BatchItemKindMismatch}` 三类**仅协议违规**错误——per-primitive 错误通过 dispatch 转发。M29 让钱包对图谱与账户的所有可证查询（单点 / 邻域 / 范围 / 时序 diff / 跨原语混合）走同一条 SPV 总线，prover 不可信，结构上不可信不存在于图谱系统：图谱的**每个**可证查询现在都能在钱包侧逐项验证。以及 **M30：信任无关跨链桥（relay + verify-from-counterparty）**——把 M22–M29 的 cert-signed SPV 机器**自然地**接到同协议两条链 A↔B（不同创世 → 不同 `genesis_hash`、对称可互为源/目的）上，构成一笔价值迁移：源链把币锁进 `BridgeLock`，relayer 把 `(header, cert, lock, proof)` 这套字节搬到目的链，目的链的"桥端"仅是**对源链的光客户端**——复用 M22 `verify_state_root_against_header`（用端点**自己的** `tracker.validators()`，relayer 替换不了验证人集）+ M25–M28 风格的 `merkle::verify`（对**新**的 `header.bridge_root`，不开 `accounts_root`）+ dest match + dedup 检查，无新 SPV 逻辑——这即里程碑的**正确性声明**：**桥 = 光客户端 + dedup 集**。新 `BridgeLock { account, amount, dest_chain: Hash, dest_account, nonce, sig }` 镜像 `StakeOp`（ed25519 签名 op，`is_well_formed()` + 余额覆盖 + account 已知 + nonce + `amount != 0` 复用 `ZeroStake`/`BadSignature`/`UnknownAccount`/`InsufficientBalance` 错误家族）；新 `BlockHeader.bridge_root: Hash` 槽位与 `graph_root` 同列、`Chain::seal` 盖/`apply` 强制度（`ChainError::BridgeRootMismatch`），头尾增 `bridge_locks_commitment`；新 `Block.bridge_locks: Vec<BridgeLock>` 与 `stake_ops` 同列进 apply；`ChainState` 增 `bridge_locked: u64`（新供应组分、`Σ余额 + treasury + bonded + unbonding + bridge_locked == supply`，lock 是 supply 内部再分配）/ `bridge_locks: BTreeMap<u64, BridgeLock>`（单调 `lock_id`、累计 append-only、proof 在任意 ≥ 创建高度仍有效）/ `bridge_lock_heights: BTreeMap<u64, u64>`（lock_id → 出块高，serve_lock 取头）/ `next_lock_id: u64`；`state_root` digest 多 fold 这四字段；新 `ChainState::bridge_lock_proof(id)` 镜像 `account_proof`/`graph_node_proof`。新模块 `node/src/bridge.rs`：`LockEnvelope { source_header, source_cert, source_tracked_set, lock_id, lock, proof }`（M28 `DiffEnvelope` 同形）+ `BridgeEndpoint { my_genesis_hash, source_genesis_hash, tracker: ValidatorTracker, consumed: BTreeSet<(Hash, u64)>, minted: BTreeMap<u64, u64> }` + `VerifiedLock` + `BridgeError::{Cert(LightError), WrongDestination, AlreadyConsumed, SourceNotFollowed}`；`verify_lock(env)` = (1) M22 cert-binding 用 `self.tracker.validators()`（**端点的**集合，不用 envelope 的），(2) inclusion `merkle::verify(&env.source_header.bridge_root, &leaf_hash(&lock.merkle_leaf(lock_id)), &env.proof)`，(3) dest match `lock.dest_chain == my_genesis_hash`，(4) replay `(source_genesis, lock_id) ∉ consumed`；`consume(v)` 入 dedup + `minted[dest_account] += amount`（bridge-module 记账——共识层不感知 mint）。新 gossip 对 `TAG_GETLOCK=14` / `TAG_LOCK=15`（继 M29 的 12/13 之后）+ `GossipNode::serve_lock(lock_id)`（用 `bridge_lock_heights` 取出块高 → 从 retained chain 取 header+cert、重放 `[..=height]` 派生 active set 作 `source_tracked_set`）；`LightGossipNode::locks: Option<LockEnvelope>` + `take_lock()`（继 M28 `take_diff` 之后）。**信任无关**：relayer 只搬运字节、无法伪造 A 验证人未签的锁——`node bridge` CLI 演示正路径 + tampered-proof / wrong-destination / replay / tampered-root 四类 `BridgeError` 负测。以及 **M31：共识级跨链赎回 + 铸造（目的链链上）**——把 M30 的**off-chain** `BridgeEndpoint::consume`（mint + replay-dedup）搬进**目的链状态机**，令 mint 由目的链验证人 BFT 强制、去重进 cert-signed 状态。两个自认证 block op：`BridgeHeader { source_chain, header, cert, next_set }` 推进**链上源链跟随器** `BridgeSource { set, head, height, consumed }`（`ValidatorTracker` 剥去 bonds/pubkeys——`follow_header` 只需 `{set, head, height}`，静态验证人集假设与 M30 一致），`BridgeRedeem { source_chain, source_header, source_cert, lock_id, lock, proof }` 对源链 cert-signed `bridge_root` 验锁后铸造到 `dest_account`。`Genesis.bridge_sources`（`(源创世 hash, 源创世验证人集)` = 信任锚，恰如 `Genesis.validators` 锚定本链 BFT，`genesis_split` 播种 `BridgeSource`）；`ChainState` 增 `genesis_hash`（本链身份、供 dest match、**排除出 `state_root`**——它由 `state_root` 经创世块哈希派生，折入即循环）/ `bridge_minted`（审计计数器镜像 `bridge_locked`）/ `bridge_sources: BTreeMap<Hash, BridgeSource>`；`Block` 增 `bridge_headers` / `bridge_redeems` 两 vec，apply 时 **headers 先于 redeems**。`apply_bridge_header` = 源已注册 + `header.height == s.height+1 && prev_hash == s.head` + cert-binding `verify_state_root_against_header` + `next_set.merkle_root() == header.next_validators_root` + 采纳。`apply_bridge_redeem` = 源已注册 + frontier `source_header.height <= s.height` + cert-binding + inclusion `merkle::verify(&source_header.bridge_root, &leaf_hash(&lock.merkle_leaf(lock_id)), &proof)` + dest match `lock.dest_chain == self.genesis_hash` + replay `!consumed.contains(&lock_id)` + `dest_account` 存在 + **mint**：`accounts[dest].balance += amount; supply += amount; bridge_minted += amount; consumed.insert(lock_id)`——验证全先于变更、坏 op 整块回滚。redeem **同增** `balance` 与 `supply`，故供应守恒 `Σ余额 + treasury + bonded + unbonding + bridge_locked == supply` **原样成立**（`bridge_minted` 只是审计镜像）。新 8 类 `ChainError::{UnknownBridgeSource, BridgeBadFollow, BridgeCertInvalid, BridgeNextSetMismatch, BridgeSourceNotFollowed, BridgeInclusionInvalid, BridgeWrongDestination, BridgeAlreadyRedeemed}`；codec 头再加 `bridge_headers_commitment` / `bridge_redeems_commitment` 两 32B 承诺（`decode_certified_header` 长度 +64B）+ `encode/decode_bridge_header` / `_bridge_redeem` + block 两新长度前缀 vec；`ChainDriver` 加 `stage_bridge_header` / `stage_bridge_redeem` + pending 池 + produce/clear 接线。relayer 仍**信任无关**：只搬 A 的 cert-signed 字节，唯有 A 验证人签名 + cert-signed `bridge_root` 授权 mint——`node redeem` CLI 演示正路径（A 锁 → B 链上跟随 → B 赎回铸造到 account 5、供应守恒）+ tampered-proof / tampered-root / wrong-destination / replay / not-followed 五类各自 `ChainError` 负测。以及 **M32：联网 tokio 守护进程（单定序器测试网）**——把 M6–M31 的**纯状态机**（`GossipNode::on_message` / `apply_certified`、`BlockLog`/`CertLog`）从进程内 `VecDeque` 总线搬上**真实 TCP**：新模块 `node/src/daemon.rs` 用**单属主 actor** 模式（`GossipNode` 独占一个 tokio task、per-peer `mpsc` 出站、**无 `Arc<Mutex>`**），帧格式为 `u32` 大端长度前缀 + `encode_gossip` 体并加 `MAX_FRAME = 16 MiB` 上限（既有阻塞版 `read_msg` 无上限），8 字节大端 id 握手在 `GossipMsg` 之外（wire tag 0..=15 范围不动）、只向 id 更大的 peer 拨号故每对恰一条连接；一个 `[producer].enabled` 节点为**过渡期定序器**，持全部验证人 seeds 经既有进程内 `Sim`/`ChainDriver` 定稿证书块，其余全节点仅经 socket 同步 + **逐块复验证书**（分布式 BFT 投票、每进程一把私钥留 **M33**），故 M32 的链工件仍逐字节确定、只有其传播被联网化；actor 是本节点日志的**唯一写者**，boot 经 `load_certified` / `ChainDriver::resume` 复验最终性恢复；新模块 `node/src/config.rs` 用 serde + toml **镜像结构**载 `NodeConfig`/`GenesisConfig`/`KeystoreConfig`（自带严格 hex 解码，共识类型 `lib.rs` 仍 **serde-free**）；`node run --config <path>` 起长驻守护进程、`node localnet` 进程内 4 节点经真实 loopback socket 收敛演示 + `testnet/` 样例配置。本里程碑为联网守护进程首次引入运行时依赖 `tokio`（异步）+ `serde`/`toml`（配置），打破"节点仅取签名库、其余纯 std"的旧口径——但引擎 `engine/` 仍**纯 std、零依赖**、共识核心 `lib.rs` 仍 serde-free，新依赖只落在 `node/` 的联网/配置边缘。以及 **M33：分布式 BFT 投票（无定序器）**——把 M32 仍中心化于单一定序器的共识**真正分布式化**：每个验证人节点各持**一把** ed25519 `Keypair` + 一个 `round::RoundState` FSM，`proposal`/`prevote`/`precommit` 经同一条 TCP 总线 gossip（新 `TAG_CONSENSUS = 16` + `GossipMsg::Consensus(Box<round::Msg>)`——proposal 体是 length-prefixed `Block`，故 proposer 编码字节与接收端 `block.hash()` 逐字节同源），round 推进由 **wall-clock 超时**驱动（`PROPOSE/PREVOTE/PRECOMMIT_TIMEOUT_BASE = 1000ms` + `TIMEOUT_DELTA = 500ms` 每 round 线性回退，落入最终同步性）。**seal/commit 幂等**是关键闩：`Chain::commit` 仅当根为 `[0u8;32]` 时才补盖，故 proposer 用 `Chain::seal` 封好的候选块经 commit 后逐字节不变 → 投票所依的 `candidate.hash()` == 提交后 `block.hash()`，已决区块直走既有 `apply_certified`，无需新的 apply 路径。`daemon.rs` 的单属主 actor 现**也独占** `kp: Option<Keypair>`（`None` 即纯 follower，只同步、永不投票）+ `cons: Option<Consensus>` + tokio timer 句柄，新 `Cmd::{StartHeight, Timeout}` 经 `self_tx` 自调度；**`GossipNode::on_message` 保持纯**——它既不知本节点私钥也触不到 timer，故对 `GossipMsg::Consensus` 直接 drop，共识在 actor 主循环里由 `Cmd::Inbound` 直接路由到 `RoundState::on_message`/`on_timeout`。三条活性/安全护栏：(1) **Byzantine-proposer 保护**——`on_consensus` 在 prevote 前用新的 `Chain::would_accept`（trial-apply 克隆、从不改 self）试跑，不能 apply 的 proposal 当作"proposer 缺席"丢弃（→ 超时 → prevote nil → 下一 honest proposer）；(2) **早到消息 lazily-start**——round-0 proposer 一提交上一高度即广播，可能先于慢节点自己的 `StartHeight` 到达，故 `on_consensus` 对本节点下一高度的消息先 `start_height` 再喂入，避免 `cons == None` 丢弃导致的假超时；(3) **sync 永远赢**——验证人只对 `node.height()+1` 跑共识，`reconcile_after_sync` 在任何高度经反熵推进后立即弃旧 round + arm 下一高度。config 用 per-node `[validator]{enabled, seed_hex}`（只带本进程一把私钥）替换 M32 的 `[producer]{keystore=all-seeds}`，启动时 `Keypair::from_seed(seed).public() != genesis.validators[id].pubkey` 即 fail-fast；`Sim`/`ChainDriver` 保留给 `bft`/`live`/`chain` 离线 demo + 单元测试，不进守护进程。4 等权验证人 quorum = `total_power*2/3 + 1 = 3`，故 3-of-4 持续推进（宕机节点当 proposer 时经换轮恢复）、2-of-4 安全停摆（永不伪造证书）；`localnet` 现为 4 验证人、零定序器，经真实 loopback socket 由分布式 prevote/precommit gossip 收敛。空块心跳（`build_candidate` 每 `BLOCK_INTERVAL = 1000ms` 出一空 sealed block）使各验证人逐高度同步起步，`create_empty_blocks = false` 是干净的后续优化。以及 **M34：观测到双签即主动罚没**——M33 补齐了 BFT 的活性，M34 补齐问责：一个 Byzantine 验证人**双签**（对同一 `(height, round)` 发两条 `block_hash` 不同的 precommit）从此被诚实节点**在投票到达时当场观测并主动罚没**。链侧惩罚（`Chain::apply_evidence` 没收 bond 入 treasury、下一高度移出）与 M19 证据传输（`GossipMsg::Evidence` flood + `pending_evidence` 暂存 + 入块 + dedup）早已齐备，唯一缺的是**到达时检测**：`round::RoundState::ingest` 现返回 `Option<SlashEvidence>`——摄入一条 precommit 时若已持有同 `(validator, height, round)` 的另一 `block_hash`，就按 `block_hash` **规范排序**（保证各检测者算出同一 `hash()` 以 dedup）组装 `SlashEvidence`，经新 `Action::Equivocation` 上抛；Actor 的 `on_equivocation` 调 `submit_local_evidence`（本地暂存 + flood）把它接入既有管线 → 传播 → 下一 proposer 入块 → 链上没收并移出 offender。到检测点的 precommit 都已验签、来自活跃验证人、且同高度（`ingest` 前置过滤），故任何冲突都是真实可归因的双签——无误报；first-wins 的 `or_insert` 保持不变（纯旁路观测，不改 FSM 安全性），prevote 双签在本模型不可罚没（保持先到先得），纯 follower（`kp=None`、无 `RoundState`）不检测但仍经 `on_evidence` 转发证据——验证人互相监督。无新 wire type / codec / config。以及**配置驱动共识时序 + `create_empty_blocks`**（M35）——把轮次超时（propose/prevote/precommit 基础 + 每 round 线性回退增量 `timeout_delta_ms`）与出块节奏 `block_interval_ms` 从编译期常量改为可选 `[consensus]` TOML 段的**文件可配**项，`ConsensusConfig::Default` 逐字段等于旧常量、成为这些数字的**唯一真源**（`daemon.rs` 五个常量删除）；并加 `create_empty_blocks=false`——验证人只在**有真实待办**（mempool ∪ 待打包 `stake_ops` ∪ 待打包证据，桥无待打包池故意不计入）时才起轮出块，空闲链**暂停**而非无限增长心跳块。门只加在被调度的 `on_start_tick`（空块关且无待办 → 不起轮、仅按 `block_interval_ms` 重排），而 `on_consensus` 的 lazy-start 路径**不设门**——peer 一提议即意味着有活，本节点即便尚未收到 gossip 的活也跟上那一轮，最坏不过多花一次 round-change 超时由持有活的提议者接手（既有换轮所依赖的同一最终同步性，**安全性不动**）；`net.rs` 新 `has_pending_work()`。段与字段两级 `#[serde(default)]` 使缺段的老 `testnet/*.toml` 与 `localnet` demo **逐字节行为不变**（后向兼容）。**配置驱动网络时序**（M36）——收尾 M35 顺延的两个**网络/守护进程生命周期**旋钮：新增可选 `[network]` TOML 段把反熵心跳间隔 `announce_interval_ms`（原 `const ANNOUNCE_SECS = 2s`）与验证人启动宽限 `startup_delay_ms`（原 `const STARTUP_DELAY = 1000ms`）从编译期常量改为文件可配，`NetworkConfig::Default` 逐字段等于旧常量（2000/1000）成为唯一真源、`daemon.rs` 两常量删除；单位秒→毫秒统一到 M35 的 `_ms` 约定（`2000ms == 2s` 行为一致）；`Node::start` 内联 hoist 出本地变量喂给心跳 `interval` 与启动 `sleep`；段/字段两级 `#[serde(default)]` 使缺 `[network]` 段的老配置与 `localnet` **逐字节行为不变**（仍收敛同一 head）。其余运维成熟度项（`tracing` 结构化日志、metrics/health、peer discovery、TLS/auth）顺延至 M37+。273 项测试（编解码含证书与 tx 与 stake op 与双签证据 wire 含 header/certified_header 编解码与不变式、签名、持久化含证书日志、hash、Merkle 树、验证人集含变更与集合 Merkle 承诺、BFT 证书、轮次状态机、认证链驱动、最终性复验、链上验证人集交接、P2P gossip 与反熵同步含头 gossip、质押绑定权重 + 解绑期生命周期、等价双签罚没、块级 ops 走 gossip、轻客户端跟随验证人集含 header-only `follow_header`、验证人集 Merkle 承诺入头、钱包 SPV 账户证明含双根强制度 + `verify_account_membership_against_header` + 端到端 gossip、M24 泛化批量化证明请求总线含 Account/Reviewer/Validator 同一 GetProof 对与 `verify_proof_against_header` 单验证器与 `MAX_PROOF_BATCH = 32` 上限与 reviewer_proof 闭合审阅人路径、状态机端到端、M25–M28 图节点 cert-signed 包含/邻域/范围/时序 diff 四类 SPV 原语 + M29 异构批 SPV 传输含 `BatchItem`/`BatchResponseItem`/`BatchResponseEnvelope` 类型 + `encode_batch_envelope`/`decode_batch_envelope` 协 + `verify_batch` 把每 slot 派回 M24/M26/M27/M28 四个验证器 + 三类仅协议违规 `LightError` + M30 信任无关跨链桥：`BridgeLock` 签名 op 含 well_formed/验签/余额覆盖/`amount != 0`/nonce 四道复用 StakeOp 家族错误（lib.rs），lock 扣余额入 `bridge_locked` 且供应守恒扩展、`bridge_lock_proof` 对 `header.bridge_root` 验证、`bridge_merkle_root` 累计跨高度稳定+新增变化、`BridgeRootMismatch` 强制度（lib.rs 四 bridge-* 测试），`BridgeEndpoint` 端点 = 光客户端+dedup+dest match+**自集合** cert-binding，含 valid / tampered-proof / wrong-destination / replay / tampered-root / source-not-followed / 双向对称 B↔A 七类（bridge.rs 七测试），`serve_lock` + `TAG_GETLOCK`/`Lock` wire 端到端 round-trip（net.rs 一测试）—— 共十二项新增；M31 共识级跨链赎回再加十三项——lib.rs 八项（redeem 铸造到 dest + 供应守恒 + `bridge_minted` 审计、`apply_bridge_header` 推进跟随器 height/head/set、未注册源 `UnknownBridgeSource`、未跟随即赎回 `BridgeSourceNotFollowed`、篡改金额失 inclusion `BridgeInclusionInvalid`、篡改 `bridge_root` 失 cert `BridgeCertInvalid`、错目的 `BridgeWrongDestination`、重放 `BridgeAlreadyRedeemed`、redeem 改 `state_root` 而 `accounts_root`/`bridge_root` 跨无 op 块稳定），codec.rs 三项（`BridgeHeader` / `BridgeRedeem` 往返 + block 携两新 op vec 往返、`decode_certified_header` 携两新承诺仍解析），driver.rs 一项（跨 driver A 锁 → B 链上跟随 → B 赎回、dest 链上入账端到端）——共二十五项；M32 联网守护进程再加九项——daemon.rs 四项（`write_frame`/`read_frame` 经 `tokio::io::duplex` 往返、超 `MAX_FRAME` 帧被拒、id 握手往返、3 个进程内 tokio 节点经真实 loopback 收敛到同一 head 并复验落盘证书最终性），config.rs 五项（`NodeConfig`/`GenesisConfig`/`KeystoreConfig` TOML 往返、`to_genesis()`/`to_seeds()` 复刻 demo 值、bad-hex/bad-addr 走 typed `ConfigError`、checked-in `testnet/` 样例载入且 keystore seeds 复刻 genesis 验证人 pubkey）；M33 分布式 BFT 投票再加八项——round.rs 一项（`decided_block` 访问器经 `Sim` 返回已决区块），codec.rs 一项（`consensus_msg_round_trip`：`Proposal`（`valid_round = -1` 与 `>= 0`）+ 两类 `Vote` 经公共 `encode/decode_consensus_msg` 往返），net.rs（`frame_round_trip_over_duplex` 扩展含 `GossipMsg::Consensus` 帧），daemon.rs 五项（`four_validators_converge_over_tcp` 四验证人零定序器经真实 socket 收敛并复验落盘证书 > 2/3、`one_crashed_validator_still_makes_progress` 3-of-4 活经换轮持续推进、`two_crashed_validators_stall_safely` 2-of-4 安全停摆高度不动、`late_joiner_syncs_then_participates` 晚加入经反熵追平再一起推进、`pure_follower_syncs_certified_chain` `kp=None` follower 只同步不投票），config.rs（`[validator]` 往返 + `validator_pubkey_mismatch_is_detectable` fail-fast + 重写 `checked_in_testnet_samples_load` 校验 4 节点 seed 复刻 genesis pubkey））；M34 观测到双签即主动罚没再加六项——round.rs 五项（`precommit_equivocation_yields_evidence` 同 `(validator,height,round)` 不同 `block_hash` 的两条 precommit → `Action::Equivocation(ev)` 且 `is_well_formed`、`duplicate_precommit_is_not_equivocation` 同 hash 重发幂等非双签、`precommits_in_different_rounds_are_not_equivocation` 跨轮 unlock/relock 合法、`prevote_equivocation_is_not_slashable` 本模型只罚 precommit、`equivocation_evidence_is_canonically_ordered` 两票任意到达序同一 `hash()` 保证跨节点 dedup）+ daemon.rs 一项（`equivocation_over_tcp_slashes_the_offender` 3 诚实验证人 + TCP 双签注入器 → 证据经 M19 flood 入块、链上没收 bond + 下一高度移出 offender）覆盖。

---

## 8. 认知地形图（Human Cognition Topography）

借鉴地理信息系统（GIS），把认知图谱渲染为可导航的地形：

| 地形元素 | 认知含义 | 计算方式 |
|---|---|---|
| 峰 (Peak) | 核心共识命题 | 引用数 × 验证人数 × 时间累积 |
| 高原 (Plateau) | 成熟稳定领域 | 邻居密度高 + 边权重稳定 |
| 山脊 (Ridge) | 跨领域桥梁 | 连接多个高密度子图的节点 |
| 山谷 (Valley) | 认知空白 | 低密度 + 孤立节点群 |
| 陡坡 (Cliff) | 范式转换 | 短期内边权重剧变 |
| 断层 (Fault) | 认知冲突 | 高 REFUTES + 高 DISPUTES |

任何人都能通过这张地图看到人类认知的全貌，找到自己能贡献的"山谷"，参与到认知提升中来——**这正是知行图激励机制的意义所在。**

---

## 9. 治理

- **治理代币**：$COG 持有者 + cNFT 持有者共同治理；
- **参数调节**：α/β/γ 权重、base_emission、Gas 费用等由链上提案调节；
- **声誉加权**：治理投票结合领域声誉，避免纯资本主导；
- **认知宪法**：明确"什么算认知贡献"的元规则，修改需超级多数 + 冷静期。

---

## 10. 路线图

### Phase 1 · 单机原型（0–6 个月）
- 选定垂直领域（如"AI 安全"或"分布式认知"）；
- PostgreSQL + Neo4j 搭建认知图谱；
- 手工录入 ~100 个核心命题；
- 实现电力/算力/认知三层最小闭环。

### Phase 2 · 社区激励（6–12 个月）
- 上线 $COG 测试网；
- 接入首个绿色电力供应商与首个开源 AI 项目；
- 用 cNFT 记录贡献者；
- 上线 Peer Review DAO 初版。

### Phase 3 · 主网 + 地形图（12–24 个月）
- DAG 主网上线；
- 发布 3D 认知地形图浏览器；
- 接入 10+ 认知领域；
- 与 OpenReview / arXiv / HuggingFace 互通。

---

## 11. 风险与开放问题

1. **伪认知贡献刷分**：如何设计评议机制防止合谋刷分？
2. **认知客观性**：如何在多元立场中保证图谱的相对客观？
3. **AI Agent 身份**：AI 是"贡献者节点"还是"工具"？其贡献如何归属？
4. **跨域对齐**：不同领域本体（ontology）如何标准化与互通？
5. **隐私 vs 透明**：个人认知贡献如何选择性披露？
6. **Oracle 可信性**：电力与算力数据源如何防伪、防篡改？
7. **冷启动**：早期图谱稀疏时，如何激励第一批探索者？

---

## 12. 结语

> **夜莺社区不应该是另一条链，而应是人类的第一张"认知地形图"——把电力变成算力，把算力变成认知，把认知变成全人类共享的山峰与山谷，并让每一个登上山峰、填补山谷的人获得 $COG。**

知行，知行合一。愿这张图，成为人类共同认知提升的坐标系。

---

## 附录 A · 术语表

| 术语 | 定义 |
|---|---|
| ΔK | 认知增量，PoK 中衡量新增认知的核心指标 |
| PoE | Proof of Energy，电力证明 |
| PoF | Proof of useful Compute，有效算力证明 |
| PoK | Proof of Knowledge，认知证明（共识机制） |
| cNFT | Cognitive Contribution NFT，认知贡献证书 |
| 认知地形图 | 认知图谱的可视化形态，以地形隐喻表达认知拓扑 |

## 附录 B · 待深化清单

> 状态说明：以下条目已从"待办"推进为"初稿（Draft）"，详见对应深化章节 B.1–B.6。仍需社区评审、形式化验证与仿真回归后方可定稿。此外，ΔK 契约已有可运行的双实现（Python 参考 + Rust 引擎，见 §7.3 与 [`engine/`](../engine/)），并由 pyo3 绑定使仿真直接运行 Rust 热路径、结果按种子逐字节一致。

- [x] PoK 共识的博弈论建模与攻击面分析 → 见 [B.1](#b1-pok-共识的博弈论建模与攻击面分析)（初稿）
- [x] ΔK 计算公式的形式化定义与参数标定 → 见 [B.2](#b2-δk-计算公式的形式化定义与参数标定)（初稿）
- [x] $COG 增发/销毁的经济仿真（agent-based simulation）→ 见 [B.3](#b3-cog-增发销毁的经济仿真agent-based-simulation)（初稿）
- [x] Energy Oracle 与绿证体系的对接标准 → 见 [B.4](#b4-energy-oracle-与绿证体系的对接标准)（初稿）
- [x] zkML / opML 在 PoF 中的可行性评估 → 见 [B.5](#b5-zkml--opml-在-pof-中的可行性评估)（初稿）
- [x] 认知图谱本体（ontology）标准草案 → 见 [B.6](#b6-认知图谱本体ontology标准草案)（初稿）

---

### B.1 PoK 共识的博弈论建模与攻击面分析

#### B.1.1 参与者与博弈结构

PoK 是一个多阶段、不完全信息的重复博弈。核心参与者及其策略空间：

| 参与者 | 策略空间 | 收益来源 | 成本 / 风险 |
|---|---|---|---|
| 贡献者 (Contributor) | {提交真实贡献, 伪造/抄袭, 灌水} | $COG 铸造 + cNFT | 质押 `S_c`、被罚没、声誉损失 |
| 评议人 (Reviewer) | {认真评议, 偷懒随机打分, 合谋} | 评议奖励 + 声誉增值 | 抽选质押、错误评议的声誉衰减 |
| 挑战者 (Challenger) | {提交有效反驳, 不参与, 恶意挑战} | 罚没分成 | 挑战质押 `S_ch` |
| 验证人/复现者 | {独立复现, 谎报复现} | 复现奖励 | 算力成本、谎报被发现的罚没 |

单轮博弈的贡献者期望收益：

```
E[U_contributor] = P_accept · (mint(ΔK) + value(cNFT))
                 − (1 − P_accept) · slash(S_c)
                 − cost_produce
```

设计目标：使 `诚实提交` 成为**子博弈精炼纳什均衡（SPNE）**，即对任意理性参与者，偏离诚实策略的期望收益为负。

#### B.1.2 攻击面清单与对策

| # | 攻击 | 机理 | 对策 | 博弈论依据 |
|---|---|---|---|---|
| A1 | 女巫攻击 (Sybil) | 单实体控制多身份刷贡献/评议 | 声誉不可转让 + 领域声誉需长期积累 + 抽选按声誉加权 | 制造有效身份的边际成本 > 边际收益 |
| A2 | 评议合谋 (Collusion) | 评议人串通抬高/压低分数 | VRF 随机抽选 + 利益回避 + 二次方评议 + 事后复现挑战 | 合谋需贿赂 ≥ 多数被抽中者，成本随抽样池指数上升 |
| A3 | 懒惰评议 (Lazy) | 不审直接给均值分套取奖励 | 引入"预测市场式"评分：偏离最终共识越远，奖励越低（peer-prediction / BTS 机制） | 说真话为占优策略 |
| A4 | 贿赂 / 暗箱 (Bribery) | 场外收买评议人 | 提交—评议采用 commit–reveal，评议前不知贡献者身份 | 提高协调与信任成本 |
| A5 | 随机数操纵 (Grinding) | 操纵抽选以选中同伙 | VRF + 未来区块熵作为种子，抽选结果不可预测/不可事后操纵 | 消除操纵杠杆 |
| A6 | 抄袭 / 重复提交 | 换皮已有认知骗取 ΔK | novelty 用语义嵌入 + 图距离检测近重复；引用溯源 | 使 `novelty ≈ 0`，ΔK 归零 |
| A7 | 抢跑 (Front-running) | 抢先提交他人预印内容 | 时间戳承诺（commit hash 先行）+ 原创性溯源 | 先承诺者得优先权 |
| A8 | 罚没规避 | 贡献者拿钱后弃质押 | 质押锁定期覆盖挑战期 + 声誉长期绑定 | 使违约净收益为负 |

#### B.1.3 关键激励约束（待形式化验证）

- **诚实提交约束**：`P_accept·mint(ΔK) − (1−P_accept)·slash(S_c) − cost > 灌水策略收益`
- **诚实评议约束（peer-prediction）**：评议奖励 `R_review = f(与后验共识的一致性)`，使 `E[R|诚实] > E[R|随机]`
- **挑战有效性约束**：`slash 分成 > S_ch`，保证有利可图，但恶意挑战因需自押而被抑制

> ⚠️ 开放问题：peer-prediction 机制在评议人数量少、领域高度专业化时的稳健性；VRF 抽样池过小导致的合谋阈值下降。需在 B.3 仿真中回归。

---

### B.2 ΔK 计算公式的形式化定义与参数标定

#### B.2.1 形式化定义

正文 §5.2 给出 `ΔK = novelty × correctness × reproducibility × domain_gap_bonus`。此处形式化每个因子，值域统一归一到 `[0,1]`（bonus 除外，取 `≥1` 的乘子）。

设已有认知图谱为 `G = (V, E)`，新提交贡献为节点 `x`，其语义嵌入为 `emb(x) ∈ ℝ^d`。

**(1) 新颖度 novelty ∈ [0,1]**

```
novelty(x) = 1 − max_{v ∈ N_domain(x)} cos_sim(emb(x), emb(v))
```

- `N_domain(x)`：同领域候选邻居集合（近似最近邻检索，如 HNSW）；
- 近重复（`cos_sim > τ_dup`，建议 `τ_dup = 0.95`）直接判定 `novelty = 0`（对应 A6 抄袭）。

**(2) 正确性 correctness ∈ [0,1]** —— 声誉加权的贝叶斯聚合

设评议人 `i` 声誉 `r_i`，打分 `s_i ∈ [0,1]`：

```
correctness = Σ_i (r_i · s_i) / Σ_i r_i
```

进一步可用 Beta-Binomial 后验建模，输出均值与置信区间；置信区间过宽（评议不足）时触发追加评议。

**(3) 可复现性 reproducibility ∈ [0,1]**

```
reproducibility = (成功独立复现数) / (总复现尝试数)
```

- 无需复现的理论型命题：由评议共识度 `agreement` 代理，`reproducibility := agreement`；
- 复现尝试数不足阈值 `n_min` 时，因子封顶（如 ≤ 0.7），避免"未经充分检验即高分"。

**(4) 跨域桥接奖励 domain_gap_bonus ≥ 1**

```
domain_gap_bonus = 1 + λ · (连接的独立子图数 − 1) · avg_gap
```

- `avg_gap`：被桥接子图间在图谱中的平均最短路径距离（越远越稀缺）；
- `λ`：桥接激励系数，`λ ∈ [0, λ_max]`，由治理设定上限防刷。

**最终**（含时间因子，鼓励尽早填补山谷，呼应 §6.2 `time_decay`）：

```
ΔK(x) = novelty · correctness · reproducibility · domain_gap_bonus · time_freshness
```

#### B.2.2 参数标定方法论

| 参数 | 初始建议 | 标定方法 |
|---|---|---|
| `τ_dup` | 0.95 | 用已知抄袭/原创对构造标注集，选 F1 最优阈值 |
| `n_min`（最少复现） | 2 | 按领域复现成本分层设定 |
| `λ`（桥接系数） | 0.3 | 仿真扫描，使跨域贡献占比达目标区间（如 15–25%） |
| `α, β, γ`（§3.1 权重） | 0.2 / 0.3 / 0.5 | 治理初值 + 反馈控制（见下） |

**闭环标定（反馈控制）**：将参数视为控制变量，以生态健康指标（基尼系数、山谷填补率、伪贡献率）为被控量，用离线仿真（B.3）→ 测试网 A/B → 治理提案的方式迭代收敛。

#### B.2.3 可实现规范（Implementable Spec）

为使 B.3 仿真与后续实现能直接落地，此处钉死 ΔK 的输入接口、归一化与边界情形。这是 §5.2 公式的**规范化版本**，是 B.1/B.3/§6 的共同依赖。

**输入接口**：`compute_delta_k(submission, graph, reviews, replications, params) -> float`

| 输入 | 类型 | 来源 |
|---|---|---|
| `submission.embedding` | `float[d]` | Cognition Engine 抽取 |
| `submission.domain` | `str` | 提交声明 + 本体校验（B.6）|
| `submission.timestamp` | `int` | 链上区块时间 |
| `graph` | 图谱句柄 | 状态层，支持同域近邻检索 |
| `reviews` | `[(reviewer_rep, score)]` | PoK 评议阶段 |
| `replications` | `(success, total)` | PoK 复现挑战阶段 |
| `params` | dict | 治理参数（下表）|

**归一化与边界规则**（全部因子在此夹紧，避免异常放大）：

| 因子 | 计算 | 边界 / 缺省 |
|---|---|---|
| `novelty` | `1 − max cos_sim(x, 同域近邻)` | 域内首节点（无邻居）→ `novelty = 1`；`cos_sim > τ_dup` → `0` |
| `correctness` | `Σ rᵢsᵢ / Σ rᵢ` | 评议数 `< n_review_min` → 因子封顶 `c_cap`（如 0.5），并触发追加评议 |
| `reproducibility` | `success / total` | 理论型（`total=0`）→ 取评议一致性 `agreement`；`total < n_min` → 封顶 0.7 |
| `domain_gap_bonus` | `1 + λ·(bridged−1)·avg_gap` | 无跨域 → `1.0`；夹紧到 `[1, bonus_max]` |
| `time_freshness` | `exp(−decay · age_days)` | 夹紧到 `[fresh_min, 1]` |

**统一治理参数表**（仿真与实现共用同一份默认值）：

```
τ_dup        = 0.95   # 近重复阈值
n_review_min = 3      # 最少评议人数
c_cap        = 0.5    # 评议不足时 correctness 上限
n_min        = 2      # 最少复现尝试
λ            = 0.3    # 跨域桥接系数
bonus_max    = 2.0    # 桥接奖励上限
decay        = 0.01   # 时间衰减率 (per day)
fresh_min    = 0.5    # 时间因子下限
delta_k_min  = 0.05   # ΔK 阈值，低于此视为 0（不铸币）
```

**参考实现（伪代码）**：

```python
def compute_delta_k(sub, graph, reviews, repl, p):
    # 1. novelty
    nbrs = graph.knn(sub.embedding, sub.domain, k=32)
    if not nbrs:
        novelty = 1.0
    else:
        max_sim = max(cos_sim(sub.embedding, v.embedding) for v in nbrs)
        novelty = 0.0 if max_sim > p.tau_dup else (1.0 - max_sim)

    # 2. correctness (reputation-weighted)
    if len(reviews) < p.n_review_min:
        correctness = min(weighted_mean(reviews), p.c_cap)   # + 触发追加评议
    else:
        correctness = weighted_mean(reviews)                 # Σrᵢsᵢ / Σrᵢ

    # 3. reproducibility
    success, total = repl
    if total == 0:
        reproducibility = agreement(reviews)                 # 理论型
    else:
        r = success / total
        reproducibility = min(r, 0.7) if total < p.n_min else r

    # 4. cross-domain bonus
    bonus = clamp(1 + p.lam * (bridged_subgraphs(sub, graph) - 1)
                    * avg_gap(sub, graph), 1.0, p.bonus_max)

    # 5. freshness
    fresh = clamp(exp(-p.decay * age_days(sub)), p.fresh_min, 1.0)

    dk = novelty * correctness * reproducibility * bonus * fresh
    return dk if dk >= p.delta_k_min else 0.0    # 阈值门控 → 不铸币
```

> **契约**：`compute_delta_k` 为纯函数（无副作用），输出 `∈ [0, bonus_max]`；`return 0.0` 即 §3.2 硬约束"ΔK≤0 不增发"的落点。B.3 仿真必须调用此同一函数，确保文档与仿真一致。

> ⚠️ 开放问题：语义嵌入模型本身可被攻击（对抗样本抬高 novelty）；需嵌入模型版本治理与对抗鲁棒性评估。

---

### B.3 $COG 增发/销毁的经济仿真（agent-based simulation）

#### B.3.1 仿真目标

回答三个问题：(1) $COG 供应在何种参数下保持长期稳定而非恶性通胀/通缩；(2) 激励是否真的引导资源流向"认知山谷"；(3) 各类攻击（B.1）在经济层面的可行性与破坏度。

#### B.3.2 智能体与状态

| Agent 类型 | 关键状态 | 决策规则 |
|---|---|---|
| 诚实贡献者 | 余额、声誉、领域偏好 | 依 `E[U]` 选择投入领域与强度 |
| 投机/灌水者 | 余额、风险偏好 | 尝试低成本高频提交，遇罚没即退出 |
| 评议人 | 声誉、专业领域 | 认真/懒惰视奖励结构而定 |
| 电力/算力提供者 | 产能、价格 | 依 $WATT/$FLOP 供需定价 |
| 认知消费者 | 需求、预算 | 消费认知服务 → 销毁 $COG |

#### B.3.3 仿真循环（每 epoch）

```
for epoch in 1..T:
    1. 提供者上报电力/算力 → 铸造 $WATT/$FLOP
    2. 贡献者根据预期收益提交贡献（消耗资源）
    3. PoK：抽选评议 → 挑战 → 计算 ΔK
    4. ΔK>0 → mint($COG)；ΔK≤0 → slash
    5. 消费者消费服务 → burn($COG)
    6. 更新声誉、价格、图谱拓扑
    7. 记录指标
```

#### B.3.4 观测指标

- **货币**：$COG 净增发率、流通量、（模拟）价格波动率；
- **公平**：贡献者收益基尼系数、声誉集中度（HHI）；
- **认知健康**：山谷填补率、跨域边占比、图谱平均置信度；
- **安全**：伪贡献通过率、合谋 ROI、攻击者净收益。

#### B.3.5 场景矩阵

| 场景 | 变量 | 期望结论 |
|---|---|---|
| 基线 | 默认参数 | 供应稳定、收益分散 |
| 通胀压力测试 | 提高 base_emission | 找到不失控上限 |
| 女巫/合谋 | 注入 10–40% 恶意 agent | 攻击 ROI 应为负 |
| 冷启动 | 稀疏初始图谱 | 验证早期激励（time_decay/bonus）有效性 |
| 熊市 | 消费需求骤降 | 通缩机制不致螺旋崩溃 |

#### B.3.6 技术实现建议

Python + [Mesa](https://mesa.readthedocs.io/) 或 [AgentPy] 起步；参数扫描用网格 / 贝叶斯优化；输出接 §B.2 的闭环标定。

#### B.3.7 原型仿真结果（Milestone 2）

已实现一个**零依赖**（纯 Python 标准库）的 ABM 原型，代码见仓库 [`sim/`](../sim/)。仿真直接调用 B.2.3 定义的同一个 `compute_delta_k` 函数，保证文档与代码使用同一份契约。运行方式：`python3 sim/run.py`。

场景矩阵（B.3.5）在默认参数、`seed=42`、200 epoch 下的结果：

| 场景 | 供应(创世→末期) | 净增发/ep | Gini | 跨域均衡 | 伪贡献通过率 | 攻击者净回报 | 诚实净回报 |
|---|---|---|---|---|---|---|---|
| baseline | 1200 → 1790 | +1.9 | 0.041 | 0.93 | 0.0 | — | +0.59 |
| inflation_stress | 1200 → 14899 | +48.0 | 0.041 | 0.93 | 0.0 | — | +2.73 |
| sybil_collusion | 1650 → 2120 | +1.3 | 0.391 | 0.90 | 0.0 | **−0.29** | +0.58 |
| cold_start | 240 → 443 | +0.7 | 0.026 | 0.83 | 0.0 | — | +1.04 |
| demand_shock | 1200 → 6700 | +19.3 | 0.041 | 0.93 | 0.0 | — | +0.59 |

> 净回报 = `(累计奖励 − 累计罚没) / 累计质押`，`< 0` 表示净亏损；伪贡献通过率取末 20 epoch 均值。

**关键发现：**

1. **攻击不经济**（核心结论）：在注入 15 spammer + 10 colluder 的 `sybil_collusion` 场景下，**攻击者净回报为 −0.29（亏损），而诚实者为 +0.58**；伪贡献通过率随声誉衰减机制作用在末期收敛到 0。这为 B.1 的激励约束提供了初步经验支持——偏离诚实策略的期望收益为负。
2. **ΔK 门控有效**：`delta_k_min` 阈值 + 近重复检测使绝大多数低质/重复提交 `ΔK=0`、不铸币且被罚没。
3. **增发上限确有必要**：`inflation_stress`（base_emission 调至 4×）令供应 12× 膨胀，印证 §6.2 需要 `base_emission` 治理上限与反馈控制。
4. **无死锁**：`demand_shock`（需求崩溃）导致供应膨胀但系统不停摆，诚实激励延续；`cold_start`（仅 8 名贡献者）仍能自举图谱（跨域均衡 0.83）。
5. **收益分散**：无攻击场景 Gini ≈ 0.04（高度均衡）；攻击场景升至 0.39，反映攻击者早期套利——这也指示需进一步强化早期检测。

**局限（诚实声明）**：此为方向性原型而非标定模型；语义嵌入用随机向量代理、未建模对抗样本；需求端为简化流量代理。结论须与测试网数据校准后方具外推力（对接 B.3.6 与 §B.2 闭环标定）。详见 [`sim/README.md`](../sim/README.md)。

> ⚠️ 开放问题：ABM 结论对行为假设敏感，需与真实测试网数据校准后才有外推效力。

---

### B.4 Energy Oracle 与绿证体系的对接标准

#### B.4.1 目标

为 PoE 提供**可信、防篡改、防重复计算**的电力数据，且能证明其"绿色属性"，将物理 kWh 映射为链上 `E_audit`。

#### B.4.2 数据模型（PoE 声明）

```json
{
  "meter_id": "did:meter:0x...",
  "period": {"start": 1789000000, "end": 1789003600},
  "energy_kwh": 1024.7,
  "green_certificates": [
    {"scheme": "I-REC", "cert_id": "IREC-...", "mwh": 1.0, "retired": true}
  ],
  "green_ratio": 0.92,
  "location": "geohash:wx4g0",
  "signature": "device_sig:...",
  "zk_proof": "zk:range+authenticity"
}
```

#### B.4.3 绿证体系对接

| 体系 | 区域 | 对接要点 |
|---|---|---|
| I-REC | 国际 | 证书须标记 `retired`（已注销），防重复出售 |
| GEC（绿电证书） | 中国 | 与国家平台核销状态对账 |
| GO / REC | 欧盟 / 北美 | 时间与地理匹配（24/7 CFE 理念） |

**核心规则**：1 份绿证只能锚定一次 `green_ratio` 提升；核销状态由 Oracle 实时核对，已注销证书方可入账。

#### B.4.4 可信采集与防伪

- **硬件**：智能电表内置安全芯片（Secure Element），数据出厂即签名；
- **传输**：设备签名 + 时间戳，Oracle 多源交叉验证（电表读数 vs 电网结算 vs 卫星/碳排数据）；
- **隐私**：用 ZK 范围证明披露"kWh 落在区间且证书有效"，而不暴露商业敏感的精确用电曲线；
- **防重复计算**：`(meter_id, period)` 唯一性约束 + 绿证注销状态双重校验。

#### B.4.5 标准草案要点

1. PoE 声明 Schema（上）作为链上标准结构；
2. Oracle 节点去中心化 + 抵押 + 错误上报罚没；
3. 与至少一个绿证注册平台的 API 核销对接规范；
4. 争议仲裁：数据异常触发挑战 → 多 Oracle 复核。

> ⚠️ 开放问题：绿电的"时间/地理匹配"严格程度（年度配额 vs 24/7 实时匹配）直接影响成本与可信度，需分阶段收紧。

---

### B.5 zkML / opML 在 PoF 中的可行性评估

#### B.5.1 待证明的命题

PoF 要证明："声明消耗的算力确实执行了指定的、有效的计算任务（训练/推理），而非空转或替换为廉价任务"。

#### B.5.2 三条技术路线对比

| 维度 | TEE（SGX/TDX） | zkML | opML（乐观式） |
|---|---|---|---|
| 信任假设 | 信任硬件厂商 | 纯密码学（最强） | 经济博弈 + 挑战期 |
| 证明开销 | ≈1×（近原生） | 10³–10⁶×（当前）| ≈1×（乐观执行） |
| 验证开销 | 远程认证 | 亚秒级、极小 | 仅争议时重算 |
| 延迟 | 低 | 高（生成慢） | 低（乐观即时，终局延后） |
| 成熟度 | 高，已商用 | 快速发展，受限于模型规模 | 中，已有实现 |
| 适用规模 | 大模型可行 | 目前限中小模型/算子 | 大模型可行 |
| 主要风险 | 侧信道、厂商信任 | 成本、算子覆盖不全 | 挑战期资金锁定、需活跃验证者 |

#### B.5.3 分阶段策略（混合方案）

```
Phase 1  以 TEE 为主：远程认证 + 度量执行环境，快速可用
Phase 2  引入 opML：对高价值任务加挑战期，降低对硬件的单点信任
Phase 3  关键/高敏任务用 zkML：对可承受成本的核心算子给出密码学证明
长期     TEE-in-ZK / 硬件加速 zkML 成熟后逐步提升 zk 覆盖率
```

#### B.5.4 与 ΔK 的耦合

PoF 不单独增发 $COG（§3.2 硬约束）——它产出 `F_useful` 作为 ΔK 计算的资源背书；即使 PoF 完美，`ΔK=0` 仍不铸币。因此 PoF 的可行性门槛可适度放宽：**用较低成本证明"算力用于真实任务"即可，不必对每一步 FLOP 做零知识证明。**

> ⚠️ 开放问题：zkML 对 Transformer 类大算子的证明成本仍偏高；opML 挑战期与主网终局性的权衡需结合 B.1 攻击面分析确定参数。

---

### B.6 认知图谱本体（ontology）标准草案

#### B.6.1 目标

为跨领域认知节点/边提供统一语义骨架，使不同领域子图可对齐、可互操作、可演进（呼应正文 §11 开放问题 4）。

#### B.6.2 三层本体架构

```
┌───────────────────────────────────────────────┐
│ 上层本体 (Upper Ontology)                      │
│  Concept / Proposition / Evidence / Actor      │  ← 全局共享，复用 SKOS/PROV-O
├───────────────────────────────────────────────┤
│ 领域本体 (Domain Ontology)                     │
│  ai/, math/, bio/ ... 各领域扩展               │  ← 社区维护，命名空间隔离
├───────────────────────────────────────────────┤
│ 实例层 (Instances)                             │
│  具体命题、论文、模型节点                       │
└───────────────────────────────────────────────┘
```

#### B.6.3 复用现有标准

| 需求 | 复用标准 | 说明 |
|---|---|---|
| 概念/词表 | SKOS | 概念、上下位、相关关系 |
| 溯源 | PROV-O (W3C) | 谁、用什么、如何产出（天然契合 PoE/PoF/PoK 溯源）|
| 标识 | DID / IRI | 贡献者与节点全局唯一标识 |
| 序列化 | RDF / JSON-LD | 与正文 §4.3 JSON 结构兼容 |
| 学术元数据 | schema.org/ScholarlyArticle、OpenAlex | 论文/引用对接 |

#### B.6.4 边语义的本体化

正文 §4.2 的边（ENTAILS/REFUTES/CITES/EXTENDS/DISPUTES/POWERED_BY/ENERGIZED_BY）定义为本体中的 `ObjectProperty`，附带域（domain）、值域（range）与逻辑特性（如 `ENTAILS` 传递性、`REFUTES` 对称性约束），便于自动推理与一致性检查。

#### B.6.5 对齐与演进机制

- **本体对齐**：不同领域相同概念用 `owl:sameAs` / `skos:exactMatch` 建立跨域桥（同时贡献 `domain_gap_bonus`）；
- **版本治理**：本体变更走链上提案（§9 认知宪法），语义化版本号，破坏性变更需超级多数；
- **自动化辅助**：LLM 抽取候选映射 → 人工/评议确认，降低本体维护成本。

> ⚠️ 开放问题：本体本身的"客观性"与权力结构（谁定义领域本体）需与治理机制共同设计，避免本体成为认知垄断工具。

---

## 附录 C · 深化章节新增术语

| 术语 | 定义 |
|---|---|
| SPNE | 子博弈精炼纳什均衡，博弈论中的均衡精炼概念 |
| VRF | 可验证随机函数，用于不可操纵的评议人抽选 |
| commit–reveal | 承诺—揭示两阶段协议，隐藏评议前的身份/内容 |
| peer-prediction / BTS | 无需标准答案即可激励诚实报告的评分机制（含贝叶斯真话血清 Bayesian Truth Serum）|
| ABM | Agent-Based Simulation，基于智能体的经济仿真 |
| TEE | 可信执行环境（如 Intel SGX/TDX）|
| zkML / opML | 零知识机器学习证明 / 乐观式机器学习验证 |
| I-REC / GEC / GO / REC | 国际及区域绿色电力证书体系 |
| SKOS / PROV-O / DID / JSON-LD | W3C 语义网与去中心化标识相关标准 |
