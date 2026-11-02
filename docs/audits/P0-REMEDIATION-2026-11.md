# P0 审计修复对照表 / Remediation Matrix

**对应审计:** [`P0-SECURITY-REVIEW-2026-11.md`](P0-SECURITY-REVIEW-2026-11.md)(1 严重 + 3 高 + 3 中)
**修复提交:** `f85c6f2`(M202)、`1056ad0`(M203)、`3489a35`(M204),基线 `8117910`(审计对象)
**交付目的:** 供审计方**回归复验** —— 确认每项修复到位、无引入新缺陷。重点复查新增代码:
SEC-05 的 **B7 wire** 与 SEC-01 的治理 gossip 入口。

## 如何复验

```sh
git log --oneline 8117910..3489a35          # 审计基线 → 修复后的三个提交
(cd node && cargo test --release -- --test-threads=1)   # 675 lib + 57 bin，全绿
(cd engine && cargo test --release)                     # 11，全绿
(cd node && cargo clippy --release --all-targets -- -D warnings)  # 零 lint
(cd node && cargo audit) && (cd engine && cargo audit)  # 现为 CI 门禁
```
每项修复都附**命名回归测试**;下表的"回归测试"列可 `cargo test -- <name>` 单独跑。

## 对照表

| 编号 | 等级 | 状态 | 根因 | 修复(文件·符号) | 回归测试 | 提交 |
|---|---|---|---|---|---|---|
| SEC-03 | 严重 | ✅ 已修 | `quorum()` 的 `2*total` 在 `u64` 回绕(total=2^63→quorum=1),<1/3 权重可认证冲突块、破坏最终性 | `validator.rs`:`quorum()` 改 `u128` 算 `floor(2T/3)+1`;`total_power()` 改 `saturating_add` | `quorum_does_not_overflow_at_large_power` | `f85c6f2` |
| SEC-02 | 高 | ✅ 已修 | `state_root` 未承诺 `base_emission_micro`/`decay_half_life_blocks`/`slash_bps`/ΔK,篡改参数的快照/创世仍匹配 cert 根→同块异参分叉 | `lib.rs`:`ChainState::state_root()` 折入全部执行参数 | `state_root_covers_execution_params`(+ demo 守卫 re-baseline `01c163ce…`) | `f85c6f2` |
| SEC-04 | 高 | ✅ 已修 | 治理执行不校验参数值,`slash_bps=20000` 令拒绝交易 `slash>stake` 回绕、破坏供应守恒;`usize` 转换平台相关 | `lib.rs`:`validate_gov_param`/`validate_consensus_params`(slash≤10000、emission 上限、ΔK finite+域、计数≤10000),在 `apply_gov_proposal` 注册 + `config.rs` 创世(`BadGenesis`)强制 | `gov_param_validation_rejects_unsafe_values`、`gov_proposal_with_unsafe_param_is_rejected_at_registration` | `f85c6f2` |
| SEC-06 | 中 | ✅ 已修 | 计数虽受 `MAX_ITEMS` 限,但 `Vec::with_capacity(n)` 在读元素前即预留,短帧声明百万项→~176MB | `codec.rs`:`prealloc`/`prealloc_usize` 封顶预留 `PREALLOC_CAP=1024`;codec 19 + net 14 处改用 | `prealloc_caps_reservation`(+ 既有 `decoders_reject_adversarial_bytes_without_panicking`) | `f85c6f2` |
| SEC-07 | 中 | ✅ 已修 | `rustls 0.23.45`/`rustls-webpki 0.103.15`/`tokio 1.53.1` 命中 RUSTSEC 公告;CI audit 为 advisory | `Cargo.lock`→`rustls 0.23.46`/`rustls-webpki 0.103.16`/`tokio 1.54.1`(审计建议修复版);`ci.yml` audit 步去 `continue-on-error`→**门禁** | `cargo audit`(CI 门禁) | `f85c6f2` |
| SEC-01 | 高 | ✅ 已修 | 治理 gossip 只按哈希去重即入池 + flood;`build_candidate` 全量克隆 pending 池并 `seal`,任一无效/过期 op 致 `None`→持续阻断出块;提交后未清理 gov op | `net.rs`+`lib.rs` 三防线:(A) `on_gov_proposal`/`on_gov_vote` 调 `gov_proposal_admissible`/`gov_vote_admissible`(账户/签名/参数域;投票查提案存在且在窗口)不合格不暂存不 flood;(B) `build_candidate` 用 `filter_applicable_gov` 在 trial 上按序试应用、只纳入干净 op;(C) `apply_certified` 清理已提交 gov op | `gov_gossip_rejects_inadmissible_ops`、`poisoned_gov_pool_does_not_stall_candidate_production` | `1056ad0` |
| SEC-05 | 中 | ✅ 已修 | unbond 证明对 `source_header.bridge_root`(提交 locks 树)验证,正常源链无法生成可验证证明(旧测试靠伪造头 patch `bridge_root`+re-cert) | `lib.rs`+`codec.rs`:新增独立 cert 签名 `unbonding_root`(源链 unbonding 队列 Merkle 根)——`unbonding_merkle_root`/`unbonding_proof`/`..._for_genesis` + `Block`/`BlockHeader` 字段(**wire B7**,前缀 252→284、n_updates 偏移 244→276)+ seal/commit 盖根 + apply `UnbondingRootMismatch`;`apply_bridge_unbond` 改对 `source_header.unbonding_root` 验证;demo/测试/`mirror-unbond` CLI 改用正常 seal 的真实头,删死代码 `rebuild_cert_after_bridge_root_patch` | `unbonding_root_mismatch_is_rejected_on_apply`;`bridge_unbond_mirrors_to_dest_and_matures`(现为**真实端到端**:源链正常 seal→出证明→目的链镜像,无伪造) | `3489a35` |

## 需审计方重点回归的新增面

1. **B7 wire(SEC-05)**:`unbonding_root` 作为**前缀根**加入 header → `decode_certified_header` 固定前缀 252→**284**、`n_updates` 偏移 244→**276**、尾承诺仍 320。请复查:编解码对称、`from_block`/`to_block` 一致、`header.hash()==block.hash()` 前缀契约、跨代际拒绝(B6↔B7)、以及源链正常 seal 确实承诺 `unbonding_root`。
2. **治理 gossip 入口(SEC-01)**:`gov_proposal_admissible`/`gov_vote_admissible` **不**校验 nonce(容许流水线超前),nonce/顺序交由 apply + `filter_applicable_gov` 把关。请复查:投票要求提案**已提交**(id 仅在 apply 时分配,故这是正确约束)、`filter_applicable_gov` 的 trial 顺序(提案先于投票)与真实 `apply_block_inner` 一致、以及"中毒池不致停摆"在多提议人下成立。
3. **state_root 变更(SEC-02)**:确认**所有**影响状态转换的参数均已折入,且创世身份与快照校验绑定同一承诺(快照已序列化这些参数,交叉校验现覆盖它们)。

## 本轮未覆盖(与审计一致,non-goals)

持久化 cargo-fuzz、形式化验证、跨实现互操作、生产渗透测试;以及 SECURITY.md §10 的已知边界
(无 liveness slashing、密钥在盘、无滚动升级、治理未对抗压测、经济参数待测试网反校)。这些
**不在本次修复范围**,属上线前另行推进项。
