# ZhixingGraph 参考节点（Rust · Milestone 6–24）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §5「PoK 共识」与 §7「技术架构」。

这是把 ΔK 引擎（[`engine/`](../engine/)）与经济仿真（[`sim/`](../sim/)）背后的规则，落成一个**可运行、确定性的 PoK 共识状态机**——真正"跑链"的最小内核：区块、交易、账户、状态转移、铸造/罚没、链上声誉、ed25519 签名交易、追加式持久化、确定性 mempool 出块、Merkle 认证状态与轻客户端证明、BFT 最终性证书与验证人集、驱动活性的 BFT 轮次状态机（超时 / 锁定 / 换轮）、逐高度生长的**BFT 认证链**（mempool → 共识 → 提交，每块附可验证证书）、**证书落盘 + 重放即最终性复验**（`blocks.log` + `certs.log`，重放时逐高度复验 > 2/3 证书，恢复的是*最终性*而非仅状态）、**链上/动态验证人集**（区块携带验证人增删/改权，由变更前的集合认证、下一高度生效，重放随之逐高度跟随演进）、**P2P gossip 与反熵状态同步**（交易 epidemic 泛洪 + 认证块拉取追赶，逐块对链上验证人集复验证书，含真实 loopback TCP 传输）、**质押绑定的验证人权重与解绑期**（账户自绑定 $COG → 成为验证人、权重 == 绑定量；解绑经时间锁提款队列，资金留在池中仍可罚没直至到期返还）、**按证据罚没等价双签**（把冲突预提交的密码学证据搬上链，罚没作恶验证人的绑定质押与解绑中金额入 treasury、下一高度移出验证人集，供应守恒）、**P2P 传播证据与质押变更**（`SlashEvidence`/`StakeOp` 经 gossip 进入每个节点的待打包池，下一区块由出块方带出——作恶可归责、质押可远程触发，不再仅靠出块人已持有），**验证人集变更的轻客户端跟随协议**（只凭创世信任根，逐高度对当前集合复验最终性证书、再复刻该块引起的验证人集迁移——不执行任何交易、不追踪账户余额，即得到与全量重放逐字节一致的活跃验证人集），**验证人集 Merkle 承诺入区块头**（把"下一高度生效的验证人集"的 Merkle 根 `next_validators_root` 折进区块头、纳入证书所签的 `block_hash`——轻客户端遂能**免复刻迁移**地凭一份证书验证整套下一验证人集`follow_committed`，或用 O(log n) 包含证明对 cert 签名的头**证明单个验证人**`verify_membership`，即 SPV 原语；M20 的 `follow` 也逐高度对该承诺根交叉校验）、**头部的轻同步传输**（`BlockHeader` 携每份体的 SHA-256 承诺，`Block::hash` 现哈希头投影使 `header.hash() == block.hash()` 恒成立；新 `CertifiedHeader` + `GossipMsg::GetHeaders/Headers` 把 SPV 原语搬上 gossip 总线；新 `LightGossipNode` 只保留头 + 一个 `ValidatorTracker`，从不解码任何交易体——M21 的 `follow_committed`/`verify_membership` 经 `follow_header` / `verify_membership_against_header` 迁移至只对头形式），**钱包的账户-成员 SPV**（`BlockHeader` 再携 `state_root` 完整共识状态 digest + `accounts_root` accounts/reviewers 二叉 Merkle 根两条承诺根，新 SPV `verify_account_membership_against_header` 在本地重算 leaf 并对头里的根验证——钱包证明自己的余额**只下头、不下体、零重放**；`Chain::commit` 把两条根走 trial 路径盖到 `block` 上，`apply_block_inner` 多两条强制度 `StateRootMismatch` / `AccountsRootMismatch`，与 M21 同形同序），**M24：批量化、类型化的统一 SPV 原语**——把 M23 单账户单 trick 的 `GetAccountProof/AccountProof` 传输**完全替换**为**一对**通用 `GossipMsg::GetProof { items }` / `Proof { items }`（wire tags 8/9 复用给新对），承载任意混合 `[(Account|Reviewer|Validator, id), ...]` 列表，**单次往返上限 `MAX_PROOF_BATCH = 32`**；新 `Reviewer::merkle_leaf()` + `ChainState::reviewer_proof(id)` 闭合审阅人包含证明的路径（审阅人叶一直就在 `accounts_root` 二叉树里，只是没有 typed producer）；新 typed `ProofEntry` 枚举（Account/Reviewer/Validator 三变体）携带 typed leaf + proof，wallet 端 M22/M23 的 `verify_membership_against_header` / `verify_account_membership_against_header` **全部删除**，只剩**一个** `ValidatorTracker::verify_proof_against_header(header, cert, tracked_set, entry)` 调度器，按 `entry.kind()` 选根——Account/Reviewer 对 `accounts_root`，Validator 对 `next_validators_root`，本地重算 leaf、零信任 prover；以及内容寻址的区块哈希链与状态根。

> **共识的前提是确定性**：给定相同的创世与相同的区块序列，每个诚实节点算出**逐字节相同**的状态（`state_root` 一致）。本 crate 就是那个状态转移函数 `apply_block`，其 ΔK 由 `zhixing_engine::compute_delta_k` 计算——与白皮书 B.2.3、Python 仿真是**同一份契约**。

## 运行

```bash
cd node
cargo run --release --bin node -- demo             # 内存演示链：创世 → 出块 → 打印
cargo run --release --bin node -- build            # mempool：乱序投递交易 → 规范排序出块
cargo run --release --bin node -- prove            # 轻客户端 Merkle 证明：单账户对状态根验证
cargo run --release --bin node -- bft              # BFT：4 验证人对区块出具可验证的最终性证书
cargo run --release --bin node -- live             # BFT 活性：轮次状态机驱动出块（含提议人宕机换轮）
cargo run --release --bin node -- chain            # BFT 认证链：mempool → 共识 → 提交，逐高度生长
cargo run --release --bin node -- validators       # 链上验证人集：逐高度增删验证人，重放随之跟随
cargo run --release --bin node -- gossip           # P2P：反熵同步（新节点追赶认证链）+ 交易 epidemic 泛洪 + 真实 TCP
cargo run --release --bin node -- light            # 轻客户端：只凭创世+(块,证书)对，逐高度跟随验证人集，不执行交易
cargo run --release --bin node -- vprove           # 验证人 Merkle 证明：对 cert 签名的区块头证明单个验证人在下一集合中；伪造叶被拒
cargo run --release --bin node -- staking          # 质押：绑定 $COG 获得验证人权重；解绑经时间锁提款到期返还
cargo run --release --bin node -- slashing         # 罚没：验证人双签 → 提交证据 → 绑定质押罚没入 treasury、移出验证人集
cargo run --release --bin node -- certs  --dir DIR # 证书落盘：产出认证链→落盘 blocks/certs→重放复验最终性
cargo run --release --bin node -- localnet         # M33：进程内 tokio 测试网（4 验证人，无定序器）经真实 socket BFT 收敛
cargo run --release --bin node -- run  --config F  # M33：联网 tokio 守护进程（TCP P2P gossip + 分布式 BFT 投票）
cargo run --release --bin node -- status --dir DIR # 重放区块日志并打印状态
cargo test --release                               # 273 项单元测试（见下）
```

演示链展示：新颖提交铸造 $COG、跨域桥接拿到 novelty+bonus（ΔK>1）、近重复/低质提交被**罚没入 treasury**、供应守恒、评审声誉按链上结果升降。

## 联网 tokio 守护进程与分布式 BFT 测试网（Milestone 33）

到 M32 为止 P2P transport 已搬到真实 socket，但**共识仍中心化**——一个 `[producer] enabled = true` 节点持**全部验证人 seeds**、在进程内 `Sim`/`ChainDriver` 里独力定稿证书块；其他节点只同步 + 逐块复验证书。`prevote`/`precommit` 投票从不出进程。

M33 把共识**真正分布式化**：每个验证人节点各持**一把** ed25519 `Keypair` + 一个 `round::RoundState` FSM，proposal / prevote / precommit 经同一条 TCP 总线 gossip 出去，round 推进由 wall-clock timeout 驱动——没有 `[producer]`、没有指定定序器。`ChainDriver`/`round::Sim` 保留给 `bft` / `live` / `chain` 离线 demo + 单元测试，不进守护进程。

进程内一键演示（4 验证人、零定序器、真实 loopback socket 收敛）：

```bash
cargo run --release --bin node -- localnet
# [node 21] listening on 127.0.0.1:19021 … role=validator
# [node 22] listening on 127.0.0.1:19022 … role=validator
# [node 23] listening on 127.0.0.1:19023 … role=validator
# [node 24] listening on 127.0.0.1:19024 … role=validator
# ✓ all 4 validators converged on the same cert-verified head via distributed voting (no sequencer)
```

真实多终端测试网（`testnet/` 已含样例：`genesis.toml` + `node21..24.toml`，**每个** nodeXX.toml 自带 `[validator] enabled=true, seed_hex=<其本地 seed>`——genesis pubkey 与本地公钥不匹配时启动直接 fail-fast）：

```bash
cargo run --release --bin node -- run --config testnet/node21.toml   # 验证人（终端 1）
cargo run --release --bin node -- run --config testnet/node22.toml   # 验证人（终端 2）
cargo run --release --bin node -- run --config testnet/node23.toml   # 验证人（终端 3）
cargo run --release --bin node -- run --config testnet/node24.toml   # 验证人（终端 4）
```

要点：`src/daemon.rs` 单属主 actor（`GossipNode` 独占一个 task、per-peer mpsc 出站、无 `Arc<Mutex>`），现在**也独占**一对 `Keypair`（follower 为 `None`）+ `Option<RoundState>` + tokio timer 句柄；帧 = `u32` BE 长度前缀 + `encode_gossip`，加 `MAX_FRAME = 16 MiB` 上限（阻塞版 `read_msg` 无上限）；8 字节 BE id 握手在 `GossipMsg` 之外（wire tag 0..=15 范围不动，新增 `TAG_CONSENSUS=16` 装 `GossipMsg::Consensus(Box<round::Msg>)`，proposal 字节 = 编码后 `Block`，接收端哈希与 proposer 端哈希逐字节相同）；只向 id 更大的 peer 拨号 → 每对恰一条连接；actor 是本节点日志唯一写者（任何命令后 `append node.blocks()[appended..]`），boot 经 `load_certified` 复验最终性恢复；纯 `GossipNode::on_message` 不收共识消息——它既不知道本节点的密钥，也没法触达 tokio 定时器；共识消息在 actor 主循环里被 `Cmd::Inbound` 直接路由到 `RoundState::on_message`/`on_timeout`。**同步永远赢**：验证人只对 `node.height()+1` 跑共识，`reconcile_after_sync()` 在任何高度推进之后立即弃旧 round + arm 下一高度——anti-entropy 永远优先于尚未决的 round。**Byzantine-proposer 活性保护**：`on_consensus` 在 prevote 之前用 `Chain::would_accept` 试跑 apply，把不能 apply 的 proposal 当成"proposer 缺席"处理（→ prevote nil → 下一 honest proposer）。**空块心跳**：每 `BLOCK_INTERVAL=1000ms` 由 `build_candidate` 出一空 sealed block 推进高度（**M35 起可经 `[consensus] create_empty_blocks=false` 关掉，见下**）。超时常量 `PROPOSE/PREVOTE/PRECOMMIT_TIMEOUT_BASE=1000ms` + `TIMEOUT_DELTA=500ms`（每 round 线性回退，落入最终同步性；**M35 起这些是 `[consensus]` 的可配默认值**）；4 等权验证人 quorum=3，所以 3-of-4 持续推进、2-of-4 安全停摆。配置在 `src/config.rs`，serde + toml **镜像结构**转换成引擎类型，共识核心 `lib.rs` 仍 serde-free。**依赖变化**：node 自 M32 起引入 `tokio`/`serde`/`toml`（引擎仍纯 std 零依赖）。

## 观测到双签即主动罚没（Milestone 34）

M33 让共识分布式化，补齐了 BFT 的**活性**；M34 补齐**问责**：一个 Byzantine 验证人**双签**（equivocate）——对同一 `(height, round)` 发两条 `block_hash` 不同的 precommit——从此会被诚实节点**当场观测并主动罚没**。

链侧的惩罚与传输早在 M18/M19 就已齐备：`Chain::apply_evidence`（`lib.rs`）复验 `SlashEvidence` 双签名、要求 offender 仍活跃、把其 bond（含 unbonding 条目）没收进 treasury 并于下一高度移除；`GossipMsg::Evidence`（tag 4）+ `submit_local_evidence` + `pending_evidence` 暂存 + flood/dedup + `build_candidate` 入块也早已端到端打通。**唯一缺的是投票到达时的检测**——此前投票摄入路径故意"先到先得"丢弃第二条冲突票，把检测推给了一个从未接线的 `detect_equivocation`。

M34 只补这一环，**不新增任何 wire type / codec / config**：`round::RoundState::ingest` 现返回 `Option<SlashEvidence>`——摄入一条 precommit 时，若已持有该 `(validator, height, round)` 的另一条 `block_hash` 不同的 precommit，就按 `block_hash` **规范排序**（`vote_a.block_hash <= vote_b.block_hash`，保证各检测者算出同一 `hash()` 以便 dedup）组装 `SlashEvidence`，经新 `Action::Equivocation(SlashEvidence)` 上抛。Actor 的 `apply_actions` 收到后调 `on_equivocation` → `submit_local_evidence`（本地暂存 + flood），证据随即传播、被下一 proposer 入块、链上没收并移除 offender。到这里的 precommit 都已**验签**且**来自活跃验证人**且**同高度**（`ingest` 前置过滤），故任何冲突都是真实、可归因的双签——无误报。first-wins 的 `or_insert` 保持不变（检测是纯旁路观测，不改 FSM 安全性）；prevote 双签在本模型不可罚没，保持先到先得。纯 follower（`kp=None`、无 `RoundState`）不做检测，但仍经 `on_evidence` 转发证据——验证人互相监督。

本里程碑只取原 M34 篮子里"主动罚没"这一薄片；其余运维成熟度项（`tracing` 结构化日志、metrics/health、`create_empty_blocks=false`、配置驱动超时、peer discovery、TLS/auth）**再次顺延至 M35+**。

## 配置驱动共识时序 + `create_empty_blocks`（Milestone 35）

到 M34 为止共识的运维旋钮仍是**编译期常量**：轮次超时（`PROPOSE/PREVOTE/PRECOMMIT_TIMEOUT_BASE`、`TIMEOUT_DELTA`）与出块节奏（`BLOCK_INTERVAL`）写死在 `daemon.rs`，且守护进程**每 `BLOCK_INTERVAL` 无条件出一个空块**——空闲链也会被心跳块无限拉长，运维也无法为快/慢网络调参。M35 取原运维篮子里的第一薄片：把共识时序**文件化可配**（沿用 `config.rs` 的 serde 镜像结构模式），并加 **`create_empty_blocks=false`**——验证人只在**有真实待办**（mempool 交易 / 待打包 `stake_ops` / 待打包罚没证据）时才出块。不引入任何新依赖。

新增 `[consensus]` TOML 段（全字段可选，`#[serde(default)]` 在段与字段两级都生效）：

```toml
[consensus]
propose_timeout_ms   = 1000   # 提议步基础超时
prevote_timeout_ms   = 1000   # prevote 步基础超时
precommit_timeout_ms = 1000   # precommit 步基础超时
timeout_delta_ms     = 500    # 每 round 线性回退增量
block_interval_ms    = 1000   # 出块/重试节奏
create_empty_blocks  = true   # false = 空闲不出块，有活才出
```

