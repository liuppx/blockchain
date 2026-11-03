# P0 审计修复对照表 / Remediation Matrix

**对应审计:** [`P0-SECURITY-REVIEW-2026-11.md`](P0-SECURITY-REVIEW-2026-11.md)(1 严重 + 3 高 + 3 中)
**审计基线:** `8117910`(M200)。第一轮修复为 M202–M204；第二轮修复为 M206(`d94af66`)；SEC-01 第三轮容量边界修复为 M207(`c7e0344`)；第四轮治理 admission 语义修复为 M208；**第五轮(回应审计方第四轮复验:多账户填池 / 密钥轮换滞留)为 M209**，本表更新至当前待复验状态。
**当前状态:** 审计方第四轮复验指出的两项剩余风险已在 M209 处理:当前 nonce 操作在池满/配额满时**优先淘汰停放最远的未来 nonce 项**(不依赖创世账户数上限,任意账户数下立即可应用的操作都不被饿死),以及**提交含 key rotation 的区块后清除该账户旧公钥签名的治理操作**;各附直接回归。**等待审计方复验，不代表 P0 审计通过或可生产部署。**
**交付目的:** 请审计方对照下表复验五轮修复，重点检查 SEC-01 治理共享 admission(nonce 窗口/按账户配额/当前 nonce 优先淘汰/结构化错误/RPC 4xx/提交后清理含轮换)、SEC-03 权重上限、SEC-04 奖励溢出防护、SEC-05 B7 wire 版本协商和 SEC-06 短帧预分配。

## 如何复验

