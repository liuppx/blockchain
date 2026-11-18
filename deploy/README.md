# 测试网部署 · blockchain.yeying.pub

> 2026-11-14 起,验证节点运行在新机 **221.130.30.75**,旧机 **122.9.144.27** 只运行浏览器与 nginx。
> 迁移经过与原因见 [`docs/design/spec-node-migration-v0.1.md`](../docs/design/spec-node-migration-v0.1.md)。

```
用户 ──443──▶ 旧机 nginx ─┬─ /         ─▶ 127.0.0.1:8080  浏览器
                         ├─ /bodies/  ─▶ 127.0.0.1:8080  正文仓库(/opt/zhixing/bodies)
                         └─ /rpc/     ─▶ 127.0.0.1:19221 ══ SSH 本地转发 ══▶ 新机 127.0.0.1:9221(n21)

新机:4 个验证节点 n21–n24,P2P / RPC / 指标只监听 127.0.0.1
```

## 新机:验证节点(社区打包规范)

| 项 | 位置 |
|---|---|
| 安装包 | `/opt/package/zhixing-testnet-service-v<版本>-<commit>.tar.gz` + `.sha256sum` + `release-manifest.json` |
| 当前版本 | `/opt/deploy/zhixing-testnet` → `zhixing-testnet-service-v<版本>-<commit>/`(保留上一版本目录用于回退) |
| 配置与密钥 | `<版本目录>/config/`(700):创世、`node2{1..4}.toml`、`keys/`(验证人密钥库,600)、`password`、`tokens.env` |
| 链数据 | `/data/zhixing-testnet/n2{1..4}/`(不随版本移动) |
| 操作日志 | `/opt/logs/zhixing-testnet-{starter,health}.log`(各 ≤ 1 MB) |
| 开机自启 | `zhixing-testnet.service` → `/opt/deploy/zhixing-testnet/scripts/starter.sh` |

```sh
cd /opt/deploy/zhixing-testnet
scripts/starter.sh status|start|stop|restart
scripts/health-check.sh --level all              # 退出码 0 通过;--format json;--retries N --interval S
scripts/test.sh --suite smoke                    # 只读冒烟;也可 --base-url https://blockchain.yeying.pub/rpc
cat config/tokens.env                            # RPC 写令牌(只私下发给成员)
```

**防双签**:`starter.sh` 按数据目录识别节点进程(不依赖 PID 文件或路径),每个节点进程持有
`<data_dir>/.node.lock` 的 flock;同一数据目录上已有进程时拒绝再启动。**同一组验证人密钥绝不能在两台机器上同时运行。**

4 个验证节点在同一台机器上:这是**测试网**的取舍(机器宕机即全链停),不是生产拓扑。

### 打包

在有 Rust 的 x86_64 机器上(新机即可),从干净 checkout:

```sh
scripts/package.sh --ref <commit> --source-commit <commit> --version <x.y.z> --output-dir /opt/package
```

只从已提交代码构建(隔离 worktree);不创建 tag、不推送、不部署。在非 x86_64 机器(如 arm64 Mac)上用 Docker 交叉编译。

### 升级(手动执行升级器的流程)

```sh
CUR=/opt/deploy/zhixing-testnet-service-v<旧>; TGT=/opt/deploy/zhixing-testnet-service-v<新>
cd /opt/package && sha256sum -c <包>.sha256sum && tar -xzf <包> -C /opt/deploy/
bash $TGT/scripts/upgrade.sh prepare $CUR $TGT
bash $CUR/scripts/starter.sh stop                         # 必须返回 0(确认已退出)
bash $TGT/scripts/upgrade.sh copy-config $CUR $TGT        # 迁移 config/,改写节点配置里的版本目录路径
bash $TGT/scripts/upgrade.sh post-copy $CUR $TGT
bash $TGT/scripts/starter.sh start
bash $TGT/scripts/health-check.sh --level all --retries 40 --interval 6   # 节点重放历史约 1–2 分钟
bash $TGT/scripts/upgrade.sh finalize $CUR $TGT
ln -sfn $(basename $TGT) /opt/deploy/zhixing-testnet
bash /opt/deploy/zhixing-testnet/scripts/test.sh --suite smoke
```

