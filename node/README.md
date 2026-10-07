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
cargo run --release --bin node -- submit-tx --config F --tx F  # M53：向运行中守护进程的 [rpc] 入口提交 codec 编码交易
cargo run --release --bin node -- encode-tx --key-file F --out F --author N …  # M56：离线装配+签名一条交易到 submit-tx 文件
cargo run --release --bin node -- status --dir DIR # 重放区块日志并打印状态
cargo test --release                               # 413 项单元测试（见下）
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

要点：`src/daemon.rs` 单属主 actor（`GossipNode` 独占一个 task、per-peer mpsc 出站、无 `Arc<Mutex>`），现在**也独占**一对 `Keypair`（follower 为 `None`）+ `Option<RoundState>` + tokio timer 句柄；帧 = `u32` BE 长度前缀 + `encode_gossip`，加 `MAX_FRAME = 16 MiB` 上限（阻塞版 `read_msg` 无上限）；8 字节 BE id 握手在 `GossipMsg` 之外（wire tag 0..=15 范围不动，新增 `TAG_CONSENSUS=16` 装 `GossipMsg::Consensus(Box<round::Msg>)`，proposal 字节 = 编码后 `Block`，接收端哈希与 proposer 端哈希逐字节相同）；只向 id 更大的 peer 拨号 → 每对恰一条连接；actor 是本节点日志唯一写者（任何命令后 `append node.blocks()[appended..]`），boot 经 `load_certified` 复验最终性恢复；纯 `GossipNode::on_message` 不收共识消息——它既不知道本节点的密钥，也没法触达 tokio 定时器；共识消息在 actor 主循环里被 `Cmd::Inbound` 直接路由到 `RoundState::on_message`/`on_timeout`。**同步永远赢**：验证人只对 `node.height()+1` 跑共识，`reconcile_after_sync()` 在任何高度推进之后立即弃旧 round + arm 下一高度——anti-entropy 永远优先于尚未决的 round。**Byzantine-proposer 活性保护**：`on_consensus` 在 prevote 之前用 `Chain::would_accept` 试跑 apply，把不能 apply 的 proposal 当成"proposer 缺席"处理（→ prevote nil → 下一 honest proposer）。**空块心跳**：每 `BLOCK_INTERVAL=1000ms` 由 `build_candidate` 出一空 sealed block 推进高度（**M35 起可经 `[consensus] create_empty_blocks=false` 关掉，见下**）。超时常量 `PROPOSE/PREVOTE/PRECOMMIT_TIMEOUT_BASE=1000ms` + `TIMEOUT_DELTA=500ms`（每 round 线性回退，落入最终同步性；**M35 起这些是 `[consensus]` 的可配默认值**）；4 等权验证人 quorum=3，所以 3-of-4 持续推进、2-of-4 安全停摆。配置在 `src/config.rs`，serde + toml **镜像结构**转换成引擎类型，共识核心 `lib.rs` 仍 serde-free。**依赖变化**：node 自 M32 起引入 `tokio`/`serde`/`toml`，M37 起再加 `tracing`/`tracing-subscriber`（结构化日志）（引擎仍纯 std 零依赖）。

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

本里程碑只交付这一薄片；其余运维成熟度项（`tracing` 结构化日志、metrics/health endpoint、peer discovery / address gossip、TLS/auth）中的**结构化日志**由 M37 接手（见下），余下再次顺延至 M38+。

## 守护进程结构化日志（`tracing`）（Milestone 37）

到 M36 为止守护进程的全部诊断都走 `daemon.rs` 里 **5 处 `eprintln!`**——手拼的 `[node {id}] …` 字符串，无级别、无过滤、无机读字段；运维想调详略必须重编译，无法按 node/peer/height 过滤，且对高价值生命周期事件（peer 连接/断开、区块落定）**零日志**。M37 取运维篮子里最小、最自包含的一片：让守护进程接入 `tracing` 门面，把 5 处 `eprintln!` 换成**带级别、带结构化字段**（node id / peer / height / error 作 typed field）的事件，并补几处今天完全没日志的生命周期事件；日志级别由 **`RUST_LOG` 环境变量**驱动（无新增配置段）。不改 wire/共识/配置文件/持久化。默认级别（`info`）与输出流（stderr）与旧行为一致，故 `localnet` 仍逐字节收敛到同一 head `44309755…ea04ba`。

```bash
RUST_LOG=info  cargo run --release --bin node -- localnet   # 默认可见度：listening / peer connected 等 INFO 结构化行
RUST_LOG=debug cargo run --release --bin node -- localnet   # 额外显示 "block committed" DEBUG 事件（4 节点 ×3 块 = 12）
RUST_LOG=warn  cargo run --release --bin node -- localnet   # 静默 INFO，仅 warn/error
```

要点：

- **依赖**：node crate 新增 `tracing` + `tracing-subscriber`（`env-filter` feature）；`engine` 仍零依赖。
- **级别映射**：append block/cert failed → `error!`；accept error → `warn!`；listening / peer connected / peer disconnected / shutdown → `info!`；block committed → `debug!`。变量全部作结构化字段（`node`、`peer`、`height`、`addr`、`error`），消息体是静态串。
- **订阅器**：`daemon::init_tracing()` 装一个 fmt 订阅器（写 **stderr**，与旧 `eprintln!` 同流，故 `status`/`certs` 的 stdout 输出不受污染），`EnvFilter` 读 `RUST_LOG`、缺省 `info`；`try_init` 吞错 → **幂等**（`cmd_run`/`cmd_localnet` 各调一次、或测试已设全局默认，均不 panic）。在 `main.rs` 的 `cmd_run` / `cmd_localnet` 首行调用。
- **无逐字节行为漂移**：`tracing` 宏在无订阅器时是 no-op（`cargo test` 下不装订阅器），故既有 273 测试逐字节不变、无新逻辑测试（结构化日志无纯逻辑可测，真正断言需全局 fmt 订阅器污染整进程或引入 `tracing-test` 依赖，均不划算）；`localnet` head 不变即为守护进程路径行为一致的守卫。
- **CLI demo 输出不动**：`main.rs` 里约 20 个内存态 demo 子命令的 `println!` 是面向用户的输出，**故意不转**。

其余运维成熟度项（`[logging]` 配置段、metrics/health endpoint、peer discovery / address gossip、TLS/auth、把 tracing span 贯穿共识轮次）**顺延至 M38+**，其中 **metrics/health endpoint** 由 M38 接手（见下）。

## 指标 / 健康端点（`[metrics]` + Prometheus）（Milestone 38）

到 M37 为止守护进程的唯一可观测面是 M37 的 `tracing` 日志流——适合人盯 stderr，却无法被 scraper 或负载均衡器的健康检查消费：没有任何端点能在不 attach 进程的情况下回答"这个节点活着吗、在哪个高度、几个 peer、是不是验证人、mempool 多深"。M38 取运维篮子里下一片：一个**默认关闭、只读**的指标 / 健康端点。新增 `[metrics]` TOML 段（`enabled` 默认 `false`）让守护进程再绑一个 TCP 监听器，用**手拼的极简 HTTP/1.1** 应答任意 `GET`，body 是 **Prometheus 文本曝露格式**（同时充当健康检查：`200 OK` ⇒ 活着）。**无新增依赖**（约 15 行手写 responder），不改 wire/共识/持久化。因为该段默认关闭，所有既有 `testnet/*.toml` 与 `localnet` demo 行为**逐字节不变**（`localnet` 仍收敛到同一 head `44309755…ea04ba`）。

```bash
# 在某份 testnet 配置里打开端点：
#   [metrics]
#   enabled = true
#   listen  = "127.0.0.1:9600"
cargo run --release --bin node -- run --config node21.toml &
curl -s http://127.0.0.1:9600/metrics     # 打印 zhixing_* gauge；返回 200 即健康
```

要点：

- **配置（`config.rs`）**：`MetricsConfig { enabled: bool (默认 false), listen: String (默认 "127.0.0.1:9600") }`，`#[serde(default)]` + 手写 `Default` + `listen_addr()`（走私有 `parse_addr`）；`NodeConfig` 加 `#[serde(default)] metrics: Option<MetricsConfig>`——缺段 ⇒ `None` ⇒ **永不绑定**（后向兼容），镜像 `[validator]` 的 opt-in 形态。裸 `[metrics]\nenabled = true` ⇒ `Some`、`listen` 回落默认。
- **快照（`daemon.rs`）**：`Cmd::Metrics(oneshot::Sender<Metrics>)` 沿用 `Cmd::Query` 的 actor 查询模式，`Metrics` 从 `actor.node.*` / `outbound.len()`（peer 数）/ `kp.is_some()`（验证人角色）/ `cons.is_some()`（有轮次在飞）一次性组装，经 `Node::metrics()` 句柄取回——只读，不能改共识/链状态。
- **渲染**：纯函数 `render_prometheus(&Metrics) -> String`，每 gauge 一对 `# HELP`/`# TYPE … gauge` + 值：`zhixing_height`、`zhixing_peers_connected`、`zhixing_is_validator`（0/1）、`zhixing_consensus_active`（0/1）、`zhixing_mempool_txs`、`zhixing_pending_stake_ops`、`zhixing_pending_evidence`，外加 `zhixing_head_info{head="<hex>"} 1`（head 哈希作 info-gauge 标签）。纯逻辑 ⇒ 有真单测。
- **监听**：`run_metrics` accept 循环（accept 错 `warn!`，镜像 `run_listener`）+ `serve_metrics_conn`（有界 best-effort 丢弃请求头 → 发 `Cmd::Metrics` → 写定形 `HTTP/1.1 200 OK` + `Content-Type: text/plain; version=0.0.4` + `Content-Length` + `Connection: close`）；任意路径都回指标，故裸 `GET /` 也是健康探针。`Node::start` 在心跳 spawn 后按 `cfg.metrics.enabled` 门控绑定；坏/占用的 `listen` 与 p2p 监听器一样 fail-fast（不是静默 no-op）。
- **测试（+7 → 280）**：`config.rs` 三个（默认关闭 / 缺段为 `None` / `enabled=true` 打开）；`daemon.rs` 四个（`render_prometheus` 全 gauge、role/head 编码、`Node::metrics()` 快照、`[metrics]` 打开后经真实 TCP 抓到 `200 OK` + `zhixing_height`）。

其余运维成熟度项（`[logging]` 配置段、TLS/auth、更丰富的指标（直方图 / 每-peer 计数 / 轮次时延）与 push exporter、把 tracing span 贯穿共识轮次）**顺延至 M39+**，其中 **peer discovery / address gossip** 由 M39 接手（见下）。

## Peer 发现 / 地址簿 gossip（`[network] enable_peer_exchange`）（Milestone 39）

到 M38 为止守护进程的 peer 集是**静态**的：`Node::start` 读 `[[peers]]` 表，为每个 id 更大的配置 peer spawn 一个 `run_connector`——节点只能连运维手列的 peer，无从得知未在文件里的 peer。这让真实组网很痛（每个节点开机就得拿到完整名册），新加的验证人对所有配置早于它的节点**不可见**。对共识更糟：`broadcast_consensus` 只把票发给**直连邻居**（纯 `GossipNode` 丢弃 `Consensus`、从不转发），故图不完全时某些验证人永远凑不齐 quorum——**没铺满的静态名册会静默停摆**。M39 取运维篮子里下一片：**peer 发现 / 地址簿 gossip**。`[[peers]]` 变成**种子 / bootstrap** 集，节点间经 gossip 交换一份小**地址簿**（`(node_id, listen_addr)` 对）；学到新的、id 更大的 peer 就**自动拨号**，于是**连通但不完全**的种子拓扑会**自补成全网状**。经新增 `[network] enable_peer_exchange` 开关可关（默认 **true**）。不改共识 / 持久化，纯引擎核保持纯。

**后向兼容护栏**：既有 `testnet/*.toml` 与 `localnet` demo 本就把每个别的节点都列为 peer（全网状——`main.rs`），故发现找不到新东西、行为**逐字节不变**（`localnet` 仍收敛到同一 head `44309755…ea04ba`）。

要点：

- **wire（`net.rs`）**：新增 `GossipMsg::Peers(Vec<(u64, String)>)` 地址簿 + `TAG_PEERS=17`（dense 0..=17）+ `MAX_PEERS=1024`（codec 上限，防敌意 peer 发无界簿 → `CodecError::TooManyItems`，复用现成错误）；`encode_gossip`/`decode_gossip` 各加一支（`u32` 计数 + 每项 `u64` id + `u32` 长度 + utf8 地址字节；解码 `from_utf8_lossy` 免加 CodecError 变体）；两个 `on_message` 核（全 / 光）各加 `GossipMsg::Peers(_) => Vec::new()` 丢弃支——**Actor 独占发现逻辑、纯核保持纯且无 I/O**（沿用 M33 `Consensus` 的先例）。
- **配置（`config.rs`）**：`NetworkConfig` 加 `enable_peer_exchange: bool`，`Default` 置 **true**；两级 `#[serde(default)]` 已覆盖缺省。`false` ⇒ 节点钉死在静态 `[[peers]]` 种子集（无发现）。
- **Actor（`daemon.rs`）**：新增字段 `addrs: HashMap<u64,String>`（id→listen，**first-wins**：配置 / 自身地址权威、不被 peer 声称覆盖）、`dialing: HashSet<u64>`（已 spawn 连接器的 id，去重防拨号风暴）、`peer_exchange: bool`。`Node::start` 用「自身 + 每个配置 peer」种下 `addrs`，用「id 更大的配置 peer」种下 `dialing`（boot 连接器已覆盖、别重拨）。三个 helper：`peers_msg()`（把 `addrs`——含自身 `(id, listen)`——打成 `Peers`，让邻居无需改握手即可学到怎么拨我们）、`gossip_peers()`（发 `peers_msg` 给全部 outbound，`peer_exchange` 门控）、`on_peers(book)`（对每项 `or_insert` 存地址；若 `id > my_id && !dialing.contains(id)` 且地址可 `parse::<SocketAddr>()` → `dialing.insert` + `tokio::spawn(run_connector(..))`，保持「只拨 id 更大」不变量：低 id 侧从同一 gossip 学到我们的地址来拨我们）。
- **`run_actor` 三处挂钩**：`Cmd::Register` 在插入 outbound 后向新 peer 发 `peers_msg()`（与既有 `Status` 一同 kick 发现）；`Cmd::Inbound` 在调 `on_message` 前把 `GossipMsg::Peers(book) => on_peers(book)` 剥出（紧挨 `Consensus` 拦截）；`Cmd::Announce` 在 `broadcast_status()` 后调 `gossip_peers()`（周期再传播 → 传递式补全）。
- **测试（+4 → 284）**：`net.rs` 一个（`Peers` 簿 encode→decode 往返 + 空簿 + 超 `MAX_PEERS` → `TooManyItems`）；`config.rs` 一个（`enable_peer_exchange=false` 解析，另扩 M36 默认断言测试加一条）；`daemon.rs` 两个（链拓扑 21-22-23 开发现后节点 21 peer 数达 2 = 拨到从未在其配置里的 23；同拓扑关发现则钉在 1 peer）。

其余运维成熟度项（`[logging]` 配置段、显式 `advertise_addr` + NAT 穿透、peer 驱逐 / staleness TTL、更丰富的指标与 push exporter）**顺延至 M40+**，其中 **TLS / peer 认证** 由 M40 接手（见下）。

## 认证握手 / peer 认证（`[network] require_peer_auth`）（Milestone 40）

M39 让 peer 集**自扩张**：节点会自动拨号任何从地址簿 gossip 学到的 `(id, addr)`。这也是新的攻击面——到 M39 为止的握手（`write_hello`/`read_hello`）只在明文里交换一个 **8 字节 node id**，没有任何东西把「声称的 id」绑到密码学身份：一个 peer 可以**声称**自己是验证人 22 而并不持有 22 的密钥，而一条恶意地址簿项 `(22, <攻击者地址>)` 会让诚实节点拨向攻击者、以为那是验证人 22。因共识票会泛洪给任何完成握手的对端，一个未认证的冒名者就直接坐在了投票路径上。M40 取运维篮子里下一片：**peer 认证**——一次**双向认证**握手，每一侧都证明自己持有 **genesis** 为其所声称验证人 id 绑定的那把 ed25519 私钥，且证明覆盖一个**每会话新鲜挑战**（防重放）。**注意范围**：本片只认证**身份**、不加密传输（真正的 TLS/rustls 仍顺延后续片）——复用树中已有的、经审计的 `ed25519-dalek`，无新增加密实现。

**后向兼容护栏**：门控于新增 `[network] require_peer_auth` 开关，默认 **false**。关时握手就是 M40 之前那套明文 8 字节 id 交换 ⇒ `testnet/*.toml` 与 `localnet` **逐字节不变**（`localnet` 仍收敛到同一 head `44309755…ea04ba`），M34 的裸 TCP 双签注入测试也不受影响。开时两侧走认证握手；这是**全网策略**（开 auth 的节点不会与关 auth 的节点完成握手）。

要点：

- **可克隆密钥（`crypto.rs`）**：`Keypair` 派生 `Clone`（内层 `SigningKey` 本就是 `Clone`）——让 `Node::start` 把一把签名克隆交给认证上下文，而共识 actor 保留自己那把用于投票。
- **纯认证核（`daemon.rs`，可脱离 socket 单测）**：`const AUTH_DOMAIN = b"zhixing-node-auth-v1"`（域分隔，令认证签名永不可当作共识票 / 交易签名重放）；`auth_transcript(signer_id, signer_nonce, peer_id, peer_nonce)` = `AUTH_DOMAIN || signer_id(8 BE) || signer_nonce || peer_id(8 BE) || peer_nonce`（**双方各出一个新鲜 nonce**，故截获的 `(nonce, sig)` 不可重放、中继也无法拼接两次会话）；`struct AuthContext { my_id, kp: Option<Keypair>, validators: HashMap<u64,PubKey>, require }`（`Node::start` 建一次、`Arc` 共享）。
- **认证握手 wire（`daemon.rs`）**：两条定长消息、全 `read_exact`（不走 `GossipMsg` 分帧、wire tags 不动）——`HelloInit = id(8) || pubkey(32) || nonce(32)`（72 B）、`HelloAuth = sig(64)`。`auth_handshake(rd, wr, ctx) -> io::Result<u64>` 两侧对称：各发 `HelloInit` → 各签绑定双 nonce 的 transcript 并发 `HelloAuth` → **验证**对端：查 `ctx.validators.get(&peer_id)`，须存在、其 pubkey 须等于 genesis 绑定值、且签名对 transcript 有效；否则 `Err`（丢连接）。非 genesis（follower）id 在严格模式一律拒（follower 认证顺延）。两侧皆先写后读、载荷微小 ⇒ 不死锁。
- **接入连接路径（`daemon.rs`）**：`handle_conn`/`run_listener`/`run_connector` 均改收 `Arc<AuthContext>`；`handle_conn` 按 `ctx.require` 二选一（开 ⇒ `auth_handshake`；关 ⇒ 逐字节等于旧 `write_hello`/`read_hello`），其后（`Register`/reader/writer）完全不变。`Actor` 新增 `auth: Arc<AuthContext>` 字段，M39 发现路径的自动拨号 `run_connector` 也传该 `Arc`（发现来的 peer 一样认证）。`Node::start` 从 `genesis.validators` 建 id→pubkey 映射、`require_peer_auth && validator_key.is_none()` 时 **fail-fast**（无密钥的 follower 无法证明自身身份 ⇒ 拒绝启动，镜像既有 pubkey-mismatch 检查）、把密钥克隆进 `AuthContext` 后再把原件交给 `Actor`。
- **配置（`config.rs`）**：`NetworkConfig` 加 `require_peer_auth: bool`，`Default` 置 **false**；两级 `#[serde(default)]` 覆盖缺省。
- **依赖（`Cargo.toml`）**：把 `getrandom = "0.2"` 提为直接依赖（lock 树里本就有 0.2.17，经 ed25519-dalek 传递引入 ⇒ 无新增编译单元），用于握手的每会话 nonce。引擎 crate 不动（仍零依赖）。
- **测试（+5 → 289）**：`daemon.rs` 四个（`auth_transcript` 确定性 + 角色顺序敏感 + 域前缀；`auth_transcript` 签名往返 + 错密钥验签失败；三验证人开 `require_peer_auth` 经真实 socket 完成认证握手并收敛；冒名者声称 id 22 却持非 genesis 密钥 → 诚实节点 peer 数钉在 0）；`config.rs` 一个（`require_peer_auth` 默认关 + `=true` 解析，另扩 M36 默认断言测试加一条）。

其余运维成熟度项（follower 认证（临时密钥 / TOFU）、超出 genesis-key 检查的「拨到地址↔id」绑定、`[logging]` 配置段、显式 `advertise_addr` + NAT 穿透、更丰富的指标（认证失败计数）与 push exporter）**顺延至 M41+**，其中**真正的传输加密 / TLS（rustls）** 由 M41 接手（见下）。

## 传输加密 / TLS（`[network] enable_tls`）（Milestone 41）

M40 让链路**可认证**（对端证明其所声称验证人 id 的 genesis 密钥），但线上字节仍是**明文**——每条分帧 `GossipMsg`（共识投票、交易、认证块、地址簿）任何在途者都可读、可篡改。M41 补上这一层：**opt-in 的 TLS 1.3 传输加密**（经 `tokio-rustls`）。

本切片的范围（用户选定）是**仅加密**：每个节点出示一张**临时自签证书**、并**接受任意对端证书**；TLS 给出针对被动窃听者的机密性 + 完整性，而**认证仍是 M40 的职责**（`auth_handshake` 跑在 TLS 隧道**内部**）。两层正交组合：`enable_tls` + `require_peer_auth` = 一张**加密且认证**的网。

**后向兼容护栏**：门控于新增 `[network] enable_tls` 开关，默认 **false**。关时就是 M41 之前那套裸 TCP 路径 ⇒ `testnet/*.toml` 与 `localnet` **逐字节不变**（`localnet` 仍收敛到同一 head `44309755…ea04ba`）。**全网策略**（TLS 节点与明文节点无法握手），与 `require_peer_auth` 同形。

- **流类型统一（`daemon.rs`）**：新增 `trait PeerStream: AsyncRead + AsyncWrite + Unpin + Send`（毯覆盖 impl），`handle_conn` 改收 `Box<dyn PeerStream>` 并用 `tokio::io::split` 替换 `TcpStream::into_split`——裸 TCP 流、服务端 / 客户端 TLS 流遂共用同一条**非泛型**代码路径，其后的握手与分帧（本就泛型于 `AsyncReadExt`/`AsyncWriteExt`）**逐字节不变**。
- **接入连接路径（`daemon.rs`）**：`AuthContext` 新增 `tls: Option<TlsSetup>`（`TlsSetup{acceptor, connector}`，均为 `tokio-rustls` 内部 `Arc` 支撑的廉价克隆）。`run_listener`/`run_connector` 先在裸 `TcpStream` 上 `set_nodelay`（TLS 包裹后不再可达），再经 `server_wrap`/`client_wrap` 包成 `Box<dyn PeerStream>`（关 ⇒ `Box::new(tcp)` 逐字节等价；开 ⇒ `acceptor.accept` / `connector.connect`）。TLS accept 放进 spawn 出的任务里做，慢 / 恶意握手不阻塞 accept 循环；TLS dial 失败按死址退避。
- **TLS 构建（`daemon.rs`）**：`build_tls_setup()` 装 ring provider（幂等，多节点 in-process 二次装忽略 Err）、经 `rcgen` 生成临时自签证书 + 密钥装服务端 `ServerConfig`、客户端用 `AcceptAnyServerCert`（实现 `ServerCertVerifier`，一律放行——仅加密无 PKI）装 `ClientConfig`。`Node::start` 仅当 `enable_tls` 时构建（仅加密无需密钥材料 ⇒ 无 fail-fast，follower 亦可用），塞进 `AuthContext`。指标端点保持明文 HTTP（Prometheus 抓取惯例）。
- **配置（`config.rs`）**：`NetworkConfig` 加 `enable_tls: bool`，`Default` 置 **false**；两级 `#[serde(default)]` 覆盖缺省。
- **依赖（`Cargo.toml`）**：加 `tokio-rustls` + `rcgen`，二者均钉在 `ring` 后端（非默认 aws-lc-rs）以免引入 cmake/NASM 的 C 工具链构建依赖；node crate 专属，引擎仍零依赖。
- **测试（+4 → 293）**：`config.rs` 一个（`enable_tls` 默认关 + `=true` 解析，另扩 M36 默认断言测试加一条）；`daemon.rs` 三个（三验证人开 `enable_tls` 经真实 socket 收敛；`enable_tls` + `require_peer_auth` 双开仍收敛——证明加密 + 认证组合；明文裸 TCP 拨号者无法加入 TLS 节点 → peer 数钉在 0）。

**已知边界（顺延至 M42+）**：仅加密的 accept-any TLS 不认证服务端，故一个**终结两端 TLS 的主动 MITM** 仍可转发内层 M40 握手（尚无信道绑定）；关掉这条（把 TLS keying-material exporter 混入 M40 transcript）、以及把证书绑定到 genesis ed25519 密钥的完整 mTLS，都是后续独立切片。

## 信道绑定 / channel binding（`[network] bind_channel`）（Milestone 42）

M41 的 accept-any TLS 只给机密性、不认证服务端，故其显式「已知边界」是：一个**终结两端 TLS 的主动 MITM** 能把内层 M40 认证握手透明转发——两个真端点都持真 genesis 密钥、签名照样通过，攻击者就坐在两条各自独立的 TLS 隧道中间读写明文。M42 关掉这条：**信道绑定**。

把每条 TLS 连接的 **keying-material exporter**（RFC 5705 / RFC 8446 §7.5）混进两端都要签名的 M40 auth transcript。MITM 的两条 TLS 腿是**不同的 TLS 会话**，导出**不同**的 exporter 值——真端点对 `… ‖ 自己这条腿的 exporter` 签的名，无法在另一真端点用 `… ‖ 另一条腿的 exporter` 重建的 transcript 上验过；转发即断。诚实直连的对端共享**同一条** TLS 会话 ⇒ 同一 exporter ⇒ 仍逐字节一致。

**后向兼容护栏**：门控于新增 `[network] bind_channel` 开关，默认 **false**。关时 `auth_transcript(…, None)` ⇒ 签名字节与 M40/M41 **逐字节相同** ⇒ `testnet/*.toml` 与 `localnet` 不变（`localnet` 仍收敛同一 head `44309755…ea04ba`）。开时为**全网策略**：绑定与未绑定的节点产出不同 transcript、彼此认证失败（与混合 `require_peer_auth` 同形）。

- **transcript（`daemon.rs`）**：`auth_transcript` 末尾新增 `channel_binding: Option<&[u8; 32]>` 参数——`Some` 追加 32 字节 exporter、`None` 保持 M40 布局。新增域分隔常量 `CHANNEL_BINDING_LABEL = b"zhixing-node-channel-binding-v1"`。
- **导出（`daemon.rs`）**：`server_wrap`/`client_wrap` 在 TLS 握手 await 完成后，若 `bind_channel` 则经 `stream.get_ref().1.export_keying_material(…)`（泛型辅助 `export_channel_binding` 覆盖服务端 / 客户端两种连接）取 32 字节，随流一并返回 `(Box<dyn PeerStream>, Option<[u8;32]>)`。TLS 关 ⇒ `None`。
- **接入握手（`daemon.rs`）**：`handle_conn` 多收一个 `binding: Option<[u8;32]>` 传给 `auth_handshake`，后者仅当 `ctx.bind_channel` 时把它折进签名与验证两处 transcript；`bind_channel` 却无 binding（无 TLS）则 `Err`（防御性，正常由启动 fail-fast 拦下）。`AuthContext` 加 `bind_channel: bool`。
- **fail-fast（`daemon.rs`）**：`Node::start` 在 `bind_channel && (!enable_tls || !require_peer_auth)` 时拒启（信道绑定既要有 TLS 信道可绑、也要有认证握手可绑入），镜像既有的 `require_peer_auth`/pubkey fail-fast。
- **配置（`config.rs`）**：`NetworkConfig` 加 `bind_channel: bool`，`Default` 置 **false**；两级 `#[serde(default)]` 覆盖缺省。
- **依赖**：无新增——复用 M41 的 `tokio-rustls`/`rustls` 栈（exporter API 是 `rustls::ConnectionCommon::export_keying_material`，已在 0.23.45 公开）。
- **测试（+5 → 298）**：`config.rs` 一个（`bind_channel` 默认关 + `=true` 解析，另扩 M36 默认断言）；`daemon.rs` 四个（`auth_transcript` 绑定进签名字节：`None` 逐字节等于 M40 布局、`Some` 追加 32B、不同绑定不同字节；绑定不匹配拒握手——A 腿签名对 B 腿 transcript 验签失败、同腿则通过，即 MITM 转发防御的密码学层证明；三验证人 `enable_tls`+`require_peer_auth`+`bind_channel` 全开经真实 socket 收敛；`bind_channel` 无 TLS 启动 fail-fast）。

**已知边界（顺延至 M43+）**：信道绑定挫败的是**转发**，而非本身即 genesis 验证人的 MITM；且它前提是 TLS 已开。完整的服务端认证（把证书绑定到 genesis ed25519 密钥的 mTLS）仍是后续更重的切片。

## 创世锚定 mTLS / genesis-pinned mTLS（`[network] require_peer_certs`）（Milestone 43）

M42 的信道绑定挫败了**转发型 MITM**，但显式「已知边界」是：TLS 层本身仍不认证任何人——任何主机都能完成 accept-any TLS 握手，绑定只在隧道**内层**（M40 应用握手）拦人。M43 把认证下沉到 TLS 层本身：**创世锚定的双向 TLS**——每个节点把**自己的 genesis ed25519 密钥**当作 TLS 凭据出示，对端仅当出示的密钥属于**创世验证人集**时才接受这条 TLS 连接，**双向**皆然。非验证人（冒名者 / 本身不是创世验证人的 MITM）连 TLS 隧道都建不起来，把 M41/M42 的边界收在 TLS 层、而非仅隧道内层。

用 **RFC 7250 raw public key**（裸公钥，无 X.509）：TLS 凭据**就是** SubjectPublicKeyInfo（SPKI），无需证书解析——ed25519 SPKI 是定长 44 字节（12B 前缀 ‖ 32B 密钥），密钥位置固定、提取无歧义。

**后向兼容护栏**：门控于新增 `[network] require_peer_certs` 开关，默认 **false**。关时 `build_tls_setup(None)` ⇒ 与 M41 加密-only 路径**逐字节相同** ⇒ `testnet/*.toml` 与 `localnet` 不变（`localnet` 仍收敛同一 head `44309755…ea04ba`）。开时为**全网策略**：mTLS 与非-mTLS 节点握手失败（与混合 `enable_tls` 同形），且无密钥的纯跟随者无法出示创世凭据、遂无法加入（故 mTLS 把网络收缩到创世验证人）。

- **密钥（`crypto.rs`）**：`Keypair::secret_seed()` 暴露 32 字节 ed25519 种子（与 `from_seed` 往返），供派生 TLS 凭据的 PKCS#8。
- **凭据（`daemon.rs`）**：从种子拼 PKCS#8 v1（16B 固定头 ‖ 32B 种子）→ `rustls::crypto::ring::sign::any_eddsa_type` → 签名器 `public_key()` 的 SPKI（末 32 字节正是本节点 genesis pubkey）→ `CertifiedKey` + `AlwaysResolves{Server,Client}RawPublicKeys` 双向出示。
- **验签（`daemon.rs`）**：单一 `GenesisPinnedVerifier` 同时实现 `ServerCertVerifier`（拨方验听方）与 `ClientCertVerifier`（听方验拨方），两者 `requires_raw_public_keys()→true`：`spki_to_ed25519` 从 SPKI 取出密钥、拒坏编码，再要求密钥 ∈ 创世验证人集；`verify_tls13_signature` 委托 `rustls::crypto::verify_tls13_signature_with_raw_key` 证明对端持私钥。仅 TLS 1.3（`builder_with_protocol_versions(&[&TLS13])`），故 `verify_tls12_signature` 防御性返回 `Err`。
- **fail-fast（`daemon.rs`）**：`Node::start` 在 `require_peer_certs && !enable_tls`、及 `require_peer_certs && validator_key.is_none()` 时拒启（mTLS 既要 TLS 信道、也要一把验证人密钥出示凭据），镜像 M42 的 `bind_channel` fail-fast。
- **配置（`config.rs`）**：`NetworkConfig` 加 `require_peer_certs: bool`，`Default` 置 **false**；`require_peer_certs`+`require_peer_auth`+`bind_channel` 可同开（加密 + TLS 认证 + 应用认证 + 信道绑定）。
- **依赖**：无新增——复用 M41 的 `tokio-rustls`/`rustls` 0.23.45 栈（裸公钥支持已在其中，经 `tokio_rustls::rustls` 触达）。
- **测试（+7 → 305）**：`crypto.rs` 一个（`secret_seed` 往返，派生 pubkey 一致）；`config.rs` 一个（`require_peer_certs` 默认关 + `=true` 解析 + 扩默认断言）；`daemon.rs` 五个（SPKI/PKCS8 派生回到 genesis pubkey + 拒坏编码/坏长度；`GenesisPinnedVerifier` 只认创世集、双向皆然；三验证人 mTLS 全开经真实 socket 收敛；纯-TLS 拨方被 mTLS 节点拒于门外、peers 恒 0；`require_peer_certs` 无 TLS 启动 fail-fast）。

**已知边界（顺延至 M44+）**：mTLS 在 TLS 层认证「对端是创世验证人」，但不单独绑定**是哪一个** id——id 绑定仍由 M40 内层握手（`require_peer_auth`）提供，二者组合。证书/密钥轮换与落盘持久化（当前用静态 genesis 密钥）、metrics 端点 TLS、显式 `advertise_addr` + NAT 穿透、`[logging]` 配置段、更丰富的指标（握手失败计数 / 每-peer / 轮次时延直方图）与 push exporter 均顺延 M44+。

## 配置驱动日志 / `[logging]` config section（Milestone 44）

M37 让守护进程接入了 `tracing`，但订阅器是**写死**的：`init_tracing()` 固定输出到 **stderr**、`RUST_LOG` 过滤（缺省 `info`）、**text** 格式。运维无法在不设环境变量的前提下改级别，也拿不到机器可解析的 JSON。M43 篮子把 `[logging]` 配置段显式点名顺延；M44 收这一薄片：新增**可选** `[logging]` TOML 段，两个旋钮——`level`（`RUST_LOG` 未设时的缺省过滤指令）与 `format`（`text` 默认 | `json`）。硬性要求：**缺段 ⇒ 与 M37 逐字节相同**（`RUST_LOG` 过滤、`info` 回落、text、stderr）。镜像 M38 `MetricsConfig` 的 opt-in 段式与 M35 `ConsensusConfig` 的逐字段默认式。

**后向兼容护栏**：缺段 ⇒ `init_tracing_with(None)` ⇒ `unwrap_or_default()`（`level "info"`/`format "text"`）⇒ 逐字节等于 M37（`localnet` 仍走 text/stderr、head `44309755…ea04ba` 不变）。`level` 只替换**缺省**过滤——`RUST_LOG` 设置时仍胜出，与 M37 语义一致。