```sh
git log --oneline 8117910..HEAD             # 审计基线 → 当前待复验提交
(cd node && cargo test --release -- --test-threads=1)   # 685 lib + 57 bin，全绿
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
| SEC-01 | 高 | ⚠️ 五轮修复完成，待审计复验 | 治理 gossip 可污染候选池并持续占用缓存/待处理池；四轮后仍有:≥256 个账户各停放窗口内未来 nonce 操作可占满全局池并持续拒绝其他账户当前 nonce 操作；密钥轮换后旧公钥签名的未来 nonce 操作仍在窗口内却永不可验签、占用账户配额 | 一至四轮见下文各节；**第五轮(M209)**:`net.rs` `admit_gov_*` 对**当前 nonce**(立即可应用)操作做优先——配额满时淘汰本账户停放最远的未来项、全局满时淘汰全池停放最远的未来项(`evict_furthest_future_gov_*`,平手取最旧),被淘汰项的哈希从去重缓存移除(`SeenSet::remove`)以便其主人日后重提;仅当池内**没有任何停放项**(全是当前 nonce)时硬上限才拒绝当前 nonce 操作;未来 nonce 操作仍直接 `GovPoolFull`。`apply_certified`:区块含 `key_rotations` 时对被轮换账户的待处理治理操作重跑完整 admissibility(含签名),旧公钥签名项即刻清除 | 五轮 `current_nonce_gov_op_evicts_parked_future_op_when_pool_full`(256 账户×16 未来项填满 → 新账户当前 nonce 入池且池仍恰在上限 → 配额满账户的当前 nonce 淘汰自己的 nonce-8 项(最旧) → 被淘汰项重提得 `GovAccountQuotaExceeded` 而非 `GovAlreadySeen` → 未来 nonce 项仍 `GovPoolFull` → 全池皆当前 nonce 时当前 nonce 项也 `GovPoolFull`)、`key_rotation_purges_pending_gov_ops_signed_by_old_key`(账户 1 停放 nonce 1..3 → 提交轮换 → 账户 nonce=1、窗口仍含 1..3 但旧签名项全部清除、账户 2 项存活 → 旧钥重提 `BadSignature`、新钥入池);四轮测试全部保留 | M203、M206、M207、M208、M209 |
| SEC-05 | 中 | ⚠️ 第一轮不完整；见第二轮 | unbond 证明对 `source_header.bridge_root`(提交 locks 树)验证,正常源链无法生成可验证证明(旧测试靠伪造头 patch `bridge_root`+re-cert) | `lib.rs`+`codec.rs`:新增独立 cert 签名 `unbonding_root`(源链 unbonding 队列 Merkle 根)并改用其验证；B7 wire 改动升级 `PROTOCOL_VERSION` 并拒绝旧版本 | `unbonding_root_mismatch_is_rejected_on_apply`、`bridge_unbond_mirrors_to_dest_and_matures`、`version_negotiation_rejects_older_wire_gen` | M204、M206 |

## 审计方第三轮复验记录（2026-11-02）

本轮基于 `80ffbc4`（M207 文档提交）及其父提交重新检查实现，并独立运行门禁。node release 测试为 **680 个库测试 + 57 个二进制测试通过、1 个忽略**，engine release 测试为 **11 个通过**；node `clippy --all-targets -- -D warnings`、node/engine `cargo fmt --check` 与 node/engine `cargo audit` 均通过。测试通过只证明现有覆盖范围内没有回归，不等同于 SEC-01 已关闭。

SEC-01 的以下容量边界已确认生效：治理与 key-rotation `SeenSet` 默认使用 `DEFAULT_OP_SEEN_CAP`；gossip 入站在 `seen.insert` 前检查 `MAX_PENDING_GOV`；本地提交路径也检查 pending 上限；候选构造会过滤无法在试算状态中应用的治理操作。

但复验发现两个仍可达的问题：

1. `gov_proposal_admissible` 明确不检查 nonce，签名正确且 nonce 超前的提案可以进入 `pending_gov_proposals`（`node/src/lib.rs:2211-2226`、`node/src/net.rs:1764-1781`）。`gov_vote_admissible` 也不检查 nonce；只要引用的提案仍开放，签名正确的超前 nonce 投票也可进入投票池（`node/src/lib.rs:2229-2247`、`node/src/net.rs:1784-1796`）。`filter_applicable_gov` 在候选试算时会跳过这些操作，却不会从 pending 队列删除它们（`node/src/net.rs:1366-1375`）；只有实际写入区块的操作才会在 `apply_certified` 清理（`node/src/net.rs:1304-1318`）。因此，持有治理账户密钥的一方可以用 4096 个不同的超前 nonce proposal/vote 占满对应池，随后当前 nonce 的合法操作会因容量检查被丢弃。候选区块仍可继续产生，但治理操作会发生可持续的容量拒绝，SEC-01 的可用性目标没有完全达到。
2. 本地/RPC 提交路径只验证签名，然后无条件向调用方返回 `Ok(hash)`；`submit_local_gov_proposal`/`submit_local_gov_vote` 在池满时返回空 gossip 并静默丢弃（`node/src/net.rs:1517-1535`），daemon 仍在 `node/src/daemon.rs:1735-1773` 回报成功。RPC 客户端会误以为操作已接收。该路径也没有在入池前调用完整的 `gov_proposal_admissible`，因此本地签名操作的安全参数校验依赖候选过滤，仍可占用 pending 槽位。

3. `gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup` 的注释说满池操作不会增长 seen cache，但测试只断言 pending 长度，未直接比较 `seen_gov_proposal.len()`。当前实现的控制流确实先检查容量再插 seen；建议补上长度断言，避免未来重排回归。

建议在最终关闭 SEC-01 前：为治理 pending 增加明确的 admission result/error（至少包含池满错误），使 RPC 在未入池时返回 4xx/结构化容量错误；对治理操作采用有限的 nonce look-ahead 或按账户/nonce 的有序队列，并在候选试算后清理确定不可应用、过期及已被更高 nonce 淘汰的项；本地和远端入口共用签名、参数和容量校验；补充“4096 个超前 nonce 后当前 nonce 仍可入池/或明确拒绝超前项”“满池 RPC 返回错误”以及 seen 长度不增长的回归测试。

## 第四轮修复（M208，逐条回应审计方第三轮复验）

| 审计方第三轮发现 / 建议 | M208 处理 | 回归测试 |
|---|---|---|
| (1) admissibility 不检查 nonce，超前 proposal/vote 可占满池且不被清理 | `lib.rs`:`gov_nonce_window` 共享检查——`nonce < acct.nonce` → `BadNonce`(永不可应用)、`nonce > acct.nonce + GOV_NONCE_LOOKAHEAD(=8)` → `NonceTooFarAhead`；`gov_proposal_admissible`/`gov_vote_admissible` 改为 `Result<(), ChainError>` 并纳入该窗口。`net.rs`:`MAX_PENDING_GOV_PER_ACCOUNT=16` 按账户配额(单账户最多 16 槽，填满 4096 需 256 个不同账户)。`apply_certified`:每次提交后用无签名开销的 `gov_proposal_not_stale`/`gov_vote_not_stale` 清理 nonce 已被(任意类型 op)消费、或提案已关闭/已成熟的滞留项 | `gov_admission_bounds_nonce_window_and_per_account_quota`(窗口内 0..=8 入池、9 → `NonceTooFarAhead`、配额满 → `GovAccountQuotaExceeded`、**其他账户当前 nonce 仍可入池**)；`stale_gov_ops_are_pruned_on_commit`(同账户另一 nonce-0 op 入块后，滞留 proposal 被清理且再提交 → `BadNonce{expected:1,got:0}`；提案关闭后滞留 vote 被清理) |
| (2) 本地/RPC 只验签、满池静默丢弃但 actor 回报 `Ok(hash)`；本地路径未走完整 admissible | `net.rs`:新增**唯一**入池路径 `admit_gov_proposal`/`admit_gov_vote`(签名→参数→nonce 窗口→`MAX_PENDING_GOV`→按账户配额→去重→入池；容量检查仍先于 `seen.insert`)，`on_gov_*` 与 `submit_local_gov_*` 都只调用它；`submit_local_gov_*` 返回 `Result<Vec<_>, ChainError>`，新增 `ChainError::GovPoolFull`/`GovAccountQuotaExceeded{account,limit}`/`GovAlreadySeen`。`daemon.rs`:actor 删除自己的重复验签，直接转发 admission 结果；`gov_reject_status` 把未入池映射为 **429**(池满/账户配额)、**409**(已见过)、**422**(签名/账户/参数/nonce/提案关闭)，只有真正入池才 200+hash | `rpc_gov_submit_returns_4xx_when_not_staged`(真实 TCP `POST /gov/proposal`:超前 nonce → 422、首个有效 → 200+hash、重复 → 409、配额满 → 429)；`submit_gov_proposal_stages_registers_and_rejects_bad_sig`(既有，BadSignature 仍经 Node 句柄返回) |
| (3) 满池测试未断言 `seen_gov_proposal.len()` | 测试重写:记录满池时 seen 长度，本地满池提交(`GovPoolFull`)与 gossip 满池入站后都断言 **seen 长度不变**；配额测试同样断言超配额 gossip 不增长 seen | `gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup`、`gov_admission_bounds_nonce_window_and_per_account_quota` |

设计说明供复核:(a) 窗口 + 配额组合——仅窗口时单账户仍可用 9 个 nonce × 不同内容填池，仅配额时单账户可停远 nonce，二者共同使"占满 = 256 个真实账户各自满配额"；(b) `filter_applicable_gov` 仍保留(候选试算层的最后防线)，清理改在 `apply_certified`(状态真正变化的时点)而非试算后，避免对未提交状态做删除；(c) 本地路径与 gossip 路径语义唯一差异是失败时前者返回结构化错误、后者静默丢弃且不转发。

## 审计方第四轮复验记录（2026-11-03）

本轮检查 M208 (`8acbcf4`) 并运行第四轮新增的治理 admission、清理和 RPC 测试。共享 admission、nonce 上限、每账户配额、池满结构化错误、RPC 非成功状态码及提交后清理均与实现一致；第三轮提出的两处直接问题及 seen 长度测试缺口已修复。定向测试 `gov_pending_pool_cap_holds_on_local_and_full_pool_does_not_grow_dedup`、`gov_admission_bounds_nonce_window_and_per_account_quota`、`stale_gov_ops_are_pruned_on_commit`、`rpc_gov_submit_returns_4xx_when_not_staged` 均通过。

验证结果：node release 测试 **683 个库测试通过、1 个忽略，57 个二进制测试通过**；engine release 测试 **11 个通过**；node release clippy 与 node/engine fmt 检查通过。node `cargo audit` 使用最新成功获取的 advisory 数据库通过；engine 在线刷新 advisory 数据库时网络失败，随后 `cargo audit --no-fetch --stale` 使用本机缓存的 1402 条公告通过，因此 engine 审计数据库的新鲜度未在本次确认。

但全局池在多账户场景仍可被填满并持续拒绝其他账户操作。代码没有运行时新增账户入口，账户集合在 genesis 建立；testnet genesis 只有 3 个账户，因此该样例的每类池最多由配额容纳 `3 × 16 = 48` 项。另一方面，`GenesisConfig::to_genesis` 和 `ChainState::genesis_split` 未设置账户总数上限，M208 的满池测试也实际构造了 256 个不同账户：每个账户提交 16 个 nonce 1–8、均在 `[acct.nonce, acct.nonce + 8]` 窗口内的操作，即填满 4096 槽。候选过滤因缺少 nonce 0 而跳过这些操作；提交后清理只在 nonce 超出窗口时删除它们，而空块不会改变 nonce，因此这些项持续占位，未占满配额的账户当前 nonce 操作返回 `GovPoolFull`。此场景需要创世时至少存在 256 个可签名账户；其是否适用于目标生产网络取决于生产 genesis，而仓库未包含该配置。

另一个可达的局部滞留路径涉及密钥轮换。治理操作入池时以账户当时的公钥验签（`node/src/lib.rs` 中 `gov_proposal_admissible`/`gov_vote_admissible`）；区块执行顺序是在治理操作前应用 `key_rotations`（`node/src/lib.rs:1949-1973`），轮换会替换账户公钥并增加 nonce（`node/src/lib.rs:2432-2465`）。提交后 `apply_certified` 的待处理清理只调用 nonce-window / proposal-open 谓词（`node/src/net.rs:1324-1334`），没有检查操作签名是否仍能由新公钥验证。因此，同一账户已排队的旧公钥签名未来 nonce 操作可在轮换后仍处于 nonce 窗口、继续占用其 16 个配额槽，但以后无法通过 `apply_gov_proposal`/`apply_gov_vote` 的签名验证；账户只能继续消耗 nonce 使它们移出窗口，或等待节点重启清空内存池。该路径可导致账户级治理提交被拒绝；与多账户占满池组合时会延长全局准入阻塞。

建议在 SEC-01 对目标生产链关闭前，确认并审计最终 genesis 账户数量；让 pending admission 对可立即应用的当前 nonce 操作保留准入空间，或在池满时优先淘汰长期不可应用的未来 nonce 项；并在提交包含 key rotation 的区块后清理该账户所有以旧公钥签名的治理操作。新增“多账户未来 nonce 填满 → 空块提交后仍滞留 → 当前 nonce 被拒绝/修复后成功入池”及“密钥轮换后旧签名 pending 操作被清理”的回归。若部署通过强制 genesis 上限保证账户总量低于 `MAX_PENDING_GOV / MAX_PENDING_GOV_PER_ACCOUNT`，也应把该上限加入验证并覆盖配置加载与启动路径。

## 第五轮修复（M209，逐条回应审计方第四轮复验）

| 审计方第四轮发现 / 建议 | M209 处理 | 回归测试 |
|---|---|---|
| (1) ≥256 个 genesis 账户各停放窗口内未来 nonce 操作可占满全局池；空块不推进 nonce，停放项持续占位，其他账户当前 nonce 操作 `GovPoolFull`。建议:为当前 nonce 操作保留准入空间 / 池满时优先淘汰长期不可应用的未来项;或强制创世账户上限 | 采用**优先淘汰**而非创世账户上限(上限会把安全性寄托在生产 genesis 之外的配置,且限制正常规模的链)。`admit_gov_proposal/vote`:判定 `current = op.nonce == acct.nonce`;配额满且 current → `evict_furthest_future_gov_*(Some(account))` 淘汰本账户停放最远项;全局满且 current → `evict_furthest_future_gov_*(None)` 淘汰全池停放最远项(距离 `op.nonce - acct.nonce` 最大,平手取最旧)。淘汰项哈希经新增 `SeenSet::remove` 从去重缓存移除,主人可日后重提。非 current 操作在满池/满配额时仍直接 `GovPoolFull`/`GovAccountQuotaExceeded`;池内无停放项时(全为当前 nonce)硬上限对当前 nonce 操作同样生效——淘汰永不丢弃可应用项。结果:任意账户数下,停放的未来项都不能饿死立即可应用的治理操作;"空块后仍滞留"由构造成立但不再有害 | `current_nonce_gov_op_evicts_parked_future_op_when_pool_full`(a 新账户当前 nonce 入满池、池仍恰在上限、恰淘汰 1 个未来项;b 配额满账户当前 nonce 淘汰自己最旧的 nonce-8 项;被淘汰项重提得配额错误而非 `GovAlreadySeen`;c 未来 nonce 项仍 `GovPoolFull`;d 全池当前 nonce 时硬上限生效) |
| (2) 密钥轮换后,旧公钥签名的未来 nonce 操作仍在窗口内(轮换只消费 1 个 nonce)却永不可验签,持续占用该账户 16 个配额槽;建议提交含 key rotation 的区块后清除该账户旧签名操作 | `apply_certified`:在既有 nonce 窗口 / 提案开放清理之后,若 `block.key_rotations` 非空,对被轮换账户的待处理 proposal/vote 重跑完整 `gov_*_admissible`(含对**新**公钥验签);签名开销上限为该账户配额(16),仅在轮换块触发 | `key_rotation_purges_pending_gov_ops_signed_by_old_key`(停放 nonce 1..3 → 提交轮换 → 账户 nonce=1、1..3 仍在窗口但全部清除、他账户项存活 → 旧钥重提 `BadSignature`、新钥 nonce 1 入池) |
| engine `cargo audit` 在线刷新失败,仅以本机缓存通过 | 非代码项;CI 门禁在线环境执行 `cargo audit`,请审计方在有网络的环境复跑 `(cd engine && cargo audit)` | — |

设计说明供复核:(a) 优先级只授予"立即可应用"(`nonce == acct.nonce`)的操作,因此攻击者无法用优先级本身互相淘汰——两个当前 nonce 操作之间不淘汰,硬上限仍然是硬上限;(b) 淘汰对象只会是停放项(`nonce > acct.nonce`),被淘汰的合法流水线操作可在有空位时重提(哈希已从 seen 移除),代价是提交者需重试,这是满池下的预期背压;(c) 轮换清理用完整 admissibility 而非仅验签,顺带覆盖轮换块同时改变的其他条件。


1. **B7 wire(SEC-05)**:`unbonding_root` 作为**前缀根**加入 header → `decode_certified_header` 固定前缀 252→**284**、`n_updates` 偏移 244→**276**、尾承诺仍 320。请复查:编解码对称、`from_block`/`to_block` 一致、`header.hash()==block.hash()` 前缀契约、跨代际拒绝(B6↔B7)、以及源链正常 seal 确实承诺 `unbonding_root`。
2. **治理 gossip 入口(SEC-01)**:M208 起 `gov_proposal_admissible`/`gov_vote_admissible` 校验 **nonce 窗口**(`[acct.nonce, acct.nonce+8]`,容许有限流水线超前),严格 `nonce == acct.nonce` 顺序仍由 apply + `filter_applicable_gov` 把关。请复查:投票要求提案**已提交**(id 仅在 apply 时分配,故这是正确约束)、`filter_applicable_gov` 的 trial 顺序(提案先于投票)与真实 `apply_block_inner` 一致、"中毒池不致停摆"在多提议人下成立、`admit_gov_*` 的检查顺序(容量先于 `seen.insert`)在本地与 gossip 两入口一致；还需确认 256 个账户可填满池的条件与 key rotation 后旧公钥签名 pending 项的淘汰。
3. **state_root 变更(SEC-02)**:确认**所有**影响状态转换的参数均已折入,且创世身份与快照校验绑定同一承诺(快照已序列化这些参数,交叉校验现覆盖它们)。

## 第二轮(追加修复与复核状态)

第一轮代码复核发现 5 项修复不彻底；M206 对其中问题追加修复。下表记录二轮代码及本轮独立复核结果，最终关闭状态由审计方确认。

| 编号 | 第一轮遗漏 | M206 二次修复 | 本轮代码复核 |
|---|---|---|---|
| SEC-03 | `Commit::verify` 仍以 `u64` 累加签名权重；饱和总权重会低估 quorum | 改用 `u128` 累加；以 `MAX_TOTAL_POWER=2^60` 在创世配置、验证人更新和证书验证处拒绝超额集合 | 实现与定向测试通过；请审计方复核上限覆盖所有构造和迁移路径 |
| SEC-04 | `Lam`/`BonusMax` 无上限；奖励转 `u64` 后普通加法可能溢出 | ΔK 旋钮设 `MAX_DELTAK_PARAM=1000`；奖励、余额、earned 和 supply 使用 checked arithmetic，溢出返回 `RewardOverflow` | 参数边界测试和全量测试通过；请审计方复核参数边界与治理执行序列 |
| SEC-01 | 无效操作先进入 seen；治理 seen 默认无界；pending 无界 | 先验签；缓存容量设置覆盖治理集合；远端 pending 加 4096 上限 | **二轮未闭环**：默认 `seen_cache=0` 时无界、池满后仍先插 seen、`submit_local_gov_*` 绕过 pending 上限。→ **三轮(M207)已补齐三处容量边界**：`SeenSet::bounded(DEFAULT_OP_SEEN_CAP=1<<16)` 默认上限，入站与本地提交均先检查 `MAX_PENDING_GOV` 再写 seen。三轮复验发现 admissibility 不检查 nonce、超前操作可填满 pending，且本地池满时 actor 仍返回 `Ok(hash)`。→ **四轮(M208)已处理上述问题**：共享 `admit_gov_*`、nonce 窗口、按账户配额、结构化错误、RPC 4xx 与提交后清理。**四轮复验仍有条件剩余项**：≥256 个 genesis 账户可占满全局池；key rotation 后旧签名操作仍可能占账户配额，见「审计方第四轮复验记录」。→ **五轮(M209)已处理**:当前 nonce 操作优先淘汰停放最远的未来项(任意账户数下有效)、轮换块后清除旧公钥签名项,见「第五轮修复」。|
| SEC-06 | `dec_validator_set_inline` 有百万项计数上限但仍按 `n` 预分配 | 改用 `prealloc(n)` 封顶预留 | 短帧回归测试与全量测试通过；请审计方复核所有不可信计数解码路径 |
| SEC-05 | B7 增加 `unbonding_root` 后仍沿用协议版本 1 | 协议版本提升为 3，并增加旧版本拒连测试 | 实现与定向测试通过；请审计方确认代际编号和升级说明一致 |

**复验要点(二次/三次/四次/五次)**:(1) `MAX_TOTAL_POWER` 在创世/更新/cert 三路都拒绝；`verify` 使用 u128。(2) ΔK 旋钮上限与 checked arithmetic 共同限制单次奖励。(3) M208 已补齐 nonce 窗口、每账户配额、共享 admission、结构化错误/RPC 4xx 与提交后清理;M209 补齐当前 nonce 优先淘汰(不依赖创世账户数)与轮换后旧签名清理,请按「第五轮修复」表逐条复核,重点:淘汰只针对停放项、两个当前 nonce 操作间不淘汰、被淘汰项可重提。(4) 每次 wire 破坏必增 `PROTOCOL_VERSION`(B7 当前为 3)。

**部署结论:** 审计方第四轮复验指出的两项剩余风险(多账户未来 nonce 占满全局池、密钥轮换后旧签名滞留)已在 M209 以不依赖生产 genesis 账户数的方式处理并附直接回归；最终关闭状态等待审计方基于代码和回归测试确认。外部审计及测试网经济参数反校等部署前置仍须单独完成，**本表不代表 P0 审计通过或可生产部署**。

## 本轮未覆盖(与审计一致,non-goals)

持久化 cargo-fuzz、形式化验证、跨实现互操作、生产渗透测试;以及 SECURITY.md §10 的已知边界
(无 liveness slashing、密钥在盘、无滚动升级、治理未对抗压测、经济参数待测试网反校)。这些
**不在本次修复范围**,属上线前另行推进项。
