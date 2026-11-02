# P0 审计修复对照表 / Remediation Matrix

**对应审计:** [`P0-SECURITY-REVIEW-2026-11.md`](P0-SECURITY-REVIEW-2026-11.md)(1 严重 + 3 高 + 3 中)
**审计基线:** `8117910`(M200)。第一轮修复为 M202–M204；第二轮修复为 M206(`d94af66`)；SEC-01 第三轮容量边界修复为 M207(`c7e0344`)，本表更新至当前待复验状态。
**当前状态:** 三轮代码均通过本地测试、clippy、fmt 与依赖审计;SEC-01 第二轮遗留的三处治理缓存容量边界已在第三轮补齐，但审计方复验仍发现治理 pending 池的超前 nonce 滞留与本地满池成功回报问题，SEC-01 **尚未完全关闭**。**不代表 P0 审计通过或可生产部署。**
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
| SEC-01 | 高 | ⚠️ 第三轮仍未完全关闭 | 签名正确但 nonce 超前的 proposal/vote 可占满有界 pending 池；候选过滤只跳过、不清理；本地提交满池时仍回报成功 | 第一轮 admissibility、候选过滤和提交清理；第二轮验签与 4096 pending 上限；第三轮默认 bounded seen、满池先于 seen 插入、本地池容量限制均已生效。仍需限制 nonce look-ahead、淘汰永久不可应用项，并让本地/RPC admission 显式返回成功或池满错误 | 现有 `poisoned_gov_pool_does_not_stall_candidate_production` 只测候选可继续产生；第三轮容量测试未覆盖超前 nonce 滞留，且未断言 seen 长度。需新增超前 nonce 与 RPC 满池错误回归 | M203、M206、M207 |
| SEC-05 | 中 | ⚠️ 第一轮不完整；见第二轮 | unbond 证明对 `source_header.bridge_root`(提交 locks 树)验证,正常源链无法生成可验证证明(旧测试靠伪造头 patch `bridge_root`+re-cert) | `lib.rs`+`codec.rs`:新增独立 cert 签名 `unbonding_root`(源链 unbonding 队列 Merkle 根)并改用其验证；B7 wire 改动升级 `PROTOCOL_VERSION` 并拒绝旧版本 | `unbonding_root_mismatch_is_rejected_on_apply`、`bridge_unbond_mirrors_to_dest_and_matures`、`version_negotiation_rejects_older_wire_gen` | M204、M206 |

## 审计方第三轮复验记录（2026-11-02）

本轮基于 `80ffbc4`（M207 文档提交）及其父提交重新检查实现，并独立运行门禁。node release 测试为 **680 个库测试 + 57 个二进制测试通过、1 个忽略**，engine release 测试为 **11 个通过**；node `clippy --all-targets -- -D warnings`、node/engine `cargo fmt --check` 与 node/engine `cargo audit` 均通过。测试通过只证明现有覆盖范围内没有回归，不等同于 SEC-01 已关闭。

SEC-01 的以下容量边界已确认生效：治理与 key-rotation `SeenSet` 默认使用 `DEFAULT_OP_SEEN_CAP`；gossip 入站在 `seen.insert` 前检查 `MAX_PENDING_GOV`；本地提交路径也检查 pending 上限；候选构造会过滤无法在试算状态中应用的治理操作。

但复验发现两个仍可达的问题：

1. `gov_proposal_admissible` 明确不检查 nonce，签名正确且 nonce 超前的提案可以进入 `pending_gov_proposals`（`node/src/lib.rs:2211-2226`、`node/src/net.rs:1764-1781`）。`gov_vote_admissible` 也不检查 nonce；只要引用的提案仍开放，签名正确的超前 nonce 投票也可进入投票池（`node/src/lib.rs:2229-2247`、`node/src/net.rs:1784-1796`）。`filter_applicable_gov` 在候选试算时会跳过这些操作，却不会从 pending 队列删除它们（`node/src/net.rs:1366-1375`）；只有实际写入区块的操作才会在 `apply_certified` 清理（`node/src/net.rs:1304-1318`）。因此，持有治理账户密钥的一方可以用 4096 个不同的超前 nonce proposal/vote 占满对应池，随后当前 nonce 的合法操作会因容量检查被丢弃。候选区块仍可继续产生，但治理操作会发生可持续的容量拒绝，SEC-01 的可用性目标没有完全达到。
2. 本地/RPC 提交路径只验证签名，然后无条件向调用方返回 `Ok(hash)`；`submit_local_gov_proposal`/`submit_local_gov_vote` 在池满时返回空 gossip 并静默丢弃（`node/src/net.rs:1517-1535`），daemon 仍在 `node/src/daemon.rs:1735-1773` 回报成功。RPC 客户端会误以为操作已接收。该路径也没有在入池前调用完整的 `gov_proposal_admissible`，因此本地签名操作的安全参数校验依赖候选过滤，仍可占用 pending 槽位。