- **配置（`config.rs`）**：新增 `LoggingConfig{level(默认 "info"), format(默认 "text")}` + 手写 `Default`（复现 M37 订阅器）+ 段与字段两级 `#[serde(default)]`（缺段/部分段回落）；`NodeConfig.logging: Option<LoggingConfig>` opt-in，缺段 ⇒ `None`；`LoggingConfig::validate()` 在载入时拒未知 `format`（新 `ConfigError::BadLogFormat`），`load_node_config` 解析后调用。
- **订阅器（`daemon.rs`）**：`init_tracing()` 改为委托 `init_tracing_with(None)`（默认路径逐字节不变）；新 `init_tracing_with(Option<&LoggingConfig>)`——`unwrap_or_default` 取旋钮，`EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&level))` 保 `RUST_LOG` 优先，`format == "json"` 走 `fmt().json()`、否则 text；`try_init` 仍幂等。
- **接线（`main.rs`）**：`cmd_run` 改为**先载入配置、再** `init_tracing_with(cfg.logging.as_ref())`（配置错误经 `eprintln` 报告、非 tracing，故换序不丢日志）；`cmd_localnet`（无配置文件）保留 `init_tracing()` 默认路径。
- **依赖**：无新增 crate——仅给既有 `tracing-subscriber` 开 `json` feature（`["env-filter", "json"]`）。
- **测试（+3 → 308）**：`config.rs` 两个（`logging` 默认关 + 缺段为 `None` + `[logging]` 解析两旋钮 + 空段回落默认；`validate` 接受 text/json 拒 yaml + `load_node_config` 端到端拒坏 format）；`daemon.rs` 一个（`init_tracing`/`init_tracing_with` 幂等、text+json 两路径皆不 panic）。

**已知边界（顺延至 M45+）**：日志文件 / 轮转 / 非-stderr 目标、超出 `RUST_LOG`/`level` 的更细模块路由、OpenTelemetry / 结构化日志 exporter 均顺延 M45+（连同 M43 篮子余项：证书/密钥轮换与落盘、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 日志文件 + 轮转 / `[logging]` file target（Milestone 45）

M44 让日志配置化，但写入端仍**写死 stderr**（`init_tracing_with` 结尾 `.with_writer(std::io::stderr)`）。运维要持久化日志只能在进程外重定向 stderr，节点自己写不了滚动日志文件——这正是 M44 篮子点名的「日志文件 / 轮转 / 非-stderr 目标」。M45 收这一薄片：给既有 `[logging]` 段再加两个旋钮——`file`（路径；**空 ⇒ stderr**、即 M44/M37 行为）与 `rotation`（`daily` 默认 | `hourly` | `minutely` | `never`）。硬性要求延续 M44：**`file` 空 / 缺段 ⇒ 与 M44/M37 逐字节相同**。只支持**单目标**（文件**或** stderr，不同时），以保留扁平 `fmt()` builder——stderr+file 同时输出（tee）顺延 M46+。

**后向兼容护栏**：`file` 空 ⇒ `is_empty()` 为真 ⇒ 精确走 M44 的 stderr 分支 ⇒ 逐字节相同（`localnet` 无 `[logging]` 仍 text/stderr、head `44309755…ea04ba` 不变）。

- **依赖**：新增 `tracing-appender = "0.2"`（node crate 独有，引擎仍零依赖）。其 `RollingFileAppender` **直接**实现 `tracing-subscriber` 的 `MakeWriter`，故可径直喂给 `fmt().with_writer(appender)` 阻塞式写入——**无需** `non_blocking`/`WorkerGuard`，`init_tracing_with` 仍返回 `()` 且幂等（写入阻塞语义与旧 stderr 路径一致）。
- **配置（`config.rs`）**：`LoggingConfig` 增 `file(默认 "")` 与 `rotation(默认 "daily")` 两字段、更新 `Default`；`validate()` 除 `format` 外再拒未知 `rotation`（新 `ConfigError::BadLogRotation`）；`file` 为自由路径不校验存在性（父目录在 init 时创建）。段与字段两级 `#[serde(default)]` 不变。
- **订阅器（`daemon.rs`）**：`init_tracing_with` 先按 `file.is_empty()` 分写入端（`.with_writer` 改变 builder 类型，故先分支再于每臂内按 `format` 分 text/json；`filter` 恰好被移动进一臂）；新 `build_file_appender(file, rotation)` 把路径拆成父目录（`create_dir_all` best-effort）+ 文件名前缀，`parse_rotation` 把校验过的字符串映射到 `Rotation`（缺省 `DAILY` 防御）。`init_tracing()` 不变。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`，文件目标自动生效。
- **测试（+3 → 311）**：`config.rs` 两个（`file`/`rotation` 解析 + 默认（file 空/daily）+ 空段回落；`validate` 接受 daily/hourly/minutely/never 拒 weekly + `load_node_config` 端到端拒坏 rotation + M44 坏 format 仍拒）；`daemon.rs` 一个（`parse_rotation` 映射经 `Debug` 比对不依赖 `Rotation: PartialEq` + 走文件分支的 `init_tracing_with` 建滚动 appender、创建目录、安装不 panic）。

**已知边界（顺延至 M46+）**：多目标日志（stderr + file 同时、经分层 `Registry`/`MakeWriterExt`）、超出 `RUST_LOG`/`level` 的更细模块路由、OpenTelemetry / 结构化日志 exporter 均顺延 M46+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter、follower 认证）。

## 多目标日志 / stderr + file tee（Milestone 46）

M45 加了文件目标，但仍是**单目标**：`file` 一非空，日志就**只**进文件、stderr 空手（`init_tracing_with` 按 `file.is_empty()` 二选一）。运维想「既落盘归档、又在 console/`journalctl` 实时看」只能进程外 `tee`——这正是 M45 篮子点名的「多目标日志（stderr + file tee）」。M46 收这一薄片：给 `[logging]` 段再加一个旋钮 `stderr`（bool，默认 `false`）——配了 `file` 且 `stderr = true` 时，日志同时进**滚动文件与 stderr**。硬性要求延续 M44/M45：**旋钮缺省 ⇒ 与 M45/M44/M37 逐字节相同**。

**`stderr` 语义（默认 `false`）**：`file` 空 ⇒ stderr only（M37/M44，旋钮在此为 no-op，不能把唯一 sink 静音）；`file` 非空 + `stderr = false`（默认）⇒ file only（M45，逐字节相同）；`file` 非空 + `stderr = true` ⇒ **tee**（文件 + stderr，M46 新增）。故缺省/M45 老配置全部走既有单-sink 分支、逐字节不变，只有显式 `file=… stderr=true` 才进新代码。

- **配置（`config.rs`）**：`LoggingConfig` 增 `stderr: bool` 字段（默认 `false`）+ 更新 `Default`。字段级 `#[serde(default)]` 已覆盖两级默认；bool 不需 `validate`、无新 `ConfigError`。
- **订阅器（`daemon.rs`）**：`init_tracing_with` 由二路变三路——前两臂（stderr-only / file-only）**逐字保留**故字节不变，新增 `else if !lc.stderr` 门后的 tee 臂委托新私有 `init_tee(filter, json, file, rotation)`。`init_tee` 用分层 `Registry`：两个 `fmt::layer()`（一写 `std::io::stderr`、一写 `build_file_appender` 复用 M45）共享同一 `EnvFilter`（`.with(...).with(...).with(filter).try_init()`），json/text 两分支各具体类型免 `boxed`。`registry`/`fmt`/`ansi` 均 `tracing-subscriber` 默认 feature 已编入——**无新依赖、无新 feature**。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`，tee 自动生效。
- **测试（+2 → 313）**：`config.rs` 一个（`stderr` 默认 `false` + 解析 `file`+`stderr=true` + 空段回落 `false`）；`daemon.rs` 一个（tee 臂 `file`+`stderr=true` 建分层 Registry + 创建目录 + 安装不 panic，扩展 M45 文件分支覆盖到双写路径）。

**已知边界（顺延至 M47+）**：超出 `RUST_LOG`/`level` 的更细模块路由、OpenTelemetry / 结构化日志 exporter、每-sink 独立 filter/级别（当前 tee 共享单一全局 `EnvFilter`）均顺延 M47+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 每-sink 独立级别 / per-sink filter（Milestone 47）

M46 让 tee 同时写 stderr 与文件，但两 sink **共享**同一个全局 `EnvFilter`（`init_tee` 把单一 `filter` 挂到 `Registry` 上），无法表达「文件收 `debug` 归档、console 只看 `info`」这类常见运维诉求——M46 边界本身点名的「每-sink 独立 filter/级别」。M47 收这一薄片：给 `[logging]` 段再加两个旋钮 `stderr_level` / `file_level`（自由格式 `EnvFilter` directive 字符串，默认空）。硬性要求延续 M44–M46：**旋钮缺省 ⇒ 与 M46/M45/M44/M37 逐字节相同**。

**级别语义（默认空 ⇒ 继承 `level`）**：每-sink 级别只在 **tee**（`file` 非空 + `stderr = true`）里有意义，故只改 tee 臂、两条单-sink 臂逐字保留。tee 内：两旋钮都空（或 `RUST_LOG` 已设）⇒ 走 M46 的 `init_tee`（共享单一 filter，逐字节相同）；`RUST_LOG` 未设且至少一个旋钮非空 ⇒ 走新 `init_tee_leveled`，每个 sink 各带自己的 `EnvFilter`（空的一侧继承 `level`）。`RUST_LOG` 仍是全局覆盖：一旦设置就对**两 sink**生效、每-sink 级别让位。

- **配置（`config.rs`）**：`LoggingConfig` 增 `stderr_level: String` / `file_level: String`（默认 `""`）+ 更新 `Default`。与 `level` 一样是自由格式 directive、`EnvFilter` 解析有损，故**不改 `validate`**、无新 `ConfigError`；空 ⇒ 继承 `level`。
- **订阅器（`daemon.rs`）**：`init_tracing_with` 先算 `rust_log = EnvFilter::try_from_default_env()`（`Ok` ⇒ RUST_LOG 已设且有效）；两条单-sink 臂不变。tee 臂按 `per_sink = rust_log.is_err() && (!stderr_level.is_empty() || !file_level.is_empty())` 二分：`false` ⇒ M46 `init_tee`（逐字节相同）；`true` ⇒ 新私有 `init_tee_leveled`，两个 `fmt::layer()` 各 `.with_filter(EnvFilter)`（`EnvFilter` 实现 `Filter`，需 `use tracing_subscriber::Layer`），`build_file_appender`/`init_tee` 复用。**无新依赖、无新 feature**。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`。
- **测试（+2 → 315）**：`config.rs` 一个（`stderr_level`/`file_level` 默认 `""` + 解析 `stderr_level="info"`/`file_level="debug"` + 空段回落 `""`）；`daemon.rs` 一个（RUST_LOG 未设 + 每-sink 级别 ⇒ 建每-sink 分层 Registry + 创建目录 + 安装不 panic）。端到端已复验：`stderr_level="warn"` + `file_level="info"` 时启动的 `listening` INFO 行**只**进文件、不进 stderr。

**已知边界（顺延至 M48+）**：超出单条 directive 字符串的更丰富的每模块 directive **数组**式配置、OpenTelemetry / 结构化日志 exporter 均顺延 M48+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 每模块 directive 数组 / per-module directives（Milestone 48）

M44–M47 的过滤旋钮（`level` 与 M47 的 `stderr_level` / `file_level`）都是**单条字符串**，各自喂给 `EnvFilter::new(&s)`。多条 directive 今天只能把逗号塞进一条 TOML 字符串（`level = "info,tokio=warn,zhixing_node::daemon=debug"`）——两条尚可读，多了就别扭。M48 收 M47 边界点名的这一薄片：给三个标量旋钮各配一个 **数组**对应物 `levels` / `stderr_levels` / `file_levels`（`Vec<String>`，默认空），让运维用 TOML 擅长的数组形状写每模块 directive：

```toml
[logging]
levels = ["info", "tokio=warn", "zhixing_node::daemon=debug"]
```

**组合语义（数组非空即胜出）**：新增纯函数 `resolve_directive(s, array)`——数组里非空、去空白的条目用 `,` 连接后即结果（数组胜过标量）；数组全空 ⇒ 标量逐字返回。`EnvFilter::new("a,b,c")` 本就解析逗号分隔的 directive，故组合只是一次 join、无新 API。优先级新增一档：`RUST_LOG`（全局）> **数组** > 标量 > （空 ⇒ 继承 base `level`）。硬性要求延续 M44–M47：**数组缺省 ⇒ 与 M47/M46/M45/M44/M37 逐字节相同**。

- **配置（`config.rs`）**：`LoggingConfig` 增 `levels` / `stderr_levels` / `file_levels`（各 `Vec<String>`，默认 `vec![]`，紧随各自标量）+ 更新 `Default`。与标量 directive 一样自由格式、`EnvFilter` 解析有损，故**不改 `validate`**、无新 `ConfigError`；空 vec ⇒ 回落对应标量。
- **订阅器（`daemon.rs`）**：新增纯 helper `resolve_directive`（不碰 tracing 类型、可单测）；`init_tracing_with` 开头一次性 `resolve_directive` 出 `base`/`se`/`fe` 三条已组合字符串，穿进既有各臂——单-sink 臂 `EnvFilter::new(&base)`、tee 的 `per_sink` 门改看 `!se.is_empty() || !fe.is_empty()`（数组也能触发 leveled 路径）、M46 共享 tee 用 `&base`、M47 `init_tee_leveled` 的空侧继承 `&base`。`init_tee`/`init_tee_leveled`/`build_file_appender`/`parse_rotation` 复用未改。**无新依赖、无新 feature**。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`。
- **测试（+3 → 318）**：`config.rs` 一个（三个 vec 默认空 + 解析 `levels=["info","tokio=warn"]`/`file_levels=["debug"]` + 空段回落全空）；`daemon.rs` 两个（`resolve_directive` 纯语义：空数组⇒标量、非空⇒逗号连接且胜出、空白条目丢弃、全空白⇒标量；以及 `levels` 数组走单-sink 组合并安装不 panic）。端到端已复验：`stderr_levels=["warn"]` ⇒ `listening` INFO 行不进 stderr；`file_levels=["info","zhixing_node::daemon=debug"]` 连接后写文件。

**已知边界（顺延至 M49+）**：OpenTelemetry / 结构化日志 exporter、每-sink 独立 **format** 覆盖均顺延 M49+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 每-sink 独立 format / per-sink formatter（Milestone 49）

M46–M48 让 tee 同时写 stderr 与文件、各带自己的 `EnvFilter`（M47/M48），但 **formatter** 仍是共享的——`init_tee`/`init_tee_leveled` 都只收单一 `json` 布尔，故 tee 被迫两 sink 同格式。运维常见诉求恰是「console 看人类可读 text、文件归档机器可解析 **JSON**」（或反之）。M49 收 M48 边界点名的这一薄片：给 `[logging]` 段再加两个旋钮 `stderr_format` / `file_format`（`text` | `json`，默认空 ⇒ 继承 `format`），镜像 M47 的每-sink `stderr_level`/`file_level`。硬性要求延续 M44–M48：**旋钮缺省 ⇒ 与 M48/M47/M46/M45/M44/M37 逐字节相同**。

**格式语义（默认空 ⇒ 继承 `format`）**：每-sink format 只在 **tee**（`file` 非空 + `stderr = true`）里有意义，故只改 tee 臂、两条单-sink 臂逐字保留。format 与 `RUST_LOG` **正交**（`RUST_LOG` 只覆盖过滤、不动 formatter），故每-sink-format 门不看 `rust_log`。tee 内：无任何每-sink 旋钮（level 与 format 皆无）⇒ 走 M46 的 `init_tee`（共享单 filter + 单 format，逐字节相同）；否则走 `init_tee_leveled`，每个 sink 各带自己的 `EnvFilter` 与 formatter。

- **配置（`config.rs`）**：`LoggingConfig` 增 `stderr_format: String` / `file_format: String`（默认 `""`）+ 更新 `Default`。与 `format` 一样是枚举式旋钮，故 `validate` 扩展为拒未知每-sink format（非空且非 `text`/`json` ⇒ 复用 `ConfigError::BadLogFormat`；空 ⇒ 继承）。
- **订阅器（`daemon.rs`）**：tee 臂新增 `sjson`/`fjson`（空 ⇒ 继承 `json`）与 `per_sink_fmt` 门；`!per_sink_level && !per_sink_fmt` ⇒ M46 `init_tee`（逐字节相同），否则走 `init_tee_leveled`——其签名由单 `json` 改为 `stderr_json`/`file_json` 两布尔，内部 `match (stderr_json, file_json)` 枚举 2×2 格式组合（每臂各建两个具体类型的 `fmt::layer()`、`.json()` 挂到要 JSON 的 sink 上，**免 boxed**，延续 M46/M47 风格）。per-sink filter 由统一闭包 `mk` 构造（`RUST_LOG` 全局优先、否则 per-sink directive、否则 base）。**无新依赖、无新 feature**。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`。
- **测试（+2 → 320）**：`config.rs` 一个（`stderr_format`/`file_format` 默认 `""` + 解析 `stderr_format="text"`/`file_format="json"` + `validate` 接受空/拒未知 `yaml` + 空段回落 `""`）；`daemon.rs` 一个（tee + 每-sink format ⇒ 建 2×2 分层 Registry + 创建目录 + 安装不 panic）。端到端已复验：`stderr_format="text"` + `file_format="json"` 时同一 `listening` 事件在 stderr 为人类可读文本行、在文件为单行 JSON 对象。

**已知边界（顺延至 M50+）**：OpenTelemetry / 结构化日志 exporter、每-sink 独立 **rotation** 覆盖均顺延 M50+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 日志文件保留 / log-file retention（Milestone 50）

M45 给日志文件加了**轮转**（`daily`/`hourly`/`minutely`/`never`），但从不**清理**——长期运行的节点会把日志目录撑到无界，这是文件 sink 距离生产就绪就差的一块：磁盘有界。`tracing-appender` 0.2.5（早已是依赖、**无新 crate**）的 `RollingFileAppender::builder().max_log_files(n)` 正好只保留最近 `n` 个轮转文件、删最旧。M50 把 `[logging]` 段一个数字旋钮 `max_files` 接到它上。硬性要求延续 M44–M49：**旋钮缺省（`max_files = 0`）⇒ 与 M49/…/M37 逐字节相同**——`0` 走原 `RollingFileAppender::new` 路径原样保留，仅 `max_files > 0` 才走 builder。

**保留语义（默认 `0` ⇒ 无界）**：`max_files` 是纯计数，故不像枚举式的 `format`/`rotation`/每-sink-format 需要 `validate`（serde 在解析期就拒非整数/负值，`0` 是合法的「无界」哨兵）。只对 `file` 目标生效（`file` 空则忽略）；与 `rotation = "never"` 组合无害（单文件、无可清理）。

- **配置（`config.rs`）**：`LoggingConfig` 增 `max_files: usize`（默认 `0`）+ 更新 `Default`。`validate` **不改**（数字无枚举可拒）。
- **订阅器（`daemon.rs`）**：`build_file_appender` 加 `max_files: usize` 形参——`0` ⇒ 原 `RollingFileAppender::new`（逐字节相同）、`> 0` ⇒ `builder().rotation(..).filename_prefix(..).max_log_files(n).build(dir)`，builder 出错则 best-effort 回落 `::new`（延续 init 吞错风格）。`init_tee`/`init_tee_leveled` 各多收一个 `max_files` 透传；`init_tracing_with` 三处调用（单-file 臂、`init_tee`、`init_tee_leveled`）传 `lc.max_files`。`parse_rotation`/`resolve_directive` 复用未改。**无新依赖、无新 feature**。
- **接线（`main.rs`）**：无改动——`cmd_run` 早已 `init_tracing_with(cfg.logging.as_ref())`。
- **测试（+2 → 322）**：`config.rs` 一个（`max_files` 默认 `0` + 解析 `max_files = 7` + validate 恒 Ok + 空段回落 `0`）；`daemon.rs` 一个（`build_file_appender(path, "minutely", 3)` 走 builder 路径建有界 appender + 创建目录 + `init_tracing_with` 文件臂带 `max_files` 安装不 panic）。

**已知边界（顺延至 M51+）**：OpenTelemetry / 结构化日志 exporter、每-sink 独立 **rotation** 覆盖均顺延 M51+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、`advertise_addr` + NAT、更丰富指标 + push exporter）。

## 广告地址 / advertise_addr（Milestone 51）

M39 的 peer 发现把地址簿（`addrs: id → listen`）连同**自身** `(id, listen)` 一起 gossip，邻居据此回拨补全 mesh。但节点为自己广告的永远是**绑定**地址（`cfg.node.listen`，`daemon.rs` 处播种）——在 NAT / 端口映射 / `0.0.0.0` 通配绑定下，绑定地址并非外部可达地址，被发现的 peer 拨过去够不着，mesh 无法自补全。M51 加 `[network] advertise_addr`：一个可选的、对外可拨的公网地址，节点用它替代绑定地址来 gossip 自己。因 `peers_msg()` 本就读 `addrs`，只需改播种一处即可全链路传播。

**语义（默认空 ⇒ 广告绑定 `listen`）**：`advertise_addr` 非空时必须解析为 `SocketAddr`（复用 `parse_addr` → `ConfigError::BadAddr`），与 `on_peers` 的拨号路径一致（它只拨能 `parse::<SocketAddr>()` 的条目）——DNS 主机名在那里不可拨，故在加载期就拒，给出清晰错误而非静默失联。**只改广告的地址，监听仍绑 `cfg.node.listen`**。

- **配置（`config.rs`）**：`NetworkConfig` 增 `advertise_addr: String`（默认 `""`）+ 更新 `Default`；`load_node_config` 在 logging validate 之后加：非空则 `parse_addr(&cfg.network.advertise_addr)?`（无独立 `NetworkConfig::validate`，加载期校验对齐 logging 的做法）。
- **守护（`daemon.rs`）**：新增纯函数 `self_advertise_addr(listen, advertise) -> String`（空 ⇒ `listen`、非空 ⇒ `advertise`）；`Node::start` 的自地址播种改调它。下游 `peers_msg`/`gossip_peers`/`on_peers` 全不动。
- **接线（`main.rs`）**：无改动。
- **测试（+2 → 324）**：`config.rs` 一个（`advertise_addr` 默认 `""` + 解析 `203.0.113.7:9021` + `load_node_config` 拒非地址值 → `BadAddr`）；`daemon.rs` 一个（`self_advertise_addr` 空 ⇒ `listen` 逐字节、非空 ⇒ override）。

**已知边界（顺延至 M52+）**：OpenTelemetry / 结构化日志 exporter、每-sink 独立 **rotation** 覆盖均顺延 M52+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、更丰富指标 + push exporter）。

## 更丰富的指标 / 单调计数器（Milestone 52）

M38 的 metrics/health 端点导出的每一条序列都是 **gauge**（瞬时水位：高度、peer 数、mempool 深度），无法回答速率 / 吞吐类问题（「本节点已提交多少块？」「本地提交过多少 tx？」「是否观测过等价欺诈？」）——那需要 Prometheus 数据模型的另一半：**单调计数器**（`# TYPE … counter`）。M52 在既有 gauge 之外加四个累计计数器，在单所有者 actor 各自的自然单一事件点自增。端点仍 opt-in（默认关）、计数器是只读簿记，故对共识**逐字节无关**：`localnet` head 不变，无 config / wire / 依赖变更。

**四个计数器（均 `u64`，actor 独占 ⇒ 裸 `+= 1`、无需 atomics）**：`zhixing_peer_connects_total`（`peer_connects` / `Cmd::Register`，每次 peer 注册）、`zhixing_local_txs_total`（`local_txs` / `Cmd::LocalTx`，提交到本节点 API 的 tx）、`zhixing_blocks_committed_total`（`blocks_committed` / `on_decided` 中 `apply_certified` 成功分支，即经本节点自身共识轮次终局化的块；anti-entropy 同步来的块**不**计入，使计数器语义单点清晰）、`zhixing_slashing_events_total`（`slashing_events` / `on_equivocation`，观测并提交的等价欺诈）。

- **守护（`daemon.rs`）**：`Metrics` 快照结构 + `Actor` 各增四个 `u64` 字段（`Node::start` 的 actor 字面量初始化为 `0`）；四个自增点如上（每点唯一、无重复计数、无跨任务状态）；`Cmd::Metrics` 快照填充四字段；`render_prometheus` 加一个镜像 `gauge` 的 `counter` 闭包（发 `# TYPE {name} counter`）并导出四条 `zhixing_*_total`。
- **配置 / 接线**：无改动（计数器始终跟踪，仅在端点启用时暴露；无 wire、无新依赖）。
- **测试（+2 → 326）**：`daemon.rs` 两个——`render_prometheus_emits_counters`（四条 `zhixing_*_total` 各带 `# TYPE … counter` 行与其值）；`metrics_counters_advance`（单验证人 genesis ⇒ quorum 1 自提交，submit 一条 tx，睡两个块间隔后经 `node.metrics()` 断言 `blocks_committed >= 1` 且 `local_txs >= 1`，端到端验证 actor 自增接线）。

**已知边界（顺延至 M53+）**：OpenTelemetry / 结构化日志 exporter、每-sink 独立 **rotation** 覆盖、指标 push exporter（OTLP）/ 直方图 / 每-peer / 每-轮次时延序列均顺延 M53+（连同 M43/M44 篮子余项：证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS）。

## 外部交易入口 RPC（Milestone 53）

到 M52 为止，运行中的 `node run` 守护进程**没有任何让外部客户端提交交易的通路**：交易只能经进程内 `Node::submit` 句柄（demo/测试用）或从已持有该 tx 的 peer gossip 进来。换言之一个真实部署只能永远出空块——这是生产可用性的**头号硬阻断**。M53 补上一个 **opt-in HTTP 入口端点**：接收一条序列化交易、解码、走正常 mempool 准入路径、回答接受（带 tx hash）或拒绝（带人读原因）；并配一个 `node submit-tx` CLI 让整条通路端到端可用。端点 opt-in（默认关）、提交路径复用既有校验，故共识**逐字节不变**：`localnet` head 不变，无 wire/共识/依赖变更。

**线格式（已定）：原始二进制 `codec` 字节**——请求体正是 `codec::encode_tx(&tx)` 的输出，服务端用 `codec::decode_tx` 解码。零新依赖；与本代码库自定义二进制、serde-free 哲学一致（引擎核 `SubmissionTx` 不实现 `Serialize`）。JSON/DTO 入口顺延后续里程碑。

- **配置（`config.rs`）**：新增 `RpcConfig { enabled: bool 默认 false, listen: String 默认 "127.0.0.1:9700" }`（`#[serde(default)]`、手写 `Default`、`listen_addr()` → `parse_addr`），镜像 `MetricsConfig`；`NodeConfig` 加 `#[serde(default)] rpc: Option<RpcConfig>`，缺省 ⇒ `None` ⇒ 不绑定；`load_node_config` 加载期校验 `listen`（坏地址 fail-fast → `BadAddr`）。
- **提交路径（`net.rs`）**：新增 `submit_local_checked(tx) -> Result<(Hash, Vec<(u64,GossipMsg)>), ChainError>`（先 `seen_tx.insert`、再 `mempool.insert?`、成功才 broadcast），surfacing mempool 的拒绝原因；`submit_local` 重写为在其上委托（`.map(...).unwrap_or_default()`）⇒ 既有调用方**逐字节不变**（同序、同拒绝即空 vec）。
- **守护（`daemon.rs`）**：`Cmd::SubmitTx { tx, reply: oneshot }`（带 ack，不同于 fire-and-forget 的 `LocalTx`）；actor 臂自增 `local_txs`（RPC 提交亦属本地 API 提交）后 `submit_local_checked`、成功 route+ack、失败 ack 错误；`Node::submit_tx(tx) -> Option<Result<Hash, ChainError>>` 句柄经 oneshot 往返。手拼 HTTP 入口 `run_rpc`/`serve_rpc_conn`（镜像 M38 `run_metrics`）：有界读 header（≤8 KiB）；非 `POST`（`GET`/`HEAD`）→ `200 ok` 兼作健康探针；`POST` 按 `Content-Length`（≤64 KiB）读体、`codec::decode_tx`（解码失败 → `400`）、经 `Cmd::SubmitTx` 提交（`Ok` → `200` + hash hex、`Err` → `422` + `ChainError` 文案）；`parse_content_length`/`http_response` 辅助；所有客户端错误被吞不影响节点。`Node::start` 在 M38 块旁 gate+bind+spawn。
- **客户端 CLI（`main.rs`）**：`submit-tx --config F --tx F` 子命令；`cmd_submit_tx` 加载配置、要求 `[rpc]` 存在且启用、读取预编码 tx 字节、本地先 `decode_tx` 自检、阻塞 `std::net::TcpStream` POST 到 `rpc.listen`、打印接受的 hash 或拒绝原因（非 2xx ⇒ 非零退出）。
- **测试（+8 → 334）**：`config.rs` 四个（默认关闭 / 缺段为 `None` / `enabled=true` 打开且 `listen_addr` 可解析 / 坏 `listen` 经 `load_node_config` → `BadAddr`）；`net.rs` 一个（`submit_local_checked_surfaces_reject`：合法 tx ⇒ `Ok` 且落池、未知账户 tx ⇒ `Err` 且 `submit_local` 仍返回空 vec）；`daemon.rs` 三个（`parse_content_length_parses_and_caps` 纯单测；`submit_tx_accepts_valid_and_rejects_invalid` 经 `Node::submit_tx` 单验证人 genesis 合法 ⇒ `Some(Ok(hash))` + mempool≥1、非法 ⇒ `Some(Err(_))`；`rpc_endpoint_accepts_tx_over_tcp` 开 `[rpc]` 真实 TCP POST `encode_tx` ⇒ `200 OK` + hash 入体 + mempool≥1，畸形体 ⇒ `400`）。

**已知边界（顺延至 M54+）**：JSON/DTO 入口、mempool 容量上限 + 限流 + 费用/反垃圾（M54 优先）、RPC auth/TLS、`node encode-tx` / tx-spec 编写助手、`submit-tx` 远程地址 flag、读类 RPC（查账户/tx 状态）均顺延 M54+。**安全注记**：M53 开了写通路但**仅**界定 header/body 大小并复用全量 mempool 校验（签名 + 余额），**未**加 mempool 容量限制 / 限流 / 费用——绑定默认为回环 `127.0.0.1`，运维在 M54 落地前**不应**在无前置代理时公网暴露该端点。端点无认证（同 M38 metrics 端点），与 metrics 端点 TLS 一并顺延运维篮子。

## mempool DoS 加固（Milestone 54）

M53 开了外部写通路后，两个**无界资源**成了真实 DoS 向量，且二者都**节点本地**（无共识 / wire / 依赖变更）：(1) **mempool 无容量上限**——`Mempool::insert` 无任何容量检查，`max_txs` 只是**每块出块上限**不是池上限，一股合法但永不入块的交易洪流能无界涨内存；(2) **入口无限流**——gossip 收到的交易在 `on_tx(from, tx)` 被无节流准入，单个 peer 即可灌满准入路径。M54 把两者都收口：可配的 mempool **容量上限**在准入处强制（拒绝带原因），以及 gossip 交易入口的**每-peer 令牌桶限流**，二者经新 `[mempool]` 配置段；顺带把原先硬编码 `64` 的每块出块上限 `max_txs` 也挪进同一段可配。**货币费用明确不在本里程碑**（用户决策）：`SubmissionTx` 加费用字段会改 `codec::encode_tx` / 签名字节 / tx 哈希并破坏 localnet head 不变量——那是专门后续里程碑的共识 / 协议变更。

**不变量保持**：容量默认大（4096，localnet 不足百笔 ⇒ 永不触发），限流默认**关**（`per_peer_tx_per_sec = 0.0` = 无限、运维 opt-in），`max_block_txs` 默认 `64`（不变）；既有测试 / driver 的 mempool 仍无界 ⇒ `localnet` head 逐字节不变 `44309755…ea04ba`，无 wire / 共识 / 依赖变更。

- **配置（`config.rs`）**：新增 `MempoolConfig{capacity(默认 4096), max_block_txs(默认 64), per_peer_tx_per_sec(默认 0.0), per_peer_tx_burst(默认 256.0)}`（`#[serde(default)]`、手写 `Default` 作旧常量唯一真源；因 `f64` 字段**只**派生 `PartialEq` 而非 `Eq`，与其它配置段不同）+ `validate()`（拒 `capacity==0` / `max_block_txs==0` / 负速率或桶 → `ConfigError::BadMempool`）；`NodeConfig` 加 `#[serde(default)] mempool: MempoolConfig`（始终在场、非 `Option`，镜像 `ConsensusConfig`/`NetworkConfig`）；`load_node_config` 加载期调 `cfg.mempool.validate()?`。
- **引擎核（`lib.rs`）**：新增 `ChainError::MempoolFull { capacity: usize }` 变种 + Display 臂 `"mempool full (capacity {capacity})"`（纯变种、不序列化，引擎核保持 serde-free / 零依赖）。
- **mempool（`mempool.rs`）**：加 `capacity: usize` 字段，`new()` 置 `usize::MAX`（无界、既有调用方零 churn）+ `set_capacity`/`capacity` 存取器；`insert` 在 `validate_tx` + 算出 `h` 后门控：`!contains_key(h) && len() >= capacity ⇒ Err(MempoolFull)`，已在池的同哈希重插仍幂等（不涨池、不违约）。该错误经 `insert → submit_local_checked → RPC` 既有路径映射为 `422`+Display（无需改 `serve_rpc_conn`）；gossip `on_tx` 本就在 `insert().is_err()` 时丢弃。
- **net（`net.rs`）**：加 `GossipNode::set_mempool_capacity(cap)` 直通 `self.mempool.set_capacity(cap)`。
- **守护（`daemon.rs`）**：令牌桶 `TokenBucket{tokens: f64, last: Instant}` + `allow(now, rate, burst)`（连续补充、封顶 burst、消费一枚）；`Actor` 加 `peer_tx_buckets: HashMap<u64, TokenBucket>` + 缓存 `tx_rate`/`tx_burst`（构造时读 `cfg.mempool`）+ 计数器 `txs_rate_limited: u64`；`allow_peer_tx(from)`（`tx_rate <= 0.0 ⇒ 直过`，否则首见 seed 满桶后 refill-and-consume）；`Cmd::Inbound` 臂**仅**对 `GossipMsg::Tx`、在 `Consensus`/`Peers` 早返回之后、`on_message` 之前门控（拒则 `txs_rate_limited += 1; continue`——本地 `LocalTx`/`SubmitTx` 是运维自身、永不限流）；时间经 `std::time::Instant::now()` 在臂内内联取（actor 单属主异步、廉价、无注入时钟、保 `on_message` 纯）。`Node::start` 把硬编码 `64` 换成 `cfg.mempool.max_block_txs` 传进 `GossipNode::new`，随后 `node.set_mempool_capacity(cfg.mempool.capacity)`。指标：`Metrics` 加 `mempool_capacity`（gauge `zhixing_mempool_capacity`，与既有 `zhixing_mempool_txs` 并列、使饱和度 txs vs capacity 可观测）+ `txs_rate_limited`（counter `zhixing_txs_rate_limited_total`）。
- **测试（+7 → 341）**：`config.rs` 三个（`mempool_config_has_safe_defaults` / `mempool_section_overrides_defaults` 解析 `[mempool]` TOML / `mempool_zero_capacity_rejected_by_loader` ⇒ `BadMempool`）；`mempool.rs` 两个（`insert_rejects_when_at_capacity`：`set_capacity(1)` 首笔 `Ok`、第二笔异哈希 ⇒ `MempoolFull{capacity:1}` / `at_capacity_still_allows_idempotent_reinsert`：满容量重插同哈希仍 `Ok` 不涨池）；`daemon.rs` 两个（`token_bucket_refills_and_throttles` 纯单测经 `Instant`+`Duration` 算术确定性推进时间、验 burst 耗尽即拒、补充后再过 / `rpc_submit_reports_mempool_full` tokio，`cfg.mempool.capacity = 1` 的纯 follower 节点（无出块排空池）首笔 `Ok`、第二笔异 tx ⇒ `MempoolFull{capacity:1}`）。