**后向兼容是硬性要求**：`ConsensusConfig::default()` 的取值**逐字段等于 M33/M34 的旧常量**，`create_empty_blocks` 默认 `true`。故缺 `[consensus]` 段的老 `testnet/*.toml` 与 `localnet` demo 行为**逐字节不变**；部分 `[consensus]` 段只覆盖写到的键、其余回落 `Default`。这五个时序数字现在**只有一处真源**（`ConsensusConfig::Default`），`daemon.rs` 里对应的五个常量已删除。

要点（`daemon.rs`）：新增一个 module-private 的 `Timing` copy 结构（六个值），在 `Node::start` 从 `cfg.consensus.*` 内联构造塞进 `Actor`（不加转换方法，保持 daemon 不依赖 serde 字段名，延续镜像→纯结构的约定）。`timeout_for(step, round)` 改为纯自由函数 `timeout_for(&Timing, step, round)`（可单测）。`create_empty_blocks` 的门只加在**被调度的起始路径** `on_start_tick`：空块关闭且 `!has_pending_work()` 时**不起轮/不提议**，仅按 `block_interval_ms` 重排下一 tick，链在当前高度**暂停**；而 `on_consensus` 的 **lazy-start 路径保持不设门**——peer 一旦提议就意味着有活，本节点即便还没收到 gossip 来的活也会跟上那一轮。

**活性论证**：非提议者从不需要独立探测有无活——它们在提议者的 proposal 上 lazy-start（不设门）；提议者只在有活时提议，而活由 `submit_local` 泛洪，故所有诚实节点在一个 gossip 延迟内收敛到同一待办集。最坏情形（round-0 提议者恰好缺一个 peer 才有的活）不过多花一次 round-change 超时，由确定性轮换换上持有该活的提议者接手——正是既有换轮所依赖的同一最终同步性。**安全性不动**（不新增任何投票逻辑，空块抑制只决定*是否起轮*）。`net.rs` 新增 `GossipNode::has_pending_work()`（mempool ∪ 待打包 stake_ops ∪ 待打包证据；桥无待打包池，故意不计入，已在注释说明）。

本里程碑交付原篮子里的**配置驱动超时 + `create_empty_blocks=false`**两片；其余（`tracing` 结构化日志、metrics/health、peer discovery、TLS/auth、可配 `ANNOUNCE_SECS`/`STARTUP_DELAY`）**再次顺延至 M36+**。

## 配置驱动网络时序（`ANNOUNCE_SECS` / `STARTUP_DELAY`）（Milestone 36）

M35 把**共识**投票时序文件化，但两个**网络/守护进程生命周期**旋钮仍是 `daemon.rs` 里的编译期常量：`ANNOUNCE_SECS=2`（反熵 `Status` 心跳间隔）与 `STARTUP_DELAY=1000ms`（验证人启动首个高度前的握手宽限）。这两项在 M35 篮子里被显式顺延；M36 收尾这一薄片：把它们搬进新的可选 `[network]` TOML 段，**逐字沿用 M35 `ConsensusConfig` 的配方**。运维遂可在大/静网上放慢心跳、或在慢拨号网络上加长启动宽限，无需重编译。不引入任何新依赖。

新增 `[network]` TOML 段（全字段可选，段与字段两级 `#[serde(default)]`）：

```toml
[network]
announce_interval_ms = 2000   # 反熵心跳间隔（旧 ANNOUNCE_SECS=2s → 2000ms）
startup_delay_ms     = 1000   # 验证人启动首高度前的握手宽限（旧 STARTUP_DELAY）
```

**后向兼容仍是硬性要求**：`NetworkConfig::default()` 逐字段等于旧常量，故缺 `[network]` 段的老 `testnet/*.toml` 与 `localnet` demo 行为**逐字节不变**（`localnet` 仍收敛到同一 head `44309755…ea04ba`）；部分 `[network]` 段只覆盖写到的键、其余回落 `Default`。**单位统一**：`ANNOUNCE_SECS` 原为秒，配置字段改为毫秒 `announce_interval_ms`（对齐 M35 的 `_ms` 约定），默认 `2000ms == 2s`、行为一致。这两个数字现在**只有一处真源**（`NetworkConfig::Default`），`daemon.rs` 两个常量已删除。

要点（`daemon.rs`）：`Node::start` 在 spawn 心跳/启动任务前把 `cfg.network.announce_interval_ms` / `cfg.network.startup_delay_ms` 各取入一个局部（因两处用于 `async move` 闭包），分别喂给 `tokio::time::interval(Duration::from_millis(..))` 与 `tokio::time::sleep(Duration::from_millis(..))`——无需塞进 `Actor` 或 `Timing`（它们只在 boot 期一次性使用）。

本里程碑只交付这一薄片；其余运维成熟度项（`tracing` 结构化日志、metrics/health endpoint、peer discovery / address gossip、TLS/auth）**再次顺延至 M37+**。

## 持久化与重放（Milestone 7）

节点状态不再只活在内存里：区块以**追加式日志**（`DIR/blocks.log`）落盘，重启后从创世**重放**日志即可重建**逐字节相同**的状态。

