# P0 审计修复对照表 / Remediation Matrix

**对应审计:** [`P0-SECURITY-REVIEW-2026-11.md`](P0-SECURITY-REVIEW-2026-11.md)(1 严重 + 3 高 + 3 中)
**审计基线:** `8117910`(M200)。第一轮修复为 M202–M204；第二轮修复为 M206(`d94af66`)；SEC-01 第三轮容量边界修复为 M207(`c7e0344`)，本表更新至当前待复验状态。
**当前状态:** 三轮代码均通过本地测试、clippy、fmt 与依赖审计;SEC-01 第二轮遗留的三处治理缓存容量边界已在第三轮闭环。**等待审计方最终复验，不代表 P0 审计通过或可生产部署。**
**交付目的:** 请审计方对照下表复验三轮修复，重点检查 SEC-01 治理 gossip 默认硬上限/池满顺序/本地提交路径、SEC-03 权重上限、SEC-04 奖励溢出防护、SEC-05 B7 wire 版本协商和 SEC-06 短帧预分配。

## 如何复验

```sh
git log --oneline 8117910..HEAD             # 审计基线 → 当前待复验提交
(cd node && cargo test --release -- --test-threads=1)   # 680 lib + 57 bin，全绿
(cd engine && cargo test --release)                     # 11，全绿
(cd node && cargo clippy --release --all-targets -- -D warnings)  # 零 lint
(cd node && cargo fmt --all -- --check)
(cd engine && cargo fmt --all -- --check)
(cd node && cargo audit) && (cd engine && cargo audit)  # 现为 CI 门禁
```
每项修复都附**命名回归测试**;下表的"回归测试"列可 `cargo test -- <name>` 单独跑。

## 对照表

| 编号 | 等级 | 状态 | 根因 | 修复(文件·符号) | 回归测试 | 提交 |
|---|---|---|---|---|---|---|
| SEC-03 | 严重 | ⚠️ 第一轮不完整；见第二轮 | `quorum()` 的 `2*total` 在 `u64` 回绕(total=2^63→quorum=1),<1/3 权重可认证冲突块、破坏最终性 | 第一轮仅改 quorum 乘法为 `u128`、总权重改饱和加法；二轮增加总权重硬上限并覆盖证书/创世/更新入口 | `quorum_does_not_overflow_at_large_power`；二轮 `verify_rejects_overflowing_validator_set` | M202、M206 |
| SEC-02 | 高 | ✅ 已修 | `state_root` 未承诺 `base_emission_micro`/`decay_half_life_blocks`/`slash_bps`/ΔK,篡改参数的快照/创世仍匹配 cert 根→同块异参分叉 | `lib.rs`:`ChainState::state_root()` 折入全部执行参数 | `state_root_covers_execution_params`(+ demo 守卫 re-baseline `01c163ce…`) | `f85c6f2` |
| SEC-04 | 高 | ⚠️ 第一轮不完整；见第二轮 | 治理执行不校验参数值,`slash_bps=20000` 令拒绝交易 `slash>stake` 回绕、破坏供应守恒;`usize` 转换平台相关 | 第一轮为 `validate_gov_param`/`validate_consensus_params` 增加 slash、发行、计数及部分浮点域检查；二轮为 ΔK 非负参数增加上限并对奖励、余额、供应增发做 checked arithmetic | `gov_param_validation_rejects_unsafe_values`、`gov_proposal_with_unsafe_param_is_rejected_at_registration` | M202、M206 |
| SEC-06 | 中 | ⚠️ 第一轮不完整；见第二轮 | 计数虽受 `MAX_ITEMS` 限,但某解码路径仍按攻击者声明数量预分配,短帧可触发大额申请 | 第一轮为 codec/net 多数列表增加 `PREALLOC_CAP=1024`;二轮修复 `dec_validator_set_inline` 并封顶预留 | `prealloc_caps_reservation`、`inline_validator_set_short_frame_rejects_without_large_alloc` | M202、M206 |
| SEC-07 | 中 | ✅ 已修 | `rustls 0.23.45`/`rustls-webpki 0.103.15`/`tokio 1.53.1` 命中 RUSTSEC 公告;CI audit 为 advisory | `Cargo.lock`→`rustls 0.23.46`/`rustls-webpki 0.103.16`/`tokio 1.54.1`(审计建议修复版);`ci.yml` audit 步去 `continue-on-error`→**门禁** | `cargo audit`(CI 门禁) | `f85c6f2` |
| SEC-01 | 高 | ⚠️ 三轮修复完成，待审计复验 | 治理 gossip 可污染候选池并持续占用缓存/待处理池 | 第一轮加入 admissibility、候选过滤和提交清理；二轮改为先验签、增加 4096 pending 上限并让可配置去重缓存覆盖治理集合；**三轮(M207)闭环容量边界：`SeenSet::bounded(DEFAULT_OP_SEEN_CAP=1<<16)` 作默认硬上限(不再依赖 `seen_cache=0`)、入站先验 `MAX_PENDING_GOV` 再 `seen.insert`、`submit_local_gov_*` 同样检查 pending 上限** | 第一轮 `gov_gossip_rejects_inadmissible_ops`、`poisoned_gov_pool_does_not_stall_candidate_production`；三轮 `bounded_seen_set_evicts_oldest`、`gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup` | M203、M206、M207 |
| SEC-05 | 中 | ⚠️ 第一轮不完整；见第二轮 | unbond 证明对 `source_header.bridge_root`(提交 locks 树)验证,正常源链无法生成可验证证明(旧测试靠伪造头 patch `bridge_root`+re-cert) | `lib.rs`+`codec.rs`:新增独立 cert 签名 `unbonding_root`(源链 unbonding 队列 Merkle 根)并改用其验证；B7 wire 改动升级 `PROTOCOL_VERSION` 并拒绝旧版本 | `unbonding_root_mismatch_is_rejected_on_apply`、`bridge_unbond_mirrors_to_dest_and_matures`、`version_negotiation_rejects_older_wire_gen` | M204、M206 |

