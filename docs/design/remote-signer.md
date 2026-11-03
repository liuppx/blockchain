# 设计:远程签名器 / Remote signer (production-readiness §10 #3)

> 状态:**设计稿**,供评审后分片实现。对应 [`docs/SECURITY.md` §10 第 3 项](../SECURITY.md)
> 「密钥在磁盘(加密 keystore),无 HSM/远程签名器」。
>
> **实现进度**:**S1 已落地(M215)**——最小 `Signer` seam(`public()` + `sign`),共识
> 投票/提案/`RoundState` 全程改走 `&dyn Signer`。**S2 Step A 已落地(M216)**——`sign` 扩为
> `sign(domain, msg) -> Result<Sig, SignerError>`(带 `SignDomain`/`ConsensusStep`),共识对
> `Err` **fail-closed**(不投票/不提案,仅排超时);进程内 `Keypair` 永不出错,故仍零行为变更。
> **S2 Step B 已落地(M217)**——`RemoteSigner`(Unix socket 客户端)+ `node signer` 参考签名
>> 进程 + 反双签高水位持久化(`src/remote_signer.rs`)。**S3 已落地(M218)**——`[validator.signer]`
> config、`node run` 据此构造 `RemoteSigner`(默认仍进程内 `Keypair`)、启动校验 signer pubkey ==
> genesis、auth 握手亦走签名器、DEPLOYMENT §2b 部署小节。远程签名器端到端可用;§10 #3 由「无」变
> 「可选启用」。**本地 socket 参考实现;HSM/云 KMS 后端仍为后续**。

## 1. 动机与威胁模型

当前验证人签名密钥由守护进程**在内存持有**(从磁盘上的明文 seed 或加密 keystore 解密载入,
见 DEPLOYMENT.md §2)。被攻破的节点进程可直接读出私钥 → **密钥外泄 + 离线无限伪造**。

远程签名器把密钥移出节点进程,放进一个**独立的签名进程/设备**,节点只持 `PubKey`,每次签名
经 IPC 请求。这改变攻击者的能力:

| | 节点持密钥(今) | 远程签名器 |
|---|---|---|
| 攻破节点能否外泄密钥 | **能** | 不能(密钥不在节点进程) |
| 攻破节点能否请求恶意签名 | 能 | **能**(仍可请签) |
| 攻破节点能否让其**双签** | 能(无防护) | **不能**——签名器维护 (height,round,step) 高水位,拒绝回退 |

**关键洞察**:远程签名器真正的安全增益不只是"密钥不外泄",而是**把反双签不变量下沉到一个
不受节点共识逻辑支配的小 TCB**。这正是 Tendermint KMS / Horcrux 的核心价值——即使共识进程
被攻破或有 bug,签名器也**物理上不可能**对同一 (height,round,step) 签两个不同区块。

**不防护的**:被攻破节点仍能请求"当前高度的恶意投票"(签名器会签,因为它不回退);防的是
**双签**(安全性)与**密钥外泄**(离线伪造)。节点可用性依赖签名器可用(见 §6)。

## 2. 范围

**纳入**(守护进程用验证人密钥签的全部三处,见代码 `crypto::Keypair::sign` 调用点):
1. 共识**预投票/预提交** `Vote::signed`(`consensus.rs`)。
2. 共识**提案** `Proposal::signed`(`round.rs`)。
3. **peer 认证握手** transcript(`daemon.rs` `auth_handshake`)。

**不纳入**(不经运行中验证人密钥,由离线 CLI 用各自密钥完成):交易签名(`encode-tx`)、
质押/密钥轮换 op、治理 op 提交——这些是钱包/运维离线动作,已可用加密 keystore,远程签名器
不改变它们。

## 3. 抽象:`Signer` trait

今:`RoundState` 的每个方法都接 `kp: &Keypair` 并在状态机内**同步**签名
(`start`/`on_message`/`on_timeout`/`cast_prevote`/`cast_precommit`/…,见 `round.rs`)。
因此引入最小抽象、**保持同步**,把 `&Keypair` 换成 `&dyn Signer`:

