# 本地多验证人测试网 / Local multi-validator testnet (M210)

把 [`docs/DEPLOYMENT.md`](../docs/DEPLOYMENT.md) §2–§6 的手工步骤脚本化,给
§11 上线前检查清单里的「**genesis 参数经测试网实测反校**」一个可重复的起点。
生成的网络在安全姿态上**与生产一致**(加密 keystore、TLS + peer 签名握手、RPC/metrics
bearer token),只是全部跑在本机 loopback。

```sh
testnet/gen.sh            # 1) 密钥 → genesis → 每节点配置 → check-config + genesis-hash 一致性
testnet/up.sh             # 2) 后台启动全部验证人(SIGTERM 优雅关闭)
testnet/smoke.sh 3        # 3) 所有节点到达高度 3 且同一区块哈希(一条链)
testnet/recover.sh 5      # 4) 备份/恢复 + 快照 fast-sync 演练(§11「备份/恢复演练」)
testnet/chaos.sh          # 5) 混沌/故障注入:崩溃容错 + 崩溃恢复 + 失去法定数的安全停摆 + 恢复
testnet/soak.sh           # 6) 长跑稳定性:活性 + 不分叉/不漂移 + 供应守恒且跨节点一致 + 无泄漏 + 干净日志
testnet/down.sh           # 7) 优雅停止
```

### 混沌演练(M231,`chaos.sh`)

对**运行中**的测试网注入两类 loopback 上可达的故障(无需 root/iptables):

- **Phase A 崩溃容错 + 恢复**:`SIGKILL` 掉**少数**验证人(= BFT 容错预算 `f = N − 法定数`)。
  存活的 >2/3 必须**继续出块且不分叉**;重启被杀节点,它必须**追回**到同一 head(崩溃恢复重放)。
  `store.rs` 对每条记录 `sync_all`,故硬杀(SIGKILL)留下干净记录边界,重启可净回放——这是对
  真实崩溃/断电的测试,不是优雅排空。
- **Phase B 失去法定数时的安全性**:再杀一个(在线 < 法定数)。链必须**停摆**(无 >2/3 不出块)
  ——证明它在失去法定数时**停摆而非分叉**。恢复需**全量重启**(不只是被杀的节点)。

> **已知边界(本演练暴露,见 [`docs/SECURITY.md`](../docs/SECURITY.md) §10.10)**:失去法定数停摆期间,
> **保持在线**的节点会对待定高度不断换轮(round++),被杀节点重启时从 round 0 开始;法定数恢复后
> 两者的轮次号**不经 gossip 自动对齐**,存活节点仍卡住(`consensus_active=1` 但高度冻结)。**安全性
> 始终不破**(不分叉、正确停摆),仅**自动活性恢复不完整**;全量重启让每个节点回到待定高度的干净
> 轮次即恢复。`chaos.sh` 的 Phase B 据此做全量重启。属共识线待修项(轮次追赶),非安全缺陷。

### 长跑稳定性 soak(M232,`soak.sh`)

对**运行中**的测试网持续采样一段时间,每个采样点断言会被泄漏/时钟漂移/状态分叉/日志暴涨
打破的不变量:**活性**(每节点高度严格递增,无停摆)、**不分叉/不漂移**(所有节点在滞后高度上
区块哈希 **与 state_root 均一致**)、**供应守恒且跨节点一致**(`/supply conserved=true` 且 supply
数值全网相同)、**无内存泄漏**(每节点 RSS 增幅 < `LEAK_PCT%`)、**干净日志**(运行期间无
error/panic 行)。

```sh
testnet/soak.sh [DURATION_S=180] [INTERVAL_S=10] [LAG=2] [LEAK_PCT=50]
```

无负载(依赖 `create_empty_blocks` 心跳,数千块无需造 tx)。**tx 负载 soak** 另需明文种子账户
(测试网发的是加密 keystore,`encode-tx --key-file` 不吃 keystore),列为后续项。真正的上线前
soak 应为**小时/天级**;本脚本是可重复的**分钟级门禁**。

默认 **4 验证人**(id 21–24,容 1 拜占庭)+ **3 个有余额账户**(id 1–3,tx 作者)+
3 个评议人(id 10–12)。`gen.sh -n 7 -a 5` 改规模;`--force` 重新生成(**销毁**旧密钥)。

### 远程签名器(M219)

`gen.sh --signer 24`(可多个:`--signer 23,24`)让这些验证人经**进程外 `node signer`**
签名——其节点配置写 `[validator.signer]`(无在盘密钥),`up.sh` 在启动该节点前先起一个
`node signer`(持 `keys/val<id>.json`,socket + 反双签高水位),`down.sh` 一并停掉。
`smoke.sh` 应照常收敛——证明远程签名验证人与在进程验证人在同一条链上共识(含 peer-auth
握手亦经签名器)。见 [`docs/design/remote-signer.md`](../docs/design/remote-signer.md) 与
DEPLOYMENT §2b。

## 生成物(`testnet/out/`,已 gitignore,**切勿提交**)

| 路径 | 内容 |
|---|---|
| `keys/val<id>.json`、`keys/acct<i>.json` | Web3 Secret Storage v3 加密 keystore(600) |
| `password` | 全网共用的 keystore 密码(600)。生产:每验证人各一,经 systemd `EnvironmentFile` 注入 |
| `tokens.env` | `METRICS_TOKEN` / `RPC_TOKEN` bearer(600) |
| `genesis.toml` | 共享创世:M200 校准值(`base_emission_micro=8000000`、`slash_bps=10000`、`decay_half_life_blocks=0`)|
| `genesis-identity.txt` | `genesis_hash` / `state_root` / `validators` —— 跨运营方带外比对的那三行 |
| `n<id>/node.toml`、`n<id>/data/`、`n<id>/node.log`、`n<id>/pid` | 每节点配置 / 持久化目录 / 日志 / pid |

端口:p2p `90<id>`、metrics `91<id>`、rpc `92<id>`(全部 `127.0.0.1`)。

## 与生产的差异(有意)

- `create_empty_blocks = true`:空块心跳让"无负载也能观察收敛";生产建议 `false`。
- RPC/metrics `enable_tls = false`:仅 loopback;非 loopback 暴露**必须**开 TLS(§5)。
- `require_peer_certs` / `bind_channel` 注释掉;许可制网络按 §5 打开。
- 一个共享 keystore 密码。

## 用它做参数反校(清单 §11 第 2 项)

1. 起网后用账户 1–3 的 keystore 构造并提交 tx:
   `node encode-tx --key-file out/keys/acct1.json …`(见 `node --help`)→
   `node submit-tx --config out/n21/node.toml --tx <file>`(写面带 `RPC_TOKEN`)。
2. 观测 `GET /metrics`(高度、mempool、提交→落块延迟直方图)与 `node status --dir out/n21/data`
   (供应/余额/`state_root`)。
3. 把实测的增发/ROI/罚没轨迹与 [`sim/CALIBRATION.md`](../sim/CALIBRATION.md) 的预测对照;
   偏差经 `POST /gov/proposal` 走链上治理调参(§10),**不要**改 genesis 重来——那会换链。
4. 升级演练(清单 §11):`down.sh` → 换二进制 → `up.sh` → `smoke.sh` 应在**同一 head** 继续。

## 自动化前提

`keygen --encrypt` 与 `node run` 都接受 `ZHIXING_KEYSTORE_PASSWORD`(无 TTY 时);
交互场景仍为 no-echo 提示。脚本只依赖 bash、curl、awk、sed(macOS/Linux)。
