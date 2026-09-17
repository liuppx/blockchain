# ZhixingGraph 参考节点（Rust · Milestone 6–20）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §5「PoK 共识」与 §7「技术架构」。

这是把 ΔK 引擎（[`engine/`](../engine/)）与经济仿真（[`sim/`](../sim/)）背后的规则，落成一个**可运行、确定性的 PoK 共识状态机**——真正"跑链"的最小内核：区块、交易、账户、状态转移、铸造/罚没、链上声誉、ed25519 签名交易、追加式持久化、确定性 mempool 出块、Merkle 认证状态与轻客户端证明、BFT 最终性证书与验证人集、驱动活性的 BFT 轮次状态机（超时 / 锁定 / 换轮）、逐高度生长的**BFT 认证链**（mempool → 共识 → 提交，每块附可验证证书）、**证书落盘 + 重放即最终性复验**（`blocks.log` + `certs.log`，重放时逐高度复验 > 2/3 证书，恢复的是*最终性*而非仅状态）、**链上/动态验证人集**（区块携带验证人增删/改权，由变更前的集合认证、下一高度生效，重放随之逐高度跟随演进）、**P2P gossip 与反熵状态同步**（交易 epidemic 泛洪 + 认证块拉取追赶，逐块对链上验证人集复验证书，含真实 loopback TCP 传输）、**质押绑定的验证人权重与解绑期**（账户自绑定 $COG → 成为验证人、权重 == 绑定量；解绑经时间锁提款队列，资金留在池中仍可罚没直至到期返还）、**按证据罚没等价双签**（把冲突预提交的密码学证据搬上链，罚没作恶验证人的绑定质押与解绑中金额入 treasury、下一高度移出验证人集，供应守恒）、**P2P 传播证据与质押变更**（`SlashEvidence`/`StakeOp` 经 gossip 进入每个节点的待打包池，下一区块由出块方带出——作恶可归责、质押可远程触发，不再仅靠出块人已持有），**验证人集变更的轻客户端跟随协议**（只凭创世信任根，逐高度对当前集合复验最终性证书、再复刻该块引起的验证人集迁移——不执行任何交易、不追踪账户余额，即得到与全量重放逐字节一致的活跃验证人集），以及内容寻址的区块哈希链与状态根。

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
cargo run --release --bin node -- staking          # 质押：绑定 $COG 获得验证人权重；解绑经时间锁提款到期返还
cargo run --release --bin node -- slashing         # 罚没：验证人双签 → 提交证据 → 绑定质押罚没入 treasury、移出验证人集
cargo run --release --bin node -- certs  --dir DIR # 证书落盘：产出认证链→落盘 blocks/certs→重放复验最终性
cargo run --release --bin node -- run  --dir DIR   # 持久化链：首次落盘演示块，之后重放
cargo run --release --bin node -- status --dir DIR # 重放区块日志并打印状态
cargo test --release                               # 125 项单元测试（见下）
```

演示链展示：新颖提交铸造 $COG、跨域桥接拿到 novelty+bonus（ΔK>1）、近重复/低质提交被**罚没入 treasury**、供应守恒、评审声誉按链上结果升降。

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
```

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 状态机核心：`Block`/`SubmissionTx`/`StakeOp`/`SlashEvidence`（含 `hash()` 内容寻址用于 gossip 去重）/`Account`/`ChainState`/`Chain`、`apply_block`（含验证人集跨高度切换、stake_ops 应用、解绑到期返还与按证据罚没）、`apply_stake_op`、`apply_evidence`、`replay`/`replay_verified`、验签、`state_root`（含验证人集 + 绑定/解绑状态）/`merkle_root`/`account_proof`、供应守恒不变量（含 bonded + 解绑中）+ 测试 |
| `src/mempool.rs` | 确定性 mempool 与出块：内容寻址排序 + 试算式 `build_block` + 测试 |
| `src/merkle.rs` | 二叉 Merkle 树：域分隔叶/节点、奇数提升、包含证明 `Proof`/`verify` + 测试 |
| `src/validator.rs` | 验证人集与确定性提议人（Tendermint 优先级累加器）、链上变更 `ValidatorUpdate`/`apply_updates` + 测试 |
| `src/consensus.rs` | BFT 投票/最终性证书：`Vote`/`Commit`/`verify`、`commit_block`、`detect_equivocation` + 测试 |
| `src/round.rs` | BFT 轮次状态机（Tendermint `upon` 规则、超时/锁定/换轮）+ 进程内网络模拟器 `Sim` + 测试 |
| `src/driver.rs` | BFT 认证链驱动 `ChainDriver`：逐高度 mempool→共识→提交 + 证书保留 + 故障注入 + 链上验证人变更（`stage_validator_update`）+ 质押变更（`stage_stake_op`）+ 罚没证据（`stage_slashing_evidence`）+ 测试 |
| `src/net.rs` | P2P gossip 与反熵同步：`GossipMsg`/`GossipNode`（纯状态机，认证块 `apply_certified` 复验证书、交易 epidemic 泛洪去重 + **`Evidence` / `StakeOp` 块级 ops 的待打包池与去重 flood**）+ 确定性 `Network` 收敛总线 + `encode_gossip`/`read_msg`/`write_msg`（真实 socket 分帧）+ 测试 |
| `src/light.rs` | 轻客户端验证人集跟随：`ValidatorTracker`（`from_genesis` / `follow` / `follow_all`，逐高度复验证书 + 复刻 `apply_block` 的集合迁移，镜像 `bonds` + 创世公钥表，不执行交易）+ `LightError` + 测试 |
| `src/crypto.rs` | ed25519 身份：`Keypair`/`verify`（封装 `ed25519-dalek`）+ 测试 |
| `src/codec.rs` | 区块的规范二进制编解码（哈希与落盘共用，含 `validator_updates`、`stake_ops` 与 `slashing_evidence`）+ `tx_signing_bytes`/`encode_tx`/`decode_tx`（签名/tx 哈希/gossip wire 字节）+ `stakeop_signing_bytes`/`encode_stakeop`/`decode_stakeop`（bond/unbond 签名与哈希）+ `encode_evidence`/`decode_evidence`（双签证据）+ `encode_commit`/`decode_commit`（证书落盘）+ 测试 |
| `src/store.rs` | 追加式日志（长度前缀记录、残缺尾检测）：`BlockLog`（区块）+ `CertLog`（证书）+ 测试 |
| `src/hash.rs` | 纯 std SHA-256（FIPS 180-4，含已知向量测试）——离线零依赖 |
| `src/main.rs` | 节点 CLI：`demo` / `build` / `prove` / `bft` / `live` / `chain` / `validators` / `gossip`（含 M19 证据 flood 演示） / `light`（M20 轻客户端跟随演示） / `staking` / `slashing` / `certs` / `run` / `status`（含确定性演示密钥） |

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
- **~~验证人集变更的轻客户端跟随协议~~**：✅ 已完成（M20，`ValidatorTracker` 只凭创世逐高度复验证书 + 复刻集合迁移，镜像 `bonds` + 创世公钥表，不执行交易即得到与 `replay_verified` 逐字节一致的活跃集合；复用 `GossipMsg::Blocks` 传输）。后续：只拉头部（`(header, cert)`，去掉交易体）的更轻同步、把验证人集 Merkle 化折进区块头以对 `block_hash` 直接证明（免去复刻迁移）、面向钱包的 SPV 账户证明跟随。
- **~~持久化~~**：✅ 已完成（M7，追加式区块日志 + 重放；M14 加证书日志）。后续可换 RocksDB、加 per-record 校验和与 segment 轮转。
- **~~Merkle 化状态树~~**：✅ 已完成（M10，二叉 Merkle 树 + 账户包含证明）。后续：非成员证明、增量更新的 Merkle-Patricia trie、把 graph/头字段也纳入根。
- **手写 SHA-256** 仅为离线零依赖演示，**生产必须换审计实现**（`sha2`）。
- **kNN 暴力扫描**：随图谱增长需换 HNSW/IVF（见 engine 局限）。

这些构成后续里程碑（~~M7 持久化~~ ✅、~~M8 签名~~ ✅、~~M9 mempool 出块~~ ✅、~~M10 Merkle 认证状态~~ ✅、~~M11 BFT 最终性内核~~ ✅、~~M12 BFT 轮次状态机/活性~~ ✅、~~M13 认证链驱动~~ ✅、~~M14 证书落盘 + 重放复验~~ ✅、~~M15 P2P + gossip~~ ✅、~~M16 动态验证人集~~ ✅、~~M17 质押绑定权重 + 解绑期~~ ✅、~~M18 按证据罚没绑定质押~~ ✅、~~M19 P2P 传播块级 ops~~ ✅、~~M20 验证人集变更的轻客户端跟随协议~~ ✅、M21 只拉头部的轻同步 / 验证人集 Merkle 承诺……），每步仍遵循"可运行、可测试、契约一致"。
