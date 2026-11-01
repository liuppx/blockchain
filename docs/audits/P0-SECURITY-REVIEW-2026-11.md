# P0 安全面安全审计报告

**审计对象：** YeYing blockchain，`8117910`（M200，2026-11-01 工作树）
**范围：** `docs/SECURITY.md` §8 所列 P0 面，重点复查 codec、跨链桥、状态根与治理、BFT 共识、密钥路径及其网络入口。
**审计性质：** 基于源码和本地门禁的独立代码审查；不是第三方审计机构的签章报告、形式化证明或生产安全认证。

## 结论

本轮发现 1 项严重、3 项高危和 3 项中危问题。严重与高危项涉及 BFT 最终性、快照状态完整性、治理消息造成的持续停块，以及治理参数破坏供应守恒。另在 node 依赖树发现多项 TLS 依赖安全公告。建议这些问题修复并通过独立回归验证前，不将当前实现视为已通过 P0 安全审计。

严重等级按可造成的影响与触发条件分级；“P0”指审计范围，不等同于每项发现的严重等级。报告没有给出 CVSS 分数。

## 发现摘要

| 编号 | 等级 | 主题 | 主要影响 |
|---|---|---|---|
| SEC-01 | 高 | 无效治理 gossip 可污染候选池 | 节点无法构造可提交区块；经 gossip 扩散可持续阻断出块 |
| SEC-02 | 高 | 共识参数未纳入状态承诺 | 经过认证的快照可带入不同执行参数，造成节点状态分叉 |
| SEC-03 | 严重 | BFT quorum 乘法溢出 | 可让低于 1/3 的权重分别认证冲突区块，破坏最终性 |
| SEC-04 | 高 | 治理参数无范围校验 | 罚没超过质押，导致余额运算回绕或检查构建崩溃，并破坏供应守恒 |
| SEC-05 | 中 | BridgeUnbond 使用了错误的承诺树 | 正常源链无法生成可供目的链验证的 unbond 证明 |
| SEC-06 | 中 | codec 计数上限仍允许超大预分配 | 很短的区块帧可请求约 176 MB 的交易向量容量 |
| SEC-07 | 中 | node TLS 依赖存在未修复安全公告 | `rustls`、`rustls-webpki` 受公告影响；CI audit 当前不阻断 |

## 详细发现

### SEC-01 — 无效治理 gossip 可污染候选池并阻断出块

**等级：高**
**位置：** `node/src/net.rs:1706–1722`、`node/src/net.rs:1239–1282`、`node/src/net.rs:1299–1338`

收到治理提案或投票时，网络路径只按内容哈希去重，然后直接放入 `pending_gov_proposals` / `pending_gov_votes` 并继续 gossip。签名、nonce、提案状态等检查要等到区块应用时才执行。另一方面，候选区块构造会复制整个治理待处理池并调用 `seal`；其中任一无效或已过期操作都会使 `seal` 失败，`build_candidate` 随即返回 `None`。默认网络配置允许关闭 peer 身份认证，因此攻击者不必先成为验证人或持有治理权重。

问题也会在正常提交后出现：`apply_certified` 会清理已提交的 evidence、stake op 和 key rotation，但没有清理已提交的治理提案与投票。提交后再次打包同一操作会因 nonce 已消费而失败。一个节点拒绝构造候选区块；由于收到的治理消息会继续向其他 peer 扩散，该问题可传播到其他验证人，反复使被选中的 proposer 超时。

**复现路径：** 向 full node 发送一个编码正确但签名无效、nonce 过期或 proposal id 不存在的 `GovProposal`/`GovVote` gossip 消息；操作进入 pending 后，下一次 `build_candidate(timestamp)` 返回 `None`。也可先提交一个有效治理操作，再检查提交节点的 pending 队列和下一高度候选构造。