**已知边界（顺延至 M55+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、`seen_tx` / 去重集上限 + 驱逐、费用优先的出块排序（当前为规范 tx-哈希序）、每账户 mempool 配额、nonce / 序列号反重放、JSON/DTO 入口、RPC auth/TLS、`node encode-tx` 编写助手均顺延 M55+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## gossip 去重集上限（Milestone 55）

M54 收口 mempool 后，节点里**最后一个无界资源**是三个 gossip 洪泛去重集 `seen_tx` / `seen_evidence` / `seen_stake_op`（`net.rs`，原均为 `BTreeSet<Hash>`）——它们**单调永涨**：无任何 `.remove` / 驱逐，交易入块后从 mempool 剪除却**永久**留在 `seen_tx`，被拒交易也记为已见；一股持续的异哈希（哪怕非法）gossip 流能无界涨内存。M55 用可配容量 + **FIFO 驱逐**给三者都收口。**为何驱逐而非到界即拒（不同于 M54 的 mempool）**：去重集的职责是**压制洪泛**——它**必须**持续接纳新哈希，故满时逐出最旧而非拒收。`seen_tx` 不担任何共识 / 反重放角色（准入独立由 `Mempool::insert` 里的 `validate_tx` 门控），驱逐一条至多让某久未出现的交易被当新的**再洪泛一次**（随即被状态校验再准入-或-丢弃、再广播一次），绝非双花或安全违约。

**不变量保持**：上限默认**关**（`seen_cache = 0` ⇒ `usize::MAX` 无界），无界模式下新结构逐字节等同今日裸 `BTreeSet`（FIFO 序追踪整段跳过）⇒ `localnet` head 逐字节不变 `44309755…ea04ba`，无 wire / 共识 / 依赖变更。

- **net（`net.rs`）**：新增私有 `SeenSet{set: BTreeSet<Hash>, order: VecDeque<Hash>, capacity: usize}`——`BTreeSet` 作 O(log n) 成员查、`VecDeque` 记插入序供 FIFO 驱逐、`capacity == usize::MAX` ⇒ 无界且 deque 永不触碰（默认路径同今日）；`insert(h) -> bool` 镜像 `BTreeSet::insert`（真 = 新插入），仅在有界时 push 入序并逐出溢出的最旧；`set_capacity` 设容量并即时 trim；`new()` 置 `usize::MAX`。三个 `GossipNode` 字段 `BTreeSet<Hash>`→`SeenSet`，六处 insert 调用点（submit/on_tx tx、evidence、stake_op）均 drop-in 不变（同 `-> bool` 契约）。加 `set_seen_capacity(cap)` 直通设三集 + `seen_tx_len`/`seen_tx_capacity` 存取器供指标。
- **配置（`config.rs`）**：`MempoolConfig` 加 `seen_cache: usize`（每集去重容量，默认 `0`）；**哨兵刻意不对称**于 `capacity`（后者 `0` 被**拒**为无意义零容量池）：此处 `0` 意为**无界 / 关**，因零容量去重缓存会彻底败坏洪泛压制，故 `0` 保留作关闭开关（恰如 `per_peer_tx_per_sec = 0.0`）；任意 `usize` 合法故 `validate()` 不变。
- **守护（`daemon.rs`）**：`Node::start` 在 `set_mempool_capacity` 后加 `if cfg.mempool.seen_cache > 0 { node.set_seen_capacity(cfg.mempool.seen_cache) }`——默认 `0` ⇒ 不调 ⇒ 三集留 `usize::MAX` ⇒ 行为逐字节不变。指标：`Metrics` 加 `seen_tx` / `seen_tx_capacity`，`Cmd::Metrics` 臂从 `seen_tx_len()` / `seen_tx_capacity()` 填充，`render_prometheus` 发 `zhixing_seen_tx` / `zhixing_seen_tx_capacity` 两 gauge（与 `zhixing_mempool_txs`/`_capacity` 并列）；tx 集是洪泛压力所在、evidence/stake-op 集低频故不曝；无界时 capacity gauge 显 `usize::MAX`（诚实的"无界"信号）。
- **测试（+5 → 346）**：`net.rs` 四个（`seen_set_evicts_oldest_at_capacity` 容量 2 插 3 异哈希 ⇒ 最旧被逐（其 insert 再返回 true）、新二仍在、`len()==2` / `seen_set_unbounded_by_default` 默认 `usize::MAX` 插多条无驱逐、`order` 恒空 / `seen_set_set_capacity_trims_when_over` 无界插 3 后 `set_capacity(1)` ⇒ `len()==1` / `bounded_seen_tx_reaccepts_evicted_flood` 集成：`set_seen_capacity(1)`、提交 A 再提交 B 逐出 A 哈希、`on_message(2, Tx(a))` 返回非空出站证驱逐重开洪泛路径）；`config.rs` 一个（`mempool_seen_cache_overrides_defaults` 解析 `[mempool] seen_cache = 1024`；`mempool_config_has_safe_defaults` 加断言 `seen_cache == 0`）；`daemon.rs` 扩 `sample_metrics` 字面量 + `render_prometheus_emits_all_gauges` 加 `zhixing_seen_tx`/`zhixing_seen_tx_capacity`。

**已知边界（顺延至 M56+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、每账户 mempool 配额、nonce / 序列号反重放、JSON/DTO 入口、RPC auth/TLS、读类 RPC、`node encode-tx` 编写助手均顺延 M56+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 交易编写助手 `node encode-tx`（Milestone 56）

M53 打通外部写入路径（`POST /submit_tx` + `node submit-tx` 读 `--tx` 原始 `encode_tx` 字节投递），但**节点里没有任何东西产出那些字节**——除了对本 crate 写 Rust，再无办法喂给 `submit-tx`（每处 `.signed(&kp(…))` 都在进程内 demo/test 里、从不落盘）。M56 补上这一缺口：`node encode-tx` 从命令行旗标装配一个 `SubmissionTx`、用 ed25519 种子签名、把 wire 编码写入文件——即 `submit-tx` 的生产端；两者直接组合（`submit-tx` 本就用 `decode_tx` 自检输入）。

**不变量保持**——平凡：这是纯离线 CLI 增量，不碰任何运行中节点、不碰共识 / mempool / wire / 状态路径、不加引擎依赖。`localnet` 出块逐字节不变，head 仍 `44309755…ea04ba`。

- **CLI（`main.rs`）**：新子命令（连字符对齐 `submit-tx`）`node encode-tx --key-file F --out F --author N --domain N --stake N --embedding f0,…,f7（恰 DIM=8） --review R:S（可重复，≥1，`validate_tx` 需要） --repl-success N --repl-total N --timestamp-days F [--config F]`；`main()` match 加 `"encode-tx"` 臂、`usage()` 加一行（紧邻 `submit-tx`）。
- **密钥输入**：`--key-file` 存 64 字符 hex 的 32 字节 ed25519 种子——**与验证人 `ValidatorKeyConfig.seed_hex` 同格式**；读文件 → `trim()`（`decode_hex` 不去空白）→ 复用 `config::decode_seed`（故 `fn`→`pub fn`，单行可见性变更）得同款解析 + `ConfigError::BadHex` → `Keypair::from_seed`；密钥走文件不走旗标（远离 shell history）。尚无 keygen 助手，种子仍手写（同验证人键）。
- **可选创世交叉校验（`--config`）**：给了就镜像 `cmd_run`（`load_node_config` → `load_genesis` → `to_genesis()`），在 `genesis.accounts` 里找 `--author`、断言 `kp.public()` == 其登记 pubkey，不匹配即 `fail_msg("author key", …)` 提前失败——在本地就逮住"键/作者不匹配"这一最常见错因（否则服务器回 `422 BadSignature`）；不给则纯离线编写。此处只校验 pubkey 匹配，完整 `validate_tx`（reviewers 已知、余额 ≥ stake）仍在 submit/apply 时跑。
- **纯函数（便于单测）**：`parse_embedding`（逗号分 `f32`、恰 `DIM` 个）、`parse_review`（`<reviewer>:<score>`）、`multi_arg`（收集全部 `--review`，单值 `config_arg`/`tx_arg` 只取首个）、`build_signed_tx`（装配 `signature=[0;64]` 后 `.signed`）；`cmd_encode_tx` 做 IO + 自检（`decode_tx(&encode_tx(&tx))` 往返，`submit-tx` 同款守卫）+ `fs::write` + 打印 `encoded <hash>` / `bytes N` / `out <path>`（`<hash>` 即节点 `accepted` 回的同一哈希）。
- **无引擎/codec/crypto 变更**：`encode_tx`/`decode_tx`/`tx_signing_bytes`、`SubmissionTx::signed`、`Keypair`、`Review` 本就公开；无新 `ChainError`，不改 `lib.rs`/`codec.rs`/`crypto.rs`/`net.rs`/`daemon.rs`/`mempool.rs`。
- **测试（+9 → 355）**：`main.rs` 本无测试模块，新增 `#[cfg(test)] mod tests`（二进制 target 也被 `cargo test` 跑）——`encode_tx_round_trips`（`SubmissionTx` 无 `PartialEq`，改断言 decode∘encode 重编码字节相等）/ `encode_tx_signature_valid`（`crypto::verify` 验签）/ `encode_tx_hash_is_stable`（无 RNG，同输入同哈希）/ `parse_embedding_*` 三个（恰 DIM / 错元数 / 非 float）/ `parse_review_ok` + `parse_review_rejects_bad_format` / `multi_arg_collects_all_occurrences`。

**已知边界（顺延至 M57+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、每账户 mempool 配额、nonce / 序列号反重放、读类 RPC、JSON/DTO 入口、RPC auth/TLS、`node keygen`（产种子/pubkey 对，免手写 encode-tx/验证人键）均顺延 M57+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 每账户 mempool 配额（Milestone 57）

M54–M55 把 mempool 准入对 DoS 做了三层加固：全局 pending **容量**上限（`MempoolFull`）、每-peer gossip **限流**（`TokenBucket`）、有界 gossip **去重集**（`seen_cache`）。但这条加固线上仍有一个缺口：**单个账户仍能霸占整个池。** mempool 只按内容哈希索引（`pending: BTreeMap<Hash, SubmissionTx>`）、从不按作者索引，故一个持有有效密钥且有余额的账户可签无数**不同** valid tx（改 `embedding`/`domain` ⇒ 不同哈希），每条都过 `validate_tx`，独自填满全部 `capacity` 槽、饿死其他账户的准入。每-peer 限流只约束 *gossip 流量*、不约束 *每作者占用*，且验证人本地提交自家 tx 从不被限流（`daemon.rs` 只门控 `GossipMsg::Tx`）。M57 用**每账户 pending 配额**补齐：限定单个 `author` 同时可持有的 pending tx 数。纯准入侧——不碰共识 / wire / 状态根 / 出块。

**不变量保持**：配额默认**关**（`per_account_limit = 0` ⇒ `usize::MAX` 无界），默认下 `localnet` 与一切既有配置逐字节同块、head 仍 `44309755…ea04ba`，无 wire / 共识 / 依赖变更（配额是纯 std）。

- **作者索引 + 配额门（`mempool.rs`）**：`Mempool` 加 `per_author: BTreeMap<u64, usize>`（作者 → 当前 pending 数，计数归零即删条目 ⇒ 映射只随活跃作者增长而非历史全体）、`per_account_limit: usize`（默认 `usize::MAX` 无界）、`rejected_quota: u64`（累计被配额拒的准入，供可观测）；配 `set_per_account_limit`/`per_account_limit()`/`rejected_quota()` 访问器（镜像 `set_capacity`/`capacity`）。`insert` 在 `validate_tx` 之后、全局 capacity 门**之侧**加配额门——与 capacity 门同位置，因作者先被签名认证、且本门防的真实攻击（一有效密钥停放多条 valid tx）签名皆过故顺序不改 CPU 成本，伪造作者的*流量*已由 M54 每-peer 限流约束；仅对*新哈希*计数/判拒，幂等重插（`is_new == false`）不重判不重计（沿用 M54 capacity 幂等契约）。`remove_included` 对每条实际移除的 tx 递减其作者计数、归零即删条目（`is_some()` 守卫确保跳过/不存在的 tx 永不令计数下溢）。
- **错误（`lib.rs`）**：`ChainError` 加 `AccountQuotaFull { author: u64, limit: usize }`（紧邻 `MempoolFull`，同为节点本地准入背压、非共识有效性错误）+ Display 臂 `account {author} over pending quota (limit {limit})`。**RPC 无需改**：`serve_rpc_conn` 本就把每个 `Err(ChainError)` 经 `e.to_string()` 映射为 `422`，故新变体自动以其 Display 冒出；`submit_local`/`on_tx` 本就吞准入错误、不变。
- **配置（`config.rs`）**：`MempoolConfig` 加 `per_account_limit: usize`，默认 `0`——沿用 `seen_cache` 的 **`0` = 关**哨兵（非 capacity 的拒-`0`）；高于 `capacity` 的值合法但永不触发（全局上限先跳）；任意值合法故 `validate()` 不变。
- **守护（`daemon.rs`）**：`Node::start` 在 M55 `set_seen_capacity` 后加 `if cfg.mempool.per_account_limit > 0 { node.set_mempool_per_account_limit(...) }`——默认 `0` ⇒ 不调 ⇒ 留 `usize::MAX` ⇒ 行为不变；`GossipNode::set_mempool_per_account_limit` 透传（镜像 `set_mempool_capacity`）。指标：`Metrics` 加 `mempool_per_account_limit`（配置上限，似 `mempool_capacity`）+ `txs_quota_rejected`（似 `txs_rate_limited`），`Cmd::Metrics` 臂从 mempool 自身 `per_account_limit()`/`rejected_quota()` 填（自计数、无需穿 gossip 路径的错误管道），`render_prometheus` 发 `zhixing_mempool_per_account_limit` gauge + `zhixing_txs_quota_rejected_total` counter。
- **测试（+6 → 361）**：`mempool.rs` 五个（`insert_rejects_when_author_over_quota` 限 2、作者 1 两条过第三条 ⇒ `AccountQuotaFull { author: 1, limit: 2 }`、`len==2`、`rejected_quota()==1` / `quota_is_per_author_not_global` 限 1、作者 1 与 2 各一条皆过 `len==2` / `idempotent_reinsert_does_not_consume_quota` 限 1、重插同哈希仍 Ok 不重计、同作者异 tx 才触发 / `remove_included_frees_author_quota` 限 1、提交块移除后同作者再获准入 / `quota_off_by_default_admits_many` 默认无界一作者五条皆过、`rejected_quota()==0`）；`config.rs` 一个（`mempool_per_account_limit_overrides_defaults` 解析 `[mempool] per_account_limit = 32`；`mempool_config_has_safe_defaults` 加断言 `per_account_limit == 0`）；`daemon.rs` 扩 `sample_metrics` 字面量 + `render_prometheus_emits_all_gauges`/`_emits_counters` 加新 gauge/counter。端到端已复验（接 M53/M56）：`[mempool] per_account_limit = 1` 跑节点，`node encode-tx` 对同账户造两条异 tx，首条 `submit-tx` → `accepted`、次条 → `422 account <id> over pending quota (limit 1)`，`:9700`/metrics 现 `zhixing_txs_quota_rejected_total 1`。

**已知边界（顺延至 M58+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、nonce / 序列号反重放、读类 RPC（需真 HTTP 路径路由——今日 RPC 端口仅按方法分发——加新 `Cmd::Get*` 变体与全新 tx-哈希→区块索引）、JSON/DTO 入口、RPC auth/TLS、`node keygen`（产种子/pubkey 对，免手写 encode-tx/验证人键）均顺延 M58+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 读类 RPC 查询（Milestone 58）

M53 给节点开了**外部入口 RPC**（`POST /submit_tx`）、M38 给了只读的 metrics/health 端点。但至今仍无从经 wire **读链上状态**：想知道当前高度、head 哈希、某账户余额，只能刮 Prometheus 文本（粗）或跑进程内 demo。M53 的 RPC 服务**仅按方法分发**——`serve_rpc_conn` 只取 HTTP 方法 token，凡非 `POST` 一律当健康探针（`200 ok`），URL 路径从不解析。M58 在既有 RPC 端点上补齐**读类 GET 路由**：`GET /height`、`GET /head`、`GET /account/{id}`。纯读，经既有单属主 actor 快照路径（`Cmd::Query`/新增 `Cmd::QueryAccount`）；不碰共识 / wire / 状态根 / 出块。

**不变量保持**：RPC 默认**关**（`[rpc] enabled`，默认 off），默认下 `localnet` 与一切既有配置逐字节同块、head 仍 `44309755…ea04ba`，无 config / wire / 依赖变更（路由全在已门控的 M53 监听器上）。

- **纯路由器 + 格式化器（`daemon.rs`）**：`route_get(path) -> GetRoute`（`Height`/`Head`/`Account(u64)`/`Health`/`NotFound`）——`/height`→Height、`/head`→Head、`/account/<digits>`→Account、`/` 与一切无法识别路径→Health（保留今日健康探针 `200 ok` 行为）、仅 `/account/<非数字>`→NotFound（`404`）；`format_account(id, &Account) -> String` 渲染 grep 友好的 `key=value` 纯文本（`balance`/`staked_total`/`earned_total`/`slashed_total`/`submissions`/`accepted`/十六进制 `pubkey`）。二者皆无 I/O、可纯单测（薄-I/O-壳模式，似 `encode-tx` 的 `parse_embedding`）。
- **读 Cmd（`daemon.rs`）**：`/height`、`/head` 复用既有 `Cmd::Query`（回 `(height, head)`）；新增 `Cmd::QueryAccount { id, reply: oneshot::Sender<Option<Account>> }`——actor 消费臂对 actor 自有状态做纯读、克隆快照（`actor.node.chain.state.accounts.get(&id).cloned()`，`Account: Clone`）；`Node::account(id) -> Option<Option<Account>>` 句柄镜像 `status`/`metrics`（外 `None` = actor 已停、内 `None` = 无此账户）。
- **GET 分支（`serve_rpc_conn`）**：在既有 POST 路径**之前**加路径提取（`head.split_whitespace().nth(1)`）+ GET/HEAD 分支——各路由经局部 `oneshot` + `cmd.send(...)`（镜像本函数内 `Cmd::SubmitTx` 之形），send 失败→`503`；`Account` 查到→`200` + `format_account`、查不到→`404 not found`。非 POST 非 GET 方法仍走旧健康探针 catch-all（`200 ok`）；POST `/submit_tx` 路径逐字不变（M53）。
- **测试（+3 → 364）**：`route_get_parses_paths`（纯，`matches!` 各路由含 `/`→Health、`/account/notanum`→NotFound）；`format_account_renders_fields`（纯，样本 `Account` 渲染预期 `key=value` 串含十六进制 pubkey）；`rpc_get_returns_reads`（tokio 真 TCP，镜像 `rpc_endpoint_accepts_tx_over_tcp`：`GET /height`→`200`+数字、`GET /head`→`200`+64-hex、`GET /account/<创世 id>`→`200`+体含 `balance=`、`GET /account/<未知>`→`404`、`GET /`→`200 ok` 保留健康探针）。端到端已复验（`[rpc] enabled = true`）：`curl :9700/height`、`:9700/head`、`:9700/account/1` 得纯文本读、`:9700/account/999999`→`404`、`:9700/`→`ok`；`POST /submit_tx` 行为不变。

**已知边界（顺延至 M59+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、nonce / 序列号反重放、JSON/DTO 响应体（此处仅纯文本）、可验证读（经 `ChainState::account_proof` 的 Merkle 证明——已有、后续暴露）、查询分页 / 范围扫描端点、RPC auth/TLS、`node keygen`（产种子/pubkey 对，免手写 encode-tx/验证人键）均顺延 M59+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 可验证账户读 RPC（Milestone 59）

M58 的读类 GET（`/height`/`/head`/`/account/{id}`）把结果**直接明文**回给调用方——读是**信任节点**的：客户端只能取服务端所报的余额。代码库自 M20–M29 已有一套完整的 **cert-bound SPV 证明栈**（`ChainState::account_proof` 产证、`light::ProofEntry::Account` typed 打包、`light::ValidatorTracker::verify_proof_against_header` 单一验证器、`GossipNode::serve_inclusion` 服务），但此前仅经 P2P gossip wire 可达、从未对外部 RPC 暴露。M59 加 **`GET /account/{id}/proof`**：回发一个轻客户端自验所需的全部字节——头的 `CertifiedHeader`（header + 其最终性 `Commit`）＋账户的 `ProofEntry`（账户快照 + Merkle 路径）——调用方本地重算 leaf、对 `header.accounts_root` 验 Merkle 路径、对自持的验证人集验 cert，把 M58 的「节点说余额是 X」升级为「X 可对 > 2/3 签名头自证」。

**不变量保持**：路由在已门控的 M53/M58 RPC 监听器上、RPC 默认**关**，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`，无 config / wire / 共识 / 状态根 / 依赖变更。客户端侧验证走**既有** `verify_proof_against_header`——节点侧零新增验证路径。

- **新证明产出（`net.rs`）**：`serve_inclusion` 私有、仅回 `ProofEntry`；新公有薄封装 `account_inclusion(id) -> Option<(CertifiedHeader, ProofEntry)>` 复用 `serve_inclusion(Account, id)` 取证、`headers_from(height).next_back()` 取头证书（height 0 → 空 → `None`，未知 id → `None`）；头块 `accounts_root` 承诺的恰是证明所据的当前状态，故该对天然自洽。
- **读 Cmd + 句柄（`daemon.rs`）**：镜像 M58 `QueryAccount`，新增 `Cmd::QueryAccountProof { id, reply: oneshot::Sender<Option<(CertifiedHeader, ProofEntry)>> }`——actor 消费臂派 `account_inclusion`；`Node::account_proof(id) -> Option<Option<...>>` 句柄（外 `None` = actor 已停、内 `None` = 无此账户 / 尚无认证头）。
- **路由 + 响应（`daemon.rs`）**：`GetRoute` 加 `AccountProof(u64)`，`route_get` 以 `strip_suffix("/proof")` 把 `/account/{id}/proof` 从 M58 的 `/account/{id}` 拆出（空 / 非数字 id → `404`）；`format_account_proof(ch, entry) -> String` 渲染两行标注 hex `certified_header=<hex>`+`proof_entry=<hex>`（经既有 `encode_certified_header`/`encode_proof_entry` + `hash::hex`，仍纯文本无 JSON）。GET 分支置于既有 POST 之前、查到→`200`+体、查不到→`404`、actor 停→`503`；POST `/submit_tx` 与 M58 读类路由逐字不变。
- **测试（+3 → 367）**：`route_get_parses_account_proof`（纯，`/account/7/proof`→AccountProof、`/account/7`→Account、`/account/notanum/proof` 与 `/account//proof`→NotFound、其余路由不受扰）；`account_inclusion_verifies_end_to_end`（tokio，单验证人 quorum 1 自出块，`Node::account_proof(1)`→`Some((ch, entry))`，codec round-trip 后 `verify_proof_against_header(&ch.header, &ch.cert, &tracked, &entry)` 对 `ValidatorTracker::from_genesis(&g).validators()` 验证、未知 id→内 `None`）；`rpc_account_proof_over_tcp`（tokio 真 TCP：`GET /account/1/proof`→`200`+体含 `certified_header=`/`proof_entry=`、`GET /account/999999/proof`→`404`、M58 `GET /account/1`→`200`+`balance=` 无回归）。

**已知边界（顺延至 M60+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、nonce / 序列号反重放、JSON/DTO 响应体（此处仅纯文本）、批量 / 异构证明 RPC（gossip `GetBatch` 的 RPC 对应——kNN / 范围 / diff / 验证人证明经 RPC）、reviewer / 图节点 / 验证人证明经 RPC（此处仅账户）、查询分页 / 范围扫描端点、RPC auth/TLS、`node keygen`（产种子/pubkey 对，免手写 encode-tx/验证人键）均顺延 M60+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 其余实体可验证读 RPC（Milestone 60）

M59 的 `GET /account/{id}/proof` 把读从「信任节点」升级为「可对 > 2/3 签名头自证」，但只接了**账户**单类——即便那套 cert-bound SPV 机器本就是 kind-泛化的：`GossipNode::serve_inclusion(kind, id)` 自 M24–M25 起已能产出全部四类 `ProofEntry`（Account / Reviewer / Validator / GraphNode），`ValidatorTracker::verify_proof_against_header` 也早已按类选根（Account/Reviewer/GraphNode → `accounts_root`，Validator → `next_validators_root`）。唯一缺口是 RPC 暴露。M60 补三条姊妹路由 **`GET /reviewer/{id}/proof`**、**`GET /validator/{id}/proof`**、**`GET /graph/{idx}/proof`**（图节点按插入序寻址），逐字复用 M59 的形状。

**不变量保持**：路由在已门控的 M53/M58/M59 RPC 监听器上、RPC 默认**关**，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`，无 config / wire / 共识 / 状态根 / 依赖变更。客户端侧验证走**既有** `verify_proof_against_header`——节点侧零新增验证路径。

- **证明产出泛化（`net.rs`）**：M59 的 `account_inclusion(id)` 泛化为公有 `inclusion(kind, id) -> Option<(CertifiedHeader, ProofEntry)>`（复用 `serve_inclusion(kind, id)` 取证 + `headers_from(height).next_back()` 取头证书，height 0 → `None`、未知 id/index → `None`）；`account_inclusion(id)` 退化为 `self.inclusion(ProofKind::Account, id)`、公有契约逐字不变。
- **读 Cmd + 句柄（`daemon.rs`）**：M59 的 `QueryAccountProof` 原样保留，新增 `Cmd::QueryInclusion { kind: ProofKind, id, reply: oneshot::Sender<Option<(CertifiedHeader, ProofEntry)>> }`——actor 消费臂派 `inclusion(kind, id)`；`Node::proof(kind, id) -> Option<Option<...>>` 句柄镜像 `account_proof`（外 `None` = actor 已停、内 `None` = 未知 id/index 或尚无认证头）。
- **路由 + 响应（`daemon.rs`）**：`GetRoute` 加 `Proof(ProofKind, u64)`（`AccountProof(u64)` 保留）；`route_get` 加 `/reviewer/`、`/validator/`、`/graph/` 三前缀各经纯辅助 `proof_route(rest, kind)` 以 `strip_suffix("/proof")` 拆出（这三类无 M58 明文读形式，故裸 `{id}`、非数字 id → `404`），无法识别路径仍回健康探针；响应臂 `GetRoute::Proof(kind, id)` 复用 kind-无关的 `format_account_proof`（它只 hex-编码 `(CertifiedHeader, ProofEntry)` 对），`404` 体经纯 `proof_kind_label(kind)` 区分实体名；查不到→`404`、actor 停→`503`。M59 账户路由与 POST `/submit_tx` 逐字不变。
- **测试（+3 → 370）**：`route_get_parses_proof_kinds`（纯，三前缀各拆 `/proof`→`Proof(kind, id)`、非数字/裸 id→NotFound、M59 账户路由与 `/`→Health 不受扰）；`inclusion_verifies_all_kinds_end_to_end`（tokio，单验证人 quorum 1 自出块，对 Reviewer 10 / Validator 21 / GraphNode 0 各 `Node::proof(kind, id)`→`Some((ch, entry))`，codec round-trip 后 `verify_proof_against_header` 对 `ValidatorTracker::from_genesis(&g).validators()` 验证——验证人证明走 `next_validators_root`、其余走 `accounts_root`、未知 id→内 `None`）；`rpc_other_proofs_over_tcp`（tokio 真 TCP：`/validator/21/proof`/`/reviewer/10/proof`/`/graph/0/proof` 各→`200`+体含 `certified_header=`/`proof_entry=`、`/validator/999/proof`→`404`、`/validator/21`（无 `/proof`）→`404`）。

**已知边界（顺延至 M61+）**：**货币费用**字段（`SubmissionTx` 加费 = 共识 / wire 变更，专门后续里程碑）、费用优先的出块排序（当前为规范 tx-哈希序）、nonce / 序列号反重放、JSON/DTO 响应体（此处仅纯文本）、批量 / 异构证明 RPC（gossip `GetBatch`/`serve_batch` 的 RPC 对应——一次 POST 承载 `Vec<BatchItem>` 的 inclusion + kNN + 范围 + diff）、kNN / 范围 / diff / 桥锁证明经 RPC 各自暴露、明文（非证明）reviewer/验证人/图读、查询分页 / 范围扫描端点、RPC auth/TLS、`node keygen`（产种子/pubkey 对，免手写 encode-tx/验证人键）均顺延 M61+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## 批量/异构证明 RPC（Milestone 61）

M59/M60 把单值读升级为可对 > 2/3 签名头自证，但钱包对账多个值仍是**每值一次** RPC 往返。gossip 层早有异构**批量**：一条 `GetBatch { items: Vec<BatchItem> }`→`GossipNode::serve_batch`→`BatchResponseEnvelope`，其槽位一次应答 Inclusion / kNN / Range / Diff 四类，客户端以 `ValidatorTracker::verify_batch` 整批验证。这套自 M29 即在，唯一缺口是 RPC 暴露——此前 RPC 的 POST 路径单一：任意 POST 都按 `decode_tx`→`Cmd::SubmitTx`（M53）。M61 补 **`POST /batch`**：请求体是编码后的 `Vec<BatchItem>`，响应是头部 `CertifiedHeader` + `BatchResponseEnvelope`（与 M59/M60 同的两行 hex）。

**不变量保持**：`/batch` 骑在已门控的 M53/M58/M59/M60 RPC 监听器上、RPC 默认**关**，且请求字节与既有 gossip `GetBatch` 逐字一致（无新 wire tag），故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`，无 config / wire / 共识 / 状态根 / 依赖变更。客户端侧验证走**既有** `verify_batch`——节点侧零新增验证路径。

- **请求编解码抽取（`net.rs`）**：`Vec<BatchItem>` 的请求格式此前只内嵌于 `encode_gossip`/`decode_gossip`，提升为公有 `encode_batch_request` / `decode_batch_request`（u32 计数，上限 `MAX_BATCH_ITEMS`→`TooManyItems`；每项 1 字节类标签 + 按类体）；`encode_gossip` 的 `GetBatch` 臂改为委托 `encode_batch_request`，gossip wire 逐字节不变（解码臂留原样以护 wire 路径，`decode_batch_request` 为 RPC 独立镜像，由 round-trip 测试钉住）。
- **证明产出（`net.rs`）**：`GossipNode::batch(items) -> Option<(CertifiedHeader, BatchResponseEnvelope)>` 镜像 M60 的 `inclusion`——复用 `serve_batch(items)` 取批量 + `headers_from(height).next_back()` 绑认证头；`None` = `serve_batch` 拒绝（超 `MAX_BATCH_ITEMS` 或退化 Diff 区间）或尚无认证头（height 0）。
- **读 Cmd + 句柄 + 响应（`daemon.rs`）**：新增 `Cmd::QueryBatch { items, reply: oneshot::Sender<Option<(CertifiedHeader, BatchResponseEnvelope)>> }`——actor 消费臂派 `node.batch(items)`；`Node::batch_proof(items) -> Option<Option<...>>` 句柄镜像 `proof`；`format_batch(ch, env)` 回两行 hex（`certified_header=` + `batch_envelope=`）。POST 路径按 `path == "/batch"` 分流：解析 content-length（411/413 同 submit_tx）、读体（截断→400）、`decode_batch_request`（失败→400）、`Cmd::QueryBatch`（send 失败→503）、`Some`→`200`+`format_batch`、`None`→`422`「batch rejected」。其余 POST 仍是 M53 提交逐字不变（体读内联复制以护 submit 路径）。
- **测试（+3 → 373）**：`batch_request_codec_round_trip`（纯，`net.rs`：四类一项往返稳定 + 超限→`TooManyItems` + 字节与内嵌 gossip `GetBatch` 尾逐字一致）；`batch_serves_and_verifies_end_to_end`（tokio，单验证人 quorum 1 自出块，`[Inclusion{Reviewer,10}, Inclusion{Validator,21}, Knn, Range]` 一批经 `Node::batch_proof`→`Some((ch, env))`、codec round-trip 后 `tracker.verify_batch(&g, &ch.header, &ch.cert, &tracked, &[], &items, &env)`→`Ok`（无 Diff 故 `blocks_in_range` 空）、未知 id→内 `None` 片仍验）；`rpc_batch_over_tcp`（tokio 真 TCP：`POST /batch` 编码体→`200`+体含 `certified_header=`/`batch_envelope=`、垃圾体→`400`、非 `/batch` POST 仍走 M53 提交）。

**已知边界（顺延至 M62+）**：Diff 片在 RPC 响应里随附区间块（现 gossip 假设钱包自备 M22 缓存，RPC 沿用同假设）、桥锁证明经 RPC（非 `BatchItem` 类，另有 `GetLock`/`Lock` 对）、明文（非证明）reviewer/验证人/图读、JSON/DTO 响应体、查询分页 / 范围扫描端点、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放、`node keygen`（产种子/pubkey 对）均顺延 M62+；连同运维篮子（证书/密钥轮换与落盘、follower 认证、metrics 端点 TLS、OTel/push exporter、每-sink rotation 覆盖、时延直方图）。

## `/batch` 随附 Diff 区间块（Milestone 62）

M61 的 `POST /batch` 四类槽中，Inclusion / kNN / Range 都对**单个**捆绑认证头自证；唯 **Diff** 不行：`ValidatorTracker::verify_diff_against_headers`（light.rs）要重放 `[1..=h₂]` 全区间以重算 added/dropped 划分，故 `verify_batch` 把 `blocks_in_range: &[(Block, Commit)]` 线程进 Diff 分支。M61 响应**不带**该区间——gossip 侧假设钱包 M22 头缓存自备，M61 沿用同假设并显式顺延。对尚未预同步的无状态客户端，这让 Diff 槽经 RPC 不可验。M62 补齐：`/batch` 额外随附 Diff 验证所需的 `[1..=h₂]` 区间，作第三行 hex `range_blocks=`。