回退:同样的步骤反过来(停新版本 → `copy-config $TGT $CUR` → 启旧版本 → 切回软链接)。

## 旧机:浏览器与 nginx

| 项 | 位置 |
|---|---|
| nginx 站点 | `/etc/nginx/conf.d/blockchain.conf`(仓库 `nginx/blockchain.conf`;读写分开限流;`/rpc/` 在旧机上指向 `127.0.0.1:19221`) |
| 浏览器 | `zhixing-explorer.service`,`/opt/zhixing/tools/explorer`,`NODE_RPC=http://127.0.0.1:19221` |
| 正文仓库 | `/opt/zhixing/bodies`;上传令牌在 `/etc/zhixing/explorer.env` |
| 节点通道 | `zhixing-tunnel.service`:`ssh -N -L 127.0.0.1:19221:127.0.0.1:9221 -p 10022 zhixing-tunnel@221.130.30.75`,专用密钥 `/etc/zhixing/tunnel_ed25519` |

新机侧通道账户 `zhixing-tunnel`(无 shell),`authorized_keys` 限定 `restrict,port-forwarding,permitopen="127.0.0.1:9221"`:
该密钥不能执行命令、不能分配终端、只能转发到节点 RPC。

```sh
systemctl status zhixing-tunnel             # 通道断开时浏览器读不到链,链本身不受影响
curl -s 127.0.0.1:19221/height              # 经通道读新机高度
```

更新浏览器:用脚本同步并重启,带自检(校验和一致 + 首页返回 200):

```sh
deploy/deploy-explorer.sh            # 同步 tools/explorer/ → 重启 zhixing-explorer → 自检
deploy/deploy-explorer.sh --check    # 只看远端状态,不改动
```

脚本只碰浏览器(旧机的 `/opt/zhixing/tools/explorer`),不碰节点、链数据与密钥——节点在新机上,
由 `/opt/deploy/zhixing-testnet/scripts/starter.sh` 管理。环境变量 `HOST`(默认 `root@122.9.144.27`)、
`DEST`(默认 `/opt/zhixing/tools/explorer`)可覆盖。

## 监控与告警

`deploy/monitor-testnet.sh`(已装到两台机器的 `/opt/zhixing/deploy/`,并接入社区的飞书通知):

| 机器 | 定时任务 | 检查 |
|---|---|---|
| 新机 | `/etc/cron.d/zhixing-testnet-monitor`(每 5 分钟) | 4 个节点 RPC 可答、链在出块、4 个节点 head 一致、`/data` 余量 |
| 旧机 | `/etc/cron.d/zhixing-tunnel-monitor`(每 5 分钟) | 旧机 → 新机的通道可答、`/data` 余量 |

脚本自己写日志(`/opt/logs/zhixing-testnet-monitor.log`,超过 1 MB 截断),发现异常时通过社区的
`send_feishu_message` 发出通知并以非零码退出(定时任务能看到失败)。通知链路已实测送达。

手动执行:

```sh
/opt/zhixing/deploy/monitor-testnet.sh                        # 新机
ZHIXING_SKIP_NODES=1 ZHIXING_CHECK_TUNNEL=1 /opt/zhixing/deploy/monitor-testnet.sh   # 旧机
```

链自身的 Prometheus 指标与告警规则见 [`docs/ops/`](../docs/ops/)(`zhixing_height`、`zhixing_peers_connected`、
`zhixing_slashing_events_total` 等);本机未部署 Prometheus,故用上面的定时脚本覆盖同样的关键指标。

## 已停用

| 文件 | 原因 |
|---|---|
| `deploy.sh` | 会在旧机上安装并启动验证节点(与新机同时运行即双签);默认拒绝执行 |
| `systemd/zhixing-node@.service` | 旧机节点单元,已从旧机删除 |
| `diskwatch.sh` + `systemd/zhixing-diskwatch.*` | 旧机磁盘看守;节点迁走后不再需要(新机 `/data` 余量 1.5 TB,由 `health-check.sh` 的磁盘检查覆盖) |