```rust
pub trait Signer: Send + Sync {
    /// 本签名器对应的验证人公钥(启动时必须 == genesis 中该 id 的 pubkey,否则 fail-fast)。
    fn public(&self) -> PubKey;
    /// 对 `msg`(已是域分隔的待签字节)签名。`domain` 让签名器区分共识投票 / 提案 /
    /// 握手,并**只对共识投票**强制反双签高水位。失败返回 Err(节点随即无法投票 = 活性降级)。
    fn sign(&self, domain: SignDomain, msg: &[u8]) -> Result<Sig, SignerError>;
}

pub enum SignDomain {
    /// 共识投票/提案,携带 (height, round, step) 供签名器做反双签高水位。
    Consensus { height: u64, round: u32, step: ConsensusStep },
    /// peer 认证握手——无高水位(可重复签不同 nonce)。
    Handshake,
}
```

- `LocalSigner(Keypair)`:默认,进程内,**与今日逐字节等价**(`sign` 直接调 `kp.sign`,忽略
  domain)。
- `RemoteSigner`:见 §5。

### 为何保持同步(而非把 RoundState 改成 async / 产出未签意图)

同步是**侵入最小**的路径:`RoundState` 结构不变,仅签名入口换类型。代价:签名是 actor 单
tokio 任务内的**阻塞**调用,会阻塞该任务直到签名返回。对**本地 Unix socket** 签名器,往返
亚毫秒级,远小于 ~1s 出块间隔,可接受。对**高延迟签名器(云 KMS,数十 ms)**,阻塞 actor 会拖
慢所有 peer 消息处理 → 那时再做"RoundState 产出未签意图、actor 异步签名"的更大重构(列为
§7 的后续项,不在首版)。

## 4. 实现方式选项(含推荐)

| 选项 | 说明 | 延迟 | 复杂度 | 结论 |
|---|---|---|---|---|
| A. 进程内(今) | `LocalSigner(Keypair)` | ~0 | 低 | **保留为默认** |
| **B. 本地外部签名器** | 独立进程持密钥 + 反双签高水位,Unix socket | 亚 ms | 中 | **推荐首个远程实现** |
| C. 云 KMS | AWS/GCP KMS 签名 | 数十 ms+ | 高 | 需先做 async 重构(§7);ed25519 支持视厂商 |
| D. 门限/MPC | n-of-m 分布式签名 | 高 | 很高 | 远期,另行 |

**B** 用最小的 TCB 拿到主要安全增益(密钥隔离 + 反双签),且本地 socket 延迟不要求 async 重构。

## 5. 本地外部签名器协议(选项 B)

**传输**:Unix domain socket(文件权限 600,仅签名器与节点同主机;跨主机留给 TLS mTLS 变体,
后续)。请求/响应均 `u32 BE 长度前缀 + 体`,体为手写定长编码(复用 codec 风格)。

```
请求:  tag(1B: 0=Consensus 1=Handshake) ‖ 若 Consensus: height(8) round(4) step(1) ‖ msg_len(4) ‖ msg
响应:  status(1B: 0=ok 1=refused_hwm 2=error) ‖ 若 ok: sig(64)
```

**反双签高水位**(签名器侧,持久化到磁盘、每次签名前 fsync):签名器记 `last = (height, round,
step)`。对 `Consensus` 请求,仅当 `(height,round,step) > last`(字典序)才签并推进 `last`;否则
`refused_hwm`。`Handshake` 无高水位。高水位文件损坏/缺失时 fail-closed(拒签,人工复位)。

**参考实现**:新增 `node signer --keystore <ks> --socket <path> --hwm <file>` 子命令——一个只做
"载密钥 + 监听 socket + 高水位"的最小进程(TCB 小、易审计),与守护进程分开部署/加固。

## 6. 失败模式与延迟预算

- **签名器宕机 / 超时** → `sign` 返回 Err → 节点**无法投票**。这是**活性**降级,非安全问题
  (节点宁可不投也不在无签名器时投)。`RemoteSigner` 带超时(默认 500ms < 出块间隔),超时即 Err。
