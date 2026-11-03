# 生产部署手册 / Production Deployment Runbook

> 面向**运营方**:从零把一个 ZhixingGraph 验证人网络搭到生产的可重复流程。命令与配置字段
> 均对 `node` CLI 与 `src/config.rs` 核验。配套阅读:[`docs/SECURITY.md`](SECURITY.md)
> (信任模型/已知边界)、[`sim/CALIBRATION.md`](../sim/CALIBRATION.md)(genesis 参数理由)。
>
> **上线前置**(本手册不覆盖,需你推动):✅ 外部审计落地并修复其 finding;✅ 测试网实测
> 反校 genesis 参数(见 CALIBRATION 诚实边界)。本手册覆盖"把网络正确、安全地搭起来"。

## 0. 架构速览

- 每个验证人 = 一个 `node run` 守护进程,持**一把** ed25519 密钥,经 TCP gossip 跑 BFT。
- 安全:quorum **> 2/3 投票权**(= 绑定质押);< 1/3 拜占庭下不分叉。
- 一份共享 **genesis**(创世验证人集 + 账户 + 参数)是所有节点的信任根。
- 可选:纯 follower(`run` 无 `[validator]`)、轻节点(`light-run`)。

## 1. 构建

```sh
cd node && cargo build --release        # 产出 target/release/node
./target/release/node --help            # 子命令一览
```
- CI 门禁(`.github/workflows/ci.yml`)须全绿:fmt / clippy `-D warnings` / 全测试 /
  跨架构矩阵。**发布二进制应来自 CI 的 tagged 构建**,不要手工机器产物。
- 可选:`cargo audit`(依赖漏洞)——审计后建议把 CI 的 audit 步升为门禁。

## 2. 验证人密钥(每个验证人各一)

**切勿**在生产用明文 `seed_hex`。用加密 keystore(Web3 Secret Storage v3,M175):

```sh
# 1) 生成密钥 → 加密 keystore(交互输入密码,no-echo)。keygen 会打印本密钥的 pubkey
#    (及可选的 genesis 条目),抄下 pubkey_hex 填进 genesis。
node keygen --encrypt --out keystore/val21.json
# 若已有明文 seed / key 文件,可另行取 pubkey:
node pubkey --seed <64hex>            # 或 --key-file <path>
```
- 运行时密码来自环境变量 `ZHIXING_KEYSTORE_PASSWORD`(无人值守)或交互 no-echo 提示
  (M181);密钥轮换的 pending key 用 `ZHIXING_PENDING_KEYSTORE_PASSWORD`。
