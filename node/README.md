# ZhixingGraph 参考节点（Rust · Milestone 6–8）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §5「PoK 共识」与 §7「技术架构」。

这是把 ΔK 引擎（[`engine/`](../engine/)）与经济仿真（[`sim/`](../sim/)）背后的规则，落成一个**可运行、确定性的 PoK 共识状态机**——真正"跑链"的最小内核：区块、交易、账户、状态转移、铸造/罚没、链上声誉、内容寻址的区块哈希链与状态根。

> **共识的前提是确定性**：给定相同的创世与相同的区块序列，每个诚实节点算出**逐字节相同**的状态（`state_root` 一致）。本 crate 就是那个状态转移函数 `apply_block`，其 ΔK 由 `zhixing_engine::compute_delta_k` 计算——与白皮书 B.2.3、Python 仿真是**同一份契约**。

## 运行

```bash
cd node
cargo run --release --bin node -- demo             # 内存演示链：创世 → 出块 → 打印
cargo run --release --bin node -- run  --dir DIR   # 持久化链：首次落盘演示块，之后重放
cargo run --release --bin node -- status --dir DIR # 重放区块日志并打印状态
cargo test --release                               # 20 项单元测试（见下）
```

演示链展示：新颖提交铸造 $COG、跨域桥接拿到 novelty+bonus（ΔK>1）、近重复/低质提交被**罚没入 treasury**、供应守恒、评审声誉按链上结果升降。

## 持久化与重放（Milestone 7）

节点状态不再只活在内存里：区块以**追加式日志**（`DIR/blocks.log`）落盘，重启后从创世**重放**日志即可重建**逐字节相同**的状态。

```bash
D=$(mktemp -d)
cargo run --release --bin node -- run    --dir "$D"   # state_root=0949627b…becb4f
cargo run --release --bin node -- status --dir "$D"   # state_root=0949627b…becb4f（从磁盘重建，一致）
```

- **同一份编码**用于区块哈希与磁盘记录（`codec`），所以区块哈希覆盖的正是落盘的字节。
- **崩溃安全**：日志为「`u32` 长度前缀 + 区块字节」的记录序列；崩溃导致的**残缺尾记录**在读取时被检测并报错，而非静默损坏重放。
- **重放即验证**：`Chain::replay` 对每个区块做与新出块完全一致的校验，被篡改的日志会在重放时失败。

## 密码学身份与交易签名（Milestone 8）

提交不再是"裸整数 id 声明"，而是**经 ed25519 签名认证**的交易：账户在创世登记公钥，每笔 `SubmissionTx` 携带作者对交易规范字节（`codec::tx_signing_bytes`，即除签名外的全部字段）的签名。`apply_tx` 先验签，再判断余额/ΔK——**无法冒用他人账户，也无法在签名后篡改任何字段**。

- 签名用**审计过的 `ed25519-dalek`**，绝不自实现签名算法（对照：内置 SHA-256 仅用于哈希演示，生产亦应换 `sha2`）。
- **引擎仍零依赖**：`ed25519-dalek` 只进 node（应用层）；`engine`（可嵌入/WASM）保持纯 std。
- 签名字段纳入区块编码与哈希，但**不纳入签名字节**（自然地避免自指）。

```
forged_signature_is_rejected        # 用别人的密钥签 -> BadSignature
tampering_a_signed_field_is_rejected # 签名后改 stake -> BadSignature
crypto::{sign_and_verify_roundtrip, tampered_message_fails, wrong_key_fails}
```

## 设计要点