- **fail-closed**:任何签名器错误都不得回退到"节点本地签名"——否则密钥隔离失效。
- **延迟**:本地 socket 往返应 « 出块间隔(默认 1000ms)。超时设为出块间隔的一半量级。
- **反双签高水位的持久化**是签名器正确性的关键:崩溃重启后不得丢失 `last`,否则可能重签。

## 7. 分片实现计划

- **S1(基础,零行为变更)✅ M215**:`Signer` trait(`public()` + `sign(&[u8]) -> Sig`)+
  `impl Signer for Keypair`(进程内默认);`round.rs`/`consensus.rs` 的 `Vote::signed`/
  `Proposal::signed`/`RoundState` 方法签名入口由 `&Keypair` 改走 `&dyn Signer`(`&Keypair`
  经非定长强制自动适配,59+3 处调用点无需改动)。守护进程仍持 `Keypair`(即一个 `Signer`)。
  **已验证**:全测试套件 + localnet 收敛(含确定性锚点测)逐位不变。
  *注*:本片保持同步状态机不变,故**未**引入 `SignDomain`/高水位/`Result`;它们随 S2 落地。
- **S2 Step A(trait 扩展 + fail-closed)✅ M216**:`sign(domain, msg) -> Result<Sig,
  SignerError>`;`Vote::signed_with`/`Proposal::signed_with` 填 `SignDomain::Consensus
  {height,round,step}`,`cast_*`/`start_round` 对 `Err` fail-closed(不产出投票/提案,排 propose
  超时推进到下任提议人)。`Vote::signed`/`Proposal::signed`(`&Keypair`,infallible)保留给
  测试/driver。**已验证**:`signer_failure_casts_no_vote_or_proposal_fail_closed`(失败签名器
  → 零广播 + 排超时)+ 全套共识/确定性/localnet 测试逐位不变。
- **S2 Step B(本地外部签名器)✅ M217**:`src/remote_signer.rs`——`RemoteSigner`(Unix socket
  客户端,per-request 连接 + 读写超时,I/O/超时→`Unavailable`、高水位拒绝→`DoubleSignRefused`,
  并对返回签名二次 `crypto::verify` 防恶意签名器)+ `serve`/`node signer` 参考进程(顺序处理、
  socket 权限 600)+ 反双签高水位持久化(tmp 写→fsync→rename,签名前先落盘)。回归:
  `remote_signer_signs_and_enforces_high_water_mark`(正常签名验签 + 同/回退 (h,r,step) 拒绝 +
  更高步/高度放行 + 握手无高水位)、`high_water_mark_survives_reload`、
  `unavailable_signer_is_fail_closed_error`;`node signer` CLI 本机 smoke 通过(keystore 解密、
  socket 0600 绑定)。
- **S3(接线 + 文档)✅ M218**:`[validator.signer] { socket, timeout_ms }` config
  (`config::SignerConfig`);`node run` 在 `[validator.signer]` 存在时构造 `RemoteSigner`、否则进程内
  `Keypair`(两者经 `Arc<dyn Signer>` 统一喂给 actor + auth 握手);`Node::start` 启动校验 signer
  pubkey == genesis 本节点 pubkey,`require_peer_auth` 据 signer 判定;DEPLOYMENT §2b「远程签名器」
  部署小节。约束:远程签名器不支持 `require_peer_certs`(需节点原始 seed),轮换在签名器侧完成。
  回归:`validator_signer_subtable_parses`、`remote_signer_backed_validator_produces_blocks`
  (无在盘密钥的验证人经签名器驱动共识出块)。

## 8. 非目标 / 已知边界

- 跨主机签名器(mTLS over TCP)、云 KMS(选项 C,需 async 重构)、门限签名(D)——均后续。
- 远程签名器**不**防"被攻破节点请求当前高度恶意签名",只防双签 + 密钥外泄(见 §1)。
- S1 不改任何 wire / 共识 / 快照格式(纯内部重构);S2/S3 的 signer 协议是**节点↔签名器**的
  私有 IPC,**非** P2P wire,不涉及 `PROTOCOL_VERSION`。
