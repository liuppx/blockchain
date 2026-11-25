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

### 发布约定:先合并到社区 main

**每次发布,代码必须先合并到社区仓库 `yeying-community/blockchain` 的 main 分支。**

发布分两个阶段,两者产物目录与升级器接口相同(`/opt/deploy/<unit>-v<版本>-<commit7>/` +
`scripts/upgrade.sh prepare|copy-config|post-copy|finalize`),切换时只换"包从哪来"这一段:

| 阶段 | 包从哪来 | 怎么升级 |
|---|---|---|
| **当前(开发期)** | 节点服务器上直接拉社区 main、在服务器上编译 | `deploy/deploy-nodes.sh` 一步完成 |
| **稳定后** | 打包机器上 `scripts/package.sh` 出包(社区打包规范) | 上传包 → 校验 sha256 → 解压 → `scripts/upgrade.sh` |

当前阶段为了提升开发效率,直接在节点服务器上编译部署:服务器上 `/root/code/blockchain` 存放社区源代码,
发布时拉社区 main 的最新代码、编译、部署。**不再从本地(开发机)编译、拷贝二进制。**
稳定后迁移到打包机器,回到"打包机出包 + `upgrade.sh` 升级"这条规范流程——`scripts/package.sh` 就是为它准备的。

```sh
deploy/install-rust.sh              # 一次性:在节点服务器上装 Rust 工具链(rustup stable)
deploy/deploy-nodes.sh              # 当前:拉社区 main → 服务器上编译 → 部署 → 自检
deploy/deploy-nodes.sh --check      # 只看远端状态
```

`deploy-nodes.sh` 会检查待发布的提交**确实在社区 main 上**,不在就拒绝发布;
服务器上的源码工作区有未提交改动时也拒绝拉取(不覆盖人改过的东西)。

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

## 协议升级(按高度激活)

排期、激活当天要看什么、出问题怎么办,见 [`docs/ops/upgrades.md`](../docs/ops/upgrades.md)。
测试网的升级时刻表以那份文档 §1 为准(时刻表不进创世哈希,仓库里要另有记录)。

```sh
deploy/activate-upgrade.sh --check                  # 看远端当前的时刻表
deploy/activate-upgrade.sh --height H --dry-run     # 只检查
deploy/activate-upgrade.sh --height H               # 排期(会重启节点)
```

**新增领域不用改代码、不用发新版本**:领域集合写在创世的 `[[domains]] id = N height = H` 里,
和 `[[upgrades]]` 一样不进创世哈希,按高度生效。

```sh
deploy/activate-domain.sh --check                       # 看远端当前的领域时刻表
deploy/activate-domain.sh --id 3 --height H --dry-run   # 只检查
deploy/activate-domain.sh --id 3 --height H             # 排期(会重启节点)
```

## 正文镜像(新机)

链上只存正文的 SHA-256,正文本身在旧机的正文仓库里(`/opt/zhixing/bodies`)。仓库只有一份时,
那台机器的盘坏了,链上的哈希就成了空壳。新机上因此留一份**可核对的副本**:

| 项 | 位置 |
|---|---|
| 副本目录 | `/data/zhixing-testnet/bodies-mirror/`(正文按哈希存,另有 `STATUS.json` 与 `MANIFEST.json`) |
| 定时任务 | `/etc/cron.d/zhixing-body-mirror`(每小时第 17 分)→ `/opt/zhixing/deploy/body-mirror.sh` |
| 工具 | `/opt/zhixing/tools/body_mirror.py`(仓库 `tools/body_mirror.py`,纯标准库) |
| 监控 | `monitor-testnet.sh` 读 `STATUS.json`:副本是否齐全、是否按时更新、主仓库是否丢了正文;异常照旧发飞书 |

副本收的是链上**全部版本**的正文,以及核验时写了说明的那些说明正文(`note_hash`;全 0 表示没写说明)。
每份都重新核对 SHA-256,哈希不符的内容一个字节都不落盘。

```sh
# 手动跑一次(新机)
/opt/zhixing/deploy/body-mirror.sh
tail -6 /opt/logs/zhixing-body-mirror.log

# 只检查主仓库是否完整(任何机器;不写盘,缺了就非零退出)
/opt/zhixing/tools/body_mirror.py --check

# 主仓库丢了正文时,用副本补回去(写入令牌放环境变量,不进命令行)
ZHIXING_BODY_TOKEN=… python3 /opt/zhixing/tools/body_mirror.py \
  --dest /data/zhixing-testnet/bodies-mirror --restore-to https://blockchain.yeying.pub
```

`tools/body_mirror.py` 也能单独用:任何人都能拿链上的引用去核对正文,不需要信任浏览器服务。

## 已停用

| 文件 | 原因 |
|---|---|
| `deploy.sh` | 会在旧机上安装并启动验证节点(与新机同时运行即双签);默认拒绝执行 |
| `systemd/zhixing-node@.service` | 旧机节点单元,已从旧机删除 |
| `diskwatch.sh` + `systemd/zhixing-diskwatch.*` | 旧机磁盘看守;节点迁走后不再需要(新机 `/data` 余量 1.5 TB,由 `health-check.sh` 的磁盘检查覆盖) |