**建议：** 在 gossip 入池阶段验证账户、签名、nonce、提案存在性和投票窗口；对同一区块内的连续 nonce 按确定顺序做池内模拟；提交后清理已提交、过期和已被更高 nonce 淘汰的治理操作；给治理池设置明确的容量与淘汰策略。保留区块应用时的完整复验，并增加“无效 gossip 不阻断候选”和“提交后不会重提”回归测试。

### SEC-02 — `state_root` 未承诺执行参数

**等级：高**
**位置：** `node/src/lib.rs:935–949`、`node/src/lib.rs:2695–2745`、`node/src/lib.rs:1948–1963`、`node/src/codec.rs:1615–1638`、`node/src/light.rs:944–961`、`node/src/daemon.rs:825–839`

`ChainState::state_root()` 承诺账户、验证人、提案等字段，却没有编码 `params`、`base_emission_micro`、`decay_half_life_blocks` 或 `slash_bps`。这些值被快照编码器写入快照，也会由治理修改，并直接决定 ΔK、增发和罚没结果。快照校验只比较高度、`state_root`、账户根和 bridge 根，所以一份字段被篡改的快照仍可能匹配已签名头中的所有根。节点随后对同一区块使用不同执行参数，结果可在后续状态转换中分叉。

同一缺口也影响创世身份：创世头承诺由 `state_root` 派生的状态，但这些规则参数未进入该根，因此除这些参数外配置相同的创世可以得到相同的创世区块哈希，却有不同的执行规则。

**复现路径：** 生成快照并保留其高度与其他状态；只改快照中的 `slash_bps` 或 ΔK 参数；用原有的已认证头和验证人集运行快照校验。现有校验重算的根不会因这些字段变化而改变。

**建议：** 将所有会影响状态转换的参数纳入共识承诺，并让创世身份及快照校验绑定同一承诺。由于区块头和快照协议已有版本，实施时需安排明确的 wire/version 升级与旧链迁移规则。增加参数逐字段改变必然改变承诺根的测试，并增加“篡改快照参数被拒绝”的端到端测试。

### SEC-03 — quorum 计算在大权重下溢出，破坏 BFT 最终性

**等级：严重**
**位置：** `node/src/validator.rs:115–116`、`node/src/validator.rs:143–146`

quorum 使用 `self.total_power() * 2 / 3 + 1`，但乘法在 `u64` 中执行，验证人权重没有对应的安全上限。总权重为 `2^63` 时，Release 构建里的 `2 * total_power` 回绕为 0，计算出的 quorum 变成 1。例：四名验证人各有 `2^61` 权重，总权重可准确表示为 `2^63`；单名验证人的签名权重就远高于错误的 quorum。权重仅占总量 1/4 的恶意验证人因而可分别签署两个冲突区块，两个证书都可能通过验证。这已经超出 Tendermint 式 BFT 对少于 1/3 拜占庭权重的安全保证。

**复现路径：** 建立上述四验证人集合，检查 `quorum()`；在默认 Release 算术下其结果为 1。再分别以一名验证人的签名构造两个不同区块的 commit，`Commit::verify` 会把单签视为达到 quorum。开启溢出检查的构建则会在 quorum 计算处 panic，同样无法正常处理该集合。

**建议：** 使用 `u128` 计算 `floor(2T/3)+1`，同时对权重汇总进行 checked accumulation，并在创世配置、验证人更新和证书验证路径统一拒绝超出协议上限的集合。增加边界测试，覆盖 `2^63`、接近 `u64::MAX`、汇总溢出及两个冲突证书。

### SEC-04 — 治理参数缺少范围校验，可破坏资金守恒

**等级：高**
**位置：** `node/src/lib.rs:1948–1963`、`node/src/lib.rs:2649–2657`