**不变量保持**：`/batch` 仍骑门控 `[rpc]` 监听器（默认**关**）；gossip `Blocks` wire 字节逐字不变（内嵌 encode 臂委托新 `encode_blocks`）；追加响应行向后兼容（M61 客户端只读前两行不受影响），故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`，无 config / wire / 共识 / 状态根 / 依赖变更。客户端验证仍走**既有** `verify_batch`——现只需把响应随附的区间传入即可。

- **未截断区间产出（`net.rs`）**：新 `GossipNode::blocks_through(up_to)` 回 `[1..=up_to]`（height 序）的 `(Block, Commit)`——与 `batch_from` 不同**不**按 `MAX_BATCH=256` 截断（Diff 验证器重放整段前缀，短截断会使长链 Diff 不可验）；`up_to == 0` ⇒ 空，越界 clamp 到链尾。
- **`batch()` 随附区间（`net.rs`）**：返回改为三元组 `BatchReply = (CertifiedHeader, BatchResponseEnvelope, Vec<(Block, Commit)>)`；在把 `items` move 进 `serve_batch` 前先算请求中 Diff 项的最大 `h2`（无 Diff ⇒ 0 ⇒ 空区间），随附 `blocks_through(max_h2)`。`max_h2` 自然受当前高度约束（`serve_batch`/`serve_diff` 拒越界 / 退化 Diff）。
- **独立区间编解码（`net.rs`）**：`encode_blocks` / `decode_blocks` 从内嵌 `TAG_BLOCKS` 体格式提升为公有（u64 计数 + 每对长度前缀 `encode_block`/`encode_commit`），`encode_gossip` 的 `Blocks` 臂委托 encode（字节逐字一致）；`decode_blocks` **不**设 `MAX_BATCH` 上限（合法 Diff 区间可覆盖整链）且**不**按声明计数预分配——循环推进，谎报长度在首个缺失对即 `UnexpectedEof` 失败而非 OOM；内嵌 gossip 解码臂保留其 256 上限不动。
- **响应 + 句柄（`daemon.rs`）**：`format_batch` 加第三行 `range_blocks=<hex>`（无 Diff ⇒ `0000000000000000` 空编码）；`Cmd::QueryBatch` 与 `Node::batch_proof` 的回值改 `BatchReply` 三元组（经 `crate::light::BatchReply` 别名，避开 clippy `type_complexity`）；`/batch` 处理臂解构三元组。
- **测试（+3 → 376）**：`blocks_codec_round_trip`（纯，`net.rs`：区间往返稳定 + 字节与内嵌 gossip `Blocks` 尾逐字一致 + 空区间 + 谎报计数→`UnexpectedEof` 不 OOM）；`batch_includes_range_for_diff_and_verifies`（纯，`net.rs`：`certified_chain(2)` + `Diff{1,2}` → `batch()` 回区间 `len == 2`，`verify_batch(&range,…)` 过；无 Diff 批区间空）；`rpc_batch_diff_over_tcp`（tokio，`daemon.rs`：`[rpc]` 开、高度 ≥ 2，`POST /batch` 带 Diff 项 → `200` 体含三行、`range_blocks=` 经 `decode_blocks` 解出 `len == 2`）。另更新两处 M61 `batch_proof` 调用点解构三元组。

**已知边界（顺延至 M63+）**：超长 Diff 区间的响应体量封顶 / 分页（现未截断随附，是无状态 Diff 验证的诚实成本，仍受实际链高约束）、桥锁证明经 RPC（非 `BatchItem` 类，另有 `GetLock`/`Lock` 对）、明文（非证明）reviewer/验证人/图读、JSON/DTO 响应体、查询分页 / 范围扫描端点、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 桥锁证明经 RPC（Milestone 63）

M59–M62 把每类 `ProofKind` 的可验证读都搬上了 RPC——`/account/{id}/proof`（M59）、`/{reviewer,validator,graph}/{id}/proof`（M60）、批量 `POST /batch`（M61–M62，M62 起随附 Diff 区间而完全自证）——唯独**桥锁**仍只走 gossip（`GetLock`/`Lock`）。桥锁证明栈其余部件早已齐备：生产者 `GossipNode::serve_lock`、自足信封 `LockEnvelope`（`encode_lock_envelope`/`decode_lock_envelope`）、客户端验证器 `BridgeEndpoint::verify_lock`。M63 补上最后一环：只读 `GET /bridge/lock/{id}/proof` 返回自足的 `LockEnvelope`。

**比 M59/M60 证明路由更简**：账户/审阅人/验证人/图读返回**二元对** `(CertifiedHeader, ProofEntry)`——生产者 `inclusion` 另取认证头、客户端自备 tracked set 验证；桥锁不同，`LockEnvelope` **已自带** `source_header` + `source_cert` + `source_tracked_set` + lock + proof，完全自足。故 **net.rs 零改动**（`serve_lock` 早是 `pub` 且返回完整 `Option<LockEnvelope>`，actor 直呼之），响应体只一行 hex `lock_envelope=`（非两行）。

**不变量保持**：`/bridge/lock/{id}/proof` 骑既有门控 `[rpc]` 监听器（默认**关**）；`serve_lock` / `encode_lock_envelope` 早在且 `pub`；纯读、无新依赖、无 config / wire / 共识 / 状态根 / net.rs 变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。客户端验证走**既有** `BridgeEndpoint::verify_lock`。

- **路由 + 解析（`daemon.rs`）**：`GetRoute` 加 `BridgeLock(u64)`；`route_get` 加 `/bridge/lock/` 分支——十进制 id、必须 `/proof` 后缀（桥锁无明文读，裸 id ⇒ `NotFound`）。
- **命令 + 句柄（`daemon.rs`）**：`Cmd::QueryLock { lock_id, reply: oneshot::Sender<Option<LockEnvelope>> }`；`Node::lock_proof` 异步封装；actor 臂直呼 `serve_lock(lock_id)`（信封自足、无需另取头）。
- **格式器（`daemon.rs`）**：`format_lock` 产单行 `lock_envelope=<hex>`（`encode_lock_envelope`）。处理臂 200 / 404（`bridge lock {id} not found`）/ 503 同既有形。
- **测试约束**：活跃守护进程**造不出**桥锁——`mempool.build_block` 恒置 `bridge_locks: Vec::new()`、`SubmissionTx` 无桥锁变体、`net.rs` 注明桥锁“无待打包池”；桥锁只经驱动造块 + `load_certified` 入链。故 live `Node::start` 链永无锁，TCP 测不能复现 200——可验 200 改以进程内驱动造链覆盖（同 `net.rs` `serve_lock_and_lock_envelope_round_trip`），TCP 测覆盖路由/门控/404。
- **测试（+3 → 379）**：`route_get_parses_bridge_lock`（纯：`/bridge/lock/0/proof` → `BridgeLock(0)`、裸 id / 非数字 / 空 id ⇒ `NotFound`）；`lock_proof_formats_and_verifies`（进程内：驱动造载锁链 → `serve_lock(0)` → `format_lock` → 拆 `lock_envelope=` → `decode_lock_envelope` → `BridgeEndpoint::new` + `follow_source` + `verify_lock` 过）；`rpc_lock_proof_over_tcp`（tokio：`[rpc]` 开、无锁链 → `GET /bridge/lock/0/proof` → `404`、裸 `/bridge/lock/0` → `404`）。

**已知边界（顺延至 M64+）**：桥锁 id 枚举 / 列举路由（现调用方须已知 `u64` id，id 自 0 单调、`bridge_lock_heights` 是唯一索引）、明文（非证明）reviewer/验证人/图读、JSON/DTO 响应体、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 桥锁 id 枚举经 RPC（Milestone 64）

M63 开放了单锁证明 `GET /bridge/lock/{id}/proof`，但留了一道显式缺口：**调用方须已先知道 `u64` lock id**。lock id 自 0 单调（`ChainState::next_lock_id`），`bridge_lock_heights` 是唯一的 id→元数据索引，却没有任何路由枚举它们——想审计一条链全部出站跨链锁的客户端无从发现有哪些 id 存在。M64 闭合之：只读 `GET /bridge/locks` 列出全链每把桥锁（id + 高度 + 锁自身字段），客户端据此发现 id、再逐个走 M63 证明路由取回可验证信封。

**明文读（非证明），同 M58 `/account/{id}`**：这是未经验证的**目录**数据，不是证明——空链仍回 `200`（空目录是合法答，异于 M63 proof 路由对未知 id 的 `404`）。骑既有门控 `[rpc]` 监听器（默认**关**）。

- **生产者（`net.rs`）**：`GossipNode::lock_listing` 读 `bridge_locks` ∪ `bridge_lock_heights` 成 `(id, height, lock)`（`BTreeMap` 迭代即 id 序）；复用 M63 `serve_lock` 已证的字段路径与可见性。目录非平凡故置于 net.rs（对照 M58 `QueryAccount` 的 actor 内联读），便于单测。
- **类型别名（`light.rs`）**：`pub type LockListing = Vec<(u64, u64, crate::BridgeLock)>;`——生产者、`Cmd`、`Node` 句柄共用一名，避 `clippy::type_complexity`（同 M62 `BatchReply` 手法）。
- **命令 + 句柄（`daemon.rs`）**：`Cmd::QueryLocks { reply: oneshot::Sender<LockListing> }`；`Node::lock_listing` 异步封装；actor 臂直呼 `lock_listing()`。
- **路由（`daemon.rs`）**：`GetRoute` 加 `BridgeLocks`；`route_get` 以**精确匹配** `"/bridge/locks"` 分流（`…lock` 后是 `s` 非 `/`，不撞 M63 `/bridge/lock/` 前缀；`/bridge/locks/` 带尾斜杠落回 M58 健康探针）。
- **格式器（`daemon.rs`）**：`format_lock_listing` 每锁一行 `lock_id=… height=… account=… amount=… dest_chain=… dest_account=… nonce=…`（空链 ⇒ 空串）。处理臂恒 `200`（send 失败 ⇒ `503`）。
- **不变量保持**：纯读、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。
- **测试（+3 → 382）**：`route_get_parses_bridge_locks`（纯：`/bridge/locks` → `BridgeLocks`、`/bridge/locks/` → `Health`、M63 单锁路由无回归）；`lock_listing_lists_and_formats`（进程内：驱动造两锁链 → `lock_listing()` 得 id `0,1` + 正确高度/字段、`format_lock_listing` 出两行；空链 ⇒ 空 listing、空串）；`rpc_bridge_locks_over_tcp`（tokio：`[rpc]` 开、无锁链 → `GET /bridge/locks` → `200` 空体，证路由/门控/空-200）。

**已知边界（顺延至 M65+）**：计数 / 分页端点或超长锁列表的游标（现 listing 无界，仅受追加式链史约束）、明文（非证明）reviewer/验证人/图读、JSON/DTO 响应体、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 明文实体读经 RPC（Milestone 65）

M58 给账户开了明文（非证明）读 `GET /account/{id}`，M60 又给 reviewer / 验证人 / 图节点开了**可验证**读 `GET /{reviewer,validator,graph}/{id}/proof`，却独缺这三类的明文姊妹读：`route_get` 的三条实体分支一律委托 `proof_route`，裸 `{id}`（无 `/proof` 后缀）一概 `404`。想只瞥一眼审阅人声誉、验证人权重、图节点字段的客户端——不要 SPV 往返——没有廉价读，尽管账户姊妹自 M58 就有。M65 闭合这道不对称：`GET /reviewer/{id}`、`GET /validator/{id}`、`GET /graph/{id}` 回实体字段的 `key=value` 明文（未验证、`200`/`404`），逐字镜像 `GET /account/{id}`。

**明文读（非证明），同 M58 `/account/{id}`**：返回未验证文本；要零信任自证的客户端仍走 M60 `/proof`。骑既有门控 `[rpc]` 监听器（默认**关**）。

- **路由（`daemon.rs`）**：`proof_route` 更名 `entity_route` 并加裸-id 臂——`strip_suffix("/proof")` 有则 M60 `GetRoute::Proof(kind,id)`、无则新 `GetRoute::Plain(ProofKind,id)`；三条实体分支由 `proof_route` 改呼 `entity_route`，非数字/空 id 两形皆 `404`。M60 `/proof` 路由逐字不变，仅 `entity_route` 增一臂。
- **命令 + 句柄（`daemon.rs`）**：`Cmd::QueryEntity { kind, id, reply: oneshot::Sender<Option<EntityView>> }`；`Node::entity(kind,id)` 异步封装；actor 臂**就地**读 `state.reviewers`/`state.validators`/`state.graph.nodes`（与 M58 `Cmd::QueryAccount` 同形的 inline 读，无新 net.rs 产出——三类皆简单查表）。
- **类型 + 格式器（`daemon.rs`）**：`EntityView` 三变体（Reviewer{id,reputation} / Validator{id,power,pubkey} / GraphNode{node_id,domain,embedding}）+ `format_entity` 一个 kind-无关渲染器——reviewer→`kind=reviewer id=… reputation=…`、validator→`kind=validator id=… power=… pubkey=<hex>`、graph→`kind=graph node_id=… domain=… dim=… embedding=<逗号接 f32>`；account 保留自有更丰快照，其余三类共用一视图一格式器。处理臂 `GetRoute::Plain` 查得 `200`、查不到 `404`（体由 `proof_kind_label` 区分）、actor 停 `503`。
- **不变量保持**：纯读、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。
- **测试（+3 → 385）**：`route_get_parses_plain_entities`（纯：`/reviewer/10`→`Plain(Reviewer,10)`、`/validator/21`→`Plain(Validator,21)`、`/graph/0`→`Plain(GraphNode,0)`、M60 `/reviewer/10/proof`→`Proof(Reviewer,10)` 无回归、`/reviewer/x` 与 `/graph/`→`NotFound`）；`format_entity_renders`（纯：每 kind 一 `EntityView` → 断言 `key=value` 体）；`rpc_plain_entities_over_tcp`（tokio：`test_genesis` 实载 reviewers/validators/graph 故真实 TCP 可断言带数据 `200`——`/reviewer/10`→`reputation=1`、`/validator/21`→`power=1`、`/graph/0`→`node_id=0`、`/reviewer/999`→`404`）。另更新 M60 `rpc_other_proofs_over_tcp` 的裸-id 断言（`/validator/21` 由 `404` 改为 `200`+`kind=validator id=21`，裸 id 现为明文读）。

**已知边界（顺延至 M66+）**：JSON/DTO 响应体、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## JSON 响应体（Milestone 66）

M58–M65 的每条明文（非证明）读——`GET /height`、`/head`、`/account/{id}`、`/reviewer|/validator|/graph/{id}`（M65）、`/bridge/locks`（M64）——都只回一种对 grep / curl 友好的 `key=value`（或裸 token）纯文本。对 shell 读者完美，对程序化客户端却尴尬：须自行再解析临时文本。M66 加**第二种、选择性启用**的渲染：在这些**结构化**读的 URL 后缀 `?format=json`，响应体即回手写极简 JSON；省略（或 `?format=text`）则响应与今日**逐字节相同**。

手写因本项目**不引入新 node 依赖**（无 `serde_json`）——与手写 HTTP（`http_response`）、手写参数解析同一纪律。纯响应渲染变更，骑既有门控 `[rpc]` 监听器（默认**关**），不碰共识 / 出块 / 状态根 / gossip-wire 任一路径。

- **查询拆分 + 格式解析（`daemon.rs`，纯）**：`split_query(target)` 在首个 `?` 处拆 `(path, query)`（无 `?`→`("<path>","")`；请求行 target 按 RFC 9112 无 `#fragment` 故刻意不拆 `#`）——须在 `route_get` **之前**拆，否则 `/account/10?format=json` 会解析成 `NotFound`。`enum RespFormat { Text, Json }`；`response_format(query)` 扫 `&`-对，精确大小写敏感 `format=json`⇒`Json`、首个 `format=` 胜、其余（缺省 / 空 / `format=text` / 未知）⇒`Text`。
- **内容类型感知应答（`daemon.rs`）**：`http_response` 委托新 `http_response_ct(status,content_type,body)`、以历史 `text/plain; charset=utf-8` 调用故输出逐字不变；JSON 体用裸 `application/json`（JSON 恒 UTF-8、无 `charset` 参）。
- **手写 JSON 渲染器（`daemon.rs`，皆纯）**：`json_str` 引号 + 转义 `"` `\` 与全部 U+0000–U+001F 控制符（`\n \r \t` 加 `\u00XX` 兜底，依 RFC 8259）；`json_u64(n)`→`json_str(&n.to_string())`（**按用户决定编码为 JSON 字符串**、任意消费者无损、链 RPC 惯例）；`json_f32(x)`→有限时 `x.to_string()`、否则 `null`（非有限声誉/嵌入绝不吐非法 token）；`json_height`/`json_head`；`json_account`（u64 字段皆字符串、`pubkey` hex 字符串）；`json_entity`（每 kind 带 `"kind"` 判别符：reviewer `{kind,id,reputation}`、validator `{kind,id,power,pubkey}`、graph `{kind,node_id,domain,dim,embedding}`，embedding 为 `json_f32` 的 JSON 数组）；`json_lock_listing`（JSON **数组**、空时 `[]`——与文本渲染器 `""` 的唯一刻意分歧）。
- **五条结构化臂穿入 `RespFormat`（`daemon.rs`）**：两个小助手 `ok_body(fmt,text,json)`（`200` 按 fmt 选 `http_response`/`http_response_ct`）、`not_found_body(fmt,msg)`（Text→纯文本 msg、Json→`{"error":"<msg>"}`）。Height/Head/Account/Plain/BridgeLocks 五臂 `200`（Account/Plain 的 `Ok(None)` 404 走 `not_found_body`）。503（`node stopped`）、`Health`、通用 `NotFound` 及全部 hex / proof 臂（`AccountProof`/`Proof`/`BridgeLock`/`POST /batch`）**保持纯文本**——`?format=json` 于其上静默回 hex 文本（文档化）；故*路由* 404（`/account/abc?format=json`→`NotFound`）回文本 `not found`，而*数据* 404（`/account/999?format=json`）回 `{"error":…}`（刻意不对称、文档化）。
- **不变量保持**：纯响应渲染、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`（RPC 默认关）。
- **测试（+7 → 392）**：`split_query_splits`、`response_format_parses`（含首个 `format=` 胜、大小写敏感、非首位 `format=json`）、`json_escapes_control_chars`（含 `NaN`/`inf`→`null`）、`json_account_renders`（`u64::MAX` 以字符串无损往返）、`json_entity_renders`（每 kind 带判别符、validator `power` 为字符串、graph `embedding` 为 JSON 数组）、`json_lock_listing_renders`（空→`[]`、单锁→单元素数组）；tokio `rpc_json_reads_over_tcp`（`test_genesis`、`[rpc]` 开、独立端口：`?format=json` 于 `/height`/`/account/{id}`/`/reviewer/10`/`/bridge/locks`→`200`+`application/json`+预期 JSON 子串、`/reviewer/999?format=json`→`404`+`{"error"`，无查询 `/height`→断言逐字节同今日 `text/plain` 裸十进制=响应层 head 不变量守卫）。

**已知边界（顺延至 M67+）**：hex / proof 读（`/…/proof`、`POST /batch`）与 `POST /submit_tx` 回执的 JSON、`Accept:` 头内容协商、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## JSON 响应体扩展至证明/提交读（Milestone 67）

M66 把 `?format=json` 加到**结构化**明文读，但 hex/proof 读——`GET /account/{id}/proof`、`GET /{reviewer,validator,graph}/{id}/proof`、`GET /bridge/lock/{id}/proof`、`POST /batch`——与 `POST /submit_tx` 回执仍只回文本：其臂直接 `http_response("200 OK", …)` 并忽略已算好的 `fmt`。想要 JSON 余额的程序化客户端须为这些端点特判回临时文本解析。M67 收口：**每条** RPC 读都提供同一双渲染——附 `?format=json` 即回手写极简 JSON，省略（或 `?format=text`）则响应与今日**逐字节相同**。

这些端点回的信封（`CertifiedHeader`、`ProofEntry`、`LockEnvelope`、`BatchResponseEnvelope`、区间块）是**不透明、自验证**的 blob，客户端以既有 `verify_proof_against_header` / `verify_batch` / `verify_lock` 栈解码。故 JSON 渲染保持它们为具名字段里的**不透明 hex 字符串**——与文本行同一字节、仅挪进 JSON 对象。无新解码、不暴露内部证明结构、无序列化分歧风险，延续 M66 纪律（u64→字符串、手写、**无 `serde_json`**）。

