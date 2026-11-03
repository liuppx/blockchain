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
testnet/down.sh           # 5) 优雅停止
```

默认 **4 验证人**(id 21–24,容 1 拜占庭)+ **3 个有余额账户**(id 1–3,tx 作者)+
3 个评议人(id 10–12)。`gen.sh -n 7 -a 5` 改规模;`--force` 重新生成(**销毁**旧密钥)。

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