```bash
D=$(mktemp -d)
cargo run --release --bin node -- run    --dir "$D"   # state_root=1a2ec34c…9935b0
cargo run --release --bin node -- status --dir "$D"   # state_root=1a2ec34c…9935b0（从磁盘重建，一致）
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

`state_root` 之外，节点再对 accounts/reviewers 状态维护一棵**二叉 Merkle 树**（`merkle_root`）。它把"整块状态摘要"升级成**可逐叶打开**的认证结构：轻客户端只持有 `merkle_root`，拿到某个账户的内容 + 一条**包含证明**（`account_proof`（**M24** 新增 `reviewer_proof(id)` 闭合审阅人路径；`ChainState::merkle_leaves` 改调 `Reviewer::merkle_leaf()`））即可验证该账户真属于此状态——无需全量状态。

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

## BFT 轮次状态机与活性（Milestone 12）

M11 给了**安全性**（可验证的最终性证书），但没有任何东西**驱动**验证人去产生它。M12 补上**活性**：一个逐验证人、逐高度的 Tendermint 式状态机（`round.rs`），忠实转写 Buchman–Kwon–Milosevic (2018) 的 `upon` 规则——propose → prevote → precommit，配合 `lockedValue`/`validValue` 与跨轮锁定。

- **超时驱动换轮**：提议人宕机/沉默时，`propose` 超时 → 全体 prevote nil → precommit nil → `precommit` 超时 → 进入下一轮，由**确定性轮换**出的新提议人（`proposer_for_round`）接手，直到出块。超时被建模为**显式事件**（无时钟），整台机器因此完全确定、可复现、可离线测试。
- **锁定保安全**：precommit 时锁定某值，`upon` 规则（L28 的 proof-of-lock、L36 的锁定/解锁条件）确保**两轮永远无法最终化相互冲突的区块**——活性机制不破坏 M11 的安全性。
- **进程内网络模拟器**：`round::Sim` 用一条进程内消息总线把 N 台状态机接起来（P2P gossip 的占位，属后续里程碑），让机制**端到端可跑**：广播送达每个存活验证人，消息静默后按固定顺序触发超时。

本里程碑仍是**单高度共识**（就一个高度定稿一个区块）；把高度串起来的链循环与真实网络在其之上。

```bash
cargo run --release --bin node -- live   # 全诚实：round 0 出块；提议人宕机：超时换轮，round 1 仍定稿同一区块
```

## BFT 认证链驱动（Milestone 13）

到 M12 为止，各部件是分立的：mempool 造**一个**区块，轮次状态机对**一个**高度定稿。M13 把它们接成一条**生长的链**：驱动器（`driver.rs`）逐高度地——从池中造下一个区块 → 用 BFT 共识定稿 → 把定稿区块应用到 `Chain` 状态 → 保留该块的 `Commit` 证书。产出是一条**认证链**：每个已提交区块都由可验证的 > 2/3 最终性证明背书。

- **端到端流水线**：`ChainDriver::produce` 串起 `mempool::build_block`（确定性造块）、`round::Sim`（共识定稿）、`Chain::commit`（校验交易 + 状态转移 + 推进 head），并在信任前**复验证书**（真 >2/3 法定人数，且恰好认证将要提交的那个区块哈希）。
- **故障优先**：`produce(ts, silent)` 接受一组离线验证人。**低于 1/3** 权重宕机 → 链继续生长（活性）；**达到/超过 1/3** → 链**停摆**而非无证书出块（安全），驱动器返回错误且**保持链状态不变**。
- **确定性**：共识跑在进程内 `Sim` 总线上（P2P gossip 属后续），因此相同创世 / 验证人 / 交易的两台驱动器生长出**逐字节相同**的链（相同 head、相同 `state_root`、逐块相同的证书链）。

本里程碑仍用进程内消息总线代替真实网络；把证书随区块落盘、真实 gossip、动态验证人集属后续里程碑。

```bash
cargo run --release --bin node -- chain   # 逐高度生长认证链；#24 离线仍出块（活性）；#23+#24 离线则停摆（安全）
```

## 证书落盘与重放即最终性复验（Milestone 14）

到 M13 为止，认证链上的 `Commit` 证书只活在内存里——重启后节点靠重放 `blocks.log` 能重建**状态**（M7），但恢复不了**最终性**：它无从判断某个区块是否真被 > 2/3 权重最终确定过。M14 把证书也落盘，并让重放**复验最终性**。

- **证书编解码**：`codec::encode_commit`/`decode_commit` 给 `Commit` 一份与区块相同的规范二进制布局（大端、长度前缀），往返稳定。
- **`certs.log`**：`store::CertLog` 与 `BlockLog` 共用同一套「长度前缀记录 + 残缺尾检测」框架，逐高度追加证书，与 `blocks.log` 顺序一一对应。
- **重放即最终性复验**：`Chain::replay_verified(genesis, blocks, certs)` 在应用每个区块**之前**，要求其证书（a）恰好绑定该区块（height 与 block_hash 一致）且（b）是**该高度生效的验证人集**下真正的 > 2/3 法定人数（`Commit::verify`）。**掉一份、换一份、伪造一份证书都会在此被拒**——即便区块本身格式完好。对照：`Chain::replay`（M7）只恢复状态，分辨不出"已最终化"与"未最终化"的链；`replay_verified` 能。
- 验证人集自 **M16** 起是链上共识状态、随区块逐高度演进（见下），重放随之跟随；不再需要调用方传入。

```bash
D=$(mktemp -d)
cargo run --release --bin node -- certs --dir "$D"   # 首次：产出认证链并落盘 blocks.log + certs.log
cargo run --release --bin node -- certs --dir "$D"   # 再次：重载两份日志，逐高度复验 > 2/3 证书
# 末尾 tamper 演示：丢一份证书 -> 纯状态重放仍成功，最终性重放拒绝
```

## 钱包的账户-成员 SPV（Milestone 23）

M22 给钱包带来了头部传输，但钱包真正想要的"**我的余额是多少**"还差一步：M10 的 `merkle_root` 在 `ChainState` 里能产生账户包含证明，可那是全节点的重放成果——没有把"账户根"也带到证书签名头里来。M23 把两条**对钱包至关重要的承诺根**同时折进 `BlockHeader`，让钱包只凭头与对端的账户证明，就能在**不下载任何交易体、不重放任何状态转移**的前提下验证自己的余额：

- **`BlockHeader.state_root`** — `ChainState::state_root()` 的完整共识状态 digest（accounts/reviewers/graph/validators/bonds/bonded/unbonding/treasury/supply 一锅 SHA-256）。钱包不重算此根——证书签的是 `header.hash()`，而根就在 `header` 字段里，由签名它的 > 2/3 验证人集**替钱包**承担校验。
- **`BlockHeader.accounts_root`** — `ChainState::merkle_root()` 的 accounts ∪ reviewers 二叉 Merkle 根。钱包对端拿到账户 `(id, account)` 与 O(log n) 包含证明 `proof`，**自己在本地**算出 `leaf = leaf_hash(account.merkle_leaf(id))` 并验证 `merkle::verify(&header.accounts_root, &leaf, proof)`——根本不必相信对端给的 leaf。
- **`Block::hash` 现哈希新的头投影**，所以 `header.hash() == block.hash()` 仍然恒成立，证书绑定的 `block_hash` 与头哈希同源。

`Chain::commit` 走既有的"克隆 trial → apply → 拿根写回"路径，把 `state_root` / `accounts_root` 一并盖到 `block` 上；`apply_block_inner` 多两条强制度——`StateRootMismatch` / `AccountsRootMismatch`，与 M21 的 `ValidatorRootMismatch` 同形同序。

新 gossip 变体：`GossipMsg::GetAccountProof { id }` / `GossipMsg::AccountProof { id, account, proof }`（wire tag 8/9）。全节点从 `chain.state.account_proof(id)` 现取现发；光节点收到后入 `account_proofs` 缓存，由 `take_account_proof(id)` 取用。新 SPV API：`verify_account_membership_against_header(header, cert, tracked_set, id, account, proof)` 与 `verify_state_root_against_header`，是 `verify_membership_against_header` 的账户侧对偶。

```bash
cargo run --release --bin node -- account   # 光端凭 cert-signed header 证明自己的余额：true；改大 → false；改根 → false
```

## 批量化、类型化的统一 SPV 原语（Milestone 24）

M23 给钱包一条 `GetAccountProof` / `AccountProof` 单账户单 trick 的传输，但它是一次性"为了 M23 demo 而存在"的形状——`accounts_root` 二叉树里**审阅人叶一直就在**（M10 的 `merkle_leaves` 后半段），只是没有 typed producer；下一集合验证人又有 `ValidatorSet::proof` 可用，但走的是另一条 SPV 路径。三种 O(log n) 包含证明、三种不同传输故事——不该如此。M24 把它们压成**一对**通用 wire + **一个** SPV 验证器：

- **完全替换 M23 的 `GetAccountProof` / `AccountProof` 对**：新 `GossipMsg::GetProof { items: Vec<(ProofKind, u64)> }` / `GossipMsg::Proof { items: Vec<Option<ProofEntry>> }`（wire tags 8/9 复用给新对）。`items` 可任意混合 `[Account|Reviewer|Validator]` × id，单次往返上限 `MAX_PROOF_BATCH = 32`，超 cap 是 codec-level 错。全节点在 `GossipNode::on_message` 现取现发（`account_proof` / `reviewer_proof` / `ValidatorSet::proof`），未知 key 返 `None`。
- **新 typed `ProofEntry` 枚举**：三变体 `Account { id, account, proof }` / `Reviewer { id, reputation, proof }` / `Validator { id, validator, proof }`，每变体都带 typed leaf + proof。Wallet 端 M22/M23 的 `verify_membership_against_header` / `verify_account_membership_against_header` **全部删除**——wallet 端**唯一**的 SPV 验证器是 `ValidatorTracker::verify_proof_against_header(header, cert, tracked_set, entry)`：先验 cert 签的就是 `header.hash()`、再用 cert 复验签名的 > 2/3 法定人数、然后**本地重算** `leaf = merkle::leaf_hash(&entry.leaf())`、最后按 `entry.kind()` 选根——Account/Reviewer 对 `header.accounts_root`，Validator 对 `header.next_validators_root`，跑 `merkle::verify`。叶是 prover 给的，但 verifier 不信——它从 typed entry **自己重算** leaf。
- **闭合审阅人路径**：新 `Reviewer::merkle_leaf()`（`u64 id ‖ f32 reputation`，12 字节，`ChainState::merkle_leaves` 后半段调用它，与既有内联字节逐字节相同）+ 新 `ChainState::reviewer_proof(id) -> Option<merkle::Proof>`（审阅人 index = `n_accounts + rindex`，与 `merkle_leaves` 布局一致）。
- **光端缓存**：`LightGossipNode::take_proof(kind, id) -> Option<ProofEntry>`，键是 `(ProofKind, u64)`，复用一个 `BTreeMap` 缓存三类证明。
- **判别码**：`ProofKind` 1 字节 tag（`0=Account`/`1=Reviewer`/`2=Validator`），`ProofEntry` 用固定 leaf 字节长度（Account=88、Reviewer=12、Validator=48）从 `Vec<u8>` 中切出 leaf 与 proof，编码稳定无歧义。
- **`cmd_account` 演示三证明批量**：单次 `GetProof { items: [(Account,1), (Reviewer,1), (Validator,25)] }` 拿回三 typed entries，对**同一 cert-signed header** 调 `verify_proof_against_header` 三遍——账户余额、审阅人声誉、验证人集合成员全部 ✓；再把 validator 的 power 改 1 → `MembershipProofInvalid`（prover 不能谎报 leaf）。

```bash
cargo run --release --bin node -- account   # 三证明批量化：账户余额 + 审阅人声誉 + 验证人集合成员，单 GetProof 往返、本地重算 leaf、对 cert-signed 头验证
```

## P2P 网络与反熵状态同步（Milestone 15）

到 M14 为止，节点的各部件都跑在**同一进程**里：`round::Sim` 用进程内总线把验证人接起来定稿一个区块，驱动器独自把链生长起来。那条总线始终只是**P2P 层的占位**。M15 补上真正的网络层（`net.rs`）：它在**不同节点之间**传播两样真正跨网的东西——**待处理交易**（共识前）与**认证块**（区块 + 其最终性证书，共识后），并让一个新节点或落后节点从对等方**追赶**到认证链头。高度内的投票 gossip 仍留在 `round`（那是验证人内部的事）；跨网传播的是已最终化、可自证的结果。

- **两条都"零信任"**：
  - **反熵同步**——节点用 `Status` 广播自己的高度；落后的一方拉取缺失的认证块（`GetBlocks → Blocks`），且每块**仅当**其证书是「该高度生效验证人集」下真正的 > 2/3 法定人数、并恰好绑定该块时才应用（与 `Chain::replay_verified` 同一道校验）。**伪造或掉包的证书会让同步停在缺口处**，而非污染状态。
  - **交易 epidemic gossip**——新颖交易准入 mempool 后转发给对等方；一个内容哈希 `seen` 集合让重复投递变成 no-op，于是泛洪一次即终止。
- **确定性内核 + 真实传输分层**：`Network` 是固定顺序、进程内的投递总线（gossip 版的 `round::Sim`），让测试断言 N 个节点**收敛**到逐字节相同的 head/`state_root`；`GossipNode::on_message` 是不做任何 I/O 的**纯状态机**，返回"要发给谁"的消息，因此在进程内总线和真实 socket 上跑得一模一样。socket 传输（`read_msg`/`write_msg`）只是同一套 wire 消息之上薄薄的「`u32` 长度前缀 + 1 字节 tag + 载荷」分帧——正确性活在确定性协议里，不在线缆上。

```bash
cargo run --release --bin node -- gossip   # 新节点反熵追赶认证链 → 收敛；交易注入一处泛洪到全网；三从节点经真实 TCP 向种子拉链
```

## 链上/动态验证人集（Milestone 16）

到 M14 为止，验证人集是**网络常量**：由调用方传入、永不改变，`replay_verified` 拿同一份集合复验每个高度。真实链上验证人会加入、退出、改变权重——M16 让验证人集成为**链上共识状态**，可通过区块携带的变更逐高度演进。

- **验证人集入状态根**：`ChainState.validators` 是创世锚定的共识状态，并**折入 `state_root`**（每个验证人的 id/pubkey/power）。因此换一套验证人集就得到不同的状态根——用错误的创世验证人集重放会被拒（`replay_under_a_different_genesis_validator_set_is_rejected`）。
- **跨高度切换规则**：区块通过 `Block.validator_updates` 携带增删/改权（`power==0` 删除，否则 upsert）。关键不变量——**携带变更的区块由变更前的集合认证**，变更**下一高度生效**：新加入者绝不为自己的加入投票。`active_set(1)` = 创世验证人；`active_set(H+1) = apply(active_set(H), updates_in_block_H)`。
- **驱动与重放对称跟随**：`ChainDriver::stage_validator_update` 把变更挂到下一个产出的区块（池空也会合成一个纯变更块）；共识用**该高度生效的集合**投票。`replay_verified` 对称地在提交每个区块**之前**、用 `chain.state.validators`（即将认证下一高度的集合）复验其证书，然后应用区块并演进集合——重放逐字节复现实时链的每一次交接。
- **一份编码**：`codec::encode_block` 在交易之后追加 `u64 count` + 每条变更（`u64 id`、原始 `pubkey[32]`、`u64 power`），往返稳定，纳入区块哈希。

```bash
cargo run --release --bin node -- validators   # 4 验证人起步 → 加入 #25（旧集合认证）→ 删除 #21 → 重放逐高度跟随交接
```

## 质押绑定的验证人权重与解绑期（Milestone 17）

到 M16 为止，验证人的**权重是链上参数**：可增删/改权，但那只是被写入的数字，与任何经济抵押无关。真实 PoS 里权重必须**由质押背书**——想要更大投票权，就得锁定更多本金作为可罚没的担保。M17 把二者绑定：账户**自绑定（self-bond）** $COG，其**账户 id 即成为验证人 id**，验证人权重 == 绑定的 micro-$COG（恒等映射，精确无舍入）。

- **绑定即赋权**：`StakeOp{account, Bond, amount}`（作者签名）把 `amount` 从账户余额移入**绑定池**（`bonded`），并按 M16 的纪律派生一条验证人变更——**由变更前的集合认证、下一高度生效**，新绑定者绝不为自己的加入投票。权重严格等于该账户的当前绑定量（`bonds[id]`）。
- **解绑经时间锁**：`Unbond` 立即撤下权重（下一高度移出验证人集），但资金**不立刻退还**——进入解绑队列 `{account, amount, mature_height = H + UNBONDING_PERIOD}`（`UNBONDING_PERIOD = 3`）。在到期前资金**仍留在系统内、仍可被罚没**（这正是 M18 按证据罚没的安全前提），到期高度应用时才返还账户余额。
- **供应守恒扩展**：不变量升级为 `Σ余额 + treasury + bonded + Σ解绑中金额 == supply`，全程有测试守护（绑定/解绑/到期返还的完整生命周期）。
- **折入状态根、块级携带**：`bonded`/`bonds`/`unbonding` 三者均折入 `state_root`（故绑定改变状态根，`merkle_root` 不含绑定、保持不变）；bond/unbond 作为**块级 `stake_ops`**（类比 `validator_updates`）由区块携带、经 BFT 认证链定稿，本里程碑暂不走 mempool/gossip。**创世验证人仍是自举集**（不占绑定池），保持模型干净。

```bash
cargo run --release --bin node -- staking   # #1 绑定 6 $COG → 权重激活（旧集合认证）→ 解绑 → 时间锁 → 到期返还；供应全程守恒；重放复验最终性
```

## 按证据罚没等价双签（Milestone 18）

到 M17 为止，验证人的权重**由可罚没的绑定质押背书**，解绑期也刻意让作恶者的本金在退出后仍能被追缴——但真正把"作恶"变成"损失"的那一步还缺：链能**检测**双签（M11 的 `detect_equivocation`），却还不能据此**罚没**。M18 补上问责闭环：把等价（equivocation）的密码学证据搬上链，罚没作恶验证人的绑定质押入 treasury——让 BFT 安全性从"可检测"变成"经济上不划算"。

- **证据即两票冲突预提交**：`SlashEvidence{vote_a, vote_b}` 是同一验证人在同一 (height, round) 对**两个不同 block_hash** 的预提交，各带该验证人密钥的有效 ed25519 签名——合起来就是不可伪造的双签铁证（`is_well_formed` 校验结构，签名对**当前验证人集**里该验证人的公钥复验）。这正是真实网络里 `consensus::detect_equivocation` 从两份冲突证书中提取的东西。
- **罚没入库、供应守恒**：`apply_evidence` 先全量校验（结构合法 + 作恶者是活跃验证人 + 两票签名有效），再把作恶者的绑定池金额（`bonds[id]`）与**任何仍在解绑队列中的金额**一并转入 treasury（解绑中的钱到期前仍可罚没，这正是 M17 时间锁的安全前提）。钱在系统内平移，`Σ余额 + treasury + bonded + Σ解绑中 == supply` 恒成立。
- **下一高度移出验证人集**：罚没后作恶者被派生成一条 `power==0` 的验证人变更（复用 M17 的 `touched` 集合与派生更新路径）——**由变更前的集合认证、下一高度生效**，与质押/验证人变更同一套跨高度纪律；`EmptyValidatorSet` 守卫拒绝把整个集合罚空的区块。
- **块级携带、只折效果入根**：证据作为**块级 `slashing_evidence`**（类比 `stake_ops`）由区块携带、经 BFT 认证链定稿、纳入区块哈希——但**不进 `state_root`**：进根的只是它的*效果*（减少的 `bonds`/`bonded`、增长的 `treasury`）。故给区块新增空的证据字段不改变 `state_root`，只改变区块 `head`。校验全程先于任何状态改动，坏证据整块回滚。

```bash
cargo run --release --bin node -- slashing   # #1 绑定 6 $COG 成为验证人 → 双签 → 提交证据 → 绑定质押罚没入 treasury、移出验证人集；供应守恒；重放复验最终性
```

## P2P 传播证据与质押变更（Milestone 19）

M15 给交易/认证块上了 gossip；M16/M17/M18 把验证人变更 / 质押变更 / 罚没证据**搬上链**，但它们都依赖"出块方已经持有它们"——只有观察到了双签的节点本地有证据，但出块方未必有，作恶者就能指望出块方"碰巧没拿到证据"而保住质押。M19 把这层"任何节点都能看到"补到"任何节点都能把证据送进出块方的待打包池"：扩展 `GossipMsg` 加两个新变体（`Evidence(SlashEvidence)`、`StakeOp(StakeOp)`），给 `GossipNode` 加 `pending_evidence` / `pending_stake_ops` 两个**待打包池**，出块前 `take_pending_*` 灌进 `ChainDriver` 的 `pending_slashing_evidence` / `pending_stake_ops`，`produce` 出的下一个区块自然就把它们带出去。

- **形状即足够、密码学留给 apply**：`on_evidence` 只做 `is_well_formed()` + `seen_evidence` 去重；签名/活跃验证人/是否陈旧等真正的密码学校验仍然只在 `chain.commit.apply_evidence` / `apply_stake_op` 时发生——同 M18 的纪律。坏证据/坏签名的 StakeOp 不会让对端崩溃，只会在被某出块方带出时让那一块整块回滚，节点丢弃本地待打包池条目即可恢复。
- **内容哈希去重、泛洪一次即终止**：`seen_evidence` / `seen_stake_op` 用 `SlashEvidence::hash` / `StakeOp::hash` 做内容寻址，重复投递 no-op；`Network` 的固定顺序 FIFO 总线保证 N 节点**收敛**到相同的待打包池。`encode_gossip` / `decode_gossip` 各加一个 1 字节 tag（`TAG_EVIDENCE=4` / `TAG_STAKEOP=5`）+ 长度前缀，复用既有的 `encode_evidence` / `encode_stakeop`，所以 gossip determinism 与既有的 `gossip_is_deterministic` 同款。
- **作恶可归责、质押可远程触发**：以前"区块级 ops"只有出块方能装——出块方没看到证据，链就永远没证据，罚没就空转。现在任意节点拿到证据后都能 flood，下一区块的出块方**自动**把它带出去。这就是把"作恶可被任何人检举、检举必定落地"的口径，从"对出块方而言"收紧到"对网络而言"。

```bash
cargo run --release --bin node -- gossip     # 注入一条双签证据 → 4 节点的 pending_evidence 池全部装好
```

## 验证人集变更的轻客户端跟随协议（Milestone 20）

到 M19 为止，要知道"某高度的活跃验证人集是谁"，只能**全量重放**每个区块——因为验证人集是折进 `state_root` 的链上状态，只有把每块的状态转移（含交易、铸造、账户）都跑一遍才导得出来。可钱包/轻客户端真正想要的往往只是"哪些密钥能定稿高度 H、各有多少权重"，不该为此跑整台状态机。M20 加上 `ValidatorTracker`：**只凭创世这一信任根，逐高度跟随活跃验证人集，不执行任何交易、不追踪账户余额**。对每个高度它只做两件事——(1) 用**当前跟随到的集合**复验该高度的最终性证书（> 2/3 权重、真实 ed25519 签名），(2) 复刻该块引起的**验证人集迁移**（`validator_updates` + 由 `stake_ops` / `slashing_evidence` 派生的权重变化），别的一概不碰。

- **可证明而非可信**：轻客户端消费的 `validator_updates` / `stake_ops` / `slashing_evidence` 全都在 `block_hash` 之内，而证书签的正是 `block_hash`——所以派生出的集合**就是**链上的集合：作恶者若不攻破 > 2/3 的签名，就无法把轻客户端引到一个假集合。`follow` 的结果与 `Chain::replay_verified` 在每个高度**逐字节一致**，却完全不碰交易/账户/图谱/铸造。
- **严格镜像既完整又可靠**：`apply_block` 派生下一集合时只读两处链上状态——`bonds` 映射与账户公钥。`bonds` 只被 bond/unbond 与罚没（移除作恶者的绑定）改动，创世 bonds 为空、解绑到期又不动 bonds/集合，所以它完全由认证块里的 `stake_ops` + 证据决定；账户公钥创世后不可变（账户仅在创世创建），故一次性从 `Genesis.accounts` 播种即可，永不缺失——而罚没作恶者派生出的是 `power==0`（移除），`apply_updates` 此时根本不看公钥。
- **复用既有传输**：`ValidatorTracker` 是 `(Block, Commit)` 对的纯消费者——正是全节点已经经 `GossipMsg::Blocks` gossip 出去的那些对。轻客户端订阅同一条同步流、丢掉用不到的交易体即可，无需任何线格式改动。

```bash
cargo run --release --bin node -- light   # 造一条三种方式改验证人集的认证链，轻客户端 0 执行交易跟到同一集合
```

## Merkle 化验证人集承诺入区块头（Milestone 21）

到 M20 为止，轻客户端 `ValidatorTracker` 虽不执行交易，却仍要**逐高度复刻验证人集迁移**：镜像 `bonds` 映射、账户公钥表，重放 `validator_updates` 与由 `stake_ops`/`slashing_evidence` 派生的权重变化——一份 `apply_block` 的字节级复制。那是比钱包所需更多的机械与信任面。M21 把一份**对"下一高度生效的验证人集"的 Merkle 承诺折进区块头**（`Block.next_validators_root`）。因为该字段落在 `block_hash` 之内、而证书签的正是 `block_hash`，轻客户端遂能：

- **免复刻迁移地验证整套下一验证人集**（`follow_committed`：验完证书后，只需比对给定集合的 `merkle_root == block.next_validators_root`），以及
- **证明单个验证人** `(id, pubkey, power)` 属于认证高度 `H+1` 的集合——凭一条锚定到 cert 签名头的 O(log n) 包含证明（`verify_membership`，即 SPV 原语）。

同时收紧 M20：`follow` 现在**每高度把自己迁移导出的集合对承诺根交叉校验**，令推导被共识确认、而非仅被信任。

- **承诺的是 post-apply 的"下一"集合**（认证 `H+1` 的那套），与既有规则一致——高度 `H` 由 `H` 应用**之前**在任的集合认证，变更下一高度生效。无循环依赖：验证人集迁移从不读 `next_validators_root`，故导出的集合与该字段取值无关。出块方在共识前 `seal`（`Chain::seal` 在 trial 克隆上跑一遍不带强制的迁移、取根写回），`apply_block` 提交时强制 `block.next_validators_root == self.validators.merkle_root()`，否则 `ChainError::ValidatorRootMismatch`。
- **一份叶契约**：`Validator::merkle_leaf` 的字节（`u64 id ‖ raw pubkey ‖ u64 power`）与 `state_root` 里每个验证人的三元组**逐字节相同**，故两处承诺同步变化，验证方只需它被告知的那个验证人。`ValidatorSet::merkle_root`/`proof` 复用 M10 的域分隔二叉树（叶 `0x00`、节点 `0x01`、奇数提升），空集合承诺到全零根。
- **一份编码**：`codec::encode_block` 在 `timestamp_days` 之后、交易之前写这 32 字节根，纳入区块哈希。这是一次性的固定布局变更（无版本化先例），旧 `blocks.log` 不再解码——对原型可接受。

```bash
cargo run --release --bin node -- light    # M20 迁移式 follow + M21 免迁移 follow_committed 达到同一集合
cargo run --release --bin node -- vprove   # 对 cert 签名头证明验证人 #25 在下一集合中 → true；伪造 (power+1) 叶 → false
```

## 头部的轻同步传输（Milestone 22）

M21 的 SPV 原语 (`follow_committed` / `verify_membership`) 仍以**完整 `(Block, Commit)` 对**为输入——那是 P2P gossip 实际在传的东西，里面装着每笔交易、每个质押 op、每条罚没证据。一个真钱包要的是"活跃验证人集"与"成员证明"，不该为此下载区块体。M22 把这两件事搬到一头：

- **`BlockHeader`**：`Block` 的证书签名投影——除 `txs` / `stake_ops` / `slashing_evidence` 之外的全部字段，外加每份体的 SHA-256 承诺（`txs_commitment` / `stake_ops_commitment` / `evidence_commitment`）。`BlockHeader::hash` 即对 header 字节的 SHA-256；`Block::hash` 现在改为哈希同一份 header 投影（带体的承诺），使**任何区块**都满足 `header.hash() == block.hash()`——证书签的 `block_hash` 即可与头哈希同源。
- **`CertifiedHeader`** = `(BlockHeader, Commit)`：比 `(Block, Commit)` 小一整个体字节（头字段以外的部分），是轻同步的单位。
- **新 gossip 变体**：`GossipMsg::GetHeaders { from }` / `GossipMsg::Headers(Vec<CertifiedHeader>)`。全节点保留这两个方法但选择不消费 `Headers`（它们只服务），新类 **`LightGossipNode`**（伴随 `GossipNode`）只保留头——运行 `ValidatorTracker`，从**不反序列化**一笔交易。
- **`LightNetwork`**：把全节点与光节点混入同一条确定性总线；头批次经一条 `next_set_for` 边带由总线喂给光节点的 `apply_header(ch, &next_set)`（光节点的 SPV 协议需要头 + 该集合的承诺，由出块方在体外提供）。`ValidatorTracker` 增 `follow_header` 与 `verify_membership_against_header` 两条 API：`follow_header` 走"对承诺根的过渡免迁移"路径（与 `follow_committed` 同结构，输入是头），`verify_membership_against_header` 是 `verify_membership` 的"只对头"对偶。
- **线缆**：`encode_certified_header` / `decode_certified_header` 是固定布局（prefix + 三承诺 + cert），`encode_gossip`/`decode_gossip` 增 `TAG_GETHEADERS=6` / `TAG_HEADERS=7`。`HeaderCodec::decode_certified_header` 在固定偏移上手工分（无 trailing bytes）以避免 cert 段被头解码拒绝。
- **测试**：`header_round_trip`、`block_hash_equals_header_hash`（关键不变式）、`certified_header_round_trip`、`header_decode_rejects_trailing_bytes`、`full_node_serves_headers_in_response_to_get_headers`、`light_node_pulls_headers_and_tracks_validator_set`、`light_node_rejects_a_wrong_next_set_for_a_header`、`header_gossip_shrinks_the_wire_payload_vs_full_blocks`、`follow_header_advances_the_tracker_with_no_block_bodies`，`wire_round_trips_every_message` 覆盖所有 8 类消息（含新两类）。

```bash
cargo run --release --bin node -- lsync    # 一台全节点 (id=1) + 一台光节点 (id=2) 共总线：光节点起步 0、只发 Status -> 只拉头 + next_set -> 跟上至 height N（0 笔交易入眼），附线缆字节节省与 verify_membership
cargo run --release --bin node -- account  # 钱包 SPV 三证明批量化：账户余额 + 审阅人声誉 + 验证人集合成员，单次 GetProof 往返、本地重算 leaf、对 cert-signed 头验证
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
| **BFT 活性** | Tendermint 轮次状态机：propose/prevote/precommit + 超时 + 锁定 + 换轮；确定性提议人轮换，提议人宕机也能出块；进程内模拟器端到端验证（M12） |
| **认证链** | 驱动器逐高度串起 mempool→共识→提交，每块附复验过的 > 2/3 证书；低于 1/3 宕机仍生长，达 1/3 则安全停摆；两台驱动器逐字节一致（M13） |
| **最终性持久化** | `Commit` 证书与区块同格式落盘（`certs.log`）；`replay_verified` 逐高度复验证书绑定+法定人数，恢复最终性而非仅状态；丢/换/伪造证书均被拒（M14） |
| **动态验证人集** | 验证人集是折入 `state_root` 的链上状态；区块携带增删/改权，由变更前的集合认证、下一高度生效；驱动与重放对称跟随交接，用错误创世集合重放被拒（M16） |
| **P2P 网络** | gossip 传播交易（epidemic 泛洪 + 内容哈希去重）与认证块（反熵拉取追赶）；每块对链上验证人集复验 > 2/3 证书才应用，伪造/掉包证书停在缺口；确定性 `Network` 保证收敛，纯状态机同时跑进程内与真实 TCP（M15） |
| **质押绑定权重** | 账户自绑定 $COG → 验证人权重 == 绑定量（恒等映射）；解绑经 `UNBONDING_PERIOD` 时间锁提款队列，资金留池仍可罚没直至到期返还；`bonded`/`bonds`/`unbonding` 折入 `state_root`，块级 `stake_ops` 经 BFT 认证；供应守恒含绑定与解绑中金额（M17） |
| **等价双签罚没** | `SlashEvidence` = 同验证人同 (h,r) 对两个不同 block_hash 的预提交（各带有效签名）；`apply_evidence` 校验后把绑定池 + 解绑中金额罚没入 treasury（供应守恒），下一高度经 `power==0` 派生更新移出验证人集；块级 `slashing_evidence` 纳入区块哈希但只折**效果**入 `state_root`；坏证据整块回滚（M18） |
| **块级 ops 走 gossip** | `GossipMsg` 加 `Evidence(SlashEvidence)` / `StakeOp(StakeOp)`，内容哈希去重（`seen_evidence` / `seen_stake_op`）+ 待打包池 `pending_evidence` / `pending_stake_ops`；出块方 `take_pending_*` 灌进 `ChainDriver.pending_*`，下一区块自动带出。密码学校验仍只在 `apply_evidence` / `apply_stake_op`；形状即足够（M19） |
| **轻客户端跟随验证人集** | `ValidatorTracker::from_genesis` 只信创世；`follow` 每高度用当前集合复验证书、再复刻 `apply_block` 的集合迁移（`validator_updates` + 由 `stake_ops`/`slashing_evidence` 派生的权重），镜像 `bonds` 映射 + 从 `Genesis.accounts` 播种的不可变公钥表；因输入全在 `block_hash`（证书所签）内，结果与 `replay_verified` 逐字节一致却 0 执行交易；复用 `GossipMsg::Blocks` 传输（M20） |
| **验证人集 Merkle 承诺入头** | `Block.next_validators_root` = 对 post-apply 下一集合的 Merkle 根（`Validator::merkle_leaf` 与 `state_root` 三元组同字节），落在证书所签的 `block_hash` 内；出块方 `Chain::seal`（trial 克隆导出根）、`apply_block` 提交强制根匹配否则 `ValidatorRootMismatch`；轻客户端 `follow_committed` 免复刻迁移地比对整套集合根，`verify_membership` 用 O(log n) 包含证明对 cert 签名头证明单个验证人（SPV 原语），`follow` 亦逐高度交叉校验；迁移从不读该字段故无循环（M21） |
| **钱包 SPV 账户证明（双根承诺）** | `BlockHeader` 再携 `state_root`（`ChainState::state_root()` 完整共识状态 digest，证书签 = 钱包信任根）与 `accounts_root`（accounts ∪ reviewers 二叉 Merkle 根，供 O(log n) 包含证明）；`Chain::commit` 走 trial 路径把两根盖到 `block` 上，`apply_block_inner` 多两条强制度 `StateRootMismatch` / `AccountsRootMismatch`；新 SPV `verify_account_membership_against_header` 在本地重算 `leaf = leaf_hash(account.merkle_leaf(id))` 并对 `header.accounts_root` 验证——钱包证明自己余额只下头、不下体、零重放（M23） |
| **M24 批量化、类型化 SPV 原语** | `GossipMsg::GetProof { items }` / `Proof { items }` 一对承载任意混合 `[(Account|Reviewer|Validator, id), ...]` 列表（`MAX_PROOF_BATCH = 32`），wire tags 8/9 完全替换 M23 的 `GetAccountProof/AccountProof`；新 `ProofEntry` typed 枚举 + `Reviewer::merkle_leaf()` + `ChainState::reviewer_proof(id)` 闭合审阅人路径；wallet 端**唯一** SPV 验证器 `ValidatorTracker::verify_proof_against_header(header, cert, tracked_set, entry)` 按 `entry.kind()` 选根（Account/Reviewer → `accounts_root`，Validator → `next_validators_root`），本地重算 leaf、零信任 prover；M22/M23 的 kind-specific 验证器全部删除 |
| **图节点 cert-signed 包含证明（M25）** | `engine::GraphNode` 加单调 `node_id` 字段；`ChainState::merkle_leaves` 增第三段承载 graph 节点（插入序）；`BlockHeader.accounts_root` 自动扩展覆盖 accounts ∪ reviewers ∪ graph 三集合的同一 Merkle 根；新 `ProofKind::GraphNode = 3` + `ProofEntry::GraphNode { node_id, graph_node, proof }` 走 M24 同一 `GetProof`/`Proof` 总线；`verify_proof_against_header` 加 `GraphNode → accounts_root` 一支；`ChainState::graph_node_proof(idx)` 闭合图节点的 O(log n) 证明路径；wallet 端仍是**唯一**的验证器+零信任 prover |
| **图节点 cert-signed 邻域证明（M26）** | `engine::CognitiveGraph::k_nearest_with_ties(query, k)` 按 cosine 降序 + `node_id` 升序稳定排序，边界 ties 全留（`len ≥ k`）；新 `KnnClaim { query, k, neighbours: Vec<(node_id, GraphNode, merkle::Proof)> }` + `ValidatorTracker::verify_knn_against_header` 把**单一** cert-signed header 拆成 (1) header/cert 绑定、(2) 每个 neighbour leaf 对 `accounts_root` 的 Merkle 验证、(3) 本地用 `cos_sim` 重排+同 cut、(4) 与 prover 序列等比——prover 不可省略 tied 邻居也不可重排 |
| **图节点 cert-signed 范围查询（M27）** | `BlockHeader` 新增 32-byte `graph_root` 槽位，对同一 cert-signed header 提交；`ChainState::graph_merkle_root()` 按 `(cos_sim(CANONICAL_PIVOT, n.embedding) desc, node_id asc)` 排序索引（`CANONICAL_PIVOT = [1,0,0,0,0,0,0,0]`，确定且 query-无关）——区别于 M25 的 `accounts_root` 插入序；新 `ChainState::graph_range_proof(a, b)` 返回 `RangeProof { sub_root, entries }`；新 `RangeClaim { query, min_sim, nodes }` + `GossipNode::serve_range(query, min_sim)` 把 cosine-cutoff `(query, min_sim)` 派给全节点；wallet 端 `ValidatorTracker::verify_range_against_header` 把单一 cert-signed header 拆成 (1) header/cert 绑定、(2) `min_sim ∈ [-1, 1]` cutoff 验证、(3) 每个 node leaf 对 **`graph_root`**（**非** `accounts_root`）的 Merkle 验证、(4) 本地用 `cos_sim` 重排+`sim >= min_sim` 的 prefix cut、(5) 与 prover 序列等比——prover 不可重排；同一 kNN 同形信任模型 |
| **图节点 cert-signed 时序 diff（M28）** | 在 M25 单点 + M26 邻域 + M27 同高度范围之外，闭合**时序轴**——钱包问"h₁ 到 h₂ 之间图节点 added/dropped 是哪些"无需下载两套图。新 `GraphLeafAtHeight { node_id, graph_node, proof }` + `DiffClaim { added, dropped }`（`changed` 臂在 M25 append-only 下**结构上不可达**，删除）；新增 `ChainState::graph_diff(&prev_state)` 在 producer 侧构造 `{added, dropped}`，每个 leaf 的 `proof` 对**各自高度**的 `accounts_root`；新增 `DiffEnvelope { header_prev, cert_prev, header_new, cert_new, diff, tracked_set_h1, tracked_set_h2 }` + `GossipNode::serve_diff(h1, h2, …)`（缓存 genesis 在 GossipNode 上可重放 h₁ 状态）；wallet 端 `ValidatorTracker::verify_diff_against_headers` 拆为 (1) 两边 header/cert 绑定 + 两边 tracked set 各自验签（动态验证人集下两个高度用不同集合）、(2) **wallet 端局部重放** `[1..=h₂]` 取 `state_at_h2` + `[1..=h₁]` 取 `state_at_h1`（与 prover 用的同一重放路径），(3) 每个 `added` leaf 对 `header_h2.accounts_root`、每个 `dropped` leaf 对 `header_h1.accounts_root` 的 Merkle 验证，(4) prover 与重放派生的 `added`/`dropped` 集合等比——**完整性由重放保障，单 leaf 证明只验身体**。M28 走 M24 同一 `GetProof`/`Proof` 总线的姊妹对 **TAG_GETDIFF=10 / TAG_DIFF=11**；`Diff` 大体走 `Box` 装箱避免 `large_enum_variant`。**不用 `graph_root`**：cosine 排序不保 `node_id` 序，无法界定 diff 大小——M28 一律走 accounts_root（插入序） |
| **异构批 SPV 传输（M29）** | 在 M24 单类 `GetProof` 批量 inclusion 之上闭合**跨原语** 一次性取齐——钱包想同时证明"账户余额" + "kNN 邻域" + "cosine 范围" + "时序 diff"不再需要 4 个往返。新 `BatchItem { Inclusion{Kind,id} \| Knn{query,k} \| Range{query,min_sim} \| Diff{h1,h2} }` + `BatchResponseItem { Inclusion(Option<ProofEntry>) \| Knn(Option<KnnClaim>) \| Range(Option<RangeClaim>) \| Diff(Box<DiffEnvelope>) }` + `BatchResponseEnvelope { items: Vec<...> }`（上限 `MAX_BATCH_ITEMS = 32` 复用 M24 容量）；新增 `GossipNode::serve_batch(items)` 复用 M24 `serve_inclusion` + M26 `serve_knn` + M27 `serve_range` + M28 `serve_diff`——**无新 SPV 逻辑**，仅一层 dispatch；`GossipMsg` 增 **TAG_GETBATCH=12 / TAG_BATCH=13** 一对；wallet 端 `ValidatorTracker::verify_batch(genesis, header, cert, tracked_set, blocks_in_range, items, response)` 把每个 slot 派回 `verify_proof_against_header` / `verify_knn_against_header` / `verify_range_against_header` / `verify_diff_against_headers` 现有四个验证器；新 `LightError::{BatchTooManyItems, BatchItemCountMismatch, BatchItemKindMismatch}` 三类**仅协议违规**错误（per-primitive 错误通过 dispatch 转发）。`Diff { envelope }` 同 `Batch { envelope }` 因体积大走 `Box` 装箱 |
| **信任无关跨链桥（M30）** | 同协议两条链 A↔B（不同创世 → 不同 `genesis_hash`，对称可互为源/目的）走**锁 + 验证 + 重放去重**三步。新 `BridgeLock { account, amount, dest_chain: Hash, dest_account, nonce, signature }`（签名 op 镜像 `StakeOp`，验签 + `amount != 0` + 余额覆盖 + account 已知，error 复用 `ZeroStake`/`BadSignature`/`UnknownAccount`/`InsufficientBalance`）；`Block.bridge_root` 进 cert-signed prefix 槽（与 `graph_root` 同位、累计+单调 `lock_id` 索引 + `Chain::seal` 盖/apply 强制度，`ChainError::BridgeRootMismatch`），头尾部 `bridge_locks_commitment` 与 `txs_commitment` 同列；`Block.bridge_locks: Vec<BridgeLock>` 与 stake_ops/slashing_evidence 同列进 apply；`ChainState` 加 `bridge_locked: u64`（新供应组分、supply 不变量扩展为 `Σ余额 + treasury + bonded + unbonding + bridge_locked == supply`，lock 是 supply 内部再分配）；`state_root` digest 加 fold `bridge_locked` / `bridge_locks` 各 leaf / `bridge_lock_heights: BTreeMap<u64,u64>` / `next_lock_id`。新模块 `node/src/bridge.rs`：`LockEnvelope { source_header, source_cert, source_tracked_set, lock_id, lock, proof }`（M28 `DiffEnvelope` 同形）+ `BridgeEndpoint { my_genesis_hash, source_genesis_hash, tracker: ValidatorTracker, consumed: BTreeSet<(Hash,u64)>, minted: BTreeMap<u64,u64> }` + `VerifiedLock` + `BridgeError::{Cert(LightError), WrongDestination, AlreadyConsumed, SourceNotFollowed}`；`verify_lock` **无新 SPV 逻辑**：cert-binding 复用 M22 `verify_state_root_against_header`（**用 endpoint 自己的 tracker 集合，不用 envelope 的**——relayer 无法替换验证人集），inclusion `merkle::verify(&header.bridge_root, &leaf, &proof)`（**对 bridge_root 不是 accounts_root**），dest match `(lock.dest_chain == my_genesis_hash)`，replay `(source_genesis, lock_id) ∈ consumed`。新 wire 对 **TAG_GETLOCK=14 / TAG_LOCK=15** + `GossipNode::serve_lock(lock_id)`（用 `bridge_lock_heights` 取出块高 → 从 retained chain 取 header+cert，从 `state.validators` 跟重放取 active set）；`LightGossipNode::locks: Option<LockEnvelope>` + `take_lock()`。**信任无关**：relayer 只搬运字节，无法伪造 A 验证人未签的锁——`node bridge` CLI 演示正路径 + tampered-proof / wrong-destination / replay / tampered-root 四类 `BridgeError` 负测 |
| **共识级跨链赎回 + 铸造（M31，目的链链上）** | 把 M30 的 off-chain `BridgeEndpoint::consume`（mint + dedup）搬进**目的链状态机**——mint 由 B 的验证人 BFT 强制，去重进 cert-signed 状态。两个自认证 block op：`BridgeHeader { source_chain, header, cert, next_set }` 推进**链上源链跟随器** `BridgeSource { set, head, height, consumed: BTreeSet<u64> }`（`ValidatorTracker` 剥去 bonds/pubkeys——`follow_header` 只需 `{set, head, height}`），`BridgeRedeem { source_chain, source_header, source_cert, lock_id, lock, proof }` 对源链 cert-signed `bridge_root` 验锁后铸造到 `dest_account`。`Genesis.bridge_sources: Vec<BridgeSourceSeed>`（`(源创世 hash, 源创世验证人集)` = 信任锚，`genesis_split` 播种 `BridgeSource { set, head: 源 hash, height: 0, consumed: {} }`）；`ChainState` 加 `genesis_hash`（本链身份、用于 dest match、**排除出 `state_root`** 因它由 `state_root` 派生、折入即循环）/ `bridge_minted: u64`（审计计数器镜像 `bridge_locked`）/ `bridge_sources: BTreeMap<Hash, BridgeSource>`；`Block` 加 `bridge_headers` / `bridge_redeems` 两 vec，apply 时 **headers 先于 redeems**（同块内跟随的 header 可被同块 redeem 引用）。`apply_bridge_header` = (1) 源已注册否则 `UnknownBridgeSource`、(2) `header.height == s.height+1 && header.prev_hash == s.head` 否则 `BridgeBadFollow`、(3) cert-binding `verify_state_root_against_header(&header, &cert, &s.set)` 否则 `BridgeCertInvalid`、(4) `next_set.merkle_root() == header.next_validators_root` 否则 `BridgeNextSetMismatch`、(5) 采纳 `s.set/head/height`。`apply_bridge_redeem` = (1) 源已注册、(2) frontier `source_header.height <= s.height` 否则 `BridgeSourceNotFollowed`、(3) cert-binding、(4) inclusion `merkle::verify(&source_header.bridge_root, &leaf_hash(&lock.merkle_leaf(lock_id)), &proof)` 否则 `BridgeInclusionInvalid`、(5) dest match `lock.dest_chain == self.genesis_hash` 否则 `BridgeWrongDestination`、(6) replay `!consumed.contains(&lock_id)` 否则 `BridgeAlreadyRedeemed`、(7) `dest_account` 存在、(8) **mint**：`accounts[dest].balance += amount; supply += amount; bridge_minted += amount; consumed.insert(lock_id)`——验证全先于变更、坏 op 整块回滚（同 `apply_stake_op` 纪律）。redeem 同时增 `balance` 与 `supply`，故 `supply_conserved()` **不变**（`bridge_minted` 只是审计镜像）。codec 头再加 `bridge_headers_commitment` / `bridge_redeems_commitment` 两 32B 承诺（`decode_certified_header` 长度 +64B）+ `encode/decode_bridge_header` / `_bridge_redeem` + block 两新长度前缀 vec；`ChainDriver` 加 `pending_bridge_headers/redeems` + `stage_bridge_header` / `stage_bridge_redeem` + produce/clear 接线。relayer 仍**信任无关**：只搬 A 的 cert-signed 字节，唯有 A 验证人签名 + cert-signed `bridge_root` 授权 mint——`node redeem` CLI 演示正路径（A 锁 → B 链上跟随 → B 赎回铸造到 account 5、供应守恒）+ tampered-proof/`BridgeInclusionInvalid`、tampered-root/`BridgeCertInvalid`、wrong-dest/`BridgeWrongDestination`、replay/`BridgeAlreadyRedeemed`、not-followed/`BridgeSourceNotFollowed` 五类负测 |
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
commit_round_trip                             证书规范编码往返稳定
decode_commit_rejects_trailing_bytes          证书解码拒绝尾部多余字节
# 持久化（store.rs）
append_then_read_back / empty_log_reads_empty / torn_tail_is_detected
cert_log_append_then_read_back                证书日志追加→读回一致
cert_log_torn_tail_is_detected                证书日志残缺尾被检测
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
proposer_is_deterministic / round_changes_the_proposer
# BFT 共识（consensus.rs）
quorum_of_precommits_commits                  3/4 预提交 → 提交（容 1 崩溃）
below_quorum_does_not_commit                  2/4 → 不提交（安全）
forged_precommit_is_rejected / double_counting_a_validator_is_rejected
a_prevote_is_not_a_valid_precommit
conflicting_commits_require_equivocation       冲突证书 → 揪出双签者
honest_validators_cannot_form_conflicting_commits  无双签则无法造冲突证书
# BFT 轮次状态机（round.rs）
all_honest_commit_in_round_zero               全诚实 → round 0 定稿
all_honest_agree_on_the_same_block            全体对同一区块达成一致
one_crash_still_commits                       1 崩溃 → 仍定稿（容错）
silent_proposer_triggers_round_change_and_still_commits  提议人宕机 → 换轮仍出块（活性）
too_many_crashes_stalls_without_forging_a_commit  2 崩溃 → 停摆但绝不伪造证书（安全）
a_proposal_from_a_non_proposer_is_ignored     非提议人的提案被丢弃
run_is_deterministic                          同输入 → 同结果同轮次
# BFT 认证链驱动（driver.rs）
grows_a_multi_height_certified_chain          逐高度生长，每块附证书，供应守恒
every_committed_height_has_a_valid_certificate  每高度证书验签通过且绑定所提交区块
certificate_binds_to_the_committed_block      证书的 height/block_hash 与链 head 一致
progresses_with_one_crashed_validator         1 验证人离线 → 链仍生长（活性）
stalls_safely_when_quorum_is_impossible       2 离线 → 停摆且链状态不变（安全）
two_drivers_grow_identical_chains             同输入 → 相同 head/state_root/证书链
# 最终性持久化（driver.rs + lib.rs）
retains_blocks_paired_with_certificates       驱动器逐高度保留区块与证书配对
persisted_certified_chain_reverifies_finality 落盘认证链重放复验最终性且 state_root 一致
replay_rejects_a_forged_certificate           证书被改绑到别的区块 → 拒绝
replay_rejects_a_dropped_certificate          证书数量与区块不符 → 拒绝
replay_rejects_a_certificate_below_quorum     证书权重不足 > 2/3 → 拒绝
# 链上/动态验证人集（validator.rs + lib.rs + codec.rs + driver.rs）
apply_updates_adds_removes_and_reweights      验证人集应用变更：增/删/改权
apply_updates_removing_absent_is_a_noop       删除不存在的验证人 → 无操作
apply_updates_result_is_order_independent     变更结果与应用顺序无关
genesis_seeds_the_validator_set_as_state      创世把验证人集播种为链上状态
a_validator_update_takes_effect_next_height   变更由旧集合认证、下一高度生效
state_root_covers_the_validator_set           验证人集折入 state_root
a_block_cannot_empty_the_validator_set        清空验证人集的区块 → 拒绝
validator_updates_round_trip_in_a_block       区块携带验证人变更编解码往返稳定
grows_across_an_on_chain_validator_change     链跨越链上验证人交接生长、重放跟随
replay_under_a_different_genesis_validator_set_is_rejected  用错误创世验证人集重放被拒
# 编解码（codec.rs）— tx wire
tx_round_trip                                 单交易 wire 编解码往返 + 拒绝尾部字节
# P2P 网络（net.rs）
wire_round_trips_every_message                四类 gossip 消息 wire 编解码往返稳定
framed_stream_round_trip                      长度前缀分帧 write_msg/read_msg 往返
decode_rejects_trailing_bytes                 gossip 解码拒绝尾部多余字节
fresh_node_syncs_the_whole_certified_chain    新节点反熵同步整条认证链、state_root 一致
sync_rejects_a_forged_certificate             伪造/不足额证书被拒，链停在缺口不被污染
tx_gossip_reaches_every_node                  一处注入的交易 epidemic 泛洪到全网 mempool
a_duplicate_tx_does_not_re_flood              已见过的交易不再转发（泛洪终止）
nodes_at_mixed_heights_all_converge           混合高度的节点全部追赶到同一 head
gossip_is_deterministic                       同输入 → 同收敛 head
evidence_gossip_reaches_every_node            一处注入的双签证据 flood 到全网 pending_evidence 池
a_duplicate_evidence_does_not_re_flood        已见过的证据不再转发
a_malformed_evidence_is_silently_dropped      形状不合规的证据 → 不广播、不入待打包池
stake_op_gossip_reaches_every_node            一处注入的 StakeOp flood 到全网 pending_stake_ops 池
gossiped_evidence_lands_in_the_next_proposed_block  gossip→待打包池→出块→罚没→供应守恒→重放复验最终性
# 质押绑定权重与解绑（lib.rs + codec.rs + driver.rs）
bonding_makes_an_account_a_validator_next_height   绑定 → 账户成为验证人、权重 == 绑定量、下一高度生效
unbond_schedules_a_delayed_withdrawal_that_matures 解绑 → 撤权 + 时间锁提款，到期返还余额
bond_beyond_balance_is_rejected               绑定超余额 → 拒绝、整块回滚
unbond_beyond_bond_is_rejected                解绑超绑定量 → 拒绝
forged_stakeop_is_rejected                    他人密钥签的 stake op → BadSignature
zero_amount_stakeop_is_rejected               绑定/解绑 0 → 拒绝
state_root_covers_bonded_stake                绑定折入 state_root（改绑定 → 根变）
a_full_bond_unbond_cycle_conserves_supply     绑定/解绑全程供应守恒
stakeop_round_trip                            stake op wire 编解码往返 + 拒绝尾部字节
stake_ops_round_trip_in_a_block               区块携带 stake_ops 编解码往返稳定、纳入哈希
bonds_stake_and_activates_a_validator_through_the_certified_chain  经 BFT 认证链绑定 → 新权重认证下一高度
# 等价双签罚没（lib.rs + codec.rs + driver.rs）
slashing_burns_bonded_stake_to_treasury_and_removes_validator  证据 → 绑定质押罚没入 treasury、移出验证人集
slashing_also_seizes_a_maturing_unbonding_entry  罚没同时追缴解绑队列中的金额
slashing_a_genesis_validator_removes_it_without_moving_money  罚没无质押的创世验证人 → 仅移出、供应守恒
malformed_evidence_is_rejected_and_rolls_back  两票同哈希（非冲突）→ 拒绝、整块回滚
evidence_against_a_non_validator_is_rejected  证据针对非验证人 → 拒绝
forged_evidence_signature_is_rejected  证据签名与被控验证人不符 → 拒绝
slashing_cannot_empty_the_validator_set  罚没清空整个验证人集的区块 → 拒绝
evidence_round_trip                           证据 wire 编解码往返 + 拒绝尾部字节
slashing_evidence_round_trip_in_a_block       区块携带 slashing_evidence 编解码往返稳定、纳入哈希
slashes_an_equivocating_validator_through_the_certified_chain  经 BFT 认证链罚没双签者、重放复验
# 轻客户端跟随验证人集（light.rs）
follows_a_plain_chain                          无集合变更时逐高度跟随到与全链一致的集合
follows_explicit_validator_updates             跟随显式 validator_updates（增 / 删验证人）
follows_staking_power_changes                  跟随 bond/unbond 引起的权重增减
follows_slashing_removal                       跟随 slashing_evidence → 作恶者被移出
ignores_transactions                          区块含真实交易，轻客户端 0 执行仍得对集合
rejects_a_forged_certificate                   证书法定人数不足 → Consensus 拒绝、跟随器不动
rejects_a_spliced_block                        高度跳变 / prev_hash 不接 → BadHeight / ForkDetected
rejects_a_cert_for_the_wrong_block             证书与区块不匹配 → CertificateMismatch
matches_replay_verified_final_set              三种迁移 + 交易混合链：跟随集合 == 全量 replay_verified 集合（逐字节）
# 验证人集 Merkle 承诺入头（validator.rs + lib.rs + light.rs）
merkle_root_is_order_independent_and_content_addressed  集合根与构造顺序无关、改任一字段则根变、空集合 → 全零根
membership_proofs_verify_for_every_member      每个成员的包含证明都对 merkle_root 验证通过
forged_validator_leaf_fails_membership         改权后的伪造叶 → 包含证明验证失败
genesis_commits_to_the_genesis_validator_set   创世头承诺到创世验证人集的根
tampered_next_validators_root_is_rejected      篡改区块头承诺根 → ValidatorRootMismatch
seal_commits_to_the_post_apply_set_across_a_stake_change  seal 承诺到 post-apply（下一高度生效）集合，跨质押变更成立
stale_root_after_appending_ops_is_rejected     seal 后再追加 ops 令根陈旧 → 提交被拒
follow_committed_matches_transition_follow      免迁移 follow_committed 与 M20 迁移式 follow 达到同一集合
follow_committed_rejects_a_wrong_next_set        给错下一集合 → 与承诺根不符被拒
follow_cross_checks_against_the_committed_root    迁移导出集合逐高度对承诺根交叉校验
verify_membership_proves_and_rejects_forgery     对 cert 签名头证明成员 → true；伪造叶 → false
# 钱包 SPV 账户证明（lib.rs + light.rs + net.rs）
app_state_roots_seal_commit_mismatch_is_rejected  seal 后改 state_root / accounts_root → commit 拒绝（Mismatch）
state_root_and_accounts_root_advance_across_each_block_in_a_certified_chain  驱动器每块盖两根、replay 与现算根逐字节一致
verify_account_membership_header_only_works_against_a_cert_signed_header  cert-signed header + 本地 leaf → Ok
verify_account_membership_rejects_an_inflated_balance  改余额 → MembershipProofInvalid
verify_account_membership_rejects_tampered_accounts_root  改根 → MembershipProofInvalid（根在证书内、遂证书亦失配）
verify_account_membership_rejects_a_wrong_certificate  错高度证书 → CertificateMismatch
verify_state_root_against_header_accepts_a_cert_signed_header  verify_state_root_against_header 对 cert-signed header → Ok
full_node_serves_an_account_proof_in_response_to_get_account_proof  全节点 GetAccountProof → AccountProof 含可用 proof
light_node_proves_account_balance_against_cert_signed_header  光端经 gossip 取证明 → 本地验 → Ok
light_node_rejects_an_inflated_account_proof  改账户 → 验失败
account_proof_request_for_unknown_id_yields_a_rejecting_proof  未知 id → 默认账户 + 空证明 → 验失败
# M24 泛化批量化证明请求总线（lib.rs + codec.rs + light.rs + net.rs）
proof_kind_round_trip                        ProofKind 三变体编码往返稳定
batch_getproof_and_proof_round_trip          GetProof/Proof 两端编码携带 Account/Reviewer/Validator 混合列表、含 None 槽
reviewer_proof_round_trip                    ChainState::reviewer_proof(id) 对 accounts_root 验证通过；改 reputation → 失败；未知 id → None
reviewer_proof_index_lies_after_all_accounts reviewer_proof 的 index 在所有 account_proof 的 index 之后
verify_proof_against_header_accepts_account_reviewer_and_validator_in_one_call  三变体 ProofEntry 各自 verify_proof_against_header 对同一 cert-signed 头成立
verify_proof_against_header_rejects_a_tampered_validator_leaf  改 validator.power → MembershipProofInvalid
verify_proof_against_header_rejects_tampered_reputation  改 reputation → MembershipProofInvalid
full_node_serves_a_batch_of_proofs_in_response_to_get_proof  三类证明批量化往返
full_node_serves_a_reviewer_proof_for_a_reviewer_id_not_in_accounts  非账户 id 的审阅人也能取到合法 proof
get_proof_with_too_many_items_is_a_codec_error  33 项超出 MAX_PROOF_BATCH → decode 拒绝
proof_request_for_unknown_id_yields_none_in_the_response  未知 id → 服务端 None、客户端缺槽
# 图节点 cert-signed 包含证明（M25，lib.rs + codec.rs + light.rs + net.rs）
graph_node_proof_round_trip                    ChainState::graph_node_proof(idx) 对 accounts_root 验证通过；越界 idx → None
graph_node_proof_lies_after_all_reviewers      graph 节点的 proof index 在所有 reviewer proof 之后
verify_proof_against_header_accepts_graph_node_in_one_call  ProofEntry::GraphNode 对 accounts_root 验证通过
verify_proof_against_header_rejects_a_tampered_graph_node  改 embedding → MembershipProofInvalid
full_node_serves_a_graph_node_proof_in_response_to_get_proof  全节点 GetProof 含 GraphNode 变种 → Proof 含可验证明
light_node_proves_graph_node_membership_against_cert_signed_header  光端经 gossip 取证明 → 本地验 → Ok
# 图节点 cert-signed 邻域证明（M26，engine + lib.rs + light.rs + net.rs）
rank_by_cosine_is_stable_across_ties          同 sim 邻居按 node_id 升序稳定
k_nearest_with_ties_returns_prefix_with_all_ties_at_boundary  边界 ties 全留，len ≥ k
verify_knn_against_header_accepts_a_cert_signed_neighborhood_claim  wallet 重排+cut 与 prover 等比 → Ok
verify_knn_against_header_rejects_a_swapped_neighbour_order  交换 → KnnRankingMismatch
verify_knn_against_header_rejects_a_tampered_neighbour_embedding  改 embedding → MembershipProofInvalid
verify_knn_against_header_rejects_a_tampered_accounts_root  改根 → CertificateMismatch（根在证书内）
verify_knn_against_header_rejects_an_empty_claim  k > 0 但空邻居 → EmptyKnnQuery
serve_knn_returns_a_typed_claim_with_verifiable_neighbours  全节点 kNN claim → 每叶 accounts_root 验通过 + wallet 端 Ok
serve_knn_returns_none_for_an_empty_graph       空图（边界情形） → None
# 图节点 cert-signed 范围查询（M27，engine + lib.rs + codec.rs + light.rs + net.rs）
graph_root_mismatch_is_rejected                篡改 graph_root → GraphRootMismatch、整块回滚
graph_merkle_root_is_deterministic_for_a_fixed_pivot  同图两次 graph_merkle_root 相等（canonical pivot 决定性）
graph_merkle_root_differs_from_accounts_root_for_a_non_trivial_graph  同图不同叶序 → 不同根
graph_range_proof_round_trip                   graph_range_proof(a, b) 子根 = graph_merkle_root；越界/空 → None
header_round_trip_with_graph_root              encode_header/decode_header 保留 graph_root
block_round_trip_with_graph_root               encode_block/decode_block 保留 graph_root；prefix 仍 180B 与 encode_header 同字节
verify_range_against_header_accepts_a_cert_signed_cutoff_claim  wallet cosine 重排+cut 与 prover 等比 → Ok
verify_range_against_header_rejects_a_missing_node_in_the_cut  交换 → RangeMismatch
verify_range_against_header_rejects_an_invalid_cutoff  min_sim ∉ [-1, 1] → RangeCutoffInvalid
verify_range_against_header_rejects_a_tampered_graph_root  改根 → CertificateMismatch（根在证书内）
serve_range_returns_a_typed_claim_with_verifiable_proofs  全节点 range claim → 每叶 graph_root 验通过 + wallet 端 Ok
serve_range_returns_none_when_no_node_meets_the_cutoff  过高 cutoff → None
# 图节点 cert-signed 时序 diff（M28，lib.rs + codec.rs + light.rs + net.rs）
verify_diff_accepts_a_cert_signed_two_header_claim  端到端：h₁=1, h₂=2 两头证书绑定 + 重放等比 → Ok
verify_diff_rejects_a_tampered_added_proof          篡改 added leaf proof → MembershipProofInvalid
verify_diff_rejects_a_tampered_h2_accounts_root     改 header_h2.accounts_root → CertificateMismatch（根在 header.hash() 里）
verify_diff_rejects_an_omitted_added_node           prover 漏报 added leaf → DiffMismatch（重放找回来了）
verify_diff_rejects_degenerate_ranges               h₁=0 / h₁≥h₂ → InvalidDiffRange
serve_diff_returns_a_typed_envelope_for_a_height_range  2-block 链 serve_diff → envelope 每叶对 h₂ accounts_root 验证 + wallet Ok + wire 回环后仍 Ok
serve_diff_returns_none_for_degenerate_ranges       h₁=0/h₁≥h₂/h₂ 越界/头高错配 → None
# 文件化配置（M32→M33，config.rs）
node_config_round_trip                         NodeConfig toml 往返 + listen/peer 地址解析
genesis_config_converts_to_demo_genesis        GenesisConfig::to_genesis() 复刻 demo_genesis 字段
validator_section_defaults_to_disabled         [validator] 缺省 = enabled=false（纯 follower）
checked_in_testnet_samples_load                testnet/*.toml 样例载入 + 4 个 node seed 复刻验证人 pubkey
validator_pubkey_mismatch_is_detectable        cfg 的种子与 genesis pubkey 不匹配 → typed ConfigError
# 联网 tokio 守护进程（M33，daemon.rs）
frame_round_trip_over_duplex                   write_frame/read_frame 经 tokio duplex 往返（扩展含 Consensus 帧）
oversized_frame_is_rejected                    超帧长度头 > MAX_FRAME → 分配前拒绝
hello_handshake_round_trip                     8 字节 BE id 握手往返
four_validators_converge_over_tcp              4 验证人、无定序器，loopback TCP 收敛到同一 head + 重放复验证书
one_crashed_validator_still_makes_progress     3/4 活（quorum=3）：连返几高度、round change 拉动
two_crashed_validators_stall_safely            2/4 活（quorum 不可达）：8s 内高度不动、安全停摆
late_joiner_syncs_then_participates            3 验证人先 commit，第 4 个晚启动 → 抗熵追平 + 一起推进 + 重放复验
pure_follower_syncs_certified_chain            4 验证人 + 1 follower（kp=None）→ 仅同步 + 持久化、不投票 + 重放复验
# 观测到双签即主动罚没（M34，round.rs + daemon.rs）
precommit_equivocation_yields_evidence         同验证人/高度/round、hash 不同的两条 precommit → Action::Equivocation(ev) 且 is_well_formed
duplicate_precommit_is_not_equivocation        同 hash 重发 → 无 Equivocation（幂等重传非双签）
precommits_in_different_rounds_are_not_equivocation  hash 冲突但 round 0 vs 1 → 无 Equivocation（跨轮 unlock/relock 合法）
prevote_equivocation_is_not_slashable          冲突 prevote → 无 Equivocation（本模型只罚 precommit）
equivocation_evidence_is_canonically_ordered   两票任意到达序 → 同一 ev.hash()（跨节点 dedup 保证）
equivocation_over_tcp_slashes_the_offender     3 诚实 + TCP 双签注入器 → 证据 flood 入块、链上没收 bond + 移出验证人集
# 配置驱动共识时序 + create_empty_blocks（M35，config.rs + daemon.rs + net.rs）
consensus_config_defaults_match_legacy_constants     ConsensusConfig::default() == 1000/1000/1000/500/1000 + create_empty_blocks=true（等于旧常量）
node_config_without_consensus_section_uses_defaults  缺 [consensus] 段的 toml → cfg.consensus == default（后向兼容）
consensus_section_partial_override_fills_from_default 只写部分键的 [consensus] → 覆盖键生效、其余回落 Default
timeout_for_uses_configured_bases_and_delta          Timing{propose 200, delta 50} → timeout_for(Propose, 2)==300ms，per-step 基础各自生效
create_empty_blocks_false_pauses_then_advances_on_work 3 验证人 create_empty_blocks=false → 空闲 ~2.5s 高度不动；submit 一 tx → 恰进一非空块
# 配置驱动网络时序（M36，config.rs）
network_config_defaults_match_legacy_constants       NetworkConfig::default() == announce_interval_ms 2000 / startup_delay_ms 1000（等于旧常量，含 2s→2000ms 换算）
node_config_without_network_section_uses_defaults    缺 [network] 段的 toml → cfg.network == default（后向兼容）
network_section_partial_override_fills_from_default   只写 announce_interval_ms 的 [network] → 该键生效、startup_delay_ms 回落 Default
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`（含 `next_validators_root` 验证人集 Merkle 承诺 + **M23 `state_root` 完整共识状态 digest 与 `accounts_root` accounts/reviewers Merkle 根两条承诺根** + **M27 `graph_root` 按 (cos_sim(CANONICAL_PIVOT, *) desc, node_id asc) 排序的图节点 Merkle 根第三条承诺根** + **M30 `bridge_root` 累计 bridge_locks Merkle 根第四条承诺根** + **M30 `bridge_locks: Vec<BridgeLock>` 与 stake_ops/slashing_evidence 同列进 apply**）/`SubmissionTx`/`StakeOp`/`BridgeLock`/`SlashEvidence`（含 `hash()` 内容寻址用于 gossip 去重）/`Account`/`ChainState`（**M30 `bridge_locked: u64` 新供应组分、`bridge_locks: BTreeMap<u64,BridgeLock>` 累计映射、`bridge_lock_heights: BTreeMap<u64,u64>` 锁-块高映射、`next_lock_id: u64` 单调计数器**）/`Chain`、`apply_block`（**M30 多 `BridgeRootMismatch` 强制度；新 `apply_bridge_lock(lock, height)` 走 ZeroStake/BadSignature/UnknownAccount/InsufficientBalance 现有错误类**）、`Chain::seal`/`next_validators_root`（**M23 同 trial 路径盖两根 + M27 同 trial 路径盖 graph_root + M30 同 trial 路径盖 bridge_root**）、`apply_stake_op`、`apply_evidence`、`replay`/`replay_verified`、验签、`state_root`（**M30 digest 多 fold `bridge_locked` / `bridge_locks` 各 `lock.merkle_leaf(id)` / `bridge_lock_heights` / `next_lock_id`**）/`merkle_root`/`account_proof`、**M27 `graph_merkle_root`（按 CANONICAL_PIVOT 排序索引）/`graph_range_proof(a, b)`（`RangeProof { sub_root, entries }`）**、**M28 `GraphLeafAtHeight { node_id, graph_node, proof }` + `DiffClaim { added, dropped }`（无 changed 臂）+ `graph_diff(&prev_state)`（每 leaf 的 proof 对**各自高度**的 `accounts_root`）**、**M30 `bridge_merkle_root`（按 lock_id 排序索引）/`bridge_lock_proof(lock_id)`（同 M22/M27 单 leaf 路径）/`bridge_merkle_root_for_genesis(g)`（空 locks 起点）**、供应守恒不变量（含 bonded + 解绑中 + **M30 `bridge_locked`**）+ 测试 |
| `src/mempool.rs` | 确定性 mempool 与出块：内容寻址排序 + 试算式 `build_block` + 测试 |
| `src/merkle.rs` | 二叉 Merkle 树：域分隔叶/节点、奇数提升、包含证明 `Proof`/`verify` + 测试 |
| `src/validator.rs` | 验证人集与确定性提议人（Tendermint 优先级累加器）、链上变更 `ValidatorUpdate`/`apply_updates`、集合 Merkle 承诺 `merkle_leaf`/`merkle_root`/`proof`（M21）+ 测试 |
| `src/consensus.rs` | BFT 投票/最终性证书：`Vote`/`Commit`/`verify`、`commit_block`、`detect_equivocation` + 测试 |
| `src/round.rs` | BFT 轮次状态机（Tendermint `upon` 规则、超时/锁定/换轮）+ 进程内网络模拟器 `Sim` + **M34 `ingest -> Option<SlashEvidence>`：摄入 precommit 时若已持有同 `(validator, height, round)` 的另一 `block_hash` → 按 hash 规范排序组装证据，经新 `Action::Equivocation(SlashEvidence)` 上抛（first-wins `or_insert` 不变，纯旁路观测）** + 测试 |
| `src/driver.rs` | BFT 认证链驱动 `ChainDriver`：逐高度 mempool→共识→提交 + 证书保留 + 故障注入 + 链上验证人变更（`stage_validator_update`）+ 质押变更（`stage_stake_op`）+ 罚没证据（`stage_slashing_evidence`）+ 测试 |
| `src/net.rs` | P2P gossip 与反熵同步：`GossipMsg`/`GossipNode`（纯状态机，认证块 `apply_certified` 复验证书、交易 epidemic 泛洪去重 + **`Evidence` / `StakeOp` 块级 ops 的待打包池与去重 flood**；M22 增 `GetHeaders` / `Headers` 服务 + **M24 完全替换 `GetProof { items }` / `Proof { items }` 一对（wire tags 8/9 复用），承载 Account/Reviewer/Validator 任意混合 `items`，上限 `MAX_PROOF_BATCH = 32`；全节点在 on_message 现取现发 account_proof/reviewer_proof/ValidatorSet::proof 三类；光节点入 `proofs` 缓存（键 `(ProofKind, u64)`）** + **M26 `GossipNode::serve_knn(query, k)` 派 `KnnClaim` 给光端（叶子账密存于 accounts_root 插入序侧）** + **M27 `GossipNode::serve_range(query, min_sim)` 派 `RangeClaim` 给光端（叶子存于 `graph_root` 排序索引侧）** + **M28 `GossipNode::serve_diff(h1, h2, header_h1, header_h2)` 派 `DiffEnvelope` 给光端（缓存 `genesis: Genesis` 字段以便从创世重放 `[0..h1]` 构造 prev 状态；envelope 含两边 header/cert/tracked set + `DiffClaim`；`Diff { envelope }` 与 `GetDiff { header_h1, header_h2 }` 因体积大走 `Box` 装箱避开 large_enum_variant；wire tags 增 `TAG_GETDIFF=10` / `TAG_DIFF=11`；`encode_diff_envelope`/`decode_diff_envelope` 共用 `codec::encode_graph_node`/`encode_proof`/`encode_validator`/`encode_header`/`encode_commit` 现成构件**）+ **M29 `GossipNode::serve_inclusion(kind, id)` 从 M24 内联抽取 + `serve_batch(items)` 派 `BatchResponseEnvelope`（复用 M24/M26/M27/M28 四个 serve_*，无新 SPV 逻辑，仅 dispatch；`Batch { envelope }` 因体积大走 `Box` 装箱；wire tags 增 `TAG_GETBATCH=12` / `TAG_BATCH=13`）** + **M30 `GossipNode::serve_lock(lock_id)` 派 `LockEnvelope` 给光端（用 `state.bridge_lock_heights` 取出块高 → 从 retained chain 取 header+cert，重放 `[..=height]` 取 active set 作 `source_tracked_set`；wire tags 增 `TAG_GETLOCK=14` / `TAG_LOCK=15`；`encode_lock_envelope`/`decode_lock_envelope` 共用 `encode_header`/`encode_commit`/`encode_validator`/`encode_bridge_lock`/`encode_proof` 现成构件；on_message 加 `GetLock` 现取现发、`Lock` 静默丢弃两支）** + 确定性 `Network` 收敛总线 + **M22 光节点 `LightGossipNode`（仅头、`ValidatorTracker`、从不解码交易）+ 混入全/光节点的总线 `LightNetwork`**（**M28 `LightGossipNode` 加 `diffs: Option<DiffEnvelope>` 缓存 + `take_diff()` 弹出；M29 加 `batches: Option<BatchResponseEnvelope>` 缓存 + `take_batch()`；M30 加 `locks: Option<LockEnvelope>` 缓存 + `take_lock()`；on_message 同步新增 `Diff` / `GetDiff` / `Batch` / `GetBatch` / `Lock` / `GetLock` 六支**）+ `encode_gossip`/`read_msg`/`write_msg`（真实 socket 分帧，含新 TAG_GETHEADERS/TAG_HEADERS + **TAG_GETPROOF=8 / TAG_PROOF=9** + **TAG_GETDIFF=10 / TAG_DIFF=11** + **TAG_GETBATCH=12 / TAG_BATCH=13** + **TAG_GETLOCK=14 / TAG_LOCK=15**） + **M35 `GossipNode::has_pending_work()`（mempool ∪ 待打包 stake_ops ∪ 待打包证据；桥无待打包池、故意不计入，daemon 的 `create_empty_blocks` 门用它判定空闲）** + 测试 |
| `src/bridge.rs` | **M30 信任无关跨链桥**：`LockEnvelope { source_header, source_cert, source_tracked_set, lock_id, lock, proof }`（M28 `DiffEnvelope` 同形）+ `BridgeEndpoint { my_genesis_hash, source_genesis_hash, tracker: ValidatorTracker, consumed: BTreeSet<(Hash,u64)>, minted: BTreeMap<u64,u64> }` + `VerifiedLock` + `BridgeError::{Cert(LightError), WrongDestination{expected, got}, AlreadyConsumed{source_chain, lock_id}, SourceNotFollowed{height}}`；`new(my_genesis, source_genesis)` 用两链创世哈希作链 id、`source_genesis` 启 `ValidatorTracker::from_genesis`；`follow_source(header, cert, next_set)` 复用 M22 `follow_header`；`verify_lock(env)` **无新 SPV 逻辑** = (1) cert-binding 复用 M22 `verify_state_root_against_header(**self.tracker.validators()**, cert)`（**endpoint 自己的集合，不用 envelope 的**——relayer 无法替换验证人集）+ (2) inclusion `merkle::verify(&env.source_header.bridge_root, &leaf_hash(&lock.merkle_leaf(lock_id)), &env.proof)` + (3) dest match `lock.dest_chain == my_genesis_hash` + (4) replay `(source_genesis, lock_id) ∉ consumed`；`consume(v)` 入 dedup + `minted[dest_account] += amount`（bridge-module 记账，consensus 不感知 mint）；`hex8` 错误消息 + `Display`/`Error` impl + 7 类测试（accept / tampered amount / tampered bridge_root / wrong dest / replay / source-not-followed / 两链 B→A 对称） |
| `src/light.rs` | 轻客户端验证人集跟随：`ValidatorTracker`（`from_genesis` / `follow` / `follow_all`，逐高度复验证书 + 复刻 `apply_block` 的集合迁移，镜像 `bonds` + 创世公钥表，不执行交易；M21 `follow` 对 `next_validators_root` 交叉校验、免迁移 `follow_committed`、SPV `verify_membership`；M22 只对头的 `follow_header` + `verify_membership_against_header` + **M24 唯一 SPV 验证器 `verify_proof_against_header`（按 entry.kind() 选根，Account/Reviewer → accounts_root、Validator → next_validators_root，本地重算 leaf）；M23 的 `verify_account_membership_against_header` 与 M22 的 `verify_membership_against_header` 全部删除；`verify_state_root_against_header` 保留** + **M26 `KnnClaim` + `verify_knn_against_header`（对 accounts_root 验邻域 leaf，本地 cos_sim 重排+cut，prover 序列等比）** + **M27 `RangeClaim` + `verify_range_against_header`（对 `graph_root` 验范围 leaf，cutoff ∈ [-1,1] 校验 + 本地 cos_sim 重排+prefix cut，prover 序列等比；新增 `LightError::RangeMismatch`/`RangeCutoffInvalid`）** + **M28 `DiffEnvelope { header_prev, cert_prev, header_new, cert_new, diff, tracked_set_h1, tracked_set_h2 }` + `verify_diff_against_headers(genesis, blocks_in_range, claim)`：两边 header/cert 绑定+各自 tracked_set 验签（动态验证人集下两高度用不同集合）+ `[1..=h₂]` 局部重放 → `state_at_h2`+`[1..=h₁]` → `state_at_h1`+每 leaf 对**各自高度** accounts_root 的 Merkle 验证+prover 与重放 `added/dropped` 集合等比；新 `LightError::InvalidDiffRange { h1, h2 }` / `DiffMismatch { height }`** + **M29 `BatchItem` + `BatchResponseItem`（4 类异构槽位 Inclusion/Knn/Range/Diff）+ `BatchResponseEnvelope { items }` + `verify_batch(genesis, header, cert, tracked_set, blocks_in_range, items, response)` 把每个 slot 派回 M24/M26/M27/M28 四个验证器——**无新 SPV 逻辑**，仅 dispatch；wallet 侧 cert-binding 上下文显式传参（`ValidatorTracker` 仅存 set/bonds/pubkeys/head/height，header/cert 来自 M22 头部缓存）；新 `LightError::{BatchTooManyItems, BatchItemCountMismatch, BatchItemKindMismatch}` 三类**仅协议违规**错误（per-primitive 错误通过 dispatch 转发）**）+ `LightError` + 测试 |
| `src/crypto.rs` | ed25519 身份：`Keypair`/`verify`（封装 `ed25519-dalek`）+ 测试 |
| `src/codec.rs` | 区块的规范二进制编解码（哈希与落盘共用，含 `validator_updates`、`stake_ops` 与 `slashing_evidence`；M22 增 `BlockHeader`（含 `txs_commitment`/`stake_ops_commitment`/`evidence_commitment` 三份 SHA-256 承诺）+ `CertifiedHeader` + `encode_header`/`decode_header` + `encode_certified_header`/`decode_certified_header` + **M23 头再加 `state_root`/`accounts_root` 两根、`Block`/`BlockHeader` 同步增两字段、`decode_certified_header` 长度算术从 `84 + n*48 + 96` 改为 `148 + n*48 + 96 = 244 + n*48`** + **M27 头再加 `graph_root` 根、`Block`/`BlockHeader` 同步增字段、`decode_certified_header` 长度算术从 `148` 改为 `180 + n*48 + 96 = 276 + n*48`，prefix 仍与 `encode_block` 字节对齐** + **M30 头再加 `bridge_root` 根 + 尾部 `bridge_locks_commitment` 承诺、`Block.bridge_locks: Vec<BridgeLock>`、`decode_certified_header` 长度算术从 `180` 改为 `212 + n*48 + 128 = 340 + n*48`**）+ `tx_signing_bytes`/`encode_tx`/`decode_tx`（签名/tx 哈希/gossip wire 字节）+ `stakeop_signing_bytes`/`encode_stakeop`/`decode_stakeop`（bond/unbond 签名与哈希）+ `encode_evidence`/`decode_evidence`（双签证据）+ `encode_commit`/`decode_commit`（证书落盘）+ **`encode_account`/`decode_account`/`encode_proof`/`decode_proof`（M23 AccountProof 的 wire 字节）** + **M28 `encode_diff_envelope`/`decode_diff_envelope`** + **M29 `encode_knn_request`/`decode_knn_request`（32-byte query + u32 k）+ `encode_range_request`/`decode_range_request`（32-byte query + f32 min_sim）+ `encode_knn_claim`/`decode_knn_claim` + `encode_range_claim`/`decode_range_claim` + `encode_batch_envelope`/`decode_batch_envelope`（u32 len + 每 slot 1-byte kind tag + per-variant body）+ `encode_batch_response_kind`/`decode_batch_response_kind`** + **M30 `encode_bridge_lock`/`decode_bridge_lock`（account/amount/dest_chain/dest_account/nonce/sig wire）+ `bridgelock_signing_bytes`** + 测试 |
| `src/store.rs` | 追加式日志（长度前缀记录、残缺尾检测）：`BlockLog`（区块）+ `CertLog`（证书）+ 测试 |
| `src/config.rs` | **M32 文件化配置（serde + toml 镜像结构，共识类型仍 serde-free）；M33 用 `[validator]{enabled, seed_hex}` 替换 `[producer]`**：`NodeConfig`（id/listen/data_dir + 静态 peer 表 + 可选 `[validator]` + **M35 可选 `[consensus]`**）/ `GenesisConfig`（`to_genesis()` hex 解码 pubkey）+ **M35 `ConsensusConfig{propose/prevote/precommit_timeout_ms, timeout_delta_ms, block_interval_ms, create_empty_blocks}` + 手写 `Default`（逐字段 = 旧 daemon 常量，`create_empty_blocks=true`，即五个时序数字的唯一真源）；段与字段两级 `#[serde(default)]` → 缺段/部分段均回落默认（后向兼容）；**M36 可选 `[network]` → `NetworkConfig{announce_interval_ms, startup_delay_ms}` + 手写 `Default`（2000/1000 = 旧 `ANNOUNCE_SECS=2s`/`STARTUP_DELAY=1000ms` 常量，两个网络时序数字的唯一真源；`ANNOUNCE_SECS` 秒→毫秒统一到 `_ms` 约定）；同样段与字段两级 `#[serde(default)]` 回落**+ `load_node_config`/`load_genesis` + `decode_seed` 自带严格 hex 解码（`hash.rs` 只编码）+ `ConfigError` typed 错误 + 测试（往返 / 载入 `testnet/` 样例 / pubkey-mismatch fail-fast / typed 错误 / 样例生成器 / **M35 默认等于旧常量 / 缺段回落 / 部分段覆盖** / **M36 `[network]` 默认等于旧常量 / 缺段回落 / 部分段覆盖**） |
| `src/daemon.rs` | **M33 联网 tokio 守护进程（TCP P2P + 分布式 BFT 投票）**：单属主 actor（`GossipNode` 独占 task、per-peer mpsc 出站、无 `Arc<Mutex>`）——actor 现在**也独占** `Keypair: Option<Keypair>` + `Option<RoundState>` + tokio timer 句柄；`write_frame`/`read_frame`（`u32` BE 长度 + `encode_gossip`，`MAX_FRAME = 16 MiB` 上限）+ 8 字节 BE id 握手（在 `GossipMsg` 之外）+ 只拨 id 更大 peer（每对一连接）+ 监听/连接/反熵心跳任务 + **`Cmd::{StartHeight, Timeout}`（tokio sleep → self_tx）**；**`GossipNode::on_message` 仍纯**（把 `GossipMsg::Consensus` 直接 drop 掉——它既没密钥也触不到 timer），共识消息在 actor 主循环里路由到 `RoundState::on_message`/`on_timeout`；**`build_candidate` 永远出一 sealed block**（空块心跳），`on_consensus` 在 prevote 之前用 `Chain::would_accept` 试跑 apply（Byzantine-proposer 保护），`reconcile_after_sync` 让 sync 永远赢；actor 是本节点日志唯一写者（`append node.blocks()[appended..]`）；`Node::start`/`run(cfg, genesis, Option<Keypair>)`——boot 经 `load_certified` 复验最终性、validator 键与 genesis pubkey 不匹配即 fail-fast；**M34 `apply_actions` 新增 `Action::Equivocation(ev) => on_equivocation`，后者调 `submit_local_evidence`（本地暂存 + flood）把观测到的双签接入既有 M19 证据管线**；**M35 五个时序常量删除、改由 module-private `Timing` copy 结构（在 `Node::start` 从 `cfg.consensus.*` 内联构造塞进 `Actor`）承载；`timeout_for` 改纯自由函数 `timeout_for(&Timing, step, round)`；新 `on_start_tick` 只在被调度的起始路径上门控 `create_empty_blocks`（空块关且 `!has_pending_work()` → 不起轮、按 `block_interval_ms` 重排；`on_consensus` 的 lazy-start 保持不设门 → peer 一提议就跟上），`BLOCK_INTERVAL` 全部换成 `self.timing.block_interval_ms`；**M36 删除 `ANNOUNCE_SECS`/`STARTUP_DELAY` 两个网络时序常量、改由 `Node::start` 从 `cfg.network.{announce_interval_ms, startup_delay_ms}` 内联 hoist 出本地变量喂给反熵心跳 `interval` 与验证人启动 `sleep`（唯一真源在 `NetworkConfig::Default`）**；测试（分帧往返 / 超帧拒绝 / 握手 / **4 验证人收敛 / 1-fault 持续推进 / 2-fault 安全停摆 / 晚加入 / 纯 follower 跟随 / M34 TCP 双签注入 → 罚没 + 移出 / M35 `timeout_for` 配置化 + `create_empty_blocks=false` 空闲暂停→有活推进**） |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 节点 CLI：`demo` / `build` / `prove` / `bft` / `live` / `chain` / `validators` / `gossip`（含 M19 证据 flood 演示） / `light`（M20 跟随 + M21 免迁移 `follow_committed` 演示） / `vprove`（M21 验证人 Merkle 成员证明） / `lsync`（M22 头部轻同步演示：全+光节点同总线、光端 0 笔交易入眼即够到全节点高度，附线缆字节节省 + `verify_membership_against_header`） / **`account`（M23 钱包账户-成员 SPV 演示：光端经 `GetAccountProof` 取账户、本地重算 leaf 对头里的 `accounts_root` 验证，含双根对比）** / **`graph`（M25 图节点 cert-signed 包含证明演示：单次 GetProof 拿图节点 + 账户，光端对 accounts_root 重算 leaf、零信任 prover）** / **`knn`（M26 cert-signed 邻域证明演示：full peer 本地 kNN → KnnClaim，wallet 端 verify_knn_against_header 重排 + cut）** / **`range`（M27 cert-signed 范围查询演示：full peer 本地 cosine cutoff → RangeClaim，wallet 端 verify_range_against_header 对 graph_root 重排 + cut，含 cut/根/cutoff 三类负测）** / **`diff`（M28 cert-signed 时序 diff 演示：full peer 本地 h₁→h₂ diff → DiffEnvelope，wallet 端 verify_diff_against_headers 局部重放等比 + 每 leaf 对各自 accounts_root 验，含 leaf-proof / accounts_root / dropped-added 三类负测）** / **`batch`（M29 异构批 SPV 演示：full peer 一次性出 `(Inclusion, Knn, Range, Diff)` 四 slot 的 `BatchResponseEnvelope`，wallet 端 `verify_batch` 派回四个 per-primitive 验证器，含 inclusion/knn-ordering/diff 三类负测）** / **`bridge`（M30 信任无关跨链桥演示：两条不同创世 A↔B，A 锁 12 µ$COG 到 B 账户 7，relayer 从 A 拿 `LockEnvelope` 投到 B 的 `BridgeEndpoint`，B 端 `verify_lock` → Ok → `minted(7)=12`，含 tampered-proof / wrong-destination / replay / tampered-root 四类 `BridgeError` 负测）** / `staking` / `slashing` / `certs` / **`localnet`（M33 进程内 tokio 4 验证人测试网经真实 loopback socket BFT 收敛）** / `run`（**M33 起 `--config` 联网 tokio 守护进程 + 分布式 BFT 投票；旧 `--dir` 播种语义由 `localnet` 取代**） / `status`（含确定性演示密钥） |

## 局限与后续（离生产还差什么）

本里程碑刻意只做**确定性状态机内核 + 单机出块 + BFT 安全性与活性内核 + 认证链驱动 + 证书落盘复验**，尚未包含：

- **~~密码学身份~~**：✅ 已完成（M8，ed25519 签名交易）。后续：账户 = 公钥的完整身份模型、动态开户、评审签名、密钥轮换。
- **~~确定性出块~~**：✅ 已完成（M9，mempool + 试算式 `build_block`）。后续：手续费/优先级排序、区块 gas 上限、交易过期。
- **~~BFT 安全性（最终性证书）~~**：✅ 已完成（M11，投票权 > 2/3 的证书 + 提议人选择 + 双签问责）。
- **~~BFT 活性（轮次状态机）~~**：✅ 已完成（M12，propose/prevote/precommit + 超时 + 锁定 + 换轮 + 进程内模拟器）。
- **~~认证链驱动~~**：✅ 已完成（M13，逐高度 mempool→共识→提交，每块附复验证书，故障下的活性/安全行为）。
- **~~证书落盘 + 重放复验最终性~~**：✅ 已完成（M14，`certs.log` + `replay_verified` 逐高度复验 > 2/3 证书）。后续：多提议人异构 mempool、拜占庭对抗测试（等价/延迟/审查）。
- **~~P2P 网络~~**：✅ 已完成（M15，交易/认证块 gossip + 反熵状态同步 + 真实 TCP 传输；`round::Sim` 仍在进程内模拟高度内投票总线）。后续：Kademlia/节点发现、连接管理与背压、投票 gossip 上真实网络、Sybil/Eclipse 抗性。
- **~~动态验证人集~~**：✅ 已完成（M16，链上增删验证人/改权、跨高度切换、`state_root` 折入验证人集、重放逐高度跟随交接）。
- **~~质押绑定权重 + 解绑期~~**：✅ 已完成（M17，账户自绑定 $COG → 权重 == 绑定量、解绑经时间锁提款队列、供应守恒含 bonded/解绑中）。后续：验证人集变更的轻客户端跟随协议、佣金/委托质押（delegation）、绑定/解绑走 mempool 与 gossip。
- **~~按证据罚没等价双签~~**：✅ 已完成（M18，链上 `slashing_evidence` 双签证据 → 罚没绑定质押 + 解绑中金额入 treasury、下一高度移出验证人集、供应守恒）。后续：更多可归责错误（放大/审查）、部分罚没与 jailing/tombstone。
- **~~P2P 传播块级 ops（证据 + 质押变更）~~**：✅ 已完成（M19，`GossipMsg::Evidence` / `GossipMsg::StakeOp` 内容哈希去重 + 节点待打包池 + 出块方 `take_pending_*` 灌进 driver → 下一区块自动带出——作恶可远程归责，不再依赖出块人已持有）。后续：Kademlia/节点发现、连接管理与背压、投票 gossip 上真实网络、Sybil/Eclipse 抗性。
- **~~验证人集变更的轻客户端跟随协议~~**：✅ 已完成（M20，`ValidatorTracker` 只凭创世逐高度复验证书 + 复刻集合迁移，镜像 `bonds` + 创世公钥表，不执行交易即得到与 `replay_verified` 逐字节一致的活跃集合；复用 `GossipMsg::Blocks` 传输）。
- **~~验证人集 Merkle 承诺入区块头~~**：✅ 已完成（M21，`Block.next_validators_root` 折进 `block_hash`；出块方 `seal`、`apply_block` 强制根匹配；轻客户端 `follow_committed` 免复刻迁移验证整套下一集合、`verify_membership` 用 O(log n) 包含证明对 cert 签名头证明单个验证人（SPV 原语），`follow` 亦逐高度交叉校验）。
- **~~只拉头部的 SPV 轻同步传输~~**：✅ 已完成（M22，`BlockHeader` 携每份体的 SHA-256 承诺、`Block::hash` 现哈希头投影使 `header.hash() == block.hash()` 恒成立；`CertifiedHeader` + `GossipMsg::GetHeaders/Headers` 把 SPV 原语搬上 gossip 总线；`LightGossipNode` 只保留头 + `ValidatorTracker`，从不解码任何交易体；`follow_header` 与 `verify_membership_against_header` 把 `follow_committed`/`verify_membership` 迁移至只对头形式；`LightNetwork` 混合全+光节点的总线）。
- **~~钱包的账户-成员 SPV~~**：✅ 已完成（M23，`BlockHeader` 再携 `state_root` 完整共识状态 digest + `accounts_root` accounts/reviewers 二叉 Merkle 根两条承诺根；`Chain::commit` 走 trial 路径盖两根、`apply_block_inner` 多两条强制度 `StateRootMismatch` / `AccountsRootMismatch`；新 `GossipMsg::GetAccountProof` / `AccountProof`（wire tag 8/9）让钱包从对端要单账户包含证明；新 SPV `verify_account_membership_against_header` 在本地重算 leaf 并对头里的 `accounts_root` 验证，`verify_state_root_against_header` 把"完整状态 digest 由证书代验"的契约写明——钱包证明自己的余额**只下头、不下体、零重放**）。
- **~~M24 批量化、类型化 SPV 原语~~**：✅ 已完成（`GossipMsg::GetProof { items }` / `Proof { items }` 一对承载任意 `[(Account|Reviewer|Validator, id), ...]` 列表，`MAX_PROOF_BATCH = 32`；wire tags 8/9 完全替换 M23 的 `GetAccountProof/AccountProof`；新 typed `ProofEntry` 枚举 + `Reviewer::merkle_leaf()` + `ChainState::reviewer_proof(id)` 闭合审阅人路径；wallet 端**唯一** SPV 验证器 `ValidatorTracker::verify_proof_against_header(header, cert, tracked_set, entry)` 按 entry.kind() 选根（Account/Reviewer → `accounts_root`，Validator → `next_validators_root`），本地重算 leaf；M22/M23 的 kind-specific 验证器全部删除）。后续：认知图谱节点的包含证明、`graph_root` 入头、跨集合原子的多 proof 原子化提交。
- **~~持久化~~**：✅ 已完成（M7，追加式区块日志 + 重放；M14 加证书日志）。后续可换 RocksDB、加 per-record 校验和与 segment 轮转。
- **~~Merkle 化状态树~~**：✅ 已完成（M10，二叉 Merkle 树 + 账户包含证明）。后续：非成员证明、增量更新的 Merkle-Patricia trie、把 graph/头字段也纳入根。
- **手写 SHA-256** 仅为离线零依赖演示，**生产必须换审计实现**（`sha2`）。
- **kNN 暴力扫描**：随图谱增长需换 HNSW/IVF（见 engine 局限）。
- **~~联网 tokio 守护进程（单定序器测试网）~~**：✅ 已完成（M32，参见上节）。**~~M33 分布式 BFT 投票~~**：✅ 已完成——用一进程一 `RoundState` + 一 `Keypair` 替换 `Sim`/全密钥，proposal/prevote/precommit 经同 TCP 总线 gossip 出去、wall-clock 超时驱动 `on_timeout`；`GossipNode::on_message` 仍纯，把 `Consensus` 直接 drop（actor 主循环按 `Cmd::Inbound` 路由到 `RoundState`），纯 follower 节点 `kp=None` 永不跑共识；`localnet` 是 4 验证人、零定序器，每个 `[validator]` 节点自带 seed_hex，seed 与 genesis pubkey 不匹配即 fail-fast；测试：4 验证人收敛、1-fault 持续推进、2-fault 安全停摆、晚加入同步、纯 follower 跟随、配置往返 + 载入样例 + pubkey-mismatch。**~~M34 观测到双签即主动罚没~~**：✅ 已完成——`round::RoundState::ingest` 现返回 `Option<SlashEvidence>`，摄入一条 precommit 时若已持有同 `(validator, height, round)` 的另一 `block_hash` 就按 hash 规范排序组装证据、经新 `Action::Equivocation` 上抛，Actor 的 `on_equivocation` 调 `submit_local_evidence` 接入既有 M19 证据管线 → flood → 入块 → 链上没收 bond 并移出 offender；无新 wire/codec/config，first-wins 摄入不变（纯旁路观测），prevote 双签本模型不罚、纯 follower 不检测但仍转发。**后续 M35**：其余运维成熟度——`tracing` 结构化日志、metrics/health 端点、更完善的关停与错误恢复、`create_empty_blocks=false`、配置驱动的超时、peer discovery、TLS/auth。
- **依赖策略变化**：M32 起 node（应用层）新增三个依赖——异步运行时 `tokio` + 配置的 `serde`/`toml`；共识核心（`lib.rs`）仍 serde-free（`config.rs` 用镜像结构转换），`engine`（可嵌入/WASM）仍纯 std 零依赖。“节点纯 std / 零外部依赖”的旧表述自 M32 起仅适用于引擎。

这些构成后续里程碑（~~M7 持久化~~ ✅、~~M8 签名~~ ✅、~~M9 mempool 出块~~ ✅、~~M10 Merkle 认证状态~~ ✅、~~M11 BFT 最终性内核~~ ✅、~~M12 BFT 轮次状态机/活性~~ ✅、~~M13 认证链驱动~~ ✅、~~M14 证书落盘 + 重放复验~~ ✅、~~M15 P2P + gossip~~ ✅、~~M16 动态验证人集~~ ✅、~~M17 质押绑定权重 + 解绑期~~ ✅、~~M18 按证据罚没绑定质押~~ ✅、~~M19 P2P 传播块级 ops~~ ✅、~~M20 验证人集变更的轻客户端跟随协议~~ ✅、~~M21 验证人集 Merkle 承诺入区块头~~ ✅、~~M22 只拉头部的 SPV 轻同步传输~~ ✅、~~M23 钱包的账户-成员 SPV（双根承诺）~~ ✅、~~M24 批量化、类型化 SPV 原语（统一 GetProof/Proof 对 + 单一 verify_proof_against_header）~~ ✅、~~M25 单点图节点 cert-signed 包含证明~~ ✅、~~M26 图节点 cert-signed kNN 邻域证明~~ ✅、~~M27 图节点 cert-signed cosine 范围证明~~ ✅、~~M28 图节点 cert-signed 时序 diff 证明~~ ✅、~~M29 异构批 SPV 传输（一次性 inclusion + kNN + range + diff）~~ ✅、~~M30 信任无关跨链桥（relay + verify-from-counterparty）~~ ✅、~~M31 共识级跨链赎回 + 铸造（目的链链上）~~ ✅、~~M32 联网 tokio 守护进程（单定序器测试网：真实 TCP gossip + 文件化 config/genesis/keystore）~~ ✅、~~M33 分布式 BFT 投票（一进程一密钥，proposal/prevote/precommit 经真实 socket gossip + wall-clock 超时，无指定定序器）~~ ✅、~~M34 观测到双签即主动罚没（投票到达时检测 precommit 双签 → 组装证据接入 M19 flood → 链上没收 + 移出）~~ ✅、~~M35 配置驱动共识时序 + `create_empty_blocks`（`[consensus]` TOML 段可配超时/节奏、默认逐字段等于旧常量；空块可关，空闲不出块、有活即出，lazy-start 保活性）~~ ✅、~~M36 配置驱动网络时序（`[network]` TOML 段可配反熵心跳 `announce_interval_ms` / 验证人启动宽限 `startup_delay_ms`，默认 2000/1000 逐字段等于旧 `ANNOUNCE_SECS`/`STARTUP_DELAY` 常量；秒→毫秒统一到 `_ms` 约定；缺段/部分段回落默认，老配置逐字节不变）~~ ✅……；运维篮子剩余（`tracing` 结构化日志、metrics/health、peer discovery、TLS/auth）顺延至 M37+），每步仍遵循"可运行、可测试、契约一致"。