治理执行直接把 `SlashBps` 写入状态，没有要求其不大于 10,000。拒绝交易时按 `slash = stake * slash_bps / 10_000` 计算，再执行 `tx.stake - slash`。例如已通过治理的 `slash_bps = 20_000`，对 `stake = 10_000` 的拒绝交易会令 `slash = 20_000`：在默认 Release 算术下 `stake - slash` 回绕并把异常余额写入账户；启用溢出检查时该路径 panic。账户余额与 treasury 的变化不再保持文档声明的供应守恒。

同一入口也接受任意 `base_emission_micro` 和非有限 ΔK 浮点值；`NReviewMin` / `NMin` 则将任意 `u64` 转成平台相关的 `usize`，在 32 位与 64 位验证人上可得到不同参数。提案 tally 限制的是谁可以通过变更，不会保证变更值本身安全。

**复现路径：** 通过治理流程执行 `SlashBps(20_000)`，然后应用一笔评分被判拒绝、质押为 10,000 的有效提交。检查账户余额、treasury 与 `supply_conserved()`；也可在开启 overflow checks 的构建观察算术失败。

**建议：** 在创世加载、治理提案准入和治理执行处共用参数验证器；至少限制罚没率为 `[0, 10_000]`，限制发行量及 ΔK 参数到经确认的有限有效域，并对 `u64 -> usize` 使用有明确协议语义的 checked conversion。对每种可治理参数加入边界与供应守恒测试。

### SEC-05 — `BridgeUnbond` 证明根与正常源链承诺不匹配

**等级：中**
**位置：** `node/src/lib.rs:1880–1881`、`node/src/lib.rs:2438–2470`、`node/src/lib.rs:2989–3002`、`node/src/lib.rs:6090–6147`

正常区块验证要求 `header.bridge_root` 等于 `bridge_locks` 的 Merkle 根；而 `apply_bridge_unbond` 用同一个 `bridge_root` 验证 `unbonding` 队列叶子。源链没有单独承诺 unbonding 队列的头字段，因此正常有效源链状态通常无法为目的链生成可验证的 unbond 证明。

当前端到端测试没有验证正常源链能产生这种头：测试先提交合法区块，再把 `bridge_root` 改成 unbonding 根并重新签证书；修改后的头没有再通过源链 `apply_block` 的 bridge-root 检查。目的链测试由此验证的是人工构造的已签名头，而非正常源链状态。

**建议：** 为 unbonding 队列增加独立、版本化且由源链状态机验证的承诺根，或设计包含类型标签的统一桥接树。回归测试必须先在源链正常 seal、认证并提交含 unbond 的区块，再从该区块生成证明并由目的链成功跟随与镜像。

### SEC-06 — codec 的百万项上限仍允许过大的预分配

**等级：中**
**位置：** `node/src/codec.rs:2541–2546`、`node/src/codec.rs:2356–2359`、`node/src/net.rs:2569–2580`、`node/src/daemon.rs:78–80,128–141`

`Dec::count()` 把列表限制为最多 1,000,000 项，但 `decode_block` 在确认元素字节存在前就按 `n_txs` 调用 `Vec::with_capacity`。在当前 64 位布局中，`SubmissionTx` 约占 176 字节，百万项意味着约 176 MB 的容量预留。`MAX_FRAME` 的 16 MiB 限制只约束外层帧大小，不限制内层声明的元素数，因此很短的、声明百万笔交易后立即截断的网络帧也会触发该预留。是否造成物理内存耗尽取决于分配器、并发度和运行环境，但单帧可造成显著的分配压力。

现有 fuzz-lite 测试验证了大于 `MAX_ITEMS` 的计数会被拒绝，没有覆盖恰好达到上限但后续字节不足的输入。

**建议：** 依据剩余字节数和元素最小编码长度拒绝不可能的计数；对攻击者控制的集合使用较小的上下文上限，并避免一次性预留完整声明容量。增加百万项截断帧测试，验证在尝试巨额分配前返回错误。

### SEC-07 — node TLS 依赖命中未修复的安全公告