- keystore 文件权限设 `600`,与节点数据分离备份。**高价值验证人**:目前密钥在盘,无
  HSM/远程签名器(SECURITY.md 已知边界 #3)——高价值场景请隔离主机 + 磁盘加密。远程签名器
  (密钥移出节点进程 + 反双签高水位)设计见 [`docs/design/remote-signer.md`](design/remote-signer.md)。

## 3. 构造 genesis(所有节点共享、逐字节一致)

以 M200 校准值为基(见 `sim/CALIBRATION.md`):

```toml
# genesis.toml —— 所有节点必须用同一份(哈希一致,否则无法组网)
base_emission_micro    = 8000000     # 诚实 ROI 稳健正、膨胀温和(4–8M 区间)
decay_half_life_blocks = 0           # M187 增发衰减:0=关;生产建议设为"引导期区块数"
slash_bps              = 10000       # 满罚:最强攻击威慑 + 最低 fake-pass(CALIBRATION)
timestamp_days         = 0.0

[[accounts]]   # 初始账户(id/balance_micro/pubkey_hex)
id = 1
balance_micro = 30000000
pubkey_hex = "…"

[[reviewers]]  # 初始评议人(id/weight)——治理声誉侧选区
id = 10
weight = 1.0

[[validators]] # 初始验证人集(id/pubkey_hex/power==绑定质押)
id = 21
pubkey_hex = "…"     # 来自 node pubkey
power = 1
# … 至少 4 个验证人以容 1 拜占庭(3f+1,f=1)
```
- **`decay_half_life_blocks`**:生产建议设为与预期"引导期"相当的区块数(按 `block_interval_ms`
  换算,如目标 2–4 年);上线后经治理按真实数据调。
- 校验 + 固化哈希:
```sh
node inspect-genesis --config node21.toml   # 人读校验
node genesis-hash   --config node21.toml    # 记录创世哈希,带外分发给所有运营方比对
```

## 4. 节点配置(每节点一份)

```toml
genesis = "genesis.toml"

[node]
id = 21
listen = "0.0.0.0:9021"
data_dir = "/var/lib/zhixing/n21"       # 持久化目录(blocks.log/certs.log)

[[peers]]                                # 静态 peer 表(无发现)
id = 22
addr = "node22.example:9022"

[validator]                              # 省略 ⇒ 纯 follower
enabled = true
encrypted_keystore_path = "keystore/val21.json"   # 生产:用加密 keystore,勿用 seed_hex

[consensus]                              # 省略字段回落默认(= 历史常量)
block_interval_ms   = 1000
create_empty_blocks = false              # 生产建议:无 pending 工作时不空转出块

[network]                                # 见 §5 安全加固
enable_tls             = true
require_peer_auth      = true
max_inbound_connections = 256

[metrics]                                # 观测(见 §7)
enabled    = true
listen     = "127.0.0.1:9121"
auth_token = "…"                         # 非 loopback 暴露必须设

[rpc]                                    # 外部 ingress(tx/治理提交);不需则省略
enabled    = true
listen     = "127.0.0.1:9221"
auth_token = "…"                         # 写面(tx/治理提交)受此 bearer 保护
enable_tls = true
```
- 启动前 `node check-config --config node21.toml`(校验 + fail-fast)。

## 5. 安全加固(`[network]`)

| 开关 | 作用 | 生产建议 |
|---|---|---|
| `enable_tls` | TLS 1.3 加密(默认 ephemeral 自签,仅机密性) | **开** |
| `require_peer_auth` | 签名握手:peer 必须证明持创世密钥 | **开**(防伪造身份接入) |
| `require_peer_certs` | genesis-pinned mTLS(双方出示创世 ed25519 key) | 许可制网络建议开 |
| `bind_channel` | RFC5705 通道绑定,防 MITM 中继(需 TLS+auth) | 开(配合上面) |
| `max_inbound_connections` | accept 上限,防连接耗尽 | 按部署规模设(默认 256) |
| `tls_cert_path`/`tls_key_path` | 持久化 TLS 证书(否则每次启动 ephemeral) | 需稳定证书时成对设 |

- **RPC/metrics**:`auth_token` 对写面(tx/治理提交)与 metrics 抓取做 bearer 鉴权;
  非 loopback 暴露**必须**设 token + `enable_tls`。读面(GET)公开。
- 共识安全**不依赖**传输层(投票 ed25519 签名);TLS/握手是额外的接入控制与机密性。
- **`require_peer_auth` 只认创世验证人身份**:签名握手用的是节点的创世验证人密钥,校验方按
  `genesis.validators` 的 id→pubkey 注册表核对。因此**无密钥的纯 follower / 轻客户端无法加入
  开启 `require_peer_auth` 的网络**(与 light node 的约束相同)。两种做法:① 需要只读副本时,该
  网段用 `enable_tls` 单独加密、不开 `require_peer_auth`(共识仍由签名保证);② 要从快照重建一个
  节点,就以**验证人身份**重建(配置带其验证人 keystore)——见 §8 的快照恢复演练。

## 6. 启动 / 关闭

```sh
ZHIXING_KEYSTORE_PASSWORD=… node run --config node21.toml     # 验证人/follower 守护进程
```
- **优雅关闭**:发 `SIGTERM`(容器 `docker stop`/K8s/systemd)或 `SIGINT`(Ctrl-C)——
  M190 两者都捕获,日志逐条 fsync,干净退出。**不要** `SIGKILL`。
- systemd 单元建议:`Restart=on-failure`、`TimeoutStopSec` 留足 fsync、密码经
  `Environment=`/`EnvironmentFile=`(600 权限)。

## 7. 观测

- **metrics**:`GET http://<metrics.listen>/metrics`(Prometheus 文本)——高度、peer 数、
  mempool、tx 提交→落块延迟直方图、auth 失败计数等。配 token 时带 `Authorization: Bearer`。
- **验证人在线度(M212)**:`zhixing_validator_missed_commits{validator="<id>"}` = 本节点近
  `zhixing_liveness_window_samples` 个已落定高度中该验证人**缺席**的次数(本节点 cert 视角)。
  缺席数随窗口样本数增长 ≈ 该验证人已离线。**无自动 jailing**:据此**手动**经验证人集变更
  (治理 / `validator_updates`)移除长期缺席者;移除后其质押可正常 unbond。注:该指标非共识状态,
  不同诚实节点可因 cert 子集不同而略有差异,用于观测而非裁决。
- **health**:`GET /`(metrics 或 rpc)→ `200 ok`。
- **status**:`node status --dir <data_dir> --config <node.toml>`(本地读 head/高度/state_root)。
- **日志**:结构化 tracing(`RUST_LOG` 控级别,`info` 缺省),支持 JSON + 文件 appender。

## 8. 持久化 / 备份 / 恢复

- 状态在 `data_dir/`:`blocks.log` + `certs.log`(追加式,逐条 fsync)。重启即**重放复验最终性**
  (逐高度复验 >2/3 证书),恢复的是*最终性*而非仅状态。
- **完整性校验**:`node verify --dir <data_dir> --config <node.toml>`(对持久链逐块复验证书)。
  **生产链必须带 `--config`(或 `--genesis`)**——否则 `verify`/`snapshot`/`status` 回落内置
  **demo** 创世,用错创世集会报"invalid signature"等误导性错误(M211 修复:三命令接受
  `--config`/`--genesis`)。
- **快照**(fast-sync 大件 / 备份):`node snapshot --dir <data_dir> --out snap.bin --config <node.toml>` →
  `node inspect-snapshot --snapshot snap.bin` →(可选)`node check-snapshot --snapshot snap.bin
  --header <GET /header/{h} 的 hex>` 交叉校验高度与三根。快照版本当前 **v5**;跨 wire 代际需重新 snap。
- 备份:定期归档 `blocks.log`/`certs.log`(或快照)+ keystore(分离保管)。
- **恢复 / fast-sync 新节点**:新 follower 以 `node run --config F --snapshot snap.bin
  --snapshot-header <hex>` 从快照起步(免全量重放),随后经 gossip 同步尾部;带
  `--snapshot-cert`+`--snapshot-validators` 可额外要求证书达钉死集 >2/3(弱主观性检查点)。
- **演练**:[`testnet/recover.sh`](../testnet/README.md) 一键跑完整流程(verify → snapshot →
  备份/恢复字节一致 → 新节点 fast-sync 收敛),即本节的可重复验证。

## 9. 升级(wire 代际)

- M192 起,握手协商 `PROTOCOL_VERSION`;版本不匹配**显式拒连**(不再静默 mis-parse)。
- **当前不支持多版本并存滚动升级**(只能同版本运行 / 异版本干净拒连)。wire 破坏(B2…B6
  历史)需**协调停机升级 + 必要时重新 snap**。升级流程:
  1. 全网公告升级窗口;2. `SIGTERM` 停全部节点;3. 换二进制;4. (若快照版本变)重新 snap;
  5. 同步重启。
- 发布务必带 CHANGELOG 注明是否为 wire/snapshot 破坏版本(本仓 README 变更日志已逐里程碑标注)。

## 10. 治理运维(M193–198)

- 查提案:`GET /proposals`(id/param/目标值/created_height/voting_period/票数)。
- 发起:`POST /gov/proposal` / `POST /gov/vote`(raw codec body;经 bearer 鉴权)。
- 到期按**双门槛**(质押 >2/3 **且** 声誉 >1/2)自动执行参数变更。4 组参数(增发/罚没/
  衰减/ΔK)均可链上调——这是上线后用真实数据迭代 genesis 初值的机制。

## 11. 上线前检查清单

- [ ] 外部审计完成,finding 已修复(SECURITY.md)。
- [ ] genesis 参数经测试网实测反校(CALIBRATION.md 诚实边界;本地多验证人测试网一键起网见
      [`testnet/README.md`](../testnet/README.md):`gen.sh` → `up.sh` → `smoke.sh`)。
- [ ] 所有运营方比对**同一 genesis 哈希**(`genesis-hash` 带外核对)。
- [ ] 验证人用**加密 keystore**,密码经 env/secret 注入,文件 600 + 分离备份。
- [ ] `[network]` 开 `enable_tls` + `require_peer_auth`(许可制再加 `require_peer_certs`+`bind_channel`)。
- [ ] RPC/metrics 若非 loopback 暴露:设 `auth_token` + `enable_tls`。
- [ ] ≥ 4 验证人(容 1 拜占庭);`check-config` 全绿;`genesis-hash` 一致。
- [ ] 监控接入 `/metrics` + 告警;日志归集。
- [ ] 备份/恢复演练过一次(snapshot + verify + 重放)。
- [ ] 升级演练:协调停机 + 换二进制 + 重启能恢复同一 head。
- [ ] 复核 SECURITY.md §10 已知边界,确认当前部署形态可接受(如无 liveness slashing、
      密钥在盘、无滚动升级)。

## 12. 已知运维边界(指针)

见 [`docs/SECURITY.md` §10](SECURITY.md#10-已知边界--非目标out-of-scope):无 liveness/宕机罚没、
密钥在盘(无 HSM)、治理新上线未对抗压测、无滚动升级、`cargo-audit` 当前 advisory、
弱主观性检查点分发为运维事项。这些**不阻塞受控/许可制部署**,但高价值去中心化主网前应逐项评估。