3. `gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup` 的注释说满池操作不会增长 seen cache，但测试只断言 pending 长度，未直接比较 `seen_gov_proposal.len()`。当前实现的控制流确实先检查容量再插 seen；建议补上长度断言，避免未来重排回归。

建议在最终关闭 SEC-01 前：为治理 pending 增加明确的 admission result/error（至少包含池满错误），使 RPC 在未入池时返回 4xx/结构化容量错误；对治理操作采用有限的 nonce look-ahead 或按账户/nonce 的有序队列，并在候选试算后清理确定不可应用、过期及已被更高 nonce 淘汰的项；本地和远端入口共用签名、参数和容量校验；补充“4096 个超前 nonce 后当前 nonce 仍可入池/或明确拒绝超前项”“满池 RPC 返回错误”以及 seen 长度不增长的回归测试。

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
| SEC-01 | 无效操作先进入 seen；治理 seen 默认无界；pending 无界 | 先验签；缓存容量设置覆盖治理集合；远端 pending 加 4096 上限 | **二轮未闭环**：默认 `seen_cache=0` 时无界、池满后仍先插 seen、`submit_local_gov_*` 绕过 pending 上限。→ **三轮(M207)已补齐三处容量边界**：`SeenSet::bounded(DEFAULT_OP_SEEN_CAP=1<<16)` 默认上限，入站与本地提交均先检查 `MAX_PENDING_GOV` 再写 seen。**本轮仍未关闭**：admissibility 不检查 nonce，超前操作可填满 pending 且不会被清理；本地池满时 actor 仍返回 `Ok(hash)`。|
| SEC-06 | `dec_validator_set_inline` 有百万项计数上限但仍按 `n` 预分配 | 改用 `prealloc(n)` 封顶预留 | 短帧回归测试与全量测试通过；请审计方复核所有不可信计数解码路径 |
| SEC-05 | B7 增加 `unbonding_root` 后仍沿用协议版本 1 | 协议版本提升为 3，并增加旧版本拒连测试 | 实现与定向测试通过；请审计方确认代际编号和升级说明一致 |

**复验要点(二次/三次)**:(1) `MAX_TOTAL_POWER` 在创世/更新/cert 三路都拒绝；`verify` 使用 u128。(2) ΔK 旋钮上限与 checked arithmetic 共同限制单次奖励。(3) SEC-01 已补齐默认 seen 上限、池满检查顺序和本地池容量边界，但还需关闭 nonce 超前滞留及满池 RPC 成功回报。(4) 每次 wire 破坏必增 `PROTOCOL_VERSION`(B7 当前为 3)。

**部署结论:** SEC-01 的默认 seen 上限、池满顺序和本地池容量边界已补齐，但上述超前 nonce 滞留与 RPC 误报成功仍未关闭；因此本轮复验**不建议生产部署**。完成治理 admission/淘汰语义、RPC 错误返回和针对性回归测试后，需再次由审计方复验；外部审计及测试网经济参数反校等部署前置仍须单独完成，**本表不代表 P0 审计通过或可生产部署**。

## 本轮未覆盖(与审计一致,non-goals)

持久化 cargo-fuzz、形式化验证、跨实现互操作、生产渗透测试;以及 SECURITY.md §10 的已知边界
(无 liveness slashing、密钥在盘、无滚动升级、治理未对抗压测、经济参数待测试网反校)。这些
**不在本次修复范围**,属上线前另行推进项。
