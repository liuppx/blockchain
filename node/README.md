# ZhixingGraph 参考节点（Rust · Milestone 6–11）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §5「PoK 共识」与 §7「技术架构」。

这是把 ΔK 引擎（[`engine/`](../engine/)）与经济仿真（[`sim/`](../sim/)）背后的规则，落成一个**可运行、确定性的 PoK 共识状态机**——真正"跑链"的最小内核：区块、交易、账户、状态转移、铸造/罚没、链上声誉、ed25519 签名交易、追加式持久化、确定性 mempool 出块、Merkle 认证状态与轻客户端证明、BFT 最终性证书与验证人集，以及内容寻址的区块哈希链与状态根。

> **共识的前提是确定性**：给定相同的创世与相同的区块序列，每个诚实节点算出**逐字节相同**的状态（`state_root` 一致）。本 crate 就是那个状态转移函数 `apply_block`，其 ΔK 由 `zhixing_engine::compute_delta_k` 计算——与白皮书 B.2.3、Python 仿真是**同一份契约**。

## 运行

```bash
cd node
cargo run --release --bin node -- demo             # 内存演示链：创世 → 出块 → 打印
cargo run --release --bin node -- build            # mempool：乱序投递交易 → 规范排序出块
cargo run --release --bin node -- prove            # 轻客户端 Merkle 证明：单账户对状态根验证
cargo run --release --bin node -- bft              # BFT：4 验证人对区块出具可验证的最终性证书
cargo run --release --bin node -- run  --dir DIR   # 持久化链：首次落盘演示块，之后重放
cargo run --release --bin node -- status --dir DIR # 重放区块日志并打印状态
cargo test --release                               # 47 项单元测试（见下）
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

## 确定性 mempool 与出块（Milestone 9）

到目前为止区块是"手工拼好再交给链"的。真实节点收到的是**零散待处理交易**，需要自己**构造**区块——而共识要求：持有相同待处理集合与相同链状态的两个节点，必须造出**逐字节相同**的区块。`mempool` 保证这一点：

- **规范排序**：交易按其内容寻址哈希（`SubmissionTx::hash`）入 `BTreeMap`，遍历顺序与到达顺序、map 内部实现都无关。
- **构造即执行**：`build_block` 在状态的克隆上按规范顺序**试算**每笔候选，只纳入能干净提交的（`max_txs` 为上限），跳过其余。因此产出的区块保证能 `commit`，且每个诚实构造者丢弃的正是同一批交易。
- **准入校验**：`insert` 用 `validate_tx`（验签/账户/评审/余额，不含 ΔK）做早期拒绝；准入不等于必然入块——余额可能在出块前变化，构造器会复检。

本里程碑仍是**单一提议者**（无出块权选举/BFT，那是后续），重点是区块的**确定性构造**。

```bash
cargo run --release --bin node -- build   # 乱序投递 3 笔 -> 构造器按 tx 哈希排序 -> 区块哈希与到达顺序无关
```

## 认证状态与轻客户端证明（Milestone 10）

`state_root` 之外，节点再对 accounts/reviewers 状态维护一棵**二叉 Merkle 树**（`merkle_root`）。它把"整块状态摘要"升级成**可逐叶打开**的认证结构：轻客户端只持有 `merkle_root`，拿到某个账户的内容 + 一条**包含证明**（`account_proof`）即可验证该账户真属于此状态——无需全量状态。

- **域分隔**：叶 `sha256(0x00‖data)`、内部节点 `sha256(0x01‖left‖right)`，杜绝把叶当内部节点的第二原象攻击。
- **奇数节点提升而非复制**：末尾落单节点原样上提（避免 CT 式"自我复制"陷阱），证明在该层不记录兄弟。
- **同一份编码**：叶字节用与 `state_root` 相同的 `codec::Enc` 布局（`Account::merkle_leaf`），两根内容寻址地同步变化；改字段两根都变，改编码只会让证明失配——leaf 契约不漂移。
- 当前是"每块从全量叶重建"的排序 Merkle 树（对参考节点足够）；生产大状态会换增量更新的 trie。非成员证明不在本里程碑范围。

```bash
cargo run --release --bin node -- prove   # 验证账户 #1 -> true；把余额谎报大一点 -> false
```

## BFT 最终性与验证人集（Milestone 11）

从"单一提议者"迈向"多验证人共识"的关键一步。共识按**投票权（质押）**而非人头计票，决策需**严格 > 2/3 总投票权**（经典 BFT 阈值，容忍 < 1/3 拜占庭权重且保持安全性——任意两个法定人数在 > 1/3 权重上相交，除非有人双签，否则无法为冲突区块出证书）。

- **确定性提议人**：Tendermint 提议人优先级累加器（`ValidatorSet::proposer_for`）——按质押比例、无漂移地轮转，每个节点算出同一提议人。
- **可验证最终性证书**：`Commit` 是一组对同一 (height, round, block_hash) 的 ed25519 预提交签名。任何人（含从未联网的轻客户端）都能 `Commit::verify` 它：逐票验签、须来自已知验证人、不得重复计票、总权达法定人数——**与 M10 的 Merkle 状态根组合，轻客户端即可信任对该区块证明出来的任意账户**。
- **问责**：`detect_equivocation` 从两份冲突证书中提取双签验证人——真实链据此罚没的密码学证据。

本里程碑实现的是共识的**安全性内核（最终性证书）**；驱动验证人在部分同步下达成提交的**轮次状态机**（提案超时、prevote/precommit 锁定、换轮——负责*活性*）留待后续。`commit_block` 在进程内模拟一轮诚实投票，使机制端到端可跑、可测。

```bash
cargo run --release --bin node -- bft   # 4 验证人：3/4 提交（容 1 崩溃）、2/4 不提交、冲突证书暴露双签者
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
| **确定性出块** | mempool 按 tx 哈希规范排序、在克隆上试算后只纳入可提交交易；相同待处理集 + 相同状态 → 逐字节相同区块（M9） |
| **认证状态** | accounts/reviewers 维护二叉 Merkle 树；轻客户端凭 `merkle_root` + `account_proof` 验证单账户，域分隔 + 奇数提升（M10） |
| **BFT 最终性** | 投票权 > 2/3 的 ed25519 预提交组成可验证 `Commit` 证书；确定性提议人；双签可被 `detect_equivocation` 问责（M11） |
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
# mempool（mempool.rs）
built_block_commits_and_orders_canonically    构造的区块可提交且按哈希规范排序
two_builders_produce_identical_blocks         到达顺序不同 → 区块哈希相同
builder_skips_a_tx_that_would_not_apply       余额不够的候选被跳过，区块仍干净提交
remove_included_clears_committed_txs           已入块交易出池
empty_pool_builds_nothing / rejects_forged_tx_at_admission
# Merkle 树（merkle.rs）
single_leaf_root_is_the_leaf_hash / empty_tree_root_is_zero
proofs_roundtrip_for_all_sizes_and_indices    1..=17 叶、各下标包含证明往返
tampered_leaf_fails_verification / proof_from_one_index_does_not_verify_another_leaf
changing_any_leaf_changes_the_root
# 认证状态（lib.rs）
merkle_root_authenticates_an_account_via_inclusion_proof  轻客户端凭证明验证账户
a_tampered_account_value_fails_the_proof      谎报余额 → 验证失败
proof_against_a_stale_root_fails_after_state_changes  旧证明对新根失效
proof_for_unknown_account_is_none
# 验证人集（validator.rs）
quorum_is_strictly_more_than_two_thirds       法定人数 > 2/3 总投票权
proposer_rotates_proportionally_to_power / higher_power_proposes_more_often
proposer_is_deterministic
# BFT 共识（consensus.rs）
quorum_of_precommits_commits                  3/4 预提交 → 提交（容 1 崩溃）
below_quorum_does_not_commit                  2/4 → 不提交（安全）
forged_precommit_is_rejected / double_counting_a_validator_is_rejected
a_prevote_is_not_a_valid_precommit
conflicting_commits_require_equivocation       冲突证书 → 揪出双签者
honest_validators_cannot_form_conflicting_commits  无双签则无法造冲突证书
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`/`SubmissionTx`/`Account`/`ChainState`/`Chain`、`apply_block`、`replay`、验签、`state_root`/`merkle_root`/`account_proof`、供应守恒不变量 + 测试 |
| `src/mempool.rs` | 确定性 mempool 与出块：内容寻址排序 + 试算式 `build_block` + 测试 |
| `src/merkle.rs` | 二叉 Merkle 树：域分隔叶/节点、奇数提升、包含证明 `Proof`/`verify` + 测试 |
| `src/validator.rs` | 验证人集与确定性提议人（Tendermint 优先级累加器）+ 测试 |
| `src/consensus.rs` | BFT 投票/最终性证书：`Vote`/`Commit`/`verify`、`commit_block`、`detect_equivocation` + 测试 |
| `src/crypto.rs` | ed25519 身份：`Keypair`/`verify`（封装 `ed25519-dalek`）+ 测试 |
| `src/codec.rs` | 区块的规范二进制编解码（哈希与落盘共用）+ `tx_signing_bytes`/`encode_tx`（签名/tx 哈希字节）+ 测试 |
| `src/store.rs` | 追加式区块日志（长度前缀记录、残缺尾检测）+ 测试 |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 节点 CLI：`demo` / `build` / `prove` / `bft` / `run` / `status`（含确定性演示密钥） |

## 局限与后续（离生产还差什么）

本里程碑刻意只做**确定性状态机内核 + 单机出块 + BFT 安全性内核**，尚未包含：

- **~~密码学身份~~**：✅ 已完成（M8，ed25519 签名交易）。后续：账户 = 公钥的完整身份模型、动态开户、评审签名、密钥轮换。
- **~~确定性出块~~**：✅ 已完成（M9，mempool + 试算式 `build_block`）。后续：手续费/优先级排序、区块 gas 上限、交易过期。
- **BFT 活性（轮次状态机）**：✅ 安全性内核已完成（M11，投票权 > 2/3 的最终性证书 + 提议人选择 + 双签问责）；**待补活性**——提案超时、prevote/precommit 锁定与解锁、换轮，才能在部分同步下保证*一定*出块。
- **P2P 网络**：交易/区块/投票的 gossip、状态同步（当前 `commit_block` 在进程内模拟一轮）。
- **~~持久化~~**：✅ 已完成（M7，追加式区块日志 + 重放）。后续可换 RocksDB、加 per-record 校验和与 segment 轮转、并持久化 `Commit` 证书。
- **~~Merkle 化状态树~~**：✅ 已完成（M10，二叉 Merkle 树 + 账户包含证明）。后续：非成员证明、增量更新的 Merkle-Patricia trie、把 graph/头字段也纳入根。
- **手写 SHA-256** 仅为离线零依赖演示，**生产必须换审计实现**（`sha2`）。
- **kNN 暴力扫描**：随图谱增长需换 HNSW/IVF（见 engine 局限）。

这些构成后续里程碑（~~M7 持久化~~ ✅、~~M8 签名~~ ✅、~~M9 mempool 出块~~ ✅、~~M10 Merkle 认证状态~~ ✅、~~M11 BFT 最终性内核~~ ✅、M12 BFT 轮次状态机/活性、M13 P2P + gossip……），每步仍遵循"可运行、可测试、契约一致"。
