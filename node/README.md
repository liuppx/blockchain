# ZhixingGraph 参考节点（Rust · Milestone 6）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §5「PoK 共识」与 §7「技术架构」。

这是把 ΔK 引擎（[`engine/`](../engine/)）与经济仿真（[`sim/`](../sim/)）背后的规则，落成一个**可运行、确定性的 PoK 共识状态机**——真正"跑链"的最小内核：区块、交易、账户、状态转移、铸造/罚没、链上声誉、内容寻址的区块哈希链与状态根。

> **共识的前提是确定性**：给定相同的创世与相同的区块序列，每个诚实节点算出**逐字节相同**的状态（`state_root` 一致）。本 crate 就是那个状态转移函数 `apply_block`，其 ΔK 由 `zhixing_engine::compute_delta_k` 计算——与白皮书 B.2.3、Python 仿真是**同一份契约**。

## 运行

```bash
cd node
cargo run --release --bin node   # 跑一条演示链：创世 → 出块 → 打印回执/状态
cargo test --release             # 8 项单元测试（见下）
```

演示链展示：新颖提交铸造 $COG、跨域桥接拿到 novelty+bonus（ΔK>1）、近重复/低质提交被**罚没入 treasury**、供应守恒、评审声誉按链上结果升降。

## 设计要点

| 主题 | 做法 |
|---|---|
| **确定性** | 状态用 `BTreeMap`（有序遍历）；金额为整数 micro-$COG（无浮点货币）；`state_root` 与区块哈希对规范字节编码做 SHA-256 |
| **原子性** | `Chain::commit` 在 state 的克隆上试算整块；任一交易非法则整块回滚，绝不留下半应用状态 |
| **一份契约** | ΔK 复用 `zhixing_engine`，不重新实现——文档/仿真/链上三处不漂移 |
| **供应守恒** | 罚没的质押转入 treasury（不销毁），`supply == Σ余额 + treasury` 恒成立，有测试守护 |
| **链上声誉** | 链上看不到"真实质量"，只能按**已定稿的结果**更新：给通过项打高分者加分，给被拒项打高分者扣分 |
| **零依赖** | 仅以 rlib 复用 engine，纯 std，可离线编译（SHA-256 内置，见下） |

## 测试覆盖

```
novel_submission_mints_and_conserves_supply   新颖提交铸造且供应守恒
near_duplicate_is_slashed_to_treasury         近重复被罚没入 treasury
deterministic_replay_same_state_root          两次相同重放 → 相同 state_root/head
tampering_a_tx_changes_the_block_hash         篡改交易 → 区块哈希改变
wrong_prev_hash_is_rejected                   prev_hash 不接续 head → 拒绝
invalid_tx_rolls_back_whole_block             块内一笔非法 → 整块回滚
cannot_stake_more_than_balance                余额不足以质押 → 拒绝
hash::known_vectors                           SHA-256 对 FIPS 180-4 向量
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`/`SubmissionTx`/`Account`/`ChainState`/`Chain`、`apply_block`、`state_root`、供应守恒不变量 + 单元测试 |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 演示节点：创世 + 出块 + 打印 |

## 局限与后续（离生产还差什么）

本里程碑刻意只做**确定性状态机内核**，尚未包含：

- **密码学身份**：交易/区块签名、账户 = 公钥。当前 `author`/`reviewer` 是裸整数 id。→ 引入 ed25519（生产用审计过的 crate）。
- **BFT 共识与区块排序**：谁出块、如何对区块达成一致（本 crate 只做"给定区块→应用"，不含出块权/最终性）。→ 引入 Tendermint/HotStuff 类 BFT 或 PoS 出块。
- **P2P 网络**：交易/区块的 gossip、状态同步。
- **持久化**：当前状态在内存中。→ 落盘（RocksDB 等）+ 崩溃恢复。
- **Merkle 化状态树**：当前 `state_root` 是全状态摘要，无法做轻客户端证明。→ Merkle Patricia Trie。
- **手写 SHA-256** 仅为离线零依赖演示，**生产必须换审计实现**（`sha2`）。
- **kNN 暴力扫描**：随图谱增长需换 HNSW/IVF（见 engine 局限）。

这些构成后续里程碑（M7 持久化 + RPC、M8 P2P + 签名、M9 BFT 共识……），每步仍遵循"可运行、可测试、契约一致"。
