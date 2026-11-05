# 安全审计准备 / Security & Audit Readiness

> 本文件面向**外部安全审计方**,是审计的入口文档(scope、信任模型、攻击面、不变量、
> 优先复查地图、已知边界)。目标是让审计可以**立即开工、聚焦高危面、复现每一条声明**。
>
> 状态:**审计前(pre-audit)**。代码本身自标 whitepaper draft v0.10 —— 参数未经经济校准
> (见 [§10 已知边界](#10-已知边界--非目标out-of-scope))。本仓库尚未经过任何第三方审计。

## 审计状态

- **2026-11 P0 代码审查**([`docs/audits/P0-SECURITY-REVIEW-2026-11.md`](audits/P0-SECURITY-REVIEW-2026-11.md)):
  1 严重 + 3 高 + 3 中,**全部已修复**(M202–M204);审计方**复验**后指出其中 5 项修复不彻底,
  已**二次修复到根**(M206,见 [`P0-REMEDIATION-2026-11.md`](audits/P0-REMEDIATION-2026-11.md)
  「第二轮」):SEC-03 `Commit::verify` 全程 u128 + `MAX_TOTAL_POWER` 三路拒绝超额集、SEC-04
  ΔK 旋钮上限 + `checked_add`、SEC-01 先验证后缓存 + 治理去重/pending 上限、SEC-06 内联验证人集
  decoder 封顶预分配、SEC-05 `PROTOCOL_VERSION` 1→3。第二轮复核进一步发现 SEC-01 三处容量边界
  (默认 `seen_cache=0` 无界、池满仍先插 seen、本地提交绕过 pending 上限),已**三次修复闭环**
  (M207):`SeenSet::bounded(1<<16)` 默认硬上限 + 入站/本地提交均先验 `MAX_PENDING_GOV` 再写 seen。
  审计方第三轮复验再指出两处可达问题(签名正确的超前 nonce 可占满有界池且不被清理；本地/RPC 满池
  仍回报 `Ok(hash)`),已**四次修复**(M208):gossip 与本地/RPC 共用唯一 `admit_gov_*` 入口、
  `GOV_NONCE_LOOKAHEAD=8` nonce 窗口、`MAX_PENDING_GOV_PER_ACCOUNT=16` 按账户配额、结构化
  `GovPoolFull/GovAccountQuotaExceeded/GovAlreadySeen`、RPC 未入池即 429/409/422、提交后清理滞留项。
  审计方第四轮复验再指出两项条件剩余(≥256 账户各停放未来 nonce 可占满全局池;密钥轮换后旧公钥
  签名项仍占账户配额),已**五次修复**(M209):当前 nonce(立即可应用)操作在池满/配额满时优先淘汰
  停放最远的未来项(不依赖创世账户数,淘汰项哈希从 seen 移除可重提,全池皆当前 nonce 时硬上限仍生效);
  含 key rotation 的区块提交后对被轮换账户重跑完整 admissibility 清除旧签名项。每项附回归测试。
- 这是一轮**代码审查**,非第三方审计机构的签章报告 / 形式化证明 / 生产安全认证。修复后仍
  建议安排独立复验,尤其对新增的 B7 wire 与治理路径。

---

## 1. 如何使用本文档(审计方)

1. 读 [§2 构建与复现](#2-构建与复现) 跑通 `cargo test` + `cargo clippy`。
2. 读 [§4 信任模型](#4-信任模型与假设) 与 [§5 攻击面](#5-攻击面) 建立心智模型。
3. 按 [§8 优先复查地图](#8-优先复查地图-p0p1p2) 从 P0 开始。
4. [§6 安全不变量](#6-安全不变量含编码它们的测试) 每条都附了**编码该不变量的测试名**——
   既是规格,也是回归守卫;请把"能否绕过该不变量"作为复查问题。
5. [§10 已知边界](#10-已知边界--非目标out-of-scope) 是我们**自认的缺口**,不必重复报告,
   但欢迎就其**严重性/利用性**给出意见。

## 2. 构建与复现

```sh
# 两个独立 crate(无 workspace),各自构建/测试:
(cd engine && cargo test --release)              # ΔK 引擎 11 测
(cd node   && cargo test --release -- --test-threads=1)  # 单线程避 RPC 端口冲突

# 门禁(CI 同款,见 .github/workflows/ci.yml):
cargo fmt --check
cargo clippy --release --all-targets -- -D warnings      # 零 lint
RUSTFLAGS="-D warnings" cargo test --release --no-run     # 零编译告警

# 跨架构确定性:CI 在 ubuntu(x86_64)+ macos(arm64) 两架构跑全套,
# 其中位锚点测(见 §6)验证共识浮点逐位一致。
```

- 测试总数:**node 757(698 lib + 59 bin)+ engine 11**,全绿。2 个 `#[ignore]`(config 的
  环境依赖测;M213 的 `seed_fuzz_corpus` 语料生成器,均非安全相关)。
- `unsafe`:**全仓非测试代码 0 处**(`grep -rn unsafe engine/src node/src`)。
- 无 `cargo-audit` 门禁(当前 advisory,依赖树未确认无 RUSTSEC —— 见 §10)。

## 3. 代码地图(scope)

两个 crate,共 ~56k 行。按**风险分层**(复查优先级见 §8):

### `engine/`(784 行)—— ΔK 认知增量引擎
| 文件 | 行 | 角色 | 层 |
|---|---|---|---|
| `lib.rs` | 692 | `compute_delta_k`:novelty/correctness/replication/freshness → ΔK;cosine 相似度、认知图谱 | **P1**(喂增发;浮点确定性关键) |

### `node/`(~55k 行)
| 文件 | 行 | 角色 | 层 |
|---|---|---|---|
| `lib.rs` | 6221 | 状态机核心:`Block`/`ChainState`/`apply_block`/`state_root`/增发·罚没/治理 tally·执行/桥 apply | **P0** |
| `codec.rs` | 4347 | 全部 wire/快照编解码(手写,解析攻击者可控字节) | **P0** |
| `net.rs` | 5725 | gossip 协议、消息分发、flood/dedup、SPV 服务 | **P0** |
| `bridge.rs` | 596 | 跨链 lock/redeem/unbond 端点 | **P0** |
| `consensus.rs` | 413 | `Commit` 证书、投票、等价双签检测 | **P0** |
| `crypto.rs` | 676 | ed25519 封装、签名/验签、BIP-39 助记词 | **P0** |
| `keystore.rs` | 645 | Web3 Secret Storage v3 加密 keystore(scrypt+AES-CTR+Keccak MAC) | **P0** |
| `daemon.rs` | 19148 | tokio 守护进程:actor、握手(TLS/认证/版本协商)、RPC ingress、mempool→共识→commit | **P0**(握手/认证/RPC)/ P1(其余) |
| `round.rs` | 1035 | BFT 轮次状态机(propose/prevote/precommit、超时、锁定、换轮) | **P0** |
| `mempool.rs` | 1108 | tx 准入、排序、配额背压 | **P1** |
| `merkle.rs` | 378 | 二叉 Merkle 树 + 包含证明(SPV 基元) | **P0** |
| `validator.rs` | 356 | 验证人集、权重、集合迁移、Merkle 根 | **P0** |
| `light.rs` | 3664 | 轻客户端 / SPV 验证 / 快照验证 / `ProofEntry` | **P1** |
| `driver.rs` | 902 | demo/离线出块器(非生产共识路径) | P2 |
| `config.rs` | 2901 | TOML 配置 + 校验 | P1 |
| `store.rs` | 381 | `blocks.log`/`certs.log` 追加式持久化 | P1 |
| `main.rs` | 6812 | CLI 子命令(demo/keygen/encrypt-key/run/...) | P2 |

`sim/`(Python 经济仿真)与 `tools/` 不在链上共识路径;`sim/` 对 **#7 经济参数校准**相关。

## 4. 信任模型与假设

- **共识安全**:Tendermint 式 BFT,`n` 个验证人、quorum **> 2/3 投票权**。安全性(不分叉)
  在 **< 1/3 投票权拜占庭**下成立;活性在 **> 2/3 诚实且在线**下成立。投票权 == 绑定质押
  (`bonds`)。
- **签名**:所有共识投票、交易、质押 op、密钥轮换、桥 op、治理 op 均 **ed25519 签名**;
  传输层身份**不**被信任(安全不依赖 TLS/握手,只依赖签名)。
- **信任根**:创世(`Genesis`)—— 创世验证人集 + 账户 + 参数。轻客户端从创世哈希起,逐高度
  对**当前集合**复验证书(弱主观性:长期离线的新节点需带外获得近期可信检查点)。
- **跨链桥**:destination 链在创世声明所信任的 source 链(`bridge_sources`:genesis_hash +
  验证人集)。redeem/unbond 的授权 = source 链 cert 签名的 `bridge_root` 包含证明。relayer
  **信任无关**(只搬运,无授权)。
- **传输**:默认 TLS 加密(ephemeral 自签,仅机密性,**无 MITM 抗性**);可选
  `require_peer_auth`(签名握手,绑定到创世密钥)+ `require_peer_certs`(genesis-pinned mTLS)
  + `bind_channel`(RFC5705 通道绑定防中继)。
- **RPC ingress**:默认关闭;开启后写面(tx/治理提交)可配 bearer token;读面公开。
- **密钥**:验证人签名密钥默认在磁盘(明文种子或 Web3 v3 加密 keystore)。M218 起可选**远程签名器**
  (`[validator.signer]`:密钥在独立进程,节点经本地 Unix socket 请签 + 反双签高水位)。无 HSM/云 KMS。

## 5. 攻击面

1. **wire/快照 codec**(`codec.rs`)—— **最高危**。手写解析器直面网络/磁盘的攻击者字节。
   复查:越界、整数溢出、**伪造 length-prefix 的无界分配**(由 `Dec::count()` 的
   `MAX_ITEMS=1_000_000` 上限 + `MAX_FRAME` 守护)、代际混淆(B2–B6,见 §9)。
2. **gossip**(`net.rs`)—— flood/dedup、消息分发。复查:放大攻击、dedup 绕过、
   轻客户端忽略规则、`MAX_PEERS`/`MAX_PROOF_BATCH` 等上限。
3. **共识/轮次**(`round.rs`/`consensus.rs`)—— 复查:双签检测可靠性、锁定/换轮安全、
   证书伪造、验证人集迁移的跨高度规则(新验证人不投自己的到场块)。
4. **跨链桥**(`bridge.rs` + `lib.rs` 的 apply)—— 复查:mint/redeem 的**供应守恒**、
   重放(`consumed`/`consumed_unbonds` 去重)、错误目标、未跟随 source、`bridge_root`
   cert 绑定。历史上此处出过多个 bug(见 git log M172)。
5. **治理**(`lib.rs` tally/执行 + `net.rs` gossip + `daemon.rs` 提交)—— **最新、未审计**。
   复查:双门槛(质押 >2/3 **且** 声誉 >1/2)不可绕过、投票权重取值时点、提案过期/执行的
   确定性、nonce 反重放、恶意提案洪流。
6. **RPC ingress**(`daemon.rs`)—— 复查:bearer 认证的 timing(已做 `ct_str_eq` + jitter)、
   body 大小上限(`MAX_RPC_BODY`)、解析健壮、写面鉴权覆盖(tx + `/gov/*`)。
7. **密钥 & keystore**(`crypto.rs`/`keystore.rs`)—— 复查:scrypt 代价参数校验、AES-CTR
   nonce、Keccak MAC、无回显密码输入、密钥轮换交接。
8. **浮点确定性**(`engine` + `lib.rs` time_decay)—— 复查:共识路径是否还有 std 超越函数
   (应全走 `libm`,见 §6);任何 `mul_add`/重结合。

## 6. 安全不变量(含编码它们的测试)

每条不变量都有对应测试;"能否构造反例"是核心复查问题。

- **供应守恒**:`supply == Σbalances + treasury + bonded + unbonding + bridge_locked`,
  在增发/罚没/桥/费用后恒成立。测试:`supply_conserved`、`novel_submission_mints_and_conserves_supply`、
  `fee_credited_to_proposer_conserves_supply`、`a_full_bond_unbond_cycle_conserves_supply`、
  `bridge_lock_drains_balance_into_bridge_locked_and_conserves_supply`、`bridge_redeem_mints_to_dest_and_conserves_supply`。
- **确定性**(分叉安全):相同创世 + 区块序列 → 逐位相同 `state_root`。共识路径的超越函数走
  纯 Rust `libm`(跨平台位级一致)。测试:`deterministic_replay_same_state_root`、
  `freshness_exp_is_bit_deterministic`、`time_decay_factor_is_bit_deterministic`、
  `run_is_deterministic`、`gossip_is_deterministic`、`proposer_is_deterministic`、
  `genesis_hash_is_deterministic_and_matches_chainstate`(+ CI 跨架构矩阵)。
- **state_root 完整性 & 覆盖**:每个共识状态分量都折入 `state_root`;头承诺它,cert 签名头;
  apply 对不上即 `StateRootMismatch` 拒块。测试:`state_root_mismatch_is_rejected`、
  `state_root_covers_{the_validator_set,bonded_stake,consumed_unbonds,contributions,proposals}`。
- **解码器永不 panic / 永不无界分配**:14 个公开 `decode_*` 入口对空/截断/随机/伪造超长
  length-prefix 只返回 `Ok`/`Err`。测试:`decoders_reject_adversarial_bytes_without_panicking`。
- **反重放**:tx/质押/密钥轮换/治理 op 严格 nonce(== 账户当前 nonce);桥重放由 `consumed*`
  去重。内容哈希 gossip dedup。
- **SPV 可靠性**:prover 提供的 leaf **不被信任**,验证方本地重算 leaf 并对 cert 签名头的根
  验证。测试:`verify_proof_against_header_*`(账户/评议人/验证人/图/cNFT 的接受 + 篡改拒绝)。
- **治理双门槛**:提案过关需质押加权 >2/3 **且** 声誉加权 >1/2;空选区永不过门。测试:
  `gov_tally_*`、`governance_*`。

## 7. 密码学清单

| 用途 | 原语 | crate |
|---|---|---|
| 签名(共识/tx/op/桥/治理) | Ed25519 | `ed25519-dalek` v2 |
| 哈希(区块/状态/Merkle/内容寻址) | SHA-256 | `sha2` 0.10 |
| keystore KDF | scrypt(代价参数经 `validate_scrypt_cost` 上限校验) | `scrypt` |
| keystore 加密 / MAC | AES-128-CTR / Keccak-256 | `aes`+`ctr` / `sha3` |
| 助记词 | BIP-39 PBKDF2-HMAC-SHA512 | `pbkdf2`+`hmac` |
| 随机 | OS RNG | `getrandom` |
| TLS | TLS 1.3 / RFC7250 raw-pubkey、ring 后端 | `tokio-rustls`+`rcgen` |
| 确定性超越函数 | pure-Rust libm(exp/pow) | `libm` |

依赖面小且主流(RustCrypto / dalek / rustls-ring / tokio)。`cargo-audit` 建议在审计期**升为门禁**。

## 8. 优先复查地图 (P0/P1/P2)

**P0(先看,价值承载/分叉/资金)**
1. `codec.rs` 全部 `decode_*` —— 内存安全、无界分配、代际混淆。
2. `bridge.rs` + `lib.rs` 桥 apply —— 供应守恒、重放、cert 绑定、错误目标(历史 bug 高发区)。
3. `lib.rs` `apply_block_inner` / `state_root` / 增发·罚没 —— 守恒、确定性、根强制。
4. `consensus.rs` + `round.rs` —— 双签检测、证书、BFT 安全/活性、验证人集迁移。
5. `crypto.rs` + `keystore.rs` —— 签名封装、keystore 参数/MAC。
6. 治理(`lib.rs` tally/执行)—— 双门槛不可绕过、确定性、反重放(**全新、零审计**)。
7. `daemon.rs` 握手 + RPC 鉴权 —— 认证、timing、body 上限、版本协商。

**P1**:`engine`(浮点确定性)、`net.rs`(gossip 放大/dedup)、`light.rs`(SPV)、
`merkle.rs`、`validator.rs`、`mempool.rs`、`config.rs` 校验、`store.rs`。

**P2**:`driver.rs`(非生产)、`main.rs`(CLI)。

## 9. wire 代际 & 快照版本

- **区块/头 wire**:B2 → B3(M171 桥 unbond)→ B4(M179 密钥轮换)→ B5(M185 `contributions_root`
  前缀根)→ B6(M194 治理 op 尾承诺)→ **B7(SEC-05 `unbonding_root` 前缀根)**。per-body 承诺尾
  追加(前缀稳定);B5/B7 加前缀根(硬断)。`decode_certified_header` 固定前缀 **284B** + 10×32B
  承诺尾。M192 起握手协商 `PROTOCOL_VERSION`,不匹配**显式拒连**(不再静默 mis-parse);
  **M221 起协商为版本区间** `[MIN_WIRE_VERSION, PROTOCOL_VERSION]`(当前均为 3),取双方最高公共
  版本、无交集则拒连——为滚动升级铺路(见 [`docs/design/rolling-upgrade.md`](design/rolling-upgrade.md);
  握手由 4 字节单值改 8 字节区间是一次性引导破坏)。
- **快照版本**:`SNAPSHOT_VERSION` 当前 **5**(v3 加 cNFT、v4 加 time_decay、v5 加治理提案)。
  旧版严格拒绝。
- **升级**:当前只支持"同版本运行 / 异版本干净拒连",**不支持多版本并存滚动升级**(需多 wire
  解码器)。wire 破坏需协调停机 + 重新 snap。

## 10. 已知边界 / 非目标(out-of-scope)

我们**自认**的缺口 —— 不必重复报告,欢迎就严重性评估:

1. **经济参数未校准**(#7):`base_emission`/`slash_bps`/`decay_half_life`/ΔK 阈值均为 draft
   初值,需 `sim/` 经济建模 + 评审再定主网 genesis。
2. **只罚等价双签**,不罚宕机/审查(无自动 liveness slashing/jailing)。M212 起提供**本地可观测**:
   每节点经 `/metrics` 暴露 `zhixing_validator_missed_commits{validator=…}`(本节点 cert 视角的
   滑窗缺席计数),运营方据此**手动**经验证人集变更移除长期缺席者。自动 jailing 需确定性的
   per-height 参与记录(区块内嵌 canonical `last_commit`),属 wire 破坏的后续项。
3. **密钥默认在磁盘**(加密 keystore)。**M218 起可选远程签名器**:`[validator.signer]` 让密钥
   移出节点进程(独立 `node signer` 进程 + 本地 Unix socket + 持久化反双签高水位),节点只持公钥、
   fail-closed;见 [`docs/design/remote-signer.md`](design/remote-signer.md) 与 DEPLOYMENT §2b。
   仍无 HSM/云 KMS 后端(远程签名器为本地 socket 参考实现)。
4. **治理刚上线**(M193–198),未经对抗/洪流/临界票数压测。
5. **无长跑/网络分区/混沌测**;fuzz 有两层:always-on 的 "fuzz-lite"(确定性 PRNG 轰全部 29 个
   `decode_*` 入口,stable CI 门禁)+ **M213 cargo-fuzz**(`node/fuzz/` libFuzzer `decode_all`
   多路目标 + 提交的种子语料,nightly CI advisory 跑;见 `node/fuzz/README.md`)。**M231 起**补
   `testnet/chaos.sh`:对 4 验证人测试网注入**崩溃容错**(杀少数、存活 >2/3 继续出块且不分叉)、
   **崩溃恢复**(重启节点追回同一 head)、**失去法定数的安全停摆**(在线 < 法定数时停摆而非分叉)
   + 恢复——均为 loopback 可达场景(无需 root/iptables)。仍无**真正的网络分区注入**(iptables/
   tc 延迟·丢包)、**活体拜占庭注入**(进程外双签 → 证据 → 罚没的端到端)、**长跑 soak**(小时/天级
   内存·磁盘增长、状态不漂移),与**超 loopback 规模**测。见 §10.10 的活性恢复边界。
6. **`cargo-audit` 当前 advisory**(依赖树未确认无 RUSTSEC)。
7. **滚动升级未支持**(只能干净拒连不兼容 peer)。设计见 [`docs/design/rolling-upgrade.md`](design/rolling-upgrade.md)(设计稿,待实现)。
8. **弱主观性检查点分发**是运维事项(协议只提供验证基元)。
9. TLS 版本协商在 `require_peer_auth` 下先于认证,MITM 篡改版本只致**拒连**(非降级),属 DoS。
10. **失去法定数停摆后的自动活性恢复不完整**(M231 `chaos.sh` Phase B 暴露)。失去 >2/3 在线的
    停摆期间,**保持在线**的验证人对待定高度不断换轮(propose 超时 → round++);被杀后重启的节点
    从 round 0 开始。法定数恢复后两侧轮次号**不经 gossip 自动对齐**(无轮次追赶 / catch-up),存活
    节点仍卡住(`/metrics` 见 `zhixing_consensus_active=1` 但 `zhixing_height` 冻结),即便所有
    peer 已重连。**安全性始终不破**:无 >2/3 不出块 → 不分叉、正确停摆(供应守恒、state_root 一致)。
    **仅自动活性恢复受影响**。恢复手段明确:**全量重启验证人集**(含存活节点)即让每个节点回到待定
    高度的干净轮次,链在同一 head 继续(实测 15→16→… 重新收敛)。属**共识线待修项**(需轮次追赶:
    节点见到更高轮次的 >2f+1 消息时跳转),非安全缺陷;修复为协议改动,风险较高,留作后续。

## 11. 负责任披露

> **占位 —— 由运营方填写**:安全联系邮箱 / PGP 公钥 / 披露政策 / 赏金范围。
> 在正式部署前,请替换为真实联系方式。