| 主题 | 做法 |
|---|---|
| **确定性** | 状态用 `BTreeMap`（有序遍历）；金额为整数 micro-$COG（无浮点货币）；`state_root` 与区块哈希对规范字节编码做 SHA-256 |
| **原子性** | `Chain::commit` 在 state 的克隆上试算整块；任一交易非法则整块回滚，绝不留下半应用状态 |
| **一份契约** | ΔK 复用 `zhixing_engine`，不重新实现——文档/仿真/链上三处不漂移 |
| **供应守恒** | 罚没的质押转入 treasury（不销毁），`supply == Σ余额 + treasury` 恒成立，有测试守护 |
| **链上声誉** | 链上看不到"真实质量"，只能按**已定稿的结果**更新：给通过项打高分者加分，给被拒项打高分者扣分 |
| **交易认证** | 账户 = 创世登记的 ed25519 公钥；提交须带作者签名，验签通过才处理（M8） |
| **依赖策略** | 引擎零依赖（可嵌入/WASM）；节点作为应用引入审计过的 `ed25519-dalek` 做签名，绝不自实现密码学 |

## 测试覆盖

```
# 状态机（lib.rs）
novel_submission_mints_and_conserves_supply   新颖提交铸造且供应守恒
near_duplicate_is_slashed_to_treasury         近重复被罚没入 treasury
deterministic_replay_same_state_root          两次相同重放 → 相同 state_root/head
tampering_a_tx_changes_the_block_hash         篡改交易 → 区块哈希改变
wrong_prev_hash_is_rejected                   prev_hash 不接续 head → 拒绝
invalid_tx_rolls_back_whole_block             块内一笔非法 → 整块回滚
cannot_stake_more_than_balance                余额不足以质押 → 拒绝
forged_signature_is_rejected                  用别人的密钥签 → 拒绝
tampering_a_signed_field_is_rejected          签名后改字段 → 拒绝
persisted_log_replays_to_identical_state      落盘日志重放 → 与内存链 state_root 一致
# 编解码（codec.rs）
round_trip / truncated_input_errors / trailing_bytes_error
# 持久化（store.rs）
append_then_read_back / empty_log_reads_empty / torn_tail_is_detected
# 哈希（hash.rs）
known_vectors                                 SHA-256 对 FIPS 180-4 向量
# 密码学（crypto.rs）
sign_and_verify_roundtrip / tampered_message_fails / wrong_key_fails
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`/`SubmissionTx`/`Account`/`ChainState`/`Chain`、`apply_block`、`replay`、验签、`state_root`、供应守恒不变量 + 测试 |
| `src/crypto.rs` | ed25519 身份：`Keypair`/`verify`（封装 `ed25519-dalek`）+ 测试 |
| `src/codec.rs` | 区块的规范二进制编解码（哈希与落盘共用）+ `tx_signing_bytes`（签名字节）+ 测试 |
| `src/store.rs` | 追加式区块日志（长度前缀记录、残缺尾检测）+ 测试 |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 节点 CLI：`demo` / `run` / `status`（含确定性演示密钥） |

## 局限与后续（离生产还差什么）

本里程碑刻意只做**确定性状态机内核**，尚未包含：

- **~~密码学身份~~**：✅ 已完成（M8，ed25519 签名交易）。后续：账户 = 公钥的完整身份模型、动态开户、评审签名、密钥轮换。
- **BFT 共识与区块排序**：谁出块、如何对区块达成一致（本 crate 只做"给定区块→应用"，不含出块权/最终性）。→ 引入 Tendermint/HotStuff 类 BFT 或 PoS 出块。
- **P2P 网络**：交易/区块的 gossip、状态同步。
- **~~持久化~~**：✅ 已完成（M7，追加式区块日志 + 重放）。后续可换 RocksDB、加 per-record 校验和与 segment 轮转。
- **Merkle 化状态树**：当前 `state_root` 是全状态摘要，无法做轻客户端证明。→ Merkle Patricia Trie。
- **手写 SHA-256** 仅为离线零依赖演示，**生产必须换审计实现**（`sha2`）。
- **kNN 暴力扫描**：随图谱增长需换 HNSW/IVF（见 engine 局限）。

这些构成后续里程碑（~~M7 持久化~~ ✅、~~M8 签名~~ ✅、M9 P2P + mempool、M10 BFT 共识……），每步仍遵循"可运行、可测试、契约一致"。