## 需审计方重点回归的新增面

1. **B7 wire(SEC-05)**:`unbonding_root` 作为**前缀根**加入 header → `decode_certified_header` 固定前缀 252→**284**、`n_updates` 偏移 244→**276**、尾承诺仍 320。请复查:编解码对称、`from_block`/`to_block` 一致、`header.hash()==block.hash()` 前缀契约、跨代际拒绝(B6↔B7)、以及源链正常 seal 确实承诺 `unbonding_root`。
2. **治理 gossip 入口(SEC-01)**:`gov_proposal_admissible`/`gov_vote_admissible` **不**校验 nonce(容许流水线超前),nonce/顺序交由 apply + `filter_applicable_gov` 把关。请复查:投票要求提案**已提交**(id 仅在 apply 时分配,故这是正确约束)、`filter_applicable_gov` 的 trial 顺序(提案先于投票)与真实 `apply_block_inner` 一致、以及"中毒池不致停摆"在多提议人下成立。
3. **state_root 变更(SEC-02)**:确认**所有**影响状态转换的参数均已折入,且创世身份与快照校验绑定同一承诺(快照已序列化这些参数,交叉校验现覆盖它们)。

## 第二轮(追加修复与复核状态)

第一轮代码复核发现 5 项修复不彻底；M206 对其中问题追加修复。下表记录二轮代码及本轮独立复核结果，最终关闭状态由审计方确认。

| 编号 | 第一轮遗漏 | M206 二次修复 | 本轮代码复核 |
|---|---|---|---|
| SEC-03 | `Commit::verify` 仍以 `u64` 累加签名权重；饱和总权重会低估 quorum | 改用 `u128` 累加；以 `MAX_TOTAL_POWER=2^60` 在创世配置、验证人更新和证书验证处拒绝超额集合 | 实现与定向测试通过；请审计方复核上限覆盖所有构造和迁移路径 |
| SEC-04 | `Lam`/`BonusMax` 无上限；奖励转 `u64` 后普通加法可能溢出 | ΔK 旋钮设 `MAX_DELTAK_PARAM=1000`；奖励、余额、earned 和 supply 使用 checked arithmetic，溢出返回 `RewardOverflow` | 参数边界测试和全量测试通过；请审计方复核参数边界与治理执行序列 |
| SEC-01 | 无效操作先进入 seen；治理 seen 默认无界；pending 无界 | 先验签；缓存容量设置覆盖治理集合；远端 pending 加 4096 上限 | **二轮未闭环**：默认 `seen_cache=0` 时无界、池满后仍先插 seen、`submit_local_gov_*` 绕过 pending 上限。→ **三轮(M207)已闭环**：`SeenSet::bounded(DEFAULT_OP_SEEN_CAP=1<<16)` 默认硬上限，入站与本地提交均先检查 `MAX_PENDING_GOV` 再写 seen。请审计方复核默认配置、池满顺序与本地/远端入口 |
| SEC-06 | `dec_validator_set_inline` 有百万项计数上限但仍按 `n` 预分配 | 改用 `prealloc(n)` 封顶预留 | 短帧回归测试与全量测试通过；请审计方复核所有不可信计数解码路径 |
| SEC-05 | B7 增加 `unbonding_root` 后仍沿用协议版本 1 | 协议版本提升为 3，并增加旧版本拒连测试 | 实现与定向测试通过；请审计方确认代际编号和升级说明一致 |

**复验要点(二次/三次)**:(1) `MAX_TOTAL_POWER` 在创世/更新/cert 三路都拒绝；`verify` 使用 u128。(2) ΔK 旋钮上限与 checked arithmetic 共同限制单次奖励。(3) SEC-01 三轮(M207)已补齐默认硬上限(`SeenSet::bounded`)、池满先于 `seen.insert` 的顺序与本地提交路径，请复核三者。(4) 每次 wire 破坏必增 `PROTOCOL_VERSION`(B7 当前为 3)。

**部署结论:** SEC-01 三处容量边界已在第三轮(M207)闭环；三轮修复的最终关闭状态等待审计方基于代码和回归测试确认;外部审计及测试网经济参数反校等部署前置仍须单独完成,**本表不代表 P0 审计通过或可生产部署**。

## 本轮未覆盖(与审计一致,non-goals)

持久化 cargo-fuzz、形式化验证、跨实现互操作、生产渗透测试;以及 SECURITY.md §10 的已知边界
(无 liveness slashing、密钥在盘、无滚动升级、治理未对抗压测、经济参数待测试网反校)。这些
**不在本次修复范围**,属上线前另行推进项。