**等级：中（依赖风险；是否可利用取决于具体功能路径）**
**位置：** `node/Cargo.lock` 中的 `rustls 0.23.45`、`rustls-webpki 0.103.15`、`tokio 1.53.1`；依赖路径为 `zhixing-node → tokio-rustls → rustls`

本轮运行 `cargo audit` 后，engine 锁文件无公告；node 锁文件有 5 项漏洞公告和 1 项 unsound 警告。5 项漏洞公告包括 `rustls 0.23.45` 的 RUSTSEC-2026-0346、0347、0348、0349，以及 `rustls-webpki 0.103.15` 的 RUSTSEC-2026-0382。审计数据库给出的修复版本分别为 `rustls >= 0.23.46`、`rustls-webpki >= 0.103.16`。另有 `tokio 1.53.1` 的 RUSTSEC-2026-0386 unsound 警告。`cargo audit` 对 node 返回非零状态。

这些公告涉及 TLS session `Debug` 输出、ECH 回退、证书压缩预认证分配、密钥协商降级及不受信任中间证书验证。当前审计确认它们存在于锁文件并经 `tokio-rustls` 进入 node；没有逐项验证对应功能是否由本项目配置启用，因此未把每项公告单独描述为已证实的可利用漏洞。

**建议：** 更新到公告修复版本，检查锁文件中的 feature resolution；若某项短期内不能升级，应逐项确认对应功能是否开启，并用可复现证据记录缓解措施。将 `cargo audit` 从 `continue-on-error` advisory 提升为合并门禁前，先修复或显式审查并记录所有公告。

## 重点路径审查记录

- **治理与网络入口：** 本轮发现 SEC-01、SEC-04。RPC 本地提案/投票入口会先验证签名，但这不覆盖远端 gossip ingress。
- **状态承诺与快照：** 本轮发现 SEC-02。快照验证有证书/可信验证人集路径，但其参数完整性仍受同一 `state_root` 缺口影响。
- **共识：** 本轮发现 SEC-03。commit 签名、重复投票拒绝等路径有既有测试；本报告只表示对相关实现做了有针对性的审阅，不构成对完整 round 状态机的形式化验证。
- **桥：** lock/redeem 的目的链、包含证明和重放检查有现有测试；本轮发现 SEC-05 仅针对 unbond 承诺路径。
- **codec：** 超过计数上限的伪造长度已被拒绝；SEC-06 是合法上限本身过大及预分配策略的问题。
- **密钥与 keystore：** 签名由 `ed25519-dalek` 提供；keystore 路径先校验 bounded scrypt 参数，验证 MAC 后才解密，并使用常时比较。此次定向审阅没有确认到可报告的密钥提取缺陷。运行时密钥仍在进程内存中，仓库也未提供 HSM/远程签名器；此项属于已披露的运维边界。node 的 TLS 依赖公告见 SEC-07。

## 验证与限制

已运行：

- `cargo fmt --manifest-path node/Cargo.toml -- --check` 与 `cargo fmt --manifest-path engine/Cargo.toml -- --check`：通过。
- `cargo clippy --manifest-path node/Cargo.toml --release --all-targets -- -D warnings`：通过。
- `RUSTFLAGS="-D warnings" cargo test --manifest-path node/Cargo.toml --release -- --test-threads=1`：667 个库测试与 57 个二进制测试通过，1 个测试忽略。
- `RUSTFLAGS="-D warnings" cargo test --manifest-path engine/Cargo.toml --release`：11 个测试通过。
- `cargo audit`：engine 无公告；node 发现 5 项漏洞公告、1 项 Tokio unsound 警告并以非零状态退出。

本轮没有执行持久化 cargo-fuzz、形式化验证、跨实现互操作或生产配置渗透测试。CI 中 `cargo-audit` 通过 `continue-on-error` 作为 advisory，因此上述 node 依赖公告不会使 CI 失败。现有常规测试全绿不能消除上述未覆盖的安全问题。