- **通用错误体（`daemon.rs`，纯）**：把 M66 的 `not_found_body` 泛化为状态无关的 `error_body(fmt, status_line, msg)`——Text→裸文本 msg、Json→`{"error":"<msg>"}`；`not_found_body(fmt,msg)` 委托 `error_body(fmt,"404 Not Found",msg)` 行为不变，让 `400`/`422` 也能回 JSON 错误体。
- **三个 hex 包装 JSON 渲染器（`daemon.rs`，皆纯）**：`json_account_proof(ch,entry)`→`{"certified_header":"<hex>","proof_entry":"<hex>"}`（AccountProof 与 Proof 共享）、`json_lock(env)`→`{"lock_envelope":"<hex>"}`、`json_batch(ch,env,range)`→`{"certified_header":…,"batch_envelope":…,"range_blocks":…}`，各以 `json_str(&hash::hex(&encode_*(…)))` 逐字段镜像其 `format_*` 姊妹；`{"hash":…}` 提交回执无需渲染器、经 `json_str` 内联。
- **四条文本臂 + 两条 POST 回执穿入 `fmt`（`daemon.rs`）**：AccountProof/Proof `Some`→`ok_body(fmt, format_*, json_account_proof)`、`None`→`not_found_body`；BridgeLock 同构用 `json_lock`；`POST /batch` `Ok(Some)`→`ok_body(fmt, format_batch, json_batch)`、`Ok(None)`→`error_body(fmt,"422…","batch rejected")`、解码 `Err`→`error_body(fmt,"400…",e)`；`POST /submit_tx` 成功→`ok_body(fmt, hex, {"hash":<hex>})`、拒绝→`error_body(fmt,"422…",e)`、解码 `Err`→`error_body(fmt,"400…",e)`。
- **刻意保持纯文本**：纯框架/传输错误（`411` 缺 content-length、`413` 过大、`400 truncated body`、`431` 头、`503 node stopped`、`Health`、路由 `NotFound`）无语义体，与 M66 同一冻结边界（数据错误 JSON、框架/路由文本）。`fmt` 在 `serve_rpc_conn` 顶部 GET/POST 分发前算一次，故 POST 目标也认 `?format=json`、无需新触发机制。
- **不变量保持**：纯响应渲染、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`（RPC 默认关）。
- **测试（+3 → 395）**：`error_body_renders`（纯：Text→裸 msg+`text/plain`、Json→`{"error":…}`+`application/json`、非-404 状态如 `422` 透传、`not_found_body` 仍 `404`）、`json_proof_renderers_render`（纯：经 `ChainDriver` 造带锁认证链、`account_inclusion`/`serve_lock`/`batch` 产真实信封、断言各 JSON 嵌入其 `format_*` 姊妹的逐字 hex）、`rpc_json_proofs_over_tcp`（tokio：`test_genesis`、`[rpc]` 开、独立端口：`/account/1/proof?format=json`→`200`+`{"certified_header":"`、`/reviewer/10/proof?format=json`→`"proof_entry":"`、`/bridge/lock/1/proof?format=json`→`404`+`{"error"`、`POST /submit_tx?format=json` 有效 tx→`{"hash":"<hex>"}`、畸形体→`400`+`{"error"`、`POST /batch?format=json`→`200`+`{"certified_header":"`、无查询 `/account/1/proof`→逐字节同今日 `text/plain` `certified_header=` 行）。

**已知边界（顺延至 M68+）**：证明信封的结构化（解码）JSON（对不透明 hex 字符串）、`Accept:` 头内容协商、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## Accept 头内容协商（Milestone 68）

M66/M67 的 `?format=json` 是**非标准、项目自造**的触发器：想要 JSON 的通用 HTTP 客户端（或按惯例设 `Accept: application/json` 的库）无从在不特判 URL 的情况下请求 JSON。M68 加**标准 HTTP 内容协商**——客户端可用 `Accept: application/json` 请求头作为查询参数的替代触发 JSON。

优先级取最直觉、最少意外的次序：**显式 `?format=` 查询胜**（最具体、最刻意的信号）、**否则 `Accept` 头**、**否则文本默认**。两者皆无（既无显式 `format=`、`Accept` 也不含 `application/json`）时仍落 Text，故每条既有响应**逐字节不变**；且 `[rpc]` 监听器默认**关**，故 `localnet` head 恒不受影响（`44309755…ea04ba`）。纯响应渲染变更：无 wire / 共识 / 状态根 / 依赖触碰、无 `serde_json`（头扫描手写、仿既有 `parse_content_length`）。

- **`response_format` 返 `Option<RespFormat>`（`daemon.rs`）**：令"无 `format=` 参数"可与"`format=text`"区分，优先级规则需要此信息——`None` = 查询无 `format=` 对（落 Accept）、`Some(Json)`/`Some(Text)` 语义同 M66（首个 `format=` 胜、非 `json` 值仍 ⇒ Text）。
- **新 `accept_format(head) -> Option<RespFormat>`（`daemon.rs`，纯，仿 `parse_content_length`）**：按 `\r\n` 拆头块、`split_once(':')`、大小写不敏感匹配 `accept`；值含 `application/json` ⇒ `Some(Json)`、任何其他显式 Accept ⇒ `Some(Text)`（text/plain 是我们的通用默认表示、未知媒体类型**不**回 `406`，且 `*/*` / `application/*` 不含该字面量故保持文本——浏览器的 `Accept: text/html,…,*/*` 仍得逐字节明文）、无 `Accept` 头 ⇒ `None`。刻意极简：子串测试、无 RFC 7231 q 值加权（顺延）。
- **新 `resolve_format(query, head) -> RespFormat`（`daemon.rs`，纯，编码优先级）**：`response_format(query).or_else(|| accept_format(head)).unwrap_or(RespFormat::Text)`。
- **分发点一行换（`daemon.rs`，~2302）**：`let fmt = response_format(query);` → `let fmt = resolve_format(query, head);`（`head` 已在作用域内）。下游全不动——`fmt` 仍穿入同一批 `ok_body`/`error_body`/`not_found_body`，GET/POST 皆受益。
- **不变量保持**：纯响应渲染、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`（RPC 默认关）。
- **测试（+3 → 398）**：`response_format_parses`（更新为 `Option`：缺省 / 未知键→`None`、`format=`→`Some(Text)`、首个胜、大小写敏感）、`accept_format_parses`（`application/json`→`Some(Json)`、小写名 `accept: text/plain`→`Some(Text)`、`*/*`→`Some(Text)`、含 json 于多类型→`Some(Json)`、无 Accept 行→`None`，以原始头块字面量构造仿 `parse_content_length_parses_and_caps`）、`resolve_format_precedence`（查询胜过 Accept、无查询时 Accept 决定、两者皆无→Text）；tokio `rpc_accept_header_over_tcp`（`test_genesis`、`[rpc]` 开、独立端口、`get` 辅助加 `accept: Option<&str>` 注入 `Accept:` 行：`/height` 带 `Accept: application/json` 无查询→`200`+`application/json`+`{`、`/account/1` 同→`{"`、`/height?format=text`+Accept json→`text/plain`+裸十进制（查询胜）、`/height` 带 `Accept: text/html,*/*`→`text/plain`（浏览器不受扰）、无 Accept 无查询 `/height`→逐字节同今日 `text/plain` 裸十进制=响应层 head 不变量守卫）。

**已知边界（顺延至 M69+）**：RFC 7231 `Accept` q 值加权 / 多类型偏好排序、`406 Not Acceptable`（不可满足的 Accept）、`Accept-Charset`/`Accept-Encoding`、证明信封的结构化（解码）JSON、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## Accept 头 q 值加权与 406（Milestone 69）

M68 的 `accept_format` 走一个**无加权子串测试**（`value.contains("application/json")`），且对无法满足的 `Accept` **从不回 `406`**、一律回退文本。两个缺口随之而来：`Accept: text/plain;q=0.9, application/json;q=0.1` 被误判为 JSON（子串匹配忽略权重），而 `Accept: application/xml` 静默回文本而非示意不匹配。M69 两处收口——按 **RFC 7231 §5.3 q 值**在我们能产出的两种表示（`application/json` 与 `text/plain`）之间择优，并在客户端的 `Accept` 把两者都排除时回 **`406 Not Acceptable`**。

优先级与 head 不变量不变：显式 `?format=` 查询仍胜（且**永不** `406`——那是我方参数）、无 `Accept` 头的请求仍落 Text（逐字节同 M68 前），`[rpc]` 监听器默认**关**故 `localnet` head 不受影响（`44309755…ea04ba`）。纯响应协商变更：无 wire / 共识 / 状态根 / 依赖触碰、无 `serde_json`。**每条既有 M68 `accept_format` 行为均保持**（逐例核验）。

- **新 `parse_qmilli(s) -> u16`（`daemon.rs`，纯）**：把 RFC q 值（`0.000`–`1.000`）解析成定点**毫单位** `0..=1000`，使一切比较皆整数——绕开 `clippy::float_cmp`（项目以 `-D warnings` 跑 clippy）。缺省 / 非法值 ⇒ `1000`（q=1.0，RFC 默认权重）、越界 `clamp`。解析 `f32` 本身不触发 lint（只有浮点 `==` 才触发，而我们从不比较浮点、结果是整数）。
- **新 `media_match(accept_value, ty, sub) -> (u16, u8)`（`daemon.rs`，纯）**：对某一具体类型（如 `application`/`json`）扫逗号分隔的媒体范围，返**最具体**匹配范围的 `(q_milli, 特异度)`——特异度 `3` = 精确 `type/subtype`、`2` = `type/*`、`1` = `*/*`、`0` = 不匹配；同特异度时取更高 q（RFC：最具体引用胜、特异度并列破为高 q）。
- **`accept_format(head) -> Negotiation`（`daemon.rs`，重写）**：三态结果 `enum Negotiation { Absent, Use(RespFormat), NotAcceptable }` 使调用方能区分「无 Accept」（⇒ 默认 Text）与「有 Accept 但不可满足」（⇒ `406`）。对 json 与 text 各取 `media_match`，两者 q 皆 `0` ⇒ `NotAcceptable`；否则高 q 胜、**精确 q 并列时仅当 json 被命名（spec ≥ 2）才偏 json**——故 `application/json, text/plain` ⇒ Json 而 `*/*` ⇒ Text。
- **`resolve_format(query, head) -> Option<RespFormat>`（`daemon.rs`，重写）**：`None` 即 `406`（查询优先级保持、查询永不 `406`）——`response_format(query)` 有值即返之，否则按 `Negotiation` 映射 `Absent⇒Some(Text)`/`Use(f)⇒Some(f)`/`NotAcceptable⇒None`。
- **分发点 `406` 早返（`daemon.rs`，~2382）**：`let fmt = match resolve_format(query, head) { Some(f) => f, None => { 写 http_response("406 Not Acceptable", …); return; } };`。单一 choke point ⇒ GET 与 POST 一体覆盖；`406` 体恒为 `text/plain`——客户端已声明不收我方类型，协商错误体无意义，文本是我们恒能产出的那一种表示。
- **不变量保持**：纯响应协商、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`（RPC 默认关）。
- **测试（+3 → 401）**：重写 `accept_format_parses`（→ `Negotiation` 五例，断言 M68 行为保持：`application/json`⇒`Use(Json)`、`text/plain`/`*/*`/`text/html, */*`⇒`Use(Text)`、`application/json, text/plain`⇒`Use(Json)`、无 Accept⇒`Absent`）、重写 `resolve_format_precedence`（→ `Option`：查询覆盖欲-`406` 的 Accept、`""`⇒`Some(Text)`、`application/xml`⇒`None`、`application/json`⇒`Some(Json)`）；新 `parse_qmilli_parses`（`"1"`/`"1.0"`→1000、`"0.9"`→900、`"0.333"`→333、`"0"`→0、`""`/`"abc"`→1000、`"2.0"`→1000 clamp、`"-1"`→0 clamp）、新 `accept_format_qvalues`（`text/plain;q=0.9, application/json;q=0.1`⇒`Use(Text)`、反序⇒`Use(Json)`、`application/json;q=0`/`application/xml`/`*/*;q=0`⇒`NotAcceptable`、`text/plain;q=0.2, application/*;q=0.9`⇒`Use(Json)` 特异度抬升）；tokio `rpc_406_over_tcp`（`test_genesis`、`[rpc]` 开、独立端口、`get` 辅助带 `accept: Option<&str>`：`Accept: application/xml`→`406`+`text/plain`、`application/json;q=0`→`406`、`text/plain;q=0.3, application/json;q=0.9`→`200`+`application/json`、`/height?format=json`+`Accept: application/xml`→`200`+`application/json`（查询胜）、无 Accept→逐字节明文=响应层 head 不变量守卫）。

**已知边界（顺延至 M70+）**：证明信封的结构化（解码）JSON、`Accept-Charset`/`Accept-Encoding`、`406` 体列出可用表示、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 406 列出可用表示（Milestone 70）

M69 在 `Accept` 排除我方两种表示时回 `406 Not Acceptable`，但响应体只是裸字符串 `not acceptable`——对客户端只字未提**本该**可接受的类型。RFC 7231 §6.5.6 要求 `406` 响应 SHOULD 携带「可用表示及其资源标识符的清单」。M70 收口：`406` 体枚举本服务能产出的两种媒体类型（`application/json`、`text/plain`），出自单一真源，使收到 `406` 的客户端能据以改请。

纯响应体变更、跑在已门控的 `[rpc]` 监听器上：无 wire / 共识 / 状态根 / 依赖触碰、无 `serde_json`。体仍 `text/plain`（客户端既已声明不收我方类型，协商**错误**体无意义，文本是我们恒能产出的那一种——与 M69 同理）。head 不变量不变：`[rpc]` 默认**关**故 `localnet` head 仍 `44309755…ea04ba`。

- **新 `OFFERED_MEDIA_TYPES: [&str; 2]`（`daemon.rs`，const）**：本服务能产出的表示，按协商偏好序（命名 JSON 赢 q 并列故列首）。`406` 体的单一真源。
- **新 `not_acceptable_body() -> String`（`daemon.rs`，纯）**：`format!("not acceptable; available: {}", OFFERED_MEDIA_TYPES.join(", "))` → `not acceptable; available: application/json, text/plain`（`[&str; N]` 解引为切片、`<[&str]>::join` 产清单）。
- **复用于 `406` 早返（`daemon.rs`，~2394）**：把字面体换成 `&not_acceptable_body()`，状态行、`text/plain` CT、flush、return、分发前置位置一概不变。
- **不变量保持**：纯 `&str`/`format!`、无新依赖、无 config / wire / 共识 / 状态根变更，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`（RPC 默认关）。
- **测试（+1 → 402）**：新纯 `not_acceptable_body_lists_representations`（断言体恰为 `not acceptable; available: application/json, text/plain`、含两类型、JSON 列于 text 之前）；扩既有 tokio `rpc_406_over_tcp`（两条 `406` 用例额外断言体 `.contains("application/json")`/`.contains("text/plain")` 且 CT 仍 `text/plain`，200 / 查询胜 / 无-Accept 用例不变守 head 不变量）。

**已知边界（顺延至 M71+）**：`Vary: Accept` 响应头、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`、证明信封的结构化（解码）JSON、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## Vary: Accept 头（Milestone 71）

M66–M70 让同一资源（`/height`、`/account/{id}`…）按请求的 `?format=` 查询或 `Accept` 头在 `text/plain` 与 `application/json` 间择一表示，但响应从不声明其体是**从请求中选出**的。RFC 7231 §7.1.4 要求服务端 SHOULD 发 `Vary`，列出其表示选择所依赖的请求头字段——此处即 `Accept`。缺了它，共享缓存/代理可能存下某客户端的 JSON 体再回放给文本客户端（或反之），送错表示。M71 收口：每条 RPC 响应携 `Vary: Accept`，缓存据此按 `Accept` 分键。

纯响应头变更、跑在已门控的 `[rpc]` 监听器上：无 wire / 共识 / 状态根 / 依赖触碰、无 `serde_json`。指标端点另有自己的响应构造器故保持字节稳定（Prometheus 抓取不要 `Vary`）。head 不变量不变：`[rpc]` 默认**关**故 `localnet` head 仍 `44309755…ea04ba`。

- **`http_response_ct(status, content_type, body)`（`daemon.rs`，~1780）加一行头字段**：在固定头块（`Content-Type`/`Content-Length`/`Connection: close`）里插 `Vary: Accept\r\n`。它是**唯一**的 RPC 响应构造器，`http_response` 委托它、`ok_body`/`error_body`/`not_found_body` 与各框架响应全经它，故每条响应一处加齐。
- **为何普适（而非仅协商臂）**：此处每条**内容**响应都是表示选出的，框架 / 健康 / `503` 响应只是共用这一构造器——给它们也标 `Vary: Accept` 无害且是标准服务端实践，单点改动保头集统一。真正不协商的指标端点有独立构造器、不动。
- **不变量保持**：一行字面头、无 `serde_json`、无 config / wire / 共识 / 状态根 / 依赖变更，仅 `[rpc]` 响应者（默认关）字节变，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。
- **测试（+1 → 403）**：新纯 `http_response_sets_vary_accept`（`http_response("200 OK","x")` 与 `http_response_ct("200 OK","application/json","{}")` 均 `.contains("\r\nVary: Accept\r\n")`，且仍 `.starts_with("HTTP/1.1 200 OK")`、各携其 `Content-Type`）；扩既有 tokio `rpc_json_reads_over_tcp`（协商 JSON `200` 与文本 `200` 各 `.contains("Vary: Accept")`）与 `rpc_406_over_tcp`（`406` 响应亦 `.contains("Vary: Accept")`）。

**已知边界（顺延至 M72+）**：机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`、证明信封的结构化（解码）JSON、桥锁目录计数 / 分页或超长列表游标、查询分页 / 范围扫描、RPC auth/TLS、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 桥锁目录分页（Milestone 72）

M64 的 `GET /bridge/locks` 目录一次性回全量 id 序清单——这是读面上唯一的无界列表：今天无害（活跃守护进程从不入锁故清单恒空），但链上锁一多，响应体便单调增长、无从索取一片。M72 加 opt-in `?offset=`/`?limit=` 分页让客户端取窗口，并落一个可复用的 `paginate` 原语供后续列表读。

纯查询参数 + 响应整形变更、跑在已门控的 `[rpc]` 监听器上：无 wire / 共识 / 状态根 / 依赖 / 引擎触碰、无 `serde_json`。**默认逐字节同旧**：无参（及无查询或仅 `?format=`）请求回与 M64/M66–M71 逐字节一致的体。head 不变量不变：`[rpc]` 默认**关**故 `localnet` head 仍 `44309755…ea04ba`。

- **新 `usize_param(query, key) -> Option<usize>`（`daemon.rs`，纯）**：从原始查询串取可选 `usize` 参数，首个匹配键的 pair 胜（仿 `response_format`），缺失键或非法值 ⇒ `None`。
- **新泛型纯 `paginate<T>(items, offset, limit) -> &[T]`（`daemon.rs`）**：offset/limit 窗口裁到边界、饱和算术防溢出（`start = offset.min(len)`、`end = limit.map_or(len, |l| start.saturating_add(l).min(len))`）；offset 越界 ⇒ 空、`limit=None` ⇒ 至末尾、`limit=Some(0)` ⇒ 空。泛型故未来列表读可复用。
- **`BridgeLocks` 臂接线（`daemon.rs`，~2528）**：取到 `listing` 后 `offset = usize_param(query,"offset").unwrap_or(0)`、`limit = usize_param(query,"limit")`、`page = paginate(&listing, offset, limit)`，再经既有 `format_lock_listing`/`json_lock_listing`（本就收 `&[(…)]`，空切片 ⇒ `""`/`[]`）渲染。
- **语义**：`?offset=M` 跳过 id 序前 M 条（缺省/非法 ⇒ 0）、`?limit=N` 截窗至 N 条（缺省/非法 ⇒ 无界，保默认体逐字节同）；`limit=0` 与 offset 越界皆回空窗但仍 `200`（空窗合法、一如 M64 空目录）；可自由叠加 `?format=json`；参数**仅**作用于 `/bridge/locks`，其余路由忽略故响应逐字节不变。
- **为何默认无上限**：给默认窗口设上限会改 M64 无参请求的体、破"新旋钮不用即逐字节同旧"的 head 不变量式保证；服务端最大页上限顺延。
- **不变量保持**：两个纯辅助 + 三行臂改、无 `serde_json`、无 config / wire / 共识 / 状态根 / 引擎变更，仅 `[rpc]` 响应者（默认关）在带参时字节变，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。
- **测试（+2 → 405）**：新纯 `usize_param_parses`（首键胜、非法/缺失/空值 ⇒ `None`）、`paginate_windows`（全量/空 limit/中段/尾段/offset 越界/limit 裁到 len/`usize::MAX` 不溢出）；扩既有 tokio `rpc_bridge_locks_over_tcp`（`?limit=1&offset=0` 与 `?format=json&limit=0` 在空链仍 `200` + 正确 CT + 空窗体，证参数解析 + 窗口路径不破帧；多条数据的窗口正确性由 `paginate_windows` 覆盖）。

**已知边界（顺延至 M73+）**：服务端最大页上限 / 默认 limit、其余列表读的通用 `?offset=`/`?limit=` 分页、超长列表的游标 / keyset 分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`、证明信封的结构化（解码）JSON、**货币费用**字段（= 共识 / wire 变更）、费用优先的出块排序、nonce / 序列号反重放（破 head 不变量）、`node keygen` 助手；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 离线密钥生成（Milestone 73）

M56 的 `encode-tx --key-file F` **消费**一个 64-char-hex 的 ed25519 种子文件来离线编写并签名交易，但节点此前无任何工具**产出**那个种子文件：用户只能手搓 64 个 hex 字符，或复用 `seed_for`/`demo_genesis` 的 demo 种子（这些并不保密）。M73 加离线 `node keygen --out F [--seed HEX]` 子命令闭合这一环——生成（或从给定种子确定性派生）一对 ed25519 密钥，把种子以 `encode-tx --key-file` 恰好期望的格式写出，并打印派生公钥以便贴进 genesis 的 `accounts` 条目。

纯离线 CLI，一如其余 `cmd_*` demo 与 `encode-tx`：不碰守护进程 / wire / 共识 / 状态根 / 引擎，**无新依赖**（`getrandom` 已是直接 node 依赖——`node/Cargo.toml:44`，`daemon.rs:299` 已用）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`）不受影响，因无任何联网或状态路径变更。

- **新纯 `keygen_derive(seed) -> (seed_hex, pub_hex)`（`main.rs`）**：对 32 字节种子返回 (64-hex 种子, 64-hex ed25519 公钥)；无 RNG / 无 I/O 故 keygen 的核心可确定性单测，种子 hex 正是 `encode-tx --key-file` 消费的那 64-char 形式。
- **新 `cmd_keygen(args)`（`main.rs`）**：`--out`（必填）；种子取用——有 `--seed <64hex>` 则复用 `config::decode_seed`（同 `encode-tx --key-file` 的解码 + BadHex 路径）确定性派生、无则经 `getrandom::getrandom` 取 32 字节 CSPRNG 种子；`keygen_derive` 后写裸 64-hex 到 `--out`（`encode-tx` 以 `.trim()` 容忍尾换行，故有无换行皆可往返）、打印 `pubkey <hex>` 与 `out <path>`。复用 `req_arg`/`opt_arg`/`fail`/`fail_msg`。
- **接线**：`main.rs` 调度加 `"keygen" => cmd_keygen(&args)`、`usage()` 加一行、模块头 doc 加一行。
- **不变量保持**：一个纯辅助 + 一个离线命令 + 一条调度臂 + usage/doc 文本，无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。
- **测试（+1 → 406）**：新纯 `keygen_derive_round_trips`——种子 hex 经 `config::decode_seed` 解回同一种子（证产出文件是合法 `--key-file`）、公钥 hex 等于 `hex(&Keypair::from_seed(seed).public())`、二次调用输出一致（确定性）、异种子异公钥；RNG + 文件系统是薄壳（与 `encode-tx` 把 `build_signed_tx` 从 `cmd_encode_tx` 拆出同理）。

**已知边界（顺延至 M74+）**：助记词 / BIP-39 短语、口令加密 keystore、直接产出现成 genesis `accounts` 条目、守护进程密钥轮换 / 落盘、keyfile 权限硬化（`0600`）；读面篮子：服务端最大页上限 / 默认 limit、其余列表读通用分页、游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`、证明信封的结构化（解码）JSON；共识 / wire 篮子（破 head 不变量）：**货币费用**字段 + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 结构化 JSON 证明信封（Milestone 74）

M66/M67 给证明 / 批量读加了 JSON 表示，但那是权宜之计：自验证信封被整体序列化成不透明 hex（`encode_certified_header` → `hex`）塞进命名 JSON 字符串字段。于是 `GET /account/{id}/proof?format=json` 今天回的是 `{"certified_header":"a1b2…（数百 hex 字符）…","proof_entry":"…"}`——一个 JSON 客户端仍须引入二进制 codec 才能读出区块高度、状态根、或哪些验证人签了最终性证书，背离了 JSON 读面的初衷。M74 开始**解码**这些信封为真正的 JSON 对象。

信封面很大（证明条目 + merkle 路径、批量条目、KNN/range/diff 声明、锁信封）。自然的第一切片是 **`CertifiedHeader`**——证明读（`json_account_proof`）与批量读（`json_batch`）**共享**的那个子信封。一次解码即为两个端点点亮认证区块头 + 最终性证书。仍不透明的部分（`proof_entry`/`batch_envelope`/`range_blocks`/`lock_envelope`）本里程碑保持 hex，顺延 M75+ 解码。

纯渲染，镜像 M66 的 `json_*` 家族，无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响，因无任何联网或状态路径变更——仅改变一个已产出的 `CertifiedHeader` 为 JSON 客户端**格式化**的方式。

- **五个新纯辅助（`daemon.rs`，紧邻 M67 渲染器）**，依赖序、均 `-> String` 纯函数，约定严格同现有家族：u64→`json_u64`；u32→`json_u64(x as u64)`（整数皆 lossless 引号串，如 `json_entity` 对 `domain` 已做）；`Hash`/`PubKey`/`Sig`→`json_str(hex(..))`；f32→`json_f32`；`Vec<T>`→`[ … ]`；枚举→串判别：
  - `json_validator_update(u)` → `{"id","pubkey","power"}`；
  - `json_vote(v)` → 含 `vote_type` 串判别（`"prevote"`/`"precommit"`，镜像 `json_entity` 的 `"kind"`）；
  - `json_commit(c)` → `{"height","round","block_hash","precommits":[…]}`；
  - `json_block_header(h)` → 区块头全 15 字段，hash 皆 hex 串，`validator_updates` 作 JSON 数组；
  - `json_certified_header(ch)` → `{"header":{…},"cert":{…}}`，替换 M67 两处读里的不透明 hex 块。
- **两处调用点换嵌入**：`json_account_proof` 与 `json_batch` 的 `certified_header` 字段从 hex 串改为 `json_certified_header(ch)`，其余字段（`proof_entry`/`batch_envelope`/`range_blocks`）仍 hex；`json_lock` 不携 `CertifiedHeader`，不动。
- **测试（+1 → 407）**：新纯 `json_certified_header_structured`——构造 `BlockHeader` + 单票 `Commit` 的 `CertifiedHeader` fixture，断言渲染为嵌套对象（含 `"height":"7"` 引号串、`"state_root":"abab…"` 64-hex、`"timestamp_days":1.5` 裸数、`"vote_type":"precommit"`、非空 `precommits`/`validator_updates` 数组、花括号平衡），并断言 `json_account_proof` 现嵌入结构化对象而 `proof_entry` 仍为 hex 串；连同更新 M67 既有两测（`json_proof_renderers_render`、`rpc_json_proofs_over_tcp`）的断言随新 shape。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M75+）**：其余信封的结构化解码——`proof_entry`（及其 typed leaf Account/Reviewer/Validator/GraphNode 与 `merkle::Proof` 步路径）、`batch_envelope`（`BatchResponseItem` Inclusion/Knn/Range/Diff 与 `KnnClaim`/`RangeClaim`/`DiffEnvelope`）、`range_blocks`、`lock_envelope`/`BridgeLock`；读面篮子：服务端最大页上限 / 默认 limit、通用 & 游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`；keygen 篮子：助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 结构化 JSON 证明条目（Milestone 75）

M74 解码了证明读与批量读**共享**的 `certified_header` 子信封，但证明读的另一半——`proof_entry`——仍是不透明 hex。于是 `GET /account/{id}/proof?format=json` 今天回 `{"certified_header":{…已结构化…},"proof_entry":"a1b2…（不透明 hex）…"}`：JSON 客户端能读出认证区块头，却仍须引入二进制 codec 才能读出被证明的**叶子**（账户余额、审阅人声誉、验证人权重、图节点嵌入）或 **Merkle 包含路径**。M75 解码 `proof_entry`——`json_account_proof` 的第二个（也是最后一个）字段——于是账户证明读与三条实体证明读（`/reviewer|/validator|/graph/{id}/proof`）**整体**变为结构化。批量读的 `batch_envelope`/`range_blocks`（其内各自嵌 `ProofEntry`）顺延 M76+。

纯渲染，新 `json_*` 辅助镜像 M66/M74 家族，无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响——仅改变一个已产出的 `ProofEntry` 为 JSON 客户端**格式化**的方式。

- **两个新纯辅助（`daemon.rs`，紧邻 M74 渲染器）**，均 `-> String` 纯函数，约定严格同现有家族：
  - `json_merkle_proof(p)` → `{"steps":[{"side":"left|right","hash":"<hex>"}, …]}`，每步一个兄弟哈希、按其并入侧（`Step::Left`/`Right`）标注 `side`（注意 `merkle::Step` 须走全路径引用，因 `daemon.rs` 已 `use crate::round::{… Step}`）；
  - `json_proof_entry(e)` → `{"kind":…, <叶子字段>, "proof":{…}}`：`kind` 判别符与叶子形状镜像 `json_entity`——`Account` 叶**复用 `json_account`**、`Reviewer`→`{id,reputation}`、`Validator`→`{id,power,pubkey}`、`GraphNode`→`{node_id,domain,dim,embedding[]}`，每支携其 `json_merkle_proof` 包含路径。
- **一处调用点换嵌入**：`json_account_proof` 的 `proof_entry` 字段从 hex 串改为 `json_proof_entry(entry)`，连同 M74 的 `json_certified_header` 使该读**整体**结构化、不再含 hex；`json_batch` 不动（其 `ProofEntry` 在仍 hex 的 `batch_envelope` 内）。
- **测试（+1 → 408）**：新纯 `json_proof_entry_structured`——每个变体各构一 fixture（含非空 `merkle::Proof { steps: [Left([0xAA;32]), Right([0xBB;32])] }`），断言各带正确 `"kind"` 标签与叶子字段（account 复用 `json_account` 的 `"balance"`/`"pubkey"`、reviewer→`"reputation"`、validator→`"power"`/`"pubkey"`、graph→`"domain"`/`"dim":"8"`/`"embedding":[`）、merkle 路径渲染为 `"proof":{"steps":[{"side":"left",…},{"side":"right",…}]}`、花括号平衡；连同更新三既有测（`json_proof_renderers_render` 的账户证明等式改组合 `json_certified_header`+`json_proof_entry`、`rpc_json_proofs_over_tcp` 的 reviewer 证明断言、M74 的 `json_certified_header_structured` 尾断言随 `proof_entry` 新 shape）。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M76+）**：其余信封的结构化解码——`batch_envelope`（`BatchResponseItem` Inclusion/Knn/Range/Diff 与 `KnnClaim`/`RangeClaim`/`DiffEnvelope`，各嵌 `ProofEntry`/`GraphNode`/`merkle::Proof`，现可复用 `json_proof_entry`/`json_merkle_proof`）、`range_blocks`、`lock_envelope`/`BridgeLock`；读面篮子：服务端最大页上限 / 默认 limit、通用 & 游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`；keygen 篮子：助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 结构化 JSON 批量信封（Milestone 76）

M74 解码了证明读与批量读**共享**的 `certified_header`、M75 解码了 `proof_entry`，于是证明读整体结构化。但批量读（`POST /batch?format=json`）的中间字段仍是不透明 hex：

```json
{"certified_header":{…M74…},"batch_envelope":"a1b2…（不透明 hex）…","range_blocks":"…hex…"}
```

JSON 客户端能读出认证区块头，却仍须引入二进制 codec 才能读出批量回答——包含条目、kNN / 范围邻居、时序 diff。M76 解码 `batch_envelope`——`BatchResponseEnvelope { items: Vec<BatchResponseItem> }`——对**全 4 种** `BatchResponseItem` 变体（Inclusion / Knn / Range / Diff），与 M75 一次解完 `ProofEntry` 全 4 变体对称。此后 `range_blocks` 是批量读最后的 hex 字段（→ M77），`lock_envelope` 仍 hex（→ M77+）。

纯渲染，新 `json_*` 辅助镜像 M66/M74/M75 家族，复用 `json_proof_entry`/`json_merkle_proof`/`json_block_header`/`json_commit`，无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响——仅改变一个已产出的 `BatchResponseEnvelope` 为 JSON 客户端**格式化**的方式。

- **新纯辅助（`daemon.rs`，紧邻 M75 渲染器）**，均 `-> String` 纯函数、约定严格同现有家族（`Option` → 空/未知槽渲染为裸 `null`、`usize` → `json_u64(x as u64)`）：
  - `json_embedding(&[f32])` → JSON 数字数组（各经 `json_f32`，非有限→`null`）；
  - `json_graph_node(gn)` → `{"node_id","domain","dim","embedding":[…]}`（嵌套形，区别于 `json_proof_entry` 扁平的 `"kind":"graph"` 叶）；
  - `json_graph_leaf(node_id, gn, proof)` → `{"node_id","graph_node":{…},"proof":{…}}`，kNN/范围邻居元组与 `DiffClaim` 的 `GraphLeafAtHeight` 共用；
  - `json_knn_claim`/`json_range_claim` → `{"query":[…],"k"|"min_sim",…,"neighbours"|"nodes":[graph_leaf…]}`；
  - `json_diff_claim` → `{"added":[…],"dropped":[…]}`、`json_validator`/`json_validator_set`（经 `.validators()`）；
  - `json_diff_envelope(e)` → 两头 + 两证书 + diff + 两 tracked 验证人集，复用 `json_block_header`/`json_commit`；
  - `json_batch_item(it)` → kind 判别（`inclusion`/`knn`/`range`/`diff`）、内体 `null` 或结构化；`json_batch_envelope(env)` → `{"items":[…]}`。
- **一处调用点换嵌入**：`json_batch` 的 `batch_envelope` 字段从 hex 串改为 `json_batch_envelope(env)`；`range_blocks` 仍 hex（块体结构化顺延 M77）。
- **测试（+1 → 409）**：新纯 `json_batch_envelope_structured`——构造含全 4 变体（Inclusion Some/None、Knn、Range、Diff）的信封 fixture（镜像 `codec.rs` 的 round-trip 测），断言各 `"kind"` 标签、`Inclusion(None)`→`{"kind":"inclusion","entry":null}`、图叶子嵌套 `graph_node` + merkle 路径、`knn` 带 `"k"`/`"query"`、`range` 带 `"min_sim"`、`diff` 带 `"header_prev":{"height"`/`"tracked_set_h1":[`/`"diff":{"added":[`、花括号平衡；连同更新 `json_proof_renderers_render` 的批量断言（`batch_envelope` 现为 `{"items":[`、`range_blocks` 仍 hex）。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M77+）**：其余信封的结构化解码——`range_blocks`（`[(Block, Commit)]` 区间：完整块体 txs / stake_ops / 罚没证据，自成一大面）、`lock_envelope`/`BridgeLock`；读面篮子：服务端最大页上限 / 默认 limit、通用 & 游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`；keygen 篮子：助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 结构化 JSON 锁信封（Milestone 77）

M76 解完 `batch_envelope` 后，读面只剩两处不透明 hex：批量读的 `range_blocks`，以及桥锁证明读（`GET /bridge/lock/{id}/proof`）的 `lock_envelope`：

```json
{"lock_envelope":"a1b2…（不透明 hex）…"}
```

JSON 客户端读不出是哪个账户把多少额度锁去哪条目的链，也读不出背书这把锁的认证源头 / 证书 / 跟踪验证人集 / merkle 路径——须引入二进制 codec。M77 解码 `lock_envelope`——`LockEnvelope { source_header, source_cert, source_tracked_set, lock_id, lock, proof }`——整体为结构化对象，彻底闭合桥锁证明读。此后 `range_blocks` 是读面最后的 hex 字段（→ M78）。

纯渲染，只一个真正新增的辅助（`json_bridge_lock`）加一个复用既有家族的信封渲染器，无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响——仅改变一个已产出的 `LockEnvelope` 为 JSON 客户端**格式化**的方式。`json_bridge_lock` 也是 `range_blocks`（M78）解 `Block.bridge_locks` / `BridgeRedeem.lock` 要用的渲染器，故此为自然的较小前置。

- **新纯辅助（`daemon.rs`，紧邻 M75/M76 渲染器）**，均 `-> String` 纯函数、约定严格同现有家族（u64 → `json_u64` 无损引号串、`Hash`/`Sig` → `json_str(hex)`）：
  - `json_bridge_lock(l)` → `{"account","amount","dest_chain","dest_account","nonce","signature"}`（全 6 字段含签名，完整保真——这是证明；区别于 `json_lock_listing` 扁平、省签名的目录形）；
  - `json_lock_envelope(env)` → `{"source_header":{…},"source_cert":{…},"source_tracked_set":[…],"lock_id",…,"lock":{…},"proof":{…}}`，复用 `json_block_header`/`json_commit`/`json_validator_set`/`json_merkle_proof`，仅 `lock` 需新渲染器。
- **一处调用点换嵌入**：`json_lock` 的 `lock_envelope` 字段从 hex 串改为 `json_lock_envelope(env)`；文本孪生 `format_lock` 仍 hex（一如历次 `format_*`）。
- **测试（+1 → 410）**：新纯 `json_lock_envelope_structured`——手搓一个 `LockEnvelope`（复用 M76 测的 `hdr`/`cert` 闭包、`ValidatorSet::new`、`BridgeLock` 字面量、两步 merkle `Proof`），断言一字段外壳、各子信封、锁叶子全 6 字段（含 `signature`）、merkle 左右步进、花括号平衡；连同把 `json_proof_renderers_render` 的锁断言从 hex 相等改为复用 `json_lock_envelope` 的组合相等（镜像其账户证明断言）。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M78+）**：其余信封的结构化解码——`range_blocks`（`[(Block, Commit)]` 区间：完整块体 `SubmissionTx`+`Review` / `StakeOp`+`BondKind` / `SlashEvidence` / `BridgeLock` / `BridgeHeader` / `BridgeRedeem`，自成一大面）；读面篮子：服务端最大页上限 / 默认 limit、通用 & 游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`；keygen 篮子：助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 结构化 JSON 区块区间（Milestone 78）

M77 解完 `lock_envelope` 后，整条读面只剩一处不透明 hex：批量读（`POST /batch?format=json`）里支撑时序-Diff 答案的 `range_blocks`——那段 `[(Block, Commit)]` 全量区块区间：

```json
{"certified_header":{…},"batch_envelope":{…},"range_blocks":"a1b2…（不透明 hex）…"}
```

JSON 客户端拿到每条批量声明都结构化了，却仍须引入二进制 codec 才能读出 Diff 所指的块体（`txs` / `stake_ops` / `slashing_evidence` / 桥接 ops）。M78 把 `range_blocks` 解码为 `[{"block":{…},"commit":{…}}, …]` 的数组，彻底结构化**整条**读面——此后任何 RPC JSON 读都不再携带不透明 hex 信封字段。

纯渲染，只为 `Block` 及其体类型新增 `json_*` 叶渲染器，全复用 M74–M77 家族（`json_block_header` 的子渲染器、`json_commit`、`json_validator_update`、`json_vote`、`json_bridge_lock`、`json_merkle_proof`、`json_validator_set`、`json_embedding`）。无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响——仅改变一个已产出的 `[(Block, Commit)]` 区间为 JSON 客户端**格式化**的方式；文本孪生 `format_batch` 仍 hex。

- **新纯辅助（`daemon.rs`，紧邻 M77 渲染器）**，均 `-> String` 纯函数、约定严格同现有家族（u64 → `json_u64`、u32/usize → `json_u64(x as u64)`、`Hash`/`Sig` → `json_str(hex)`、f32 → `json_f32`、`Vec<T>` → `[…]`、enum → 判别串）：
  - `json_review(r)` → `{"reviewer","score"}`；
  - `json_submission_tx(tx)` → `{"author","embedding":[…],"domain","stake","reviews":[…],"repl_success","repl_total","timestamp_days","signature"}`；
  - `json_stake_op(op)` → `{"account","kind":"bond"|"unbond","amount","signature"}`（`BondKind` 判别串）；
  - `json_slash_evidence(e)` → `{"vote_a":{…},"vote_b":{…}}`（复用 `json_vote`）；
  - `json_bridge_header(h)` → `source_chain` + `json_block_header` + `json_commit` + `json_validator_set`；
  - `json_bridge_redeem(r)` → `source_chain` + 头 + 证书 + `lock_id` + `json_bridge_lock` + `json_merkle_proof`；
  - `json_block(b)` → 8 头标量字段 + 7 体向量（各走其叶渲染器）。因 `Block` 持真正的体向量而非 `BlockHeader` 的 `*_commitment` digest，故 `json_block` 自成一个渲染器，不复用 `json_block_header`。
  - `json_range_blocks(range)` → `[{"block":{…},"commit":{…}}, …]`（空区间 → `[]`）。
- **一处调用点换嵌入**：`json_batch` 的 `range_blocks` 字段从 `json_str(hex(encode_blocks(range)))` 改为 `json_range_blocks(range)`。
- **测试（+1 → 411）**：新纯 `json_range_blocks_structured`——手搓一个体向量全非空的 `Block`（镜像 `codec.rs:sample_block` + 桥 headers/redeems 往返），配一个 `Commit`，断言外层数组外壳、块标量、`txs`（含嵌套 `embedding`/`reviews`）、`stake_ops`（两种 `BondKind`）、`slashing_evidence`、桥接三类 ops、配对 `commit` 与花括号/方括号平衡；连同把 `json_proof_renderers_render` 的空区间断言从 `"range_blocks":"` 改为 `"range_blocks":[]`。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M79+）**：M78 后结构化 JSON 读面**已完整**，无证明信封再待解码。剩余篮子：读面篮子（服务端最大页上限 / 默认 limit、通用 & 游标分页、`total`/`next` 信封、机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`）；keygen 篮子（助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`）；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 分页 total/next 信封（Milestone 79）

M72 给唯一的列表读 `GET /bridge/locks` 加了 `?offset=`/`?limit=` 窗口，但响应仍是**裸列表**——窗口内的锁直接渲染（文本：每锁一行 `key=value`；JSON：裸 `[…]` 数组），没有任何分页元数据。客户端能请求一个窗口，却读不出**一共有多少把锁**，也判不出**是否还有下一页**——只能一直往后请求、盯着空窗口才知道到底了。M79 把窗口页裹进一个小信封，携 `total`（未切前的全量计数）与 `next`（取下一页该用的 offset，窗口到末尾时为无），让客户端**确定性翻页**。

纯渲染 + 一处 handler 算术。无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响——仅重塑一个 RPC 读响应，不涉出块。（此信封取代 M64/M72「裸列表 / 默认逐字节不变」的 `/bridge/locks` 形——那是 RPC 面承诺，非共识承诺。）

- **新纯辅助（`daemon.rs`，紧邻各自的逐项渲染器）**，均 `-> String` 纯函数、通用于任意列表读（契合 `paginate` 的「future endpoints reuse it」）、复用既有逐项渲染器原样作内层：
  - `format_page(items, total, next)` → 恒一行 `total={n}`；仅当还有下页才加 `next={off}` 行（其存在即 grep 友好的「还有下页」信号）；再接 item 行（空页时省略）。
  - `json_page(items, total, next)` → `{"total":"N","next":"M"|null,"items":[…]}`，`total`/`next` 走无损引号串约定，`next` 在末页为裸 `null`（与文本「省略 `next=` 行」为唯一分歧）。
- **`next` 语义**（offset 游标）：`next = Some(start + page.len())` 当且仅当其 `< total`，否则 `None`，`start = offset.min(total)`（镜像 `paginate` 对 offset 的内部钳制）。默认（无参）⇒ offset 0、满页 ⇒ `next = None`；`total = listing.len()`（窗口前全量）。
- **一处 handler 改算术**：`GetRoute::BridgeLocks` 在 `paginate` 后算出 `total`/`next`，`ok_body` 改调 `format_page(&format_lock_listing(page), …)` / `json_page(&json_lock_listing(page), …)`。
- **测试（+1 → 412）**：新纯 `lock_page_envelopes`——直接驱动 `format_page`/`json_page` 的首/中页（`next=Some`）、末页（`next=None`）、空页三态，断言信封外壳、`total`/`next` 引号串与 `null`、文本省略 `next=` 行；连同更新桥锁 TCP 测的三处空链断言（裸 `""`/`[]` → `total=0` / `{"total":"0","next":null,"items":[]}`）及 `rpc_json_reads_over_tcp` 的锁体断言（由裸数组改为信封）。
- **不变量保持**：纯渲染增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M80+）**：读面篮子剩余：机读（JSON）`406` 体、`Accept-Charset`/`Accept-Encoding`（压缩须引新依赖）、服务端最大页上限 / 默认 limit、其余列表读通用 & 游标分页；keygen 篮子（助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`）；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 机读 JSON 406 体（Milestone 80）

自 M66 起，RPC 读面被逐步做成机读：每个 `200` 体、每个语义错误（`400`/`404`/`422`）都已遵从 `?format=`/`Accept` 并能应 `application/json`（`error_body`/`not_found_body` → `{"error":…}`）。**唯一**还硬编码为纯文本的响应是 `406 Not Acceptable`：内容协商失败时 `not_acceptable_body()` 返字面句子 `not acceptable; available: application/json, text/plain`、`Content-Type: text/plain; charset=utf-8`。命中 406 的机读客户端（它的 `Accept` 排除了两种表示）得字符串解析一句人话才知道能换请什么。M80 把 406 体做成结构化 `{"error":"not_acceptable","available":["application/json","text/plain"]}`、`Content-Type: application/json`，补齐整条读面的内容协商对称。

纯渲染 + 一处 emit 点替换。无新读 / wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响。

- **新纯辅助 `not_acceptable_json()`（`daemon.rs`，取代 `not_acceptable_body`）**：从单一真相源 `OFFERED_MEDIA_TYPES`（JSON 优先）以 `json_str` 逐项转义渲染出 `{"error":"not_acceptable","available":[…]}`。
- **恒 JSON 之由**：406 **只**在显式不可满足的 `Accept`（`application/xml`、`application/json;q=0`、`*/*;q=0`）触发，故没有「客户端想要 JSON 却拿到 406」的偏好信号可供协商错误体——406 体因此**恒为 JSON**（务实的机读选择，RFC 7231 §6.5.6 仍因列出可用表示而满足）。无 `Accept` 的请求从不 406，故明文 head 不变量读路径不受触碰。
- **一处 emit 点替换**：`serve_rpc_conn` 中 `resolve_format` 返 `None` 的分支改写 `http_response_ct("406 Not Acceptable", "application/json", &not_acceptable_json())`——`http_response_ct` 仍自带 `Vary: Accept`，M71 行为保留。
- **测试（净 0 → 仍 412）**：纯测 `not_acceptable_body_lists_representations` 重写为 `not_acceptable_json_lists_representations`，断言体恰为 `{"error":"not_acceptable","available":["application/json","text/plain"]}`、含两种媒体类型、JSON 先于 text；`rpc_406_over_tcp` 两条 406 路径（`application/xml`、`application/json;q=0`）的 `Content-Type` 断言由 `text/plain; charset=utf-8` 改为 `application/json`，保留 `Vary: Accept` 与两类型并列断言、加 `"error":"not_acceptable"` 断言。
- **不变量保持**：纯渲染替换、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M81+）**：读面篮子剩余：`Accept-Charset`/`Accept-Encoding`（压缩须引新依赖）、服务端最大页上限 / 默认 limit、其余列表读通用 & 游标分页；keygen 篮子（助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`）；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

## 页大小上限与默认 limit（Milestone 81）

M72 给唯一的列表读 `GET /bridge/locks` 加了 `?offset=`/`?limit=` 窗口，M79 又把窗口页裹进 `total`/`next` 信封。但 `?limit` 还留着**两道口子**：其一，**缺省** `?limit` 返**整表**——`paginate` 把 `limit == None` 当「走到末尾」，省掉参数的客户端一次就把全量未切列表捞回来，没有默认页大小；其二，**显式** `?limit` 除了被自然钳到列表长度外**无上界**，客户端永远能一次要走整张表。锁目录长大后，这两道口子都让单次 RPC 读返回**不设限的响应体**。M81 以一处纯辅助 + 两行 handler 算术堵上：缺省 `?limit` 回落 `DEFAULT_PAGE_LIMIT`、任何请求 `?limit` 钳到 `MAX_PAGE_LIMIT`。有效 limit 切顶时 `total`/`next` 信封照样标出余页，受限客户端仍能**确定性翻到末尾**。

这是一处 **RPC 面变更**（取代 M72/M79「缺省 `limit` ⇒ 整表」——那是 RPC 承诺，非共识承诺）。纯渲染 / 算术。无新读、无 wire / 共识 / 状态变更、**无新依赖**（JSON 一如既往手搓，无 `serde_json`）。`[rpc]`/`localnet` head 不变量（`44309755…ea04ba`，RPC 默认关）不受影响；唯一的分页端点在 TCP 测里跑空链，`total=0` 行为不变。

- **两个模块级常量（`daemon.rs`，紧邻 `paginate`）**：`DEFAULT_PAGE_LIMIT = 50`（缺省页大小）、`MAX_PAGE_LIMIT = 500`（请求 limit 的硬上界）。
- **新纯辅助 `effective_limit(requested: Option<usize>) -> usize`**：`None ⇒ DEFAULT_PAGE_LIMIT`；`Some(l) ⇒ l.min(MAX_PAGE_LIMIT)`；`Some(0)` 仍为 `0`（显式空页，`paginate` 原样兑现）。通用于任意列表读。
- **一处 handler 改**：`GetRoute::BridgeLocks` 把 `let limit = usize_param(query, "limit");` 换成 `let limit = Some(effective_limit(usize_param(query, "limit")));`——下游 `paginate`/`total`/`next` 算术逐字不变（limit 恒为 `Some`）。
- **测试（+1 → 413）**：新纯 `effective_limit_caps_and_defaults`——断言缺省回落默认、低于上界原样兑现、`Some(0)` 保持空页、等于/超过上界一律钳到 `MAX_PAGE_LIMIT`、`usize::MAX` 亦钳顶；常量以符号引用，调值不会锈蚀测试。桥锁 TCP 测跑空链（`total=0`）故默认/上界不显形，断言原样有效。
- **不变量保持**：纯算术增量、无 `serde_json`、无引擎 / codec / crypto / wire / 共识变更、无新依赖，故 `localnet` 逐字节同块、head 仍 `44309755…ea04ba`。

**已知边界（顺延至 M82+）**：读面篮子剩余：`Accept-Charset`/`Accept-Encoding`（压缩须引新依赖）、其余列表读通用 & 游标分页；keygen 篮子（助记词 / BIP-39、口令 keystore、现成 genesis 条目、密钥轮换、keyfile `0600`）；共识 / wire 篮子（破 head 不变量）：**货币费用** + 费用优先排序、nonce / 序列号反重放；连同运维篮子：证书/密钥轮换与落盘、follower 认证、指标端 TLS、OTel/push exporter、每-sink 独立 rotation 覆盖、时延直方图。

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
# Peer 发现 / 地址簿 gossip（M39，net.rs + config.rs + daemon.rs）
peers_gossip_round_trips                              Peers 地址簿 encode→decode 往返相等 + 空簿 + 超 MAX_PEERS 计数 → CodecError::TooManyItems
peer_exchange_can_be_disabled                         [network] enable_peer_exchange=false 解析生效，其余键回落 Default（默认断言测试另加 enable_peer_exchange==true 一条）
discovery_completes_partial_mesh                      链拓扑 21-22-23（21 只知 22、23 只知 22）开发现 → 节点 21 peer 数达 2（拨到从未在其配置里的 23）
peer_exchange_disabled_stays_seeded                   同链拓扑关发现 → 节点 21 钉在 1 peer（过一个 announce tick 也不泄露地址簿）
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`（含 `next_validators_root` 验证人集 Merkle 承诺 + **M23 `state_root` 完整共识状态 digest 与 `accounts_root` accounts/reviewers Merkle 根两条承诺根** + **M27 `graph_root` 按 (cos_sim(CANONICAL_PIVOT, *) desc, node_id asc) 排序的图节点 Merkle 根第三条承诺根** + **M30 `bridge_root` 累计 bridge_locks Merkle 根第四条承诺根** + **M30 `bridge_locks: Vec<BridgeLock>` 与 stake_ops/slashing_evidence 同列进 apply**）/`SubmissionTx`/`StakeOp`/`BridgeLock`/`SlashEvidence`（含 `hash()` 内容寻址用于 gossip 去重）/`Account`/`ChainState`（**M30 `bridge_locked: u64` 新供应组分、`bridge_locks: BTreeMap<u64,BridgeLock>` 累计映射、`bridge_lock_heights: BTreeMap<u64,u64>` 锁-块高映射、`next_lock_id: u64` 单调计数器**）/`Chain`、`apply_block`（**M30 多 `BridgeRootMismatch` 强制度；新 `apply_bridge_lock(lock, height)` 走 ZeroStake/BadSignature/UnknownAccount/InsufficientBalance 现有错误类**）、`Chain::seal`/`next_validators_root`（**M23 同 trial 路径盖两根 + M27 同 trial 路径盖 graph_root + M30 同 trial 路径盖 bridge_root**）、`apply_stake_op`、`apply_evidence`、`replay`/`replay_verified`、验签、`state_root`（**M30 digest 多 fold `bridge_locked` / `bridge_locks` 各 `lock.merkle_leaf(id)` / `bridge_lock_heights` / `next_lock_id`**）/`merkle_root`/`account_proof`、**M27 `graph_merkle_root`（按 CANONICAL_PIVOT 排序索引）/`graph_range_proof(a, b)`（`RangeProof { sub_root, entries }`）**、**M28 `GraphLeafAtHeight { node_id, graph_node, proof }` + `DiffClaim { added, dropped }`（无 changed 臂）+ `graph_diff(&prev_state)`（每 leaf 的 proof 对**各自高度**的 `accounts_root`）**、**M30 `bridge_merkle_root`（按 lock_id 排序索引）/`bridge_lock_proof(lock_id)`（同 M22/M27 单 leaf 路径）/`bridge_merkle_root_for_genesis(g)`（空 locks 起点）**、供应守恒不变量（含 bonded + 解绑中 + **M30 `bridge_locked`**）+ **M54 `ChainError::MempoolFull { capacity }` 变种（mempool 容量上限准入拒绝，纯变种不序列化，经 Display 外泄为 RPC `422`）** + **M57 `ChainError::AccountQuotaFull { author, limit }` 变种（每账户 pending 配额准入拒绝，同为节点本地背压、经 Display 自动外泄为 RPC `422`）** + 测试 |
| `src/mempool.rs` | 确定性 mempool 与出块：内容寻址排序 + 试算式 `build_block` + **M54 `capacity: usize` 容量上限字段（`new()` ⇒ `usize::MAX` 无界、`set_capacity`/`capacity` 存取器）+ `insert` 准入门控（新哈希且 `len() >= capacity` ⇒ `ChainError::MempoolFull`，同哈希重插仍幂等不涨池）** + **M57 每账户配额：`per_author: BTreeMap<u64,usize>` 作者→计数索引（归零即删条目）+ `per_account_limit: usize`（`new()` ⇒ `usize::MAX` 无界）+ `rejected_quota: u64` 计数器 + `set_per_account_limit`/`per_account_limit`/`rejected_quota` 存取器；`insert` 在 capacity 门之侧判配额（仅新哈希、作者先经 `validate_tx` 认证、幂等重插不重计）⇒ `ChainError::AccountQuotaFull`、`remove_included` 归还作者槽位（`is_some()` 守卫防下溢）** + 测试 |
| `src/merkle.rs` | 二叉 Merkle 树：域分隔叶/节点、奇数提升、包含证明 `Proof`/`verify` + 测试 |
| `src/validator.rs` | 验证人集与确定性提议人（Tendermint 优先级累加器）、链上变更 `ValidatorUpdate`/`apply_updates`、集合 Merkle 承诺 `merkle_leaf`/`merkle_root`/`proof`（M21）+ 测试 |
| `src/consensus.rs` | BFT 投票/最终性证书：`Vote`/`Commit`/`verify`、`commit_block`、`detect_equivocation` + 测试 |
| `src/round.rs` | BFT 轮次状态机（Tendermint `upon` 规则、超时/锁定/换轮）+ 进程内网络模拟器 `Sim` + **M34 `ingest -> Option<SlashEvidence>`：摄入 precommit 时若已持有同 `(validator, height, round)` 的另一 `block_hash` → 按 hash 规范排序组装证据，经新 `Action::Equivocation(SlashEvidence)` 上抛（first-wins `or_insert` 不变，纯旁路观测）** + 测试 |
| `src/driver.rs` | BFT 认证链驱动 `ChainDriver`：逐高度 mempool→共识→提交 + 证书保留 + 故障注入 + 链上验证人变更（`stage_validator_update`）+ 质押变更（`stage_stake_op`）+ 罚没证据（`stage_slashing_evidence`）+ 测试 |
| `src/net.rs` | P2P gossip 与反熵同步：`GossipMsg`/`GossipNode`（纯状态机，认证块 `apply_certified` 复验证书、交易 epidemic 泛洪去重 + **`Evidence` / `StakeOp` 块级 ops 的待打包池与去重 flood**；M22 增 `GetHeaders` / `Headers` 服务 + **M24 完全替换 `GetProof { items }` / `Proof { items }` 一对（wire tags 8/9 复用），承载 Account/Reviewer/Validator 任意混合 `items`，上限 `MAX_PROOF_BATCH = 32`；全节点在 on_message 现取现发 account_proof/reviewer_proof/ValidatorSet::proof 三类；光节点入 `proofs` 缓存（键 `(ProofKind, u64)`）** + **M26 `GossipNode::serve_knn(query, k)` 派 `KnnClaim` 给光端（叶子账密存于 accounts_root 插入序侧）** + **M27 `GossipNode::serve_range(query, min_sim)` 派 `RangeClaim` 给光端（叶子存于 `graph_root` 排序索引侧）** + **M28 `GossipNode::serve_diff(h1, h2, header_h1, header_h2)` 派 `DiffEnvelope` 给光端（缓存 `genesis: Genesis` 字段以便从创世重放 `[0..h1]` 构造 prev 状态；envelope 含两边 header/cert/tracked set + `DiffClaim`；`Diff { envelope }` 与 `GetDiff { header_h1, header_h2 }` 因体积大走 `Box` 装箱避开 large_enum_variant；wire tags 增 `TAG_GETDIFF=10` / `TAG_DIFF=11`；`encode_diff_envelope`/`decode_diff_envelope` 共用 `codec::encode_graph_node`/`encode_proof`/`encode_validator`/`encode_header`/`encode_commit` 现成构件**）+ **M29 `GossipNode::serve_inclusion(kind, id)` 从 M24 内联抽取 + `serve_batch(items)` 派 `BatchResponseEnvelope`（复用 M24/M26/M27/M28 四个 serve_*，无新 SPV 逻辑，仅 dispatch；`Batch { envelope }` 因体积大走 `Box` 装箱；wire tags 增 `TAG_GETBATCH=12` / `TAG_BATCH=13`）** + **M30 `GossipNode::serve_lock(lock_id)` 派 `LockEnvelope` 给光端（用 `state.bridge_lock_heights` 取出块高 → 从 retained chain 取 header+cert，重放 `[..=height]` 取 active set 作 `source_tracked_set`；wire tags 增 `TAG_GETLOCK=14` / `TAG_LOCK=15`；`encode_lock_envelope`/`decode_lock_envelope` 共用 `encode_header`/`encode_commit`/`encode_validator`/`encode_bridge_lock`/`encode_proof` 现成构件；on_message 加 `GetLock` 现取现发、`Lock` 静默丢弃两支）** + 确定性 `Network` 收敛总线 + **M22 光节点 `LightGossipNode`（仅头、`ValidatorTracker`、从不解码交易）+ 混入全/光节点的总线 `LightNetwork`**（**M28 `LightGossipNode` 加 `diffs: Option<DiffEnvelope>` 缓存 + `take_diff()` 弹出；M29 加 `batches: Option<BatchResponseEnvelope>` 缓存 + `take_batch()`；M30 加 `locks: Option<LockEnvelope>` 缓存 + `take_lock()`；on_message 同步新增 `Diff` / `GetDiff` / `Batch` / `GetBatch` / `Lock` / `GetLock` 六支**）+ `encode_gossip`/`read_msg`/`write_msg`（真实 socket 分帧，含新 TAG_GETHEADERS/TAG_HEADERS + **TAG_GETPROOF=8 / TAG_PROOF=9** + **TAG_GETDIFF=10 / TAG_DIFF=11** + **TAG_GETBATCH=12 / TAG_BATCH=13** + **TAG_GETLOCK=14 / TAG_LOCK=15**） + **M35 `GossipNode::has_pending_work()`（mempool ∪ 待打包 stake_ops ∪ 待打包证据；桥无待打包池、故意不计入，daemon 的 `create_empty_blocks` 门用它判定空闲）** + **M39 `GossipMsg::Peers(Vec<(u64,String)>)` 地址簿 + `TAG_PEERS=17`（dense 0..=17）+ `MAX_PEERS=1024`（codec 上限 → `TooManyItems`）；`encode_gossip`/`decode_gossip` 各加一支（`u32` 计数 + 每项 `u64` id + `u32` 长度 + utf8 地址，解码 `from_utf8_lossy`）；两 `on_message` 核（全 / 光）各加 `Peers(_) => Vec::new()` 丢弃支（Actor 独占发现，纯核保持纯）** + **M53 `submit_local_checked(tx) -> Result<(Hash, Vec<(u64,GossipMsg)>), ChainError>`：surfacing mempool 准入的拒绝原因（先 `seen_tx.insert` 再 `mempool.insert?` 成功才 broadcast）；`submit_local` 重写为在其上委托（`.map(|(_,out)|out).unwrap_or_default()`）⇒ 既有调用方逐字节不变（同序、拒绝即空 vec），供 M53 外部交易入口 RPC 回答接受/拒绝** + **M54 `GossipNode::set_mempool_capacity(cap)` 直通 `self.mempool.set_capacity(cap)`（守护进程按 `[mempool] capacity` 接线容量上限）** + **M57 `GossipNode::set_mempool_per_account_limit(n)` 直通 `self.mempool.set_per_account_limit(n)`（守护进程按 `[mempool] per_account_limit` 接线每账户配额）** + **M55 私有 `SeenSet`（`BTreeSet`+`VecDeque`+容量）替换三个洪泛去重集 `seen_tx`/`seen_evidence`/`seen_stake_op`（原裸 `BTreeSet<Hash>` 单调永涨）：`insert(h)->bool` 镜像 `BTreeSet::insert`、有界时 FIFO 逐出最旧、`capacity==usize::MAX` ⇒ 无界且 deque 永不触碰（默认逐字节同今日）；加 `set_seen_capacity(cap)` 直通设三集 + `seen_tx_len`/`seen_tx_capacity` 存取器供指标** + **M59 `GossipNode::account_inclusion(id) -> Option<(CertifiedHeader, ProofEntry)>`：薄封装 `serve_inclusion(Account,id)` 的 `ProofEntry` + `headers_from(height).next_back()` 的头 `CertifiedHeader`，为可验证账户读 RPC 一次性备齐证明与其所验的认证头（height 0 / 未知 id → `None`）** + **M60 `GossipNode::inclusion(kind, id)`：把 M59 的 `account_inclusion` 泛化为任意 `ProofKind`（reviewer/验证人/图节点），`account_inclusion` 退化为 `inclusion(ProofKind::Account, id)` 委托，为其余三类实体的可验证读 RPC 一次性备齐证明 + 认证头** + **M61 `encode_batch_request`/`decode_batch_request`：把内嵌于 `encode_gossip` 的批量请求编解码抽为公有（字节与 gossip `GetBatch` 逐字一致、`encode_gossip` 委托之）+ `GossipNode::batch(items)` 复用 `serve_batch` 绑认证头，为 `POST /batch` 异构批量可验证读备齐 `(CertifiedHeader, BatchResponseEnvelope)`；M62 加 `blocks_through(up_to)` 未截断产 `[1..=up_to]` + `batch()` 回 `BatchReply` 三元组随附 Diff 区间块 + 独立 `encode_blocks`/`decode_blocks`（内嵌 gossip `Blocks` 字节、encode 委托、decode 无 256 上限/不预分配）** + **M64 `lock_listing()` 读 `bridge_locks` ∪ `bridge_lock_heights` → `(id,height,lock)` id 序（`light.rs` 别名 `LockListing`，供 daemon `GET /bridge/locks` 明文目录枚举）** + 测试 |
| `src/bridge.rs` | **M30 信任无关跨链桥**：`LockEnvelope { source_header, source_cert, source_tracked_set, lock_id, lock, proof }`（M28 `DiffEnvelope` 同形）+ `BridgeEndpoint { my_genesis_hash, source_genesis_hash, tracker: ValidatorTracker, consumed: BTreeSet<(Hash,u64)>, minted: BTreeMap<u64,u64> }` + `VerifiedLock` + `BridgeError::{Cert(LightError), WrongDestination{expected, got}, AlreadyConsumed{source_chain, lock_id}, SourceNotFollowed{height}}`；`new(my_genesis, source_genesis)` 用两链创世哈希作链 id、`source_genesis` 启 `ValidatorTracker::from_genesis`；`follow_source(header, cert, next_set)` 复用 M22 `follow_header`；`verify_lock(env)` **无新 SPV 逻辑** = (1) cert-binding 复用 M22 `verify_state_root_against_header(**self.tracker.validators()**, cert)`（**endpoint 自己的集合，不用 envelope 的**——relayer 无法替换验证人集）+ (2) inclusion `merkle::verify(&env.source_header.bridge_root, &leaf_hash(&lock.merkle_leaf(lock_id)), &env.proof)` + (3) dest match `lock.dest_chain == my_genesis_hash` + (4) replay `(source_genesis, lock_id) ∉ consumed`；`consume(v)` 入 dedup + `minted[dest_account] += amount`（bridge-module 记账，consensus 不感知 mint）；`hex8` 错误消息 + `Display`/`Error` impl + 7 类测试（accept / tampered amount / tampered bridge_root / wrong dest / replay / source-not-followed / 两链 B→A 对称） |
| `src/light.rs` | 轻客户端验证人集跟随：`ValidatorTracker`（`from_genesis` / `follow` / `follow_all`，逐高度复验证书 + 复刻 `apply_block` 的集合迁移，镜像 `bonds` + 创世公钥表，不执行交易；M21 `follow` 对 `next_validators_root` 交叉校验、免迁移 `follow_committed`、SPV `verify_membership`；M22 只对头的 `follow_header` + `verify_membership_against_header` + **M24 唯一 SPV 验证器 `verify_proof_against_header`（按 entry.kind() 选根，Account/Reviewer → accounts_root、Validator → next_validators_root，本地重算 leaf）；M23 的 `verify_account_membership_against_header` 与 M22 的 `verify_membership_against_header` 全部删除；`verify_state_root_against_header` 保留** + **M26 `KnnClaim` + `verify_knn_against_header`（对 accounts_root 验邻域 leaf，本地 cos_sim 重排+cut，prover 序列等比）** + **M27 `RangeClaim` + `verify_range_against_header`（对 `graph_root` 验范围 leaf，cutoff ∈ [-1,1] 校验 + 本地 cos_sim 重排+prefix cut，prover 序列等比；新增 `LightError::RangeMismatch`/`RangeCutoffInvalid`）** + **M28 `DiffEnvelope { header_prev, cert_prev, header_new, cert_new, diff, tracked_set_h1, tracked_set_h2 }` + `verify_diff_against_headers(genesis, blocks_in_range, claim)`：两边 header/cert 绑定+各自 tracked_set 验签（动态验证人集下两高度用不同集合）+ `[1..=h₂]` 局部重放 → `state_at_h2`+`[1..=h₁]` → `state_at_h1`+每 leaf 对**各自高度** accounts_root 的 Merkle 验证+prover 与重放 `added/dropped` 集合等比；新 `LightError::InvalidDiffRange { h1, h2 }` / `DiffMismatch { height }`** + **M29 `BatchItem` + `BatchResponseItem`（4 类异构槽位 Inclusion/Knn/Range/Diff）+ `BatchResponseEnvelope { items }` + `verify_batch(genesis, header, cert, tracked_set, blocks_in_range, items, response)` 把每个 slot 派回 M24/M26/M27/M28 四个验证器——**无新 SPV 逻辑**，仅 dispatch；wallet 侧 cert-binding 上下文显式传参（`ValidatorTracker` 仅存 set/bonds/pubkeys/head/height，header/cert 来自 M22 头部缓存）；新 `LightError::{BatchTooManyItems, BatchItemCountMismatch, BatchItemKindMismatch}` 三类**仅协议违规**错误（per-primitive 错误通过 dispatch 转发）**）+ `LightError` + 测试 |
| `src/crypto.rs` | ed25519 身份：`Keypair`/`verify`（封装 `ed25519-dalek`）+ **M40 `Keypair` 派生 `Clone`（供 `AuthContext` 与共识 actor 各持一把签名）** + **M43 `Keypair::secret_seed()`（暴露 32B 种子，与 `from_seed` 往返，供派生 mTLS 凭据的 PKCS#8）** + 测试 |
| `src/codec.rs` | 区块的规范二进制编解码（哈希与落盘共用，含 `validator_updates`、`stake_ops` 与 `slashing_evidence`；M22 增 `BlockHeader`（含 `txs_commitment`/`stake_ops_commitment`/`evidence_commitment` 三份 SHA-256 承诺）+ `CertifiedHeader` + `encode_header`/`decode_header` + `encode_certified_header`/`decode_certified_header` + **M23 头再加 `state_root`/`accounts_root` 两根、`Block`/`BlockHeader` 同步增两字段、`decode_certified_header` 长度算术从 `84 + n*48 + 96` 改为 `148 + n*48 + 96 = 244 + n*48`** + **M27 头再加 `graph_root` 根、`Block`/`BlockHeader` 同步增字段、`decode_certified_header` 长度算术从 `148` 改为 `180 + n*48 + 96 = 276 + n*48`，prefix 仍与 `encode_block` 字节对齐** + **M30 头再加 `bridge_root` 根 + 尾部 `bridge_locks_commitment` 承诺、`Block.bridge_locks: Vec<BridgeLock>`、`decode_certified_header` 长度算术从 `180` 改为 `212 + n*48 + 128 = 340 + n*48`**）+ `tx_signing_bytes`/`encode_tx`/`decode_tx`（签名/tx 哈希/gossip wire 字节）+ `stakeop_signing_bytes`/`encode_stakeop`/`decode_stakeop`（bond/unbond 签名与哈希）+ `encode_evidence`/`decode_evidence`（双签证据）+ `encode_commit`/`decode_commit`（证书落盘）+ **`encode_account`/`decode_account`/`encode_proof`/`decode_proof`（M23 AccountProof 的 wire 字节）** + **M28 `encode_diff_envelope`/`decode_diff_envelope`** + **M29 `encode_knn_request`/`decode_knn_request`（32-byte query + u32 k）+ `encode_range_request`/`decode_range_request`（32-byte query + f32 min_sim）+ `encode_knn_claim`/`decode_knn_claim` + `encode_range_claim`/`decode_range_claim` + `encode_batch_envelope`/`decode_batch_envelope`（u32 len + 每 slot 1-byte kind tag + per-variant body）+ `encode_batch_response_kind`/`decode_batch_response_kind`** + **M30 `encode_bridge_lock`/`decode_bridge_lock`（account/amount/dest_chain/dest_account/nonce/sig wire）+ `bridgelock_signing_bytes`** + 测试 |
| `src/store.rs` | 追加式日志（长度前缀记录、残缺尾检测）：`BlockLog`（区块）+ `CertLog`（证书）+ 测试 |
| `src/config.rs` | **M32 文件化配置（serde + toml 镜像结构，共识类型仍 serde-free）；M33 用 `[validator]{enabled, seed_hex}` 替换 `[producer]`**：`NodeConfig`（id/listen/data_dir + 静态 peer 表 + 可选 `[validator]` + **M35 可选 `[consensus]`**）/ `GenesisConfig`（`to_genesis()` hex 解码 pubkey）+ **M35 `ConsensusConfig{propose/prevote/precommit_timeout_ms, timeout_delta_ms, block_interval_ms, create_empty_blocks}` + 手写 `Default`（逐字段 = 旧 daemon 常量，`create_empty_blocks=true`，即五个时序数字的唯一真源）；段与字段两级 `#[serde(default)]` → 缺段/部分段均回落默认（后向兼容）；**M36 可选 `[network]` → `NetworkConfig{announce_interval_ms, startup_delay_ms}` + 手写 `Default`（2000/1000 = 旧 `ANNOUNCE_SECS=2s`/`STARTUP_DELAY=1000ms` 常量，两个网络时序数字的唯一真源；`ANNOUNCE_SECS` 秒→毫秒统一到 `_ms` 约定）；同样段与字段两级 `#[serde(default)]` 回落**；**M38 可选 `[metrics]` → `MetricsConfig{enabled(默认 false), listen(默认 "127.0.0.1:9600")}` + 手写 `Default` + `listen_addr()`（走私有 `parse_addr`）；`NodeConfig.metrics: Option<MetricsConfig>` opt-in，缺段 ⇒ `None` ⇒ 端点不绑（镜像 `[validator]`）**+ **M39 `NetworkConfig.enable_peer_exchange: bool`（`Default` = true）：peer 发现 / 地址簿 gossip 开关，`false` 钉死静态种子集；两级 `#[serde(default)]` 覆盖缺省**+ **M40 `NetworkConfig.require_peer_auth: bool`（`Default` = false）：认证握手开关，`true` 要求对端证明其所声称验证人 id 的 genesis 密钥（全网策略）；两级 `#[serde(default)]` 覆盖缺省**+ **M41 `NetworkConfig.enable_tls: bool`（`Default` = false）：TLS 传输加密开关，`true` 把每条 P2P 链路包进 TLS 1.3（仅加密、临时自签证书、accept-any；全网策略）；两级 `#[serde(default)]` 覆盖缺省**+ **M42 `NetworkConfig.bind_channel: bool`（`Default` = false）：信道绑定开关，`true` 把 TLS exporter 混进 M40 auth transcript（需 `enable_tls`+`require_peer_auth`，daemon 否则 fail-fast；全网策略）；两级 `#[serde(default)]` 覆盖缺省**+ **M43 `NetworkConfig.require_peer_certs: bool`（`Default` = false）：创世锚定 mTLS 开关，`true` 要求每个节点以其 genesis ed25519 密钥作 TLS 凭据（RFC 7250 裸公钥）、仅当对端密钥 ∈ 创世验证人集才接受连接（需 `enable_tls`+验证人密钥；全网策略）；两级 `#[serde(default)]` 覆盖缺省**+ **M44 可选 `[logging]` → `LoggingConfig{level(默认 "info"), format(默认 "text")}` + 手写 `Default`（复现 M37 订阅器）+ `validate()`（拒未知 format → `ConfigError::BadLogFormat`，`load_node_config` 载入时调用）；`NodeConfig.logging: Option<LoggingConfig>` opt-in，缺段 ⇒ `None` ⇒ M37 默认；两级 `#[serde(default)]` 覆盖缺省**+ **M45 `LoggingConfig` 再加 `file(默认 "")` + `rotation(默认 "daily")` 两字段；`validate()` 除 format 外再拒未知 rotation → `ConfigError::BadLogRotation`；`file` 空 ⇒ stderr（M44/M37 行为）、非空 ⇒ 滚动文件路径，不校验存在性（父目录 init 时建）**+ **M46 `LoggingConfig` 再加 `stderr: bool`（默认 `false`）：配了 `file` 且 `=true` ⇒ tee 到文件+stderr；bool 不需 `validate`；字段级 `#[serde(default)]` 覆盖两级默认**+ **M47 `LoggingConfig` 再加 `stderr_level`/`file_level`（`String`，默认 `""`）：tee 的每-sink filter 覆盖，与 `level` 一样自由格式 directive、不需 `validate`（`EnvFilter` 解析有损）、空 ⇒ 继承 `level`**+ **M48 `LoggingConfig` 再加 `levels`/`stderr_levels`/`file_levels`（各 `Vec<String>`，默认 `vec![]`）：三标量旋钮的数组对应物、非空即以 `,` 连接后胜过标量、空 vec 回落标量、与标量一样不需 `validate`**+ **M49 `LoggingConfig` 再加 `stderr_format`/`file_format`（`String`，默认 `""`）：tee 的每-sink formatter 覆盖，`text`/`json`、空 ⇒ 继承 `format`；与 `format` 一样枚举式故 `validate` 扩展拒未知（复用 `BadLogFormat`）**+ **M50 `LoggingConfig` 再加 `max_files`（`usize`，默认 `0`）：保留的轮转日志文件上限，`0` ⇒ 无界（M49/M45 原 `::new` 路径逐字节相同）、`> 0` ⇒ builder `max_log_files`；纯计数故 `validate` **不改**（serde 解析期拒非整数）**+ **M51 `NetworkConfig.advertise_addr: String`（`Default` = `""`）：M39 peer 发现中节点为自己广告的可拨地址（NAT / 端口映射）；空 ⇒ 广告绑定 `listen`（逐字节同 M39），非空则 `load_node_config` 校验须解析为 `SocketAddr`（复用 `parse_addr` → `BadAddr`，拒 DNS 主机名）；两级 `#[serde(default)]` 覆盖缺省**+ **M53 可选 `[rpc]` → `RpcConfig{enabled(默认 false), listen(默认 "127.0.0.1:9700")}` + 手写 `Default` + `listen_addr()`（走私有 `parse_addr`），镜像 `MetricsConfig`；`NodeConfig.rpc: Option<RpcConfig>` opt-in，缺段 ⇒ `None` ⇒ 入口端点不绑；`load_node_config` 加载期校验 `listen`（坏地址 fail-fast → `BadAddr`）**+ **M54 `[mempool]` → `MempoolConfig{capacity(默认 4096), max_block_txs(默认 64), per_peer_tx_per_sec(默认 0.0), per_peer_tx_burst(默认 256.0)}` + 手写 `Default`（旧常量唯一真源）；因 `f64` 字段只派生 `PartialEq` 非 `Eq`；`validate()` 拒 `capacity==0`/`max_block_txs==0`/负速率或桶 → `ConfigError::BadMempool`；`NodeConfig.mempool` 始终在场（非 `Option`，镜像 `[consensus]`/`[network]`），`load_node_config` 加载期调 `validate`**+ **M55 `MempoolConfig` 加 `seen_cache: usize`（每集 gossip 去重容量，默认 `0`）；**哨兵刻意不对称**于 `capacity`（后者 `0` 被拒）：此处 `0` = 无界/关（恰如 `per_peer_tx_per_sec = 0.0`）；任意 `usize` 合法故 `validate()` 不变**+ **M57 `MempoolConfig` 加 `per_account_limit: usize`（单账户 pending 配额，默认 `0`）；沿用 `seen_cache` 的 `0` = 无界/关哨兵（不对称于 `capacity` 的拒-0）；大于 `capacity` 合法但永不绑（全局 cap 先触）；任意 `usize` 合法故 `validate()` 不变**+ `load_node_config`/`load_genesis` + `decode_seed` 自带严格 hex 解码（`hash.rs` 只编码；**M56 由 `fn`→`pub fn` 公开，供 `encode-tx` 复用同款 32 字节种子 hex 解码 + `BadHex` 错误**）+ `ConfigError` typed 错误 + 测试（往返 / 载入 `testnet/` 样例 / pubkey-mismatch fail-fast / typed 错误 / 样例生成器 / **M35 默认等于旧常量 / 缺段回落 / 部分段覆盖** / **M36 `[network]` 默认等于旧常量 / 缺段回落 / 部分段覆盖** / **M38 `[metrics]` 默认关闭 / 缺段为 `None` / `enabled=true` 打开** / **M39 `enable_peer_exchange` 默认 true / `=false` 可关** / **M40 `require_peer_auth` 默认 false / `=true` 解析** / **M41 `enable_tls` 默认 false / `=true` 解析** / **M42 `bind_channel` 默认 false / `=true` 解析** / **M43 `require_peer_certs` 默认 false / `=true` 解析** / **M44 `logging` 默认关 + 缺段为 `None` + `[logging]` 两旋钮 = `info`/`text` 默认、`=debug`/`=json` 解析 + 空段回落默认 / 拒坏 format** / **M45 `file`/`rotation` 解析 + 默认（file 空 / rotation `daily`）+ 空段回落 / `validate` 接受 daily/hourly/minutely/never 拒 weekly + `load_node_config` 端到端拒坏 rotation** / **M46 `stderr` 默认 `false` + 解析 `file`+`stderr=true` + 空段回落 `false`** / **M47 `stderr_level`/`file_level` 默认 `""` + 解析 `stderr_level="info"`/`file_level="debug"` + 空段回落 `""`** / **M48 `levels`/`stderr_levels`/`file_levels` 默认空 vec + 解析 `levels=["info","tokio=warn"]`/`file_levels=["debug"]` + 空段回落全空** / **M49 `stderr_format`/`file_format` 默认 `""` + 解析 `text`/`json` + validate 接受空/拒未知 yaml + 空段回落 `""`** / **M50 `max_files` 默认 `0` + 解析 `max_files = 7` + validate 恒 Ok + 空段回落 `0`** / **M51 `advertise_addr` 默认 `""` + 解析 `203.0.113.7:9021` + `load_node_config` 拒非地址值 → `BadAddr`** / **M53 `[rpc]` 默认关闭 / 缺段为 `None` / `enabled=true` 打开且 `listen_addr` 可解析 / 坏 `listen` 经 `load_node_config` → `BadAddr`** / **M54 `[mempool]` 安全默认（capacity 4096 / max_block_txs 64 / rate 0）/ 段覆盖默认 / `capacity = 0` 经 `load_node_config` → `BadMempool`** / **M55 `mempool_config_has_safe_defaults` 加断言 `seen_cache == 0` / `mempool_seen_cache_overrides_defaults` 解析 `[mempool] seen_cache = 1024`** / **M57 `mempool_config_has_safe_defaults` 加断言 `per_account_limit == 0` / `mempool_per_account_limit_overrides_defaults` 解析 `[mempool] per_account_limit = 32`**） |
| `src/daemon.rs` | **M33 联网 tokio 守护进程（TCP P2P + 分布式 BFT 投票）**：单属主 actor（`GossipNode` 独占 task、per-peer mpsc 出站、无 `Arc<Mutex>`）——actor 现在**也独占** `Keypair: Option<Keypair>` + `Option<RoundState>` + tokio timer 句柄；`write_frame`/`read_frame`（`u32` BE 长度 + `encode_gossip`，`MAX_FRAME = 16 MiB` 上限）+ 8 字节 BE id 握手（在 `GossipMsg` 之外）+ 只拨 id 更大 peer（每对一连接）+ 监听/连接/反熵心跳任务 + **`Cmd::{StartHeight, Timeout}`（tokio sleep → self_tx）**；**`GossipNode::on_message` 仍纯**（把 `GossipMsg::Consensus` 直接 drop 掉——它既没密钥也触不到 timer），共识消息在 actor 主循环里路由到 `RoundState::on_message`/`on_timeout`；**`build_candidate` 永远出一 sealed block**（空块心跳），`on_consensus` 在 prevote 之前用 `Chain::would_accept` 试跑 apply（Byzantine-proposer 保护），`reconcile_after_sync` 让 sync 永远赢；actor 是本节点日志唯一写者（`append node.blocks()[appended..]`）；`Node::start`/`run(cfg, genesis, Option<Keypair>)`——boot 经 `load_certified` 复验最终性、validator 键与 genesis pubkey 不匹配即 fail-fast；**M34 `apply_actions` 新增 `Action::Equivocation(ev) => on_equivocation`，后者调 `submit_local_evidence`（本地暂存 + flood）把观测到的双签接入既有 M19 证据管线**；**M35 五个时序常量删除、改由 module-private `Timing` copy 结构（在 `Node::start` 从 `cfg.consensus.*` 内联构造塞进 `Actor`）承载；`timeout_for` 改纯自由函数 `timeout_for(&Timing, step, round)`；新 `on_start_tick` 只在被调度的起始路径上门控 `create_empty_blocks`（空块关且 `!has_pending_work()` → 不起轮、按 `block_interval_ms` 重排；`on_consensus` 的 lazy-start 保持不设门 → peer 一提议就跟上），`BLOCK_INTERVAL` 全部换成 `self.timing.block_interval_ms`；**M36 删除 `ANNOUNCE_SECS`/`STARTUP_DELAY` 两个网络时序常量、改由 `Node::start` 从 `cfg.network.{announce_interval_ms, startup_delay_ms}` 内联 hoist 出本地变量喂给反熵心跳 `interval` 与验证人启动 `sleep`（唯一真源在 `NetworkConfig::Default`）**；**M37 守护进程接入 `tracing`：5 处 `eprintln!` → 带级别带字段的事件（append block/cert failed=`error!`、accept error=`warn!`、listening/peer connected/peer disconnected/shutdown=`info!`、block committed=`debug!`），新 `pub fn init_tracing()`（stderr + `RUST_LOG` EnvFilter 缺省 `info` + `try_init` 幂等）由 `main.rs` 的 `cmd_run`/`cmd_localnet` 首行调用；宏无订阅器时 no-op 故测试逐字节不变**；**M38 opt-in 只读指标/健康端点：`Cmd::Metrics(oneshot)` 沿用 `Cmd::Query` 查询模式从 `node.*`/`outbound.len()`/`kp.is_some()`/`cons.is_some()` 组装 `Metrics` 快照、经 `Node::metrics()` 取回；纯 `render_prometheus(&Metrics)→String`（Prometheus 文本曝露 v0.0.4：8 条 `zhixing_*` gauge + `zhixing_head_info{head}`）；`run_metrics`/`serve_metrics_conn` 手拼极简 HTTP/1.1 responder（有界丢弃请求、任意路径回指标、`200 OK`⇒健康、无新增依赖）；`Node::start` 按 `cfg.metrics.enabled` 门控绑第二个 TCP 监听器（默认关 ⇒ 行为逐字节不变）**；**M39 peer 发现 / 地址簿 gossip：`Actor` 新增 `addrs: HashMap<u64,String>`（first-wins，配置/自身权威）+ `dialing: HashSet<u64>`（去重防拨号风暴）+ `peer_exchange: bool`；`Node::start` 用「自身 + 配置 peer」种 `addrs`、「id 更大配置 peer」种 `dialing`；`peers_msg()`（含自身 `(id,listen)`，免改握手）/`gossip_peers()`/`on_peers(book)`（`or_insert` 存址；`id>my_id && !dialing` 且可 `parse::<SocketAddr>()` → `dialing.insert` + spawn `run_connector`，保持只拨 id 更大不变量）；`run_actor` 三挂钩（`Register` 向新 peer 发 `peers_msg`、`Inbound` 剥 `Peers(book)=>on_peers`、`Announce` 后调 `gossip_peers`）；纯核照旧丢弃 `Peers`（沿用 `Consensus` 先例）**；**M40 认证握手 / peer 认证：`crypto::Keypair` 派生 `Clone`；`daemon.rs` 加纯核 `AUTH_DOMAIN` + `auth_transcript(signer_id, signer_nonce, peer_id, peer_nonce)`（域分隔 + 绑定双 nonce）+ `struct AuthContext{my_id, kp, validators, require}`（`Arc` 共享）+ `async auth_handshake`（对称双向：各发 `HelloInit{id,pubkey,nonce}` + 各签 transcript 发 `HelloAuth{sig}`，验对端 id 属 genesis、pubkey 匹配、签名有效，否则 `Err` 丢连接）；`handle_conn`/`run_listener`/`run_connector` 改收 `Arc<AuthContext>`，`handle_conn` 按 `ctx.require` 二选一（关 ⇒ 逐字节等于旧 hello）；`Actor` 加 `auth` 字段（M39 自动拨号也传该 `Arc`）；`Node::start` 从 `genesis.validators` 建 id→pubkey 映射、`require_peer_auth && key.is_none()` fail-fast、克隆密钥进 `AuthContext`；`Cargo.toml` 提 `getrandom="0.2"` 为直接依赖（供 nonce，lock 树本就有 ⇒ 无新增编译单元）**；**M41 传输加密 / TLS：新增 `trait PeerStream`（毯 impl）让 `handle_conn` 改收 `Box<dyn PeerStream>` + `tokio::io::split`（裸 TCP / 服务端 / 客户端 TLS 流共用一条非泛型路径，握手与分帧不变）；`AuthContext` 加 `tls: Option<TlsSetup{acceptor,connector}>`；`server_wrap`/`client_wrap`（关 ⇒ `Box::new(tcp)`；开 ⇒ `acceptor.accept`/`connector.connect`）；`build_tls_setup`（ring provider 幂等装 + `rcgen` 临时自签证书 + `AcceptAnyServerCert` accept-any 客户端验证器）；`run_listener`/`run_connector` 先 `set_nodelay` 再包裹（TLS accept 进 spawn 任务不阻塞 accept 循环）；`Node::start` 仅 `enable_tls` 时构建（无需密钥 ⇒ 无 fail-fast），指标端点保持明文 HTTP；`Cargo.toml` 加 `tokio-rustls`+`rcgen`（均 ring 后端，避开 aws-lc-rs 的 C 工具链）**；**M42 信道绑定：`auth_transcript` 末尾加 `Option<&[u8;32]>` channel binding（`Some` 追 32B exporter、`None` 逐字节等于 M40）+ 域分隔常量 `CHANNEL_BINDING_LABEL`；`server_wrap`/`client_wrap` 握手完成后经泛型 `export_channel_binding(conn)`（`ConnectionCommon::export_keying_material`）取 exporter 随流返回 `(Box<dyn PeerStream>, Option<[u8;32]>)`；`handle_conn` 多收 `binding` 传 `auth_handshake`（仅 `ctx.bind_channel` 时折入签名与验证两处，否则 `Err` 防御）；`AuthContext` 加 `bind_channel`，`Node::start` 在 `bind_channel && (!enable_tls || !require_peer_auth)` fail-fast；无新增依赖（复用 M41 rustls 栈）**；**M43 创世锚定 mTLS：`build_tls_setup` 改收 `Option<MtlsMaterial{seed, validators}>`——`None` 拆出的 `build_encrypt_only_tls_setup` 逐字节等于 M41 accept-any 路径，`Some` 走裸公钥路径：种子拼 PKCS#8 v1（`ED25519_PKCS8_PREFIX ‖ seed`）→ `rustls::crypto::ring::sign::any_eddsa_type` → 签名器 `public_key()` SPKI → `CertifiedKey` + `AlwaysResolves{Server,Client}RawPublicKeys` 双向出示，`ServerConfig`/`ClientConfig` 用 `builder_with_protocol_versions(&[&TLS13])` 钉 TLS 1.3；单一 `GenesisPinnedVerifier{validators, algs}` 同时实现 `ServerCertVerifier`+`ClientCertVerifier`（均 `requires_raw_public_keys→true`），`spki_to_ed25519`（校验 44B 定长 + `ED25519_SPKI_PREFIX` 前缀后切末 32B）取密钥、`check_pinned` 要求 ∈ 创世集，`verify_tls13_signature` 委托 `rustls::crypto::verify_tls13_signature_with_raw_key`、`verify_tls12_signature` 防御性 `Err`；`Node::start` 在 `enable_tls && require_peer_certs` 时以 `validator_key.secret_seed()` + 创世 pubkey 集构 `MtlsMaterial`，并加两条 fail-fast（`require_peer_certs` 无 `enable_tls`、`require_peer_certs` 无验证人密钥）；无新增依赖（rustls 裸公钥 API 经 `tokio_rustls::rustls` 触达）**；**M44 `init_tracing()` 改为委托 `init_tracing_with(None)`（默认路径逐字节不变）；新 `init_tracing_with(Option<&LoggingConfig>)`：`unwrap_or_default` 取旋钮、`EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&level))` 保 `RUST_LOG` 优先、`format=="json"` 走 `fmt().json()`、否则 text，`try_init` 仍幂等**；**M45 `init_tracing_with` 先按 `file.is_empty()` 分写入端（stderr 分支逐字节等于 M44），非空走新 `build_file_appender(file, rotation)`（路径拆父目录 `create_dir_all` best-effort + 文件名前缀 → `RollingFileAppender::new`，直接作 `MakeWriter` 阻塞写、无 `WorkerGuard`）+ `parse_rotation`（字符串 → `Rotation`，缺省 `DAILY` 防御）；`init_tracing()` 不变**；**M46 `init_tracing_with` 由二路变三路：前两臂（stderr-only / file-only）逐字保留故字节不变，新增 `else if !lc.stderr` 门后 tee 臂委托新私有 `init_tee(filter, json, file, rotation)`——分层 `Registry` 两个 `fmt::layer()`（写 stderr + 写 `build_file_appender`）共享单一 `EnvFilter`，json/text 两分支具体类型免 boxed，无新依赖/feature**；**M47 `init_tracing_with` tee 臂再分岔：先算 `rust_log = try_from_default_env()`，`per_sink = rust_log.is_err() && (!stderr_level.is_empty() || !file_level.is_empty())` 为 `false` ⇒ M46 `init_tee`（逐字节相同）、`true` ⇒ 新私有 `init_tee_leveled`（两个 `fmt::layer()` 各 `.with_filter(EnvFilter)`，空侧继承 `level`；需 `use tracing_subscriber::Layer`），单-sink 两臂不变，无新依赖/feature**；**M48 `init_tracing_with` 开头一次性经新纯 helper `resolve_directive(s, array)`（数组非空、去空白条目 `,` 连接后胜出，否则标量逐字）算出 `base`/`se`/`fe` 三条已组合 directive 穿进各臂：单-sink `EnvFilter::new(&base)`、tee `per_sink` 门改看 `!se.is_empty() || !fe.is_empty()`、M46 共享 tee 用 `&base`、M47 `init_tee_leveled` 空侧继承 `&base`；`init_tee`/`init_tee_leveled`/`build_file_appender`/`parse_rotation` 复用未改，无新依赖/feature**；**M49 `init_tracing_with` tee 臂再加每-sink formatter：`sjson`/`fjson` 按 `stderr_format`/`file_format` 空则继承 `json`、`per_sink_fmt = 任一非空`；门由 `per_sink_level` 扩为 `per_sink_level || per_sink_fmt`（format 独立于 `RUST_LOG`），二者皆假仍走 M46 `init_tee`（逐字节相同）；`init_tee_leveled` 签名由单 `json` 改双 `stderr_json`/`file_json`，体内 `if json` 换 `match (stderr_json, file_json)` 枚举 2×2 组合、各臂两 `fmt::layer()`（要 JSON 的 `.json()`）带各自 filter，无 boxing；`init_tee`/`build_file_appender`/`parse_rotation`/`resolve_directive` 复用未改，无新依赖/feature**；**M50 `build_file_appender` 加 `max_files: usize` 形参：`0` ⇒ 原 `RollingFileAppender::new`（逐字节相同）、`> 0` ⇒ `builder().rotation().filename_prefix().max_log_files(n).build()`、出错 best-effort 回落 `::new`；`init_tee`/`init_tee_leveled` 各多收 `max_files` 透传，`init_tracing_with` 三处调用传 `lc.max_files`；`parse_rotation`/`resolve_directive` 复用未改，无新依赖/feature**；**M51 广告地址：新增纯函数 `self_advertise_addr(listen, advertise)`（空 ⇒ `listen`、非空 ⇒ `advertise`），`Node::start` 的自地址播种 `addrs.insert(my_id, …)` 改调它（不再直接 `cfg.node.listen.clone()`）；下游 `peers_msg`/`gossip_peers`/`on_peers` 全不动，监听仍绑 `cfg.node.listen`；无新依赖/feature**；**M52 更丰富指标 / 单调计数器：`Metrics` 快照结构 + `Actor` 各增四个 `u64` 计数器字段（actor 独占 ⇒ 裸 `+= 1`、无 atomics；`Node::start` 字面量初始化为 `0`）——`peer_connects`（`Cmd::Register`）/`local_txs`（`Cmd::LocalTx`）/`blocks_committed`（`on_decided` 中 `apply_certified` 成功分支，anti-entropy 同步块不计）/`slashing_events`（`on_equivocation`）；`Cmd::Metrics` 快照填四字段；`render_prometheus` 加镜像 `gauge` 的 `counter` 闭包（发 `# TYPE {name} counter`）并导出四条 `zhixing_*_total`；端点仍 opt-in（默认关）、只读簿记 ⇒ `localnet` head 不变、无 config/wire/依赖变更**；**M53 外部交易入口 RPC：`Cmd::SubmitTx{tx, reply: oneshot}`（带 ack，区别于 fire-and-forget 的 `LocalTx`）、actor 臂自增 `local_txs` 后 `submit_local_checked`、成功 route+ack / 失败 ack 错误；`Node::submit_tx(tx) -> Option<Result<Hash, ChainError>>` 句柄经 oneshot 往返；`run_rpc`/`serve_rpc_conn` 手拼极简 HTTP/1.1（镜像 M38 `run_metrics`）——有界读 header（`MAX_RPC_HEADER=8192`）、非 `POST`（`GET`/`HEAD`）→ `200 ok` 兼健康探针、`POST` 按 `Content-Length`（`MAX_RPC_BODY=65536` 上限）读体、`codec::decode_tx`（失败 → `400`）、经 `Cmd::SubmitTx` 提交（`Ok` → `200`+hash hex、`Err` → `422`+`ChainError` 文案）；`parse_content_length`/`http_response` 辅助；客户端错误吞掉不影响节点；`Node::start` 在 M38 块旁按 `cfg.rpc.enabled` 门控绑第三个 TCP 监听器（默认关 ⇒ 行为逐字节不变、提交路径复用既有校验故共识逐字节无关）**；**M54 mempool DoS 加固：令牌桶 `TokenBucket{tokens: f64, last: Instant}` + `allow(now, rate, burst)`（连续补充、封顶 burst、消费一枚）；`Actor` 加 `peer_tx_buckets: HashMap<u64,TokenBucket>` + 缓存 `tx_rate`/`tx_burst`（构造时读 `cfg.mempool`）+ 计数器 `txs_rate_limited: u64`；`allow_peer_tx(from)`（`tx_rate<=0.0` ⇒ 直过、否则首见 seed 满桶后 refill-and-consume）；`Cmd::Inbound` 臂**仅**对 `GossipMsg::Tx`、在 `Consensus`/`Peers` 早返回后 `on_message` 前门控（拒则 `txs_rate_limited += 1; continue`——本地 `LocalTx`/`SubmitTx` 永不限流），时间经 `std::time::Instant::now()` 臂内内联取（actor 单属主异步、无注入时钟、保 `on_message` 纯）；`Node::start` 硬编码 `64` → `cfg.mempool.max_block_txs` 并随后 `node.set_mempool_capacity(cfg.mempool.capacity)`；`Metrics` 加 `mempool_capacity`（gauge `zhixing_mempool_capacity`，与 `zhixing_mempool_txs` 并列使饱和度可观测）+ `txs_rate_limited`（counter `zhixing_txs_rate_limited_total`）**；**M55 gossip 去重集上限：`Node::start` 在 `set_mempool_capacity` 后加 `if cfg.mempool.seen_cache > 0 { node.set_seen_capacity(cfg.mempool.seen_cache) }`（默认 `0` ⇒ 不调 ⇒ 三集留 `usize::MAX` ⇒ 逐字节不变）；`Metrics` 加 `seen_tx`/`seen_tx_capacity`，`Cmd::Metrics` 从 `seen_tx_len()`/`seen_tx_capacity()` 填充，`render_prometheus` 发 `zhixing_seen_tx`/`zhixing_seen_tx_capacity` 两 gauge（无界时 capacity gauge 显 `usize::MAX` 作诚实"无界"信号）**；**M57 每账户配额接线：`Node::start` 在 M55 `set_seen_capacity` 块旁加 `if cfg.mempool.per_account_limit > 0 { node.set_mempool_per_account_limit(cfg.mempool.per_account_limit) }`（默认 `0` ⇒ 不调 ⇒ 留 `usize::MAX` ⇒ 逐字节不变）；`Metrics` 加 `mempool_per_account_limit`（gauge `zhixing_mempool_per_account_limit`，与 `zhixing_mempool_capacity` 并列）+ `txs_quota_rejected`（counter `zhixing_txs_quota_rejected_total`，与 `zhixing_txs_rate_limited_total` 并列），`Cmd::Metrics` 从 `per_account_limit()`/`rejected_quota()` 填充**；**M58 读类 RPC GET 路由：纯 `route_get(path)->GetRoute`（`Height`/`Head`/`Account(u64)`/`Health`/`NotFound`——`/` 与无法识别路径回 Health 保留健康探针、仅 `/account/<非数字>`→NotFound）+ 纯 `format_account(id,&Account)->String`（grep 友好 `key=value`）；`Cmd::QueryAccount{id, reply: oneshot<Option<Account>>}` 变体 + actor 消费臂克隆 `accounts.get(&id).cloned()` + `Node::account(id)->Option<Option<Account>>` 句柄（镜像 `status`/`metrics`），`/height`/`/head` 复用 `Cmd::Query`；`serve_rpc_conn` 在既有 POST 之前加路径提取（`split_whitespace().nth(1)`）+ GET/HEAD 分支（各路由局部 oneshot + `cmd.send`，send 失败→`503`、账户查到→`200`+`format_account`、查不到→`404`），非 POST 非 GET 仍走旧健康探针 catch-all、POST `/submit_tx` 逐字不变；路由全在已门控 M53 监听器上、RPC 默认关 ⇒ localnet head 不变、无 config/wire/共识/依赖变更**；**M59 可验证账户读 RPC：`GetRoute` 加 `AccountProof(u64)`，`route_get` 以 `strip_suffix("/proof")` 把 `/account/{id}/proof` 从 M58 `/account/{id}` 拆出（空/非数字 id→`404`）；`Cmd::QueryAccountProof{id, reply: oneshot<Option<(CertifiedHeader, ProofEntry)>>}` 变体 + actor 消费臂派 `node.account_inclusion(id)` + `Node::account_proof(id)->Option<Option<...>>` 句柄；`format_account_proof(ch,entry)->String` 渲染两行标注 hex `certified_header=<hex>`+`proof_entry=<hex>`（经既有 `encode_certified_header`/`encode_proof_entry`+`hash::hex`，无 JSON）；GET 分支置于既有 POST 之前、查到→`200`+体、查不到→`404`、actor 停→`503`，M58 读类路由与 POST `/submit_tx` 逐字不变；客户端验证走既有 `verify_proof_against_header`、节点侧零新增验证路径，RPC 默认关 ⇒ localnet head 不变、无 config/wire/共识/依赖变更**；**M60 其余实体可验证读 RPC：`GetRoute` 加 `Proof(ProofKind,u64)`（`AccountProof` 保留），`route_get` 加 `/reviewer/`/`/validator/`/`/graph/` 三前缀各经纯 `proof_route(rest,kind)` 以 `strip_suffix("/proof")` 拆出（裸 / 非数字 id→`404`）；`Cmd::QueryInclusion{kind,id, reply: oneshot<Option<(CertifiedHeader, ProofEntry)>>}` 变体 + actor 消费臂派 `node.inclusion(kind,id)` + `Node::proof(kind,id)->Option<Option<...>>` 句柄（镜像 `account_proof`）；响应臂 `GetRoute::Proof(kind,id)` 复用 kind-无关 `format_account_proof`、`404` 体经纯 `proof_kind_label(kind)` 区分实体名；验证人证明走 `next_validators_root`、其余走 `accounts_root`，客户端仍用既有 `verify_proof_against_header`、节点侧零新增验证路径，M59 账户路由与 POST `/submit_tx` 逐字不变、RPC 默认关 ⇒ localnet head 不变、无 config/wire/共识/依赖变更**；测试（分帧往返 / 超帧拒绝 / 握手 / **4 验证人收敛 / 1-fault 持续推进 / 2-fault 安全停摆 / 晚加入 / 纯 follower 跟随 / M34 TCP 双签注入 → 罚没 + 移出 / M35 `timeout_for` 配置化 + `create_empty_blocks=false` 空闲暂停→有活推进 / M38 `render_prometheus` 全 gauge + role/head 编码 + `Node::metrics()` 快照 + `[metrics]` 打开后经真实 TCP 抓 `200 OK`+`zhixing_height` / M39 链拓扑开发现→节点 21 达 2 peer、关发现→钉在 1 peer / M40 `auth_transcript` 确定性 + 顺序敏感、签名往返 + 错密钥失败、三验证人开 auth 经真实 socket 收敛、冒名者无 genesis 密钥被拒 peer 数钉 0 / M41 三验证人开 `enable_tls` 经真实 socket 收敛、`enable_tls`+`require_peer_auth` 双开仍收敛、明文裸 TCP 拨号者被 TLS 节点拒 peer 数钉 0 / M42 `auth_transcript` 绑定进签名字节（None 等于 M40 布局、Some 追 32B、异绑定异字节）、绑定不匹配拒握手（A 腿签名对 B 腿 transcript 验签失败、同腿通过 = MITM 转发防御的密码学层）、三验证人三开（`enable_tls`+`require_peer_auth`+`bind_channel`）经真实 socket 收敛、`bind_channel` 无 TLS 启动 fail-fast / M43 SPKI/PKCS8 派生回到 genesis pubkey + 拒坏编码/坏长度、`GenesisPinnedVerifier` 只认创世集双向皆然、三验证人 mTLS 全开经真实 socket 收敛、纯-TLS 拨方被 mTLS 节点拒于门外 peer 数钉 0、`require_peer_certs` 无 TLS 启动 fail-fast / M44 `init_tracing`/`init_tracing_with` 幂等、text + json 两路径皆不 panic** / **M45 `parse_rotation` 映射 hourly/minutely/never/daily + 未知回落 DAILY（经 `Debug` 比对不依赖 `Rotation: PartialEq`）、走文件分支的 `init_tracing_with` 建滚动 appender + 创建目录 + 安装不 panic** / **M46 tee 臂（`file`+`stderr=true`）建分层 Registry + 创建目录 + 安装不 panic** / **M47 RUST_LOG 未设 + 每-sink 级别（stderr=info/file=debug）建每-sink 分层 Registry + 创建目录 + 安装不 panic** / **M48 `resolve_directive` 纯语义（空数组⇒标量、非空⇒逗号连接且胜出、空白条目丢弃、全空白⇒标量）+ `levels` 数组走单-sink 组合并安装不 panic** / **M49 RUST_LOG 未设 + 每-sink format（stderr_format=text/file_format=json）建 2×2 分层 Registry + 创建目录 + 安装不 panic** / **M50 `build_file_appender(path, "minutely", 3)` 走 builder 建有界 appender + 创建目录 + `init_tracing_with` 文件臂带 `max_files` 安装不 panic** / **M51 `self_advertise_addr` 空 ⇒ `listen` 逐字节、非空 ⇒ override** / **M52 `render_prometheus_emits_counters` 四条 `zhixing_*_total` 各带 `# TYPE … counter` + 值 / `metrics_counters_advance` 单验证人 genesis quorum 1 自提交、submit tx、断言 `blocks_committed>=1` 且 `local_txs>=1`** / **M53 `parse_content_length_parses_and_caps` 纯单测（大小写不敏感 + 缺失/垃圾 ⇒ None）/ `submit_tx_accepts_valid_and_rejects_invalid` 经 `Node::submit_tx` 单验证人 genesis 合法 ⇒ `Some(Ok(hash))`+mempool≥1、未知账户 ⇒ `Some(Err(_))` / `rpc_endpoint_accepts_tx_over_tcp` 开 `[rpc]` 真实 TCP POST `encode_tx` ⇒ `200 OK`+hash 入体+mempool≥1、畸形体 ⇒ `400`** / **M54 `token_bucket_refills_and_throttles` 纯单测经 `Instant`+`Duration` 确定性推进时间（burst 耗尽即拒、补充后再过）/ `rpc_submit_reports_mempool_full` tokio，`cfg.mempool.capacity = 1` 的纯 follower（无出块排空池）首笔 `Ok`、第二笔异 tx ⇒ `MempoolFull{capacity:1}`** / **M55 `sample_metrics` 字面量 + `render_prometheus_emits_all_gauges` 加 `zhixing_seen_tx`/`zhixing_seen_tx_capacity`** / **M57 `sample_metrics` 加 `mempool_per_account_limit`/`txs_quota_rejected` + `render_prometheus_emits_all_gauges` 加 `zhixing_mempool_per_account_limit` + `render_prometheus_emits_counters` 加 `zhixing_txs_quota_rejected_total`** / **M58 `route_get_parses_paths` 纯单测（`/height`→Height、`/head`→Head、`/account/7`→Account(7)、`/` 与 `/anything` →Health、`/account/notanum`→NotFound）/ `format_account_renders_fields` 纯单测（样本 `Account` 渲染预期 `key=value` 串含十六进制 pubkey）/ `rpc_get_returns_reads` tokio 真实 TCP（开 `[rpc]`：`GET /height`→`200`+数字、`GET /head`→`200`+64-hex、`GET /account/<创世 id>`→`200`+体含 `balance=`、`GET /account/<未知>`→`404`、`GET /`→`200 ok` 保留健康探针）** / **M59 `route_get_parses_account_proof` 纯单测（`/account/7/proof`→AccountProof(7)、`/account/7`→Account(7)、`/account/notanum/proof` 与 `/account//proof`→NotFound、`/height`/`/` 不受扰）/ `account_inclusion_verifies_end_to_end` tokio（单验证人 quorum 1 自出块，`Node::account_proof(1)`→`Some((ch,entry))`，codec round-trip 后 `verify_proof_against_header(&ch.header,&ch.cert,&tracked,&entry)` 对 `ValidatorTracker::from_genesis(&g).validators()` 验证、未知 id→内 `None`）/ `rpc_account_proof_over_tcp` tokio 真实 TCP（`GET /account/1/proof`→`200`+体含 `certified_header=`/`proof_entry=`、`GET /account/999999/proof`→`404`、M58 `GET /account/1`→`200`+`balance=` 无回归）** / **M60 `route_get_parses_proof_kinds` 纯单测（`/reviewer/10/proof`→Proof(Reviewer,10)、`/validator/21/proof`→Proof(Validator,21)、`/graph/0/proof`→Proof(GraphNode,0)、`/reviewer/x/proof` 与 `/validator/21`（无 /proof）与 `/graph//proof`→NotFound、M59 账户路由与 `/`→Health 不受扰）/ `inclusion_verifies_all_kinds_end_to_end` tokio（单验证人 quorum 1，对 Reviewer 10 / Validator 21 / GraphNode 0 各 `Node::proof(kind,id)`→`Some((ch,entry))`，codec round-trip 后 `verify_proof_against_header` 对创世集验证——验证人证明走 `next_validators_root`、其余走 `accounts_root`、未知 id→内 `None`）/ `rpc_other_proofs_over_tcp` tokio 真实 TCP（`/validator/21/proof`/`/reviewer/10/proof`/`/graph/0/proof` 各→`200`+体含 `certified_header=`/`proof_entry=`、`/validator/999/proof`→`404`、`/validator/21`（无 `/proof`）→`404`）** / **M61 `batch_serves_and_verifies_end_to_end` tokio（单验证人 quorum 1，`[Inclusion{Reviewer,10},Inclusion{Validator,21},Knn,Range]` 一批 `Node::batch_proof`→`Some((ch,env))`、codec round-trip 后 `tracker.verify_batch(…,&[],&items,&env)`→`Ok`、未知 id→内 `None` 片仍验）/ `rpc_batch_over_tcp` tokio 真实 TCP（`POST /batch` 编码体→`200`+体含 `certified_header=`/`batch_envelope=`、垃圾体→`400`、非 `/batch` POST 仍走 M53 提交）；另 `net.rs` 增 `batch_request_codec_round_trip` 纯测；M62 `format_batch` 加 `range_blocks=` 行、`Cmd::QueryBatch`/`Node::batch_proof` 回 `BatchReply` 三元组、`/batch` 解构三元组，另 `rpc_batch_diff_over_tcp` tokio 真实 TCP（Diff 项→`200`+体含 `range_blocks=` 经 `decode_blocks` 解 `len==2`）；M63 加 `GetRoute::BridgeLock` + `route_get` `/bridge/lock/` 分支、`Cmd::QueryLock`/`Node::lock_proof`/`format_lock`（单行 hex `lock_envelope=`）、actor 臂直呼 `serve_lock` 供自足 `LockEnvelope`，另 `route_get_parses_bridge_lock`/`lock_proof_formats_and_verifies`/`rpc_lock_proof_over_tcp` 三测**） |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 节点 CLI：`demo` / `build` / `prove` / `bft` / `live` / `chain` / `validators` / `gossip`（含 M19 证据 flood 演示） / `light`（M20 跟随 + M21 免迁移 `follow_committed` 演示） / `vprove`（M21 验证人 Merkle 成员证明） / `lsync`（M22 头部轻同步演示：全+光节点同总线、光端 0 笔交易入眼即够到全节点高度，附线缆字节节省 + `verify_membership_against_header`） / **`account`（M23 钱包账户-成员 SPV 演示：光端经 `GetAccountProof` 取账户、本地重算 leaf 对头里的 `accounts_root` 验证，含双根对比）** / **`graph`（M25 图节点 cert-signed 包含证明演示：单次 GetProof 拿图节点 + 账户，光端对 accounts_root 重算 leaf、零信任 prover）** / **`knn`（M26 cert-signed 邻域证明演示：full peer 本地 kNN → KnnClaim，wallet 端 verify_knn_against_header 重排 + cut）** / **`range`（M27 cert-signed 范围查询演示：full peer 本地 cosine cutoff → RangeClaim，wallet 端 verify_range_against_header 对 graph_root 重排 + cut，含 cut/根/cutoff 三类负测）** / **`diff`（M28 cert-signed 时序 diff 演示：full peer 本地 h₁→h₂ diff → DiffEnvelope，wallet 端 verify_diff_against_headers 局部重放等比 + 每 leaf 对各自 accounts_root 验，含 leaf-proof / accounts_root / dropped-added 三类负测）** / **`batch`（M29 异构批 SPV 演示：full peer 一次性出 `(Inclusion, Knn, Range, Diff)` 四 slot 的 `BatchResponseEnvelope`，wallet 端 `verify_batch` 派回四个 per-primitive 验证器，含 inclusion/knn-ordering/diff 三类负测）** / **`bridge`（M30 信任无关跨链桥演示：两条不同创世 A↔B，A 锁 12 µ$COG 到 B 账户 7，relayer 从 A 拿 `LockEnvelope` 投到 B 的 `BridgeEndpoint`，B 端 `verify_lock` → Ok → `minted(7)=12`，含 tampered-proof / wrong-destination / replay / tampered-root 四类 `BridgeError` 负测）** / `staking` / `slashing` / `certs` / **`localnet`（M33 进程内 tokio 4 验证人测试网经真实 loopback socket BFT 收敛）** / `run`（**M33 起 `--config` 联网 tokio 守护进程 + 分布式 BFT 投票；旧 `--dir` 播种语义由 `localnet` 取代**） / **`submit-tx`（M53 向运行中守护进程的 `[rpc]` 入口提交交易：加载配置、要求 `[rpc]` 启用、读 `--tx` 文件的 codec 编码字节、本地先 `decode_tx` 自检、阻塞 `std::net::TcpStream` POST 到 `rpc.listen`、打印接受的 hash 或拒绝原因，非 2xx ⇒ 非零退出）** / **`encode-tx`（M56 离线编写交易 = submit-tx 的生产端：从命令行旗标装配 `SubmissionTx`、`--key-file` 64-hex 32 字节种子经 `config::decode_seed` → `Keypair::from_seed` ed25519 签名、可选 `--config` 交叉校验派生 pubkey 对 genesis 作者（防"键/作者不匹配"）、纯函数 `parse_embedding`/`parse_review`/`multi_arg`/`build_signed_tx` + 新 `#[cfg(test)] mod tests`（+9）、`cmd_encode_tx` 自检 `decode_tx∘encode_tx` 往返后 `fs::write` 并打印 `encoded`/`bytes`/`out`；纯离线、无共识/wire/依赖变更）** / `status`（含确定性演示密钥） |

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
- **依赖策略变化**：M32 起 node（应用层）新增三个依赖——异步运行时 `tokio` + 配置的 `serde`/`toml`；M37 起再加 `tracing`/`tracing-subscriber`（守护进程结构化日志）；共识核心（`lib.rs`）仍 serde-free（`config.rs` 用镜像结构转换），`engine`（可嵌入/WASM）仍纯 std 零依赖。“节点纯 std / 零外部依赖”的旧表述自 M32 起仅适用于引擎。

这些构成后续里程碑（~~M7 持久化~~ ✅、~~M8 签名~~ ✅、~~M9 mempool 出块~~ ✅、~~M10 Merkle 认证状态~~ ✅、~~M11 BFT 最终性内核~~ ✅、~~M12 BFT 轮次状态机/活性~~ ✅、~~M13 认证链驱动~~ ✅、~~M14 证书落盘 + 重放复验~~ ✅、~~M15 P2P + gossip~~ ✅、~~M16 动态验证人集~~ ✅、~~M17 质押绑定权重 + 解绑期~~ ✅、~~M18 按证据罚没绑定质押~~ ✅、~~M19 P2P 传播块级 ops~~ ✅、~~M20 验证人集变更的轻客户端跟随协议~~ ✅、~~M21 验证人集 Merkle 承诺入区块头~~ ✅、~~M22 只拉头部的 SPV 轻同步传输~~ ✅、~~M23 钱包的账户-成员 SPV（双根承诺）~~ ✅、~~M24 批量化、类型化 SPV 原语（统一 GetProof/Proof 对 + 单一 verify_proof_against_header）~~ ✅、~~M25 单点图节点 cert-signed 包含证明~~ ✅、~~M26 图节点 cert-signed kNN 邻域证明~~ ✅、~~M27 图节点 cert-signed cosine 范围证明~~ ✅、~~M28 图节点 cert-signed 时序 diff 证明~~ ✅、~~M29 异构批 SPV 传输（一次性 inclusion + kNN + range + diff）~~ ✅、~~M30 信任无关跨链桥（relay + verify-from-counterparty）~~ ✅、~~M31 共识级跨链赎回 + 铸造（目的链链上）~~ ✅、~~M32 联网 tokio 守护进程（单定序器测试网：真实 TCP gossip + 文件化 config/genesis/keystore）~~ ✅、~~M33 分布式 BFT 投票（一进程一密钥，proposal/prevote/precommit 经真实 socket gossip + wall-clock 超时，无指定定序器）~~ ✅、~~M34 观测到双签即主动罚没（投票到达时检测 precommit 双签 → 组装证据接入 M19 flood → 链上没收 + 移出）~~ ✅、~~M35 配置驱动共识时序 + `create_empty_blocks`（`[consensus]` TOML 段可配超时/节奏、默认逐字段等于旧常量；空块可关，空闲不出块、有活即出，lazy-start 保活性）~~ ✅、~~M36 配置驱动网络时序（`[network]` TOML 段可配反熵心跳 `announce_interval_ms` / 验证人启动宽限 `startup_delay_ms`，默认 2000/1000 逐字段等于旧 `ANNOUNCE_SECS`/`STARTUP_DELAY` 常量；秒→毫秒统一到 `_ms` 约定；缺段/部分段回落默认，老配置逐字节不变）~~ ✅、~~M37 守护进程结构化日志（`tracing` 门面替换 5 处 `eprintln!` 为带级别带字段事件 + peer 连接/断开/区块落定生命周期事件；`RUST_LOG` 环境变量驱动级别，`init_tracing` 幂等订阅器写 stderr；宏无订阅器时 no-op 故行为逐字节不变）~~ ✅、~~M38 指标/健康端点（opt-in `[metrics]` TOML 段默认关闭，绑第二个 TCP 监听器以手拼极简 HTTP/1.1 应答 `GET` → Prometheus 文本曝露的 8 条 `zhixing_*` gauge + `zhixing_head_info`，`200 OK` 兼作健康检查；`Metrics` 快照经 `Cmd::Metrics` 只读取回，纯 `render_prometheus` 可单测；无新增依赖，默认关闭故 `localnet` head 逐字节不变）~~ ✅、~~M39 peer 发现 / 地址簿 gossip（`[[peers]]` 变种子集，节点间 gossip `(id, listen)` 地址簿、自动拨号 id 更大的新 peer 自补成全网状；`[network] enable_peer_exchange` 默认 true 可关；纯核照旧丢弃 `Peers`，Actor 独占发现；默认全网状配置逐字节不变）~~ ✅、~~M40 认证握手 / peer 认证（opt-in `[network] require_peer_auth` 默认 false，开则双向 ed25519 认证握手：各证明持有 genesis 为所声称验证人 id 绑定的密钥、签名覆盖双方新鲜 nonce 防重放；域分隔的 `auth_transcript`、`Arc<AuthContext>` 贯穿连接路径、无密钥 fail-fast；关时逐字节等于旧明文 hello 故 `localnet` head 不变）~~ ✅、~~M41 传输加密 / TLS（opt-in `[network] enable_tls` 默认 false，开则每条 P2P 连接包 TLS 1.3：临时自签证书 + accept-any 对端证书，仅加密不认证身份、认证仍归 `require_peer_auth` 且 M40 握手跑在隧道内可组合；`PeerStream` trait 统一裸 `TcpStream` 与 `TlsStream` 经 `Box<dyn>`+`tokio::io::split`，`tokio-rustls`+`rcgen` 钉 ring 后端避 C 工具链；指标端仍明文；关时 `Box::new(tcp)` 逐字节等价故 `localnet` head 不变）~~ ✅、~~M42 信道绑定 / channel binding（opt-in `[network] bind_channel` 默认 false，开则把每条 TLS 连接的 keying-material exporter（RFC 5705/8446）混进 M40 auth transcript，绑定认证身份到该 TLS 信道——MITM 的两条 TLS 腿导出不同 exporter 故转发的签名验不过，闭合 M41 的已知 MITM 边界；需 `enable_tls`+`require_peer_auth`，daemon 否则 fail-fast；无新增依赖复用 rustls 栈；关时 `auth_transcript(…, None)` 逐字节等于 M40 故 `localnet` head 不变）~~ ✅、~~M43 创世锚定 mTLS（opt-in `[network] require_peer_certs` 默认 false，开则双向 TLS 用 RFC 7250 裸公钥把每个节点的 genesis ed25519 密钥当 TLS 凭据出示，`GenesisPinnedVerifier` 同时作 server/client 验证器、仅当对端裸公钥 ∈ 创世验证人集才接受连接——非验证人连 TLS 隧道都建不起来，把 M41/M42 边界收在 TLS 层；仅 TLS 1.3，`secret_seed`→PKCS#8→`any_eddsa_type`→`AlwaysResolves*RawPublicKeys`；需 `enable_tls`+验证人密钥，daemon 否则 fail-fast；无新增依赖复用 rustls 栈；关时 `build_tls_setup(None)` 逐字节等于 M41 故 `localnet` head 不变）~~ ✅、~~M44 配置驱动日志 / `[logging]` 段（可选 `[logging]` 段两旋钮：`level` 覆盖 `RUST_LOG` 未设时的默认过滤、`format` `text` 默认 | `json`；缺段⇒逐字节等于 M37（`RUST_LOG` 过滤、`info` 回落、text、stderr），`init_tracing()` 委托 `init_tracing_with(None)`，`cmd_run` 改为先加载配置再装订阅者；`json` 用 `tracing-subscriber` 的 `json` feature 非新依赖）~~ ✅、~~M45 日志文件目标 + 轮转（`[logging]` 段再加 `file` 路径 + `rotation` = `daily` 默认 | `hourly` | `minutely` | `never`：`file` 空⇒stderr（M44/M37 行为）、非空⇒经 `tracing-appender` 的 `RollingFileAppender` 阻塞写滚动文件，直接作 `MakeWriter` 无 `WorkerGuard`；单目标（文件或 stderr、不 tee）；`validate` 拒未知 rotation → `BadLogRotation`；缺段/`file` 空⇒逐字节等于 M44/M37 故 `localnet` head 不变；`tracing-appender` 为 node crate 专属小依赖、引擎仍零依赖）~~ ✅、~~M46 多目标日志 stderr + file tee（`[logging]` 段再加 `stderr` bool 默认 `false`：配了 `file` 且 `=true` ⇒ 日志同时进滚动文件与 stderr；`file` 空⇒stderr only、`file` 非空+`stderr=false`⇒file only（M45）、`file` 非空+`stderr=true`⇒tee；前两单-sink 臂逐字保留、tee 臂经分层 `Registry` 两个 `fmt::layer` 共享单一 `EnvFilter`；无新依赖/feature（registry/fmt/ansi 均默认 feature）；缺省⇒逐字节等于 M45/M44/M37 故 `localnet` head 不变）~~ ✅、~~M47 每-sink 独立日志级别（`[logging]` 段再加 `stderr_level`/`file_level` 两 directive 字符串默认空：tee 的两 sink 各带自己的 `EnvFilter`、空⇒继承 `level`；只在 tee（`file`+`stderr=true`）且 `RUST_LOG` 未设且至少一非空时走新 `init_tee_leveled`（每 layer `.with_filter`），否则回落 M46 共享-filter `init_tee`；`RUST_LOG` 仍全局覆盖；缺省⇒逐字节等于 M46/M45/M44/M37 故 `localnet` head 不变）~~ ✅、~~M48 每模块 directive 数组（`[logging]` 段给三个标量过滤旋钮各配数组对应物 `levels`/`stderr_levels`/`file_levels` 默认空 vec：非空即经纯 `resolve_directive` 以 `,` 连接后胜过标量、空 vec 回落标量，让每模块 directive 用 TOML 数组形状而非往单条字符串塞逗号；`EnvFilter::new("a,b,c")` 本就解析逗号故只是一次 join、无新 API；优先级 `RUST_LOG` > 数组 > 标量 > 继承 `level`；缺省⇒逐字节等于 M47/M46/M45/M44/M37 故 `localnet` head 不变）~~ ✅、~~M49 每-sink 独立日志格式（`[logging]` 段再加 `stderr_format`/`file_format` 两枚举旋钮 `text`|`json` 默认空：tee 的两 sink 各自选 formatter、空⇒继承 `format`，实现"控制台人读文本、文件机读 JSON"（或反之）；只在 tee（`file`+`stderr=true`）且至少一非空时把 `init_tee_leveled` 泛化为 2×2 `match (stderr_json, file_json)` 各建带 `.json()` 的 typed layer、否则回落 M46 共享 `init_tee`；format 独立于 `RUST_LOG`（后者只管过滤不管格式）、与标量 `format` 一样枚举式校验（拒未知 → `BadLogFormat`、空⇒继承）；缺省⇒逐字节等于 M48/M47/M46/M45/M44/M37 故 `localnet` head 不变）~~ ✅、~~M50 日志文件保留（`[logging]` 段再加数字旋钮 `max_files` 默认 `0`：轮转日志文件保留上限、超出删最旧，`0` ⇒ 无界即 M45 原 `RollingFileAppender::new` 路径逐字节相同、`> 0` ⇒ 走 `tracing-appender` builder 的 `max_log_files(n)`、builder 出错 best-effort 回落 `::new`；纯计数故不需 `validate`；只对 `file` 目标生效、与 `rotation="never"` 组合无害；`build_file_appender`/`init_tee`/`init_tee_leveled` 各透传 `max_files`；无新依赖复用 tracing-appender；缺省⇒逐字节等于 M49/…/M37 故 `localnet` head 不变）~~ ✅、~~M51 广告地址 / advertise_addr（`[network] advertise_addr` 默认 `""`：M39 peer 发现中节点为自己广告的可拨地址，用于 NAT / 端口映射 / `0.0.0.0` 通配绑定——绑定仍是 `cfg.node.listen`、仅改 gossip 出去的自地址；非空须解析为 `SocketAddr`（复用 `parse_addr` → `BadAddr`，拒 DNS 主机名）、加载期校验；纯函数 `self_advertise_addr` 只改播种一处、下游 `peers_msg`/`on_peers` 不动；空 ⇒ 逐字节等于 M39 故 `localnet` head 不变）~~ ✅、~~M52 更丰富指标 / 单调计数器（M38 metrics 端点在 8 条 gauge 之外加四条累计 counter `zhixing_{peer_connects,local_txs,blocks_committed,slashing_events}_total`，回答速率/吞吐类问题；`u64` 计数器 actor 独占裸 `+= 1` 无 atomics、在 `Cmd::Register`/`Cmd::LocalTx`/`on_decided` 成功分支/`on_equivocation` 四个单一事件点自增，`blocks_committed` 只计本节点自身共识终局化块（anti-entropy 同步块不计）；`render_prometheus` 加 `counter` 闭包发 `# TYPE … counter`；端点仍 opt-in 默认关、只读簿记无 config/wire/依赖变更 故 `localnet` head 不变）~~ ✅、~~M53 外部交易入口 RPC（opt-in `[rpc]` TOML 段默认关闭，绑第三个 TCP 监听器以手拼极简 HTTP/1.1 应答 `POST /submit_tx`——请求体为原始 `codec::encode_tx` 字节、解码后走正常 mempool 准入、`200`+hash / `400` 解码失败 / `422`+原因，`GET`/`HEAD` → `200 ok` 兼作健康探针；`submit_local_checked` surfacing 拒绝原因、`submit_local` 在其上委托逐字节不变；`Cmd::SubmitTx` 带 oneshot ack、`Node::submit_tx` 句柄、`parse_content_length`/`http_response` 辅助、有界读 header≤8KiB / body≤64KiB；配 `node submit-tx --config F --tx F` CLI 经阻塞 `TcpStream` POST；补上生产头号阻断「无外部交易入口」；端点默认关且提交路径复用既有校验故 `localnet` head 不变、无 wire/共识/依赖变更）~~ ✅、~~M54 mempool DoS 加固（可配 `[mempool]` 段：容量上限 `capacity`（默认 4096）在 `Mempool::insert` 准入处强制、满则 `ChainError::MempoolFull`→RPC `422`；每-peer 令牌桶限流 `per_peer_tx_per_sec`（默认 `0.0`=关）/`per_peer_tx_burst` 只门控 `GossipMsg::Tx` 入口、本地提交永不限流；`max_block_txs`（默认 64）出块上限也挪进本段可配；指标加 `zhixing_mempool_capacity` gauge + `zhixing_txs_rate_limited_total` counter；货币费用明确另列后续共识里程碑；默认不触发/默认关故 `localnet` head 不变、无 wire/共识/依赖变更）~~ ✅、~~M55 gossip 去重集上限 + FIFO 驱逐（三个洪泛去重集 `seen_tx`/`seen_evidence`/`seen_stake_op` 原为无界 `BTreeSet` 单调永涨 = 最后的无界内存 DoS 向量；新私有 `SeenSet` = `BTreeSet`（O(log n) 成员）+ `VecDeque`（插入序 FIFO 驱逐）+ 容量，满则逐出最旧而非拒收——去重集须持续接纳以压制洪泛，驱逐仅致有界再洪泛、不碰共识安全（准入仍由 `validate_tx` 独立门控）；经 `[mempool] seen_cache` 配，默认 `0` = 无界/关，哨兵刻意不对称于 mempool `capacity` 的 `0`=拒；`usize::MAX` 无界模式下 deque 从不触碰故逐字节等于旧 `BTreeSet`；指标加 `zhixing_seen_tx`/`zhixing_seen_tx_capacity` gauge；默认无界故 `localnet` head 不变、无 wire/共识/依赖变更）~~ ✅、~~M56 交易编写助手 node encode-tx（M53 打通 submit-tx 投递但节点无任何东西产出 encode_tx 字节；新 encode-tx 子命令从命令行旗标装配 SubmissionTx + ed25519 种子签名 + 写 wire 编码到文件 = submit-tx 生产端；--key-file 存 64-hex 32 字节种子同验证人 seed_hex 格式、复用 config::decode_seed（fn→pub fn）、可选 --config 交叉校验派生 pubkey 对 genesis 作者防"键/作者不匹配"、纯函数 parse_embedding/parse_review/multi_arg/build_signed_tx 便于单测、cmd_encode_tx 自检 decode∘encode 往返后落盘并打印 encoded/bytes/out；纯离线 CLI 增量不碰共识/mempool/wire/状态、无引擎/codec/crypto 变更故 localnet head 不变）~~ ✅、~~M57 每账户 mempool 配额（mempool 只按内容哈希索引、从不按作者索引，故一个有效账户可签无数异 tx 独霸全部 capacity 槽饿死他人准入——每-peer 限流只管流量不管每作者占用；M57 加每账户 pending 配额：`Mempool` 加 `per_author` 作者→计数索引 + `per_account_limit`（默认 `usize::MAX` 无界）+ `rejected_quota` 计数器，`insert` 在 validate_tx 后、capacity 门之侧判配额（仅新哈希计数、幂等重插不重计）、`remove_included` 归还槽；新 `ChainError::AccountQuotaFull` 经既有映射自动 RPC `422`；配 `[mempool] per_account_limit` 默认 `0`=关沿用 seen_cache 哨兵；指标加 `zhixing_mempool_per_account_limit` gauge + `zhixing_txs_quota_rejected_total` counter；纯准入侧默认关故 localnet head 不变、无 wire/共识/依赖变更）~~ ✅、~~M58 读类 RPC 查询（M53 开了 `POST /submit_tx` 入口但至今无从经 wire 读链上状态——RPC 服务仅按方法分发、URL 路径从不解析；M58 加读类 GET 路由 `GET /height`/`/head`/`/account/{id}`：纯路由器 `route_get`（`/` 与无法识别路径仍回健康探针 `200 ok`、仅 `/account/<非数字>`→`404`）+ 纯 `format_account` 渲染 `key=value` 纯文本；`/height`/`/head` 复用 `Cmd::Query`、账户查询加新 `Cmd::QueryAccount`→`Option<Account>` 克隆快照 + `Node::account` 句柄；GET 分支置于既有 POST 之前、send 失败→`503`、查不到→`404`，POST `/submit_tx` 逐字不变；RPC 默认关故 localnet head 不变、无 config/wire/共识/依赖变更）~~ ✅、~~M59 可验证账户读 RPC（M58 读类 GET 把余额明文回调用方＝信任节点；M59 加 `GET /account/{id}/proof` 复用 M20–M29 既有 SPV 栈——新公有 `account_inclusion(id)` 包 `serve_inclusion(Account,id)` 的 `ProofEntry` + 头的 `CertifiedHeader`，经 `Cmd::QueryAccountProof` oneshot + `Node::account_proof` 句柄，`GetRoute::AccountProof` 从 `/account/{id}` 拆出 `/account/{id}/proof`，`format_account_proof` 回两行 hex `certified_header=`/`proof_entry=`（复用既有编码、无 JSON）；客户端用既有 `verify_proof_against_header` 对自持创世验证人集本地复算+验证、节点侧零新增验证路径；把读从「节点说余额是 X」升级为「X 可对 > 2/3 签名头自证」；RPC 默认关故 localnet head 不变、无 wire/共识/依赖变更）~~ ✅、~~M60 其余实体可验证读 RPC（M59 的 `GET /account/{id}/proof` 只接账户单类，而 cert-bound SPV 栈自 M24–M25 已覆盖全部四类 ProofKind、仅差 RPC 暴露；M60 补 `GET /reviewer|/validator|/graph/{id}/proof` 三姊妹路由（图节点按插入序）：`account_inclusion` 泛化为 `inclusion(kind,id)`、经 `Cmd::QueryInclusion`+`Node::proof(kind,id)` 句柄、`route_get` 以 `proof_route` 拆三前缀 `/proof` 后缀（裸 id→404）、响应复用 kind-无关 `format_account_proof`、404 体由 `proof_kind_label` 区分；验证人证明走 next_validators_root、其余走 accounts_root，客户端仍用既有 verify_proof_against_header 零信任验证、节点侧零新增验证路径；RPC 默认关故 localnet head 不变、无 wire/共识/依赖变更）~~ ✅、~~M61 批量/异构证明 RPC（M59/M60 单值可验证读每值一次往返；gossip 侧异构批量 `GetBatch`→`serve_batch`→`BatchResponseEnvelope`（Inclusion/kNN/Range/Diff 四类）自 M29 即在、仅差 RPC 暴露；M61 补 `POST /batch` 承载编码 `Vec<BatchItem>`、回头部 `CertifiedHeader` + `BatchResponseEnvelope` 两行 hex——抽出独立 `encode_batch_request`/`decode_batch_request`（字节与 gossip `GetBatch` 逐字一致、`encode_gossip` 委托）、`GossipNode::batch` 复用 `serve_batch` 绑认证头、`Cmd::QueryBatch` + `Node::batch_proof` + `format_batch`、POST 按 `/batch` 分流其余仍 M53 提交、客户端以既有 `verify_batch` 零信任验整批；RPC 默认关故 localnet head 不变、无 wire/共识/依赖变更）；M62 `/batch` 随附 Diff `[1..=h₂]` 区间块——新 `blocks_through` 未截断产区间、`batch()` 回 `BatchReply` 三元组、独立 `encode_blocks`/`decode_blocks`（内嵌 gossip `Blocks` 字节、encode 委托）、`format_batch` 加 `range_blocks=` 行，Diff 槽经既有 `verify_batch(&range,…)` 免预同步自证；gossip `Blocks` wire 逐字不变、head 不变；M63 桥锁证明经 RPC——`GET /bridge/lock/{id}/proof` 复用自足的 `serve_lock`/`LockEnvelope`（header+cert+tracked set+lock+proof 自带、无需新 net.rs 产出）、新 `Cmd::QueryLock`/`Node::lock_proof`/`format_lock`（单行 hex `lock_envelope=`）、`route_get` 加 `/bridge/lock/` 分支，客户端经既有 `BridgeEndpoint::verify_lock` 自证免预取；守护进程不入桥锁故 200 以驱动造链在进程内验、TCP 测 404；RPC 默认关故 head 不变、无 wire/共识/net.rs 变更；M64 桥锁 id 枚举经 RPC——`GET /bridge/locks` 明文目录列全链每把锁（补 M63 单锁证明的发现面，调用方不再须先知 id）：`net.rs` `lock_listing`（`bridge_locks` ∪ `bridge_lock_heights` → `(id,height,lock)` id 序）、`light.rs` 别名 `LockListing`、`Cmd::QueryLocks`/`Node::lock_listing`/`format_lock_listing`（每锁一行 `lock_id=…`）、`route_get` 精确匹配 `/bridge/locks`（不撞 M63 `/bridge/lock/` 前缀）；明文读故空链 `200`（空目录合法，异于 M63 的 `404`）；RPC 默认关故 head 不变、无 wire/共识/依赖变更；M65 明文实体读经 RPC——`GET /reviewer|/validator|/graph/{id}` 补 M58 账户明文读的三类姊妹读（M60 只开了这三类的 `/proof` 可验证读、独缺明文）：`proof_route` 更名 `entity_route` 加裸-id 臂（有 `/proof` 后缀→M60 `Proof`、无则新 `GetRoute::Plain(ProofKind,id)`）、`Cmd::QueryEntity`→`Option<EntityView>` actor 内就地读 `reviewers`/`validators`/`graph.nodes`（与 M58 `QueryAccount` 同形、无 net.rs 产出）、`Node::entity` 句柄、`EntityView` 三变体 + kind-无关 `format_entity`（reviewer→`reputation=`、validator→`power= pubkey=`、graph→`node_id= domain= dim= embedding=`）、`GetRoute::Plain` 处理臂 `200`/`404`/`503`；明文未验证读，零信任客户端仍走 M60 `/proof`；`test_genesis` 实载三类故真实 TCP 可断言带数据 `200`；RPC 默认关故 head 不变、无 wire/共识/依赖变更；M66 选择性 JSON 响应体——M58–M65 的结构化明文读只回一种 `key=value`/裸 token 文本，对程序化客户端尴尬；M66 加 `?format=json` 查询参数于五条结构化读（`/height`/`/head`/`/account/{id}`/`/reviewer|/validator|/graph/{id}`/`/bridge/locks`）回手写极简 JSON（无 `serde_json`、无新依赖）、省略或 `format=text` 则逐字节同今日文本：纯 `split_query`（首 `?` 拆 path/query、不拆 `#`）+ `RespFormat`/`response_format`（精确大小写敏感 `format=json`、首个 `format=` 胜）、`http_response` 委托 `http_response_ct` 使文本输出不变、JSON 体用裸 `application/json`；手写 `json_str`（RFC 8259 转义控制符）/`json_u64`（**按用户决定编码为 JSON 字符串**、无损）/`json_f32`（非有限→`null`）/`json_account`/`json_entity`（带 `"kind"` 判别符、embedding→JSON 数组）/`json_lock_listing`（JSON 数组、空→`[]`）、`ok_body`/`not_found_body` 穿入五臂；hex/proof 读（`/…/proof`、`POST /batch`）与 503/health/路由-404 保持纯文本（`?format=json` 于其上静默回 hex；故数据-404 回 `{"error":…}` 而路由-404 回文本 `not found`，刻意不对称）；RPC 默认关故 head 不变、无 wire/共识/依赖变更；M67 JSON 响应体扩展至证明/提交读——M66 的 `?format=json` 止于结构化明文读，hex/proof 读（`/…/proof`、`POST /batch`）与 `POST /submit_tx` 回执仍只回文本；M67 把同一双渲染收口到整条读面：通用 `error_body`（泛化 `not_found_body`、`400`/`422` 也回 `{"error":…}`）+ 三个 hex 包装渲染器 `json_account_proof`/`json_lock`/`json_batch`（信封保持不透明 hex 字符串、客户端仍以既有 verify_* 栈解码、无新解码/无结构暴露）+ 提交成功 `{"hash":<hex>}`，四条证明臂与两条 POST 回执穿入 `serve_rpc_conn` 顶部已算的 `fmt`、纯框架/路由错误仍文本；无 `serde_json`/无新依赖、RPC 默认关故 head 不变、无 wire/共识/依赖变更；M68 Accept 头内容协商——M66/M67 的 `?format=json` 是项目自造触发器，通用 HTTP 客户端无从在不特判 URL 下请求 JSON；M68 加标准 HTTP 内容协商：客户端可用 `Accept: application/json` 请求头替代查询参数触发 JSON，优先级显式 `?format=` 查询 > `Accept` 头 > 文本默认——`response_format` 改返 `Option`（区分"无 format 参数"与"format=text"）、新纯 `accept_format`（仿 `parse_content_length` 大小写不敏感扫头、值含 `application/json` 即 Json、`*/*`/`application/*` 保持文本、无 q 值加权）、新纯 `resolve_format` 编码优先级、分发点一行换；两者皆无时逐字节同旧、无 `serde_json`/无新依赖、RPC 默认关故 head 不变；M69 Accept 头 q 值加权与 406——M68 的 `accept_format` 走无加权子串测试且对无法满足的 `Accept` 从不回 `406`，故 `Accept: text/plain;q=0.9, application/json;q=0.1` 被误判为 JSON、`Accept: application/xml` 静默回文本；M69 按 RFC 7231 §5.3 q 值在 `application/json` 与 `text/plain` 间择优并在两者皆被排除时回 `406 Not Acceptable`：新纯 `parse_qmilli`（q 值→定点毫单位 `0..=1000`、整数比较避 `clippy::float_cmp`、缺省/非法→1000、越界 clamp）/`media_match`（对一具体类型扫媒体范围返 `(q_milli, 特异度)`、特异度 exact=3>`type/*`=2>`*/*`=1>0、并列破为高 q）、`accept_format` 改返 `Negotiation{Absent,Use(RespFormat),NotAcceptable}` 三态（json 与 text 皆 q=0⇒`NotAcceptable`、否则高 q 胜、精确 q 并列仅当 json 被命名 spec≥2 才偏 json）、`resolve_format` 改返 `Option<RespFormat>`（`None`=`406`、查询仍优先且永不 `406`）、分发点加一处 `406` 早返（体恒 `text/plain`）；每条 M68 `accept_format` 行为逐一保持；无 `serde_json`/无新依赖、RPC 默认关故 head 不变、无 wire/共识/状态根变更；M70 `406` 列出可用表示——M69 的 `406` 体只是裸 `not acceptable`、未告知本该可接受的类型；M70 按 RFC 7231 §6.5.6 让 `406` 体枚举本服务可产表示（`application/json`、`text/plain`），出自单一 `OFFERED_MEDIA_TYPES` 真源 + 纯 `not_acceptable_body`、体仍 `text/plain`（客户端已拒我方类型故协商错误体无意义）、显式 `?format=` 仍优先且永不 `406`、无 `Accept` 仍回文本默认故逐字节同旧；无 `serde_json`/无新依赖、RPC 默认关故 head 不变、无 wire/共识/状态根/依赖变更；M71 `Vary: Accept` 响应头——M66–M70 让同一资源按 `?format=` 查询或 `Accept` 头择 `text/plain` 或 `application/json`，但响应从不声明体是协商选出的；RFC 7231 §7.1.4 要求发 `Vary` 列出表示选择所依赖的请求头（此处 `Accept`），否则共享缓存可能把一客户端的 JSON 体回放给文本客户端或反之；M71 给每条 RPC 响应加 `Vary: Accept`，缓存据此按 `Accept` 分键——一行头字段加在唯一响应构造器 `http_response_ct`（协商内容、框架/健康/503 一致携带，无害且合惯例），指标端点另有构造器故不动、Prometheus 抓取头稳定；无 `serde_json`/无新依赖、RPC 默认关故 head 不变、无 wire/共识/状态根变更；M72 桥锁目录分页——`GET /bridge/locks`（读面唯一无界列表）加 opt-in `?offset=`/`?limit=` 窗口：`?offset=M` 跳前 M 条、`?limit=N` 截窗至 N 条，缺省/非法⇒offset 0/无界 limit 故默认体与 M64 逐字节同，`limit=0`/offset 越界回空窗仍 `200`（空窗合法一如 M64 空目录）；纯 `usize_param`（首个匹配键胜、非法/缺失⇒`None`、仿 `response_format`）+ 泛型纯 `paginate`（offset/limit 窗口、饱和算术防溢出、空 limit⇒至末尾、未来列表读可复用）+ `BridgeLocks` 臂三行接线经既有 `format_lock_listing`/`json_lock_listing` 渲染子切片，参数仅作用于 `/bridge/locks`、可叠 `?format=json`；无 `serde_json`/无新依赖、无 wire/共识/状态根/引擎变更、RPC 默认关故 head 不变；M73 离线密钥生成 node keygen——新 `node keygen --out F [--seed HEX]` 补 M56 `encode-tx --key-file` 的生产端（此前节点无工具产出 64-hex 种子文件）：缺 `--seed` 经已是直接依赖的 `getrandom` 取 32 字节 CSPRNG 种子、有则复用 `config::decode_seed`（同 `--key-file` 的解码 + BadHex 路径）确定性派生，`Keypair::from_seed` 后写 64-hex 种子到 `--out`（可直接喂 `encode-tx --key-file`）并打印派生 pubkey（可贴进 genesis `accounts` 条目）；纯 `keygen_derive(seed)->(seed_hex,pub_hex)` 无 RNG/IO 便确定性单测、`cmd_keygen` 仅作种子取用 + 落盘薄壳复用 `req_arg`/`opt_arg`/`fail`/`fail_msg`；纯离线 CLI 增量、无引擎/codec/crypto/wire/共识变更、无新依赖（`getrandom` 已是直接依赖）故 head 不变~~ ✅、~~M74 结构化 JSON 证明信封——M66/M67 把证明/批量读的自验证信封整体编码成不透明 hex 塞进命名 JSON 字段，客户端读高度/状态根仍须引入二进制 codec；M74 起解码证明读与批量读共享的 `certified_header` 子信封为 `{"header":{…},"cert":{…}}` 嵌套对象（新 `json_block_header` 区块头 15 字段 + `json_commit`/`json_vote`/`json_validator_update`，沿用 M66 `json_*` 约定——u64/u32 lossless 引号串、hash/sig 走 `json_str(hex)`、f32 数或 null、`vote_type` 串判别），`proof_entry`/`batch_envelope`/`range_blocks`/`lock_envelope` 仍 hex（顺延 M75+）；两处调用点 `json_account_proof`/`json_batch` 换嵌入、`json_lock` 不动；新纯测 `json_certified_header_structured` + 更新 M67 两测，共 407 测；纯渲染增量、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M75 结构化 JSON 证明条目——M74 只解证明/批量读共享的 `certified_header`、`proof_entry` 仍不透明 hex（JSON 客户端读不出被证明的账户余额/声誉/权重/嵌入或 merkle 路径）；M75 续解之，新 `json_merkle_proof`（Merkle 路径 `{"side":"left|right","hash":hex}` 逐步）+ `json_proof_entry`（4 种 typed leaf：account 复用 `json_account`、reviewer/validator/graph 镜像 `json_entity`，每支带包含路径），`proof_entry` 由 hex 变 `{"kind":…,<叶子字段>,"proof":{…}}` 对象、`json_account_proof`（账户+三实体证明读共用）整读全结构化不再含 hex，`batch_envelope`/`range_blocks`/`lock_envelope` 仍 hex（`json_batch` 不动，其 `ProofEntry` 在批量信封内 → M76+）；新纯测 `json_proof_entry_structured` + 更新三测，共 408 测；纯渲染增量、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M76 结构化 JSON 批量信封——M74/M75 已解 `certified_header`/`proof_entry` 使证明读整体结构化、但批量读（`POST /batch?format=json`）的 `batch_envelope` 仍不透明 hex（JSON 客户端读不出批量回答：包含条目 / kNN / 范围邻居 / 时序 diff）；M76 续解之，对全 4 种 `BatchResponseItem` 变体（Inclusion/Knn/Range/Diff）：Inclusion 复用 `json_proof_entry`，Knn/Range 经新 `json_graph_node`（嵌套形）/`json_graph_leaf` + `json_knn_claim`/`json_range_claim`，Diff 经 `json_diff_envelope` 复用 `json_block_header`/`json_commit` + `json_validator_set`，`None`→裸 `null`；`batch_envelope` 由 hex 变 `{"items":[{"kind":…,…}]}` 对象、`json_batch` 一处调用点换嵌入，`range_blocks`（块体区间 → M77）/`lock_envelope` 仍 hex；新纯测 `json_batch_envelope_structured` + 更新批量断言，共 409 测；纯渲染增量、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M77 结构化 JSON 锁信封——M76 解完批量信封后读面仅剩 `range_blocks` 与桥锁证明读（`GET /bridge/lock/{id}/proof`）的 `lock_envelope` 仍 hex；M77 解码 `lock_envelope`——`LockEnvelope`（认证源头 + 证书 + 跟踪验证人集 + 锁 + merkle 路径）整体结构化，新 `json_bridge_lock`（6 字段含签名）+ `json_lock_envelope` 复用 `json_block_header`/`json_commit`/`json_validator_set`/`json_merkle_proof`，`json_lock` 一字段由 hex 变嵌套对象，`range_blocks` 为读面最后的 hex 字段（→ M78）；新纯测 `json_lock_envelope_structured` + 更新锁断言，共 410 测；纯渲染增量、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M78 结构化 JSON 区块区间——M77 解完锁信封后整条读面仅剩批量读的 `range_blocks`（`[(Block, Commit)]` 全量块体）仍 hex；M78 解码之为 `[{"block":{…},"commit":{…}}]`，新 `json_block`（8 头标量 + 7 体向量）+ `json_review`/`json_submission_tx`/`json_stake_op`（`bond`/`unbond` 判别串）/`json_slash_evidence`/`json_bridge_header`/`json_bridge_redeem` 叶渲染器，复用 `json_block_header`/`json_commit`/`json_validator_update`/`json_vote`/`json_bridge_lock`/`json_validator_set`/`json_merkle_proof`/`json_embedding`，整条读面再无不透明 hex 字段；新纯测 `json_range_blocks_structured` + 更新 `json_proof_renderers_render` 空区间断言，共 411 测；纯渲染增量、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M79 分页 `total`/`next` 信封——M72 给 `/bridge/locks` 加 `?offset`/`?limit` 窗口却仍返裸列表，读不出总数、判不出是否还有下页；M79 裹进 `total`（未切总数）/`next`（下页 offset，末页为无）信封，新 `format_page`/`json_page` 通用包裹器复用 `format_lock_listing`/`json_lock_listing` 与 `paginate`/`usize_param`，文本加 `total=`/`next=` 头行、JSON 变 `{"total":"N","next":"M"|null,"items":[…]}`；新纯测 `lock_page_envelopes` + 更新三处桥锁 TCP 断言，共 412 测；纯渲染 + 一处 handler 算术、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M80 机读 JSON `406` 体——内容协商失败的 `406` 体此前仍硬编码 `text/plain` 人读句子；M80 换成机读 `{"error":"not_acceptable","available":["application/json","text/plain"]}`、`Content-Type: application/json`，新 `not_acceptable_json` 从单一真相源 `OFFERED_MEDIA_TYPES` 渲染、取代 `not_acceptable_body`，`http_response_ct` 仍带 `Vary: Accept`；`406` 仅在显式不可满足 `Accept` 触发故体恒 JSON、无 `Accept` 明文读路径不变，补齐读面协商对称；重写纯测 `not_acceptable_json_lists_representations` + 更新 `rpc_406_over_tcp` 断言为 `application/json`，仍 412 测；纯渲染 + 一处 emit 点替换、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅、~~M81 页大小上限与默认 limit——M72/M79 给 `/bridge/locks` 的 `?limit` 留了两道口子：缺省返整表（`paginate` 的 `None` 走到末尾）、显式又无上界，列表长大后单读体不设限；M81 加纯辅助 `effective_limit` + 两常量 `DEFAULT_PAGE_LIMIT=50`/`MAX_PAGE_LIMIT=500`：缺省 `?limit` 回落默认、请求 `?limit` 钳到上界、`?limit=0` 仍为显式空页，取代 M72/M79「缺省 limit ⇒ 整表」RPC 承诺，`total`/`next` 信封标余页故仍可翻到末尾；新纯测 `effective_limit_caps_and_defaults`，共 413 测；纯算术 + 两行 handler、无 `serde_json`/无新读/wire/共识/依赖、RPC 默认关故 head 不变~~ ✅……；运维篮子剩余（货币费用（独立共识里程碑）、费用优先出块排序、nonce 反重放、`Accept-Charset`/`Accept-Encoding`、RPC auth/TLS、证书/密钥轮换与落盘、follower 认证、每-sink 独立 rotation 覆盖、OTEL/结构化日志 exporter、指标端 TLS、指标 push exporter/直方图/每-peer/每-轮次时延序列）顺延至 M82+），每步仍遵循"可运行、可测试、契约一致"。
