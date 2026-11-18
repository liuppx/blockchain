# spec-node-migration-v0.1:测试网节点迁移到新服务器

> 状态:**已完成**(2026-11-14 16:44 切换,约 40 分钟停机,见文末执行记录)。按 [`OPERATIONAL_GUIDANCE`](../../../books/yeying/specification/OPERATIONAL_GUIDANCE.MD)
> "重要变更必须先形成方案与 checklist、保留书面记录"的要求编写。执行结果记在本文末尾的"执行记录"。

## 1. 背景

测试网 4 个验证节点、区块浏览器、nginx 都在 `122.9.144.27`(下称**旧机**)上。旧机 2 核 / 3.4 GB,
根盘只剩 7.7 GB,同时承载 MySQL、Docker 等其他项目;链每天增长约 240 MB,约 26 天后写满(见 `deploy/README.md` 磁盘一节)。

`221.130.30.75:10022`(下称**新机**)72 核 / 32 GB,`/data` 剩 1.5 TB,已按社区目录规范使用
(`/opt/deploy`、`/opt/package`、`/opt/logs`、`/data/<服务>`)。

## 2. 目标与非目标

**目标**
1. 4 个验证节点迁到新机,**保留全部链历史与验证人密钥**(同一条链继续出块,不重开);
2. 节点按社区规范打包与部署:安装包 + `scripts/{starter,health-check,test,upgrade}.sh`;
3. 浏览器、正文仓库、nginx、TLS 留在旧机;**DNS 不变**;
4. 旧机浏览器通过**加密通道**访问新机节点,新机不向公网开放任何节点端口;
5. 迁移完成并验证后,清理旧机上的节点进程与链数据(用户决定:不保留)。

**非目标**:改变共识参数、出块间隔、创世;把浏览器迁到新机;接入社区 WebDAV 发布流程(本次本地打包、`scp` 传输)。

## 3. 拓扑

```
用户 ──443──▶ 旧机 nginx ─┬─ /         ─▶ 127.0.0.1:8080  浏览器(旧机)
                         ├─ /bodies/  ─▶ 127.0.0.1:8080  正文仓库(旧机,/opt/zhixing/bodies)
                         └─ /rpc/     ─▶ 127.0.0.1:19221 ═══ SSH 本地转发 ═══▶ 新机 127.0.0.1:9221(n21 RPC)

新机:4 个验证节点 n21–n24,P2P / RPC / 指标全部只监听 127.0.0.1
```

浏览器服务器自身也把 `/rpc` 代理到 `NODE_RPC`,改为 `http://127.0.0.1:19221`。

### 3.1 通道:SSH 本地端口转发

| 项 | 设计 |
|---|---|
| 方向 | 旧机发起 `ssh -N -L 127.0.0.1:19221:127.0.0.1:9221 -p 10022 zhixing-tunnel@新机`(本地转发,旧机主动连接;新机无需新开端口) |
| 账户 | 新机新建系统用户 `zhixing-tunnel`,无 shell(`/usr/sbin/nologin`) |
| 授权 | 旧机生成专用 ed25519 密钥;新机 `authorized_keys` 中限定:`restrict,port-forwarding,permitopen="127.0.0.1:9221"`——不能执行命令、不能分配终端、只能转发到 9221 |
| 常驻 | 旧机 systemd 单元 `zhixing-tunnel.service`,`ServerAliveInterval=15`、`ExitOnForwardFailure=yes`、`Restart=always`(不依赖 autossh) |
| 主机密钥 | 旧机 `known_hosts` 预置新机主机公钥,`StrictHostKeyChecking=yes` |

只转发 RPC(9221)。P2P、指标端口都留在新机本机。

## 4. 打包与部署(社区规范)

| 规范要求 | 本项目实现 |
|---|---|
| `scripts/package.sh` + `scripts/package-api.json`(v1) | 在隔离 worktree 中按 `--ref` 构建 `node` 发布二进制,产出 `zhixing-testnet-service-v<ver>-<commit7>.tar.gz` 与 `release-manifest.json`(`type: service`) |
| 包内 `scripts/starter.sh [start\|stop\|restart]` | 管理 4 个验证节点进程;stop:SIGTERM → 等 `STOP_TIMEOUT_SECONDS`(30)→ SIGKILL → 再确认退出;只清除已确认退出进程的 PID 文件 |
| `scripts/health-check.sh --level liveness\|readiness\|dependency\|all` | liveness:4 个进程在、RPC 端口在听;readiness:4 个节点高度推进且 head 一致;dependency:磁盘余量;支持 `--format json`、退出码 0–4 |
| `scripts/test.sh --suite smoke` | 只读冒烟:读高度/版本/知识单元/成员,检查供应守恒;不写链 |
| `scripts/upgrade.sh prepare\|copy-config\|post-copy\|finalize` | `copy-config` 迁移 `config/`(节点配置、创世、密钥库、密码文件) |
| 部署目录 | `/opt/deploy/zhixing-testnet-service-v<ver>-<commit7>/`,软链接 `/opt/deploy/zhixing-testnet` |
| 持久数据 | `/data/zhixing-testnet/n2{1..4}/`(区块日志、证书日志) |
| 操作日志 | `/opt/logs/zhixing-testnet-*.log`(单文件 ≤ 1 MB 截断) |
| 安装包 | `/opt/package/zhixing-testnet-service-v<ver>-<commit7>.tar.gz` + `.sha256sum` |

**配置与密钥不进安装包**:节点配置、创世、验证人密钥库、密钥库密码放在部署目录的 `config/`(权限 700/600),
首次安装时从旧机迁入,升级时由 `copy-config` 带到新版本目录。密码不出现在命令行或日志里。

**进程管理**:社区参考服务由 `starter.sh` 直接后台运行(`nohup` + PID 文件),不使用 systemd。
本项目同样由 `starter.sh` 管理。为了开机自启,新机增加一个 systemd 单元 `zhixing-testnet.service`(`Type=forking`)调用
`/opt/deploy/zhixing-testnet/scripts/starter.sh`,不另起一套进程管理。

## 5. 迁移步骤与 checklist

**准备(不影响线上)**
- [ ] P1 仓库新增打包与运维脚本,本地 `package.sh` 构建出安装包,`release-manifest.json` 与 SHA-256 校验通过
- [ ] P2 安装包在本地全新目录解压:`starter.sh` 启动临时 4 节点链 → `health-check.sh --level all` 通过 → `test.sh --suite smoke` 通过 → `stop` 优雅退出
- [ ] P3 新机:建 `/data/zhixing-testnet`、`/opt/backup`;安装包传到 `/opt/package` 并校验 SHA-256;解压到 `/opt/deploy/...`;软链接
- [ ] P4 新机:建 `zhixing-tunnel` 用户与受限 `authorized_keys`;旧机:生成专用密钥、预置新机主机公钥、写好通道 systemd 单元(先不启用)

**切换(停机窗口,预计 5 分钟)**
- [ ] C1 记录旧机当前高度 H 与 head
- [ ] C2 旧机停 4 个验证节点(SIGTERM),确认进程退出;停后再记一次高度与 head(= 最终状态)
- [ ] C3 备份旧机 `out/` 到旧机 `/opt/backup/zhixing-testnet-out-<时间>.tar.gz`(回退用)
- [ ] C4 传输:旧机 4 份 `data/`、创世、密钥库、密码 → 新机 `/data/zhixing-testnet/` 与部署目录 `config/`;逐文件 SHA-256 比对
- [ ] C5 新机 `starter.sh` 启动;`health-check.sh --level all --wait 120` 通过
- [ ] C6 **一致性**:新机 4 个节点在高度 H 的 head 与旧机停机前记录的 head 完全一致;之后继续出块
- [ ] C7 旧机启用通道;`curl 127.0.0.1:19221/height` 正常
- [ ] C8 旧机浏览器 `NODE_RPC` 改为 `http://127.0.0.1:19221`,nginx `/rpc/` 改指 19221,`nginx -t` 后 reload
- [ ] C9 对外验证:`https://blockchain.yeying.pub/rpc/height` 递增;浏览器 Knowledge 页正常;写入路径(无令牌 401)正常;`test.sh --suite smoke --base-url https://blockchain.yeying.pub/rpc` 通过

**收尾**
- [ ] F1 新机 `zhixing-testnet.service` 开机自启
- [ ] F2 旧机:禁用并删除 `zhixing-node@*`、磁盘看守单元;删除链数据(备份保留在 `/opt/backup`)
- [ ] F3 旧机浏览器页面顶部的磁盘提醒撤除(`/status/disk.json` 不再生成)
- [ ] F4 更新 `deploy/README.md`、`testnet/MEMBERS.md`(无变化则不改)与本文执行记录

## 6. 回退

| 失败点 | 回退 |
|---|---|
| C5/C6(新机起不来或 head 不一致) | 新机 `starter.sh stop`;旧机 `systemctl start zhixing-node@{21..24}`(数据未动)。对外无感知,只是停了几分钟 |
| C7–C9(通道或代理问题) | 同上回退,并把旧机 nginx 与浏览器 `NODE_RPC` 改回 `127.0.0.1:9221` |
| F2 之后 | 从旧机 `/opt/backup/zhixing-testnet-out-<时间>.tar.gz` 恢复;但新机期间产生的区块不在备份里——以新机为准,只在新机彻底不可用时才这样做 |

**两边同时运行是被禁止的**:同一组验证人密钥在两台机器上同时出块会产生双签。切换步骤保证旧机节点先停、确认退出后才启动新机节点;
F2 删除旧机节点单元,从根本上排除误启动。

## 7. 风险与监测

| 风险 | 缓解 / 监测 |
|---|---|
| 通道断开 → 浏览器读不到链,写不进去 | systemd `Restart=always`;`ServerAliveInterval`;浏览器显示连接错误。链本身不受影响 |
| 新机重启后节点未起来 | `zhixing-testnet.service` 开机自启;`health-check.sh` 可随时执行 |
| 4 个验证人仍在同一台机器 | 测试网的已知取舍(机器宕机即全链停),与迁移前相同 |
| 新机负载高(load ≈ 104,主要是僵尸 `libvirtd`) | 节点合计 CPU 很低;迁移后观察出块间隔是否稳定在 1 秒 |
| 双签 | 见第 6 节:严格顺序 + 删除旧机节点单元 |

## 8. AI 协作记录

方案由 Claude(claude-opus-5-5)根据两台服务器的只读勘察与社区规范起草;用户确定了三项关键决策:
DNS 不切换、浏览器留在旧机、通道采用 SSH 转发、旧机节点不保留。

## 执行记录

### 触发

2026-11-14 约 15:50,旧机内存耗尽(3.4 GB,MySQL、PHP 与 4 个节点共用;节点重放时每个约 600–830 MB),
内核 OOM 反复杀掉节点进程,SSH 与网站无响应。用户重启旧机后,要求尽快把节点迁到新机。
因此本次迁移没有走"准备完毕再切换"的顺序,而是**先停旧机节点止损**,再迁移;原计划的完整安装包流程(P1/P2)推迟。

### 实际步骤

| 时间 | 步骤 | 结果 |
|---|---|---|
| 16:06 | 确认原因 | 内核日志:`Out of memory: Killed process … (node) anon-rss:602MB`,多次 |
| 16:07 | 停止并禁用旧机 4 个节点与磁盘看守 | 空闲内存 141 MB → 2.8 GB |
| 16:10 | 全量重放校验 4 份链数据(`node snapshot`) | n22/n23/n24:高度 **419195**,head `1afce7b8…1d0e59`,state_root `91799580…fa460f`,三者一致;n21 为 419190(被 OOM 杀于写入中途),是 n22 的严格前缀 |
| 16:18 | 旧机备份 | `/opt/backup/zhixing-testnet-out-20261114161812.tar.gz`(541 MB,含密钥库与密码,用于回退) |
| 16:22 | 新机按已提交源码构建 `node` | 与旧二进制的差别:包含 `02c592b`(`--key-file` 接受带注释种子文件) |
| 16:25–16:38 | 传输 | 旧机出口仅约 0.17 MB/s,经本机中转;传完 n21 全部数据与配置后改为**只取 n22 最后 5 个区块**(块记录固定 376 字节、证书 416 字节,共 3960 字节) |
| 16:40 | 拼装并校验 | n21 日志 + n22 尾部 → 全量重放:高度 419195、head、state_root 与旧机完全一致,快照字节哈希相同(`cd50544f…`);所有配置、密钥库、密码逐文件 SHA-256 与源一致 |
| 16:43 | 安装 | `/opt/deploy/zhixing-testnet-service-v0.1.0-967d1b2/`、软链接 `/opt/deploy/zhixing-testnet`、数据 `/data/zhixing-testnet/n2{1..4}`;节点配置改写为新路径,端口不变、只监听 127.0.0.1 |
| 16:44 | 再次确认旧机无节点进程后启动 | 约 100 秒重放后 4 个节点在 419210 一致,继续出块 |
| 16:47 | 通道 | 旧机 `zhixing-tunnel.service`:`127.0.0.1:19221 → 新机 127.0.0.1:9221`;新机 `zhixing-tunnel` 账户 `restrict,port-forwarding,permitopen="127.0.0.1:9221"`。验证:该密钥执行命令被拒、转发到 9222 被拒(administratively prohibited) |
| 16:48 | 旧机 nginx `/rpc/` 与浏览器 `NODE_RPC` 改指 19221 | 原配置备份在 `/opt/backup/*.pre-tunnel.*` |
| 16:49 | 对外验证 | 公网高度与新机一致(差 1 块);15 个单元、Alice 的 3 次核验、2 名成员、正文仓库完整;无令牌写入 401 |
| 16:50 | `health-check.sh --level all` | 15/15 PASS,退出码 0;JSON 输出合法;非法参数退出码 2 |
| 16:52 | 清理旧机(用户决定不保留) | 删除节点 systemd 单元、磁盘看守、链数据、旧二进制、密钥库密码文件;释放 1.3 GB |
| 16:53 | 新机开机自启 | `zhixing-testnet.service`(`Type=oneshot`,调用 `starter.sh start/stop`) |
| 16:54 | 停用 `deploy/deploy.sh` | 防止误把节点重新装回旧机造成双签 |

### 与方案的偏差

1. **安装包流程未完成**:`scripts/package.sh`、`package-api.json`、`starter.sh`、`health-check.sh` 已写;`test.sh`、`upgrade.sh` 未写;
   本次部署目录是手工组装的(二进制 + starter + health-check + 配置),**不是**由 `package.sh` 生成的安装包。后续补齐并按规范验收。
2. 停机约 40 分钟(15:50 OOM 起至 16:49 恢复),主要耗时在旧机极慢的出口带宽。
3. 迁移期间没有链上操作,不存在需要补发的待处理交易。

### 现状

| 机器 | 运行 |
|---|---|
| 新机 221.130.30.75 | 4 个验证节点(`/opt/deploy/zhixing-testnet`,数据 `/data/zhixing-testnet`) |
| 旧机 122.9.144.27 | nginx(TLS)、浏览器、正文仓库、SSH 通道 |

### 后续(同日 17:00–17:25):按规范打包、升级,以及一次差点双签的事故

1. 补齐 `scripts/test.sh`(smoke,6 个只读用例)与 `scripts/upgrade.sh`(四个阶段);健康检查改为在共同高度比较区块哈希。
2. `scripts/package.sh` 在新机从干净 checkout 构建出 v0.1.1 安装包;manifest、SHA-256、单一顶层目录、四个入口脚本、
   不含密钥与链数据,全部通过。
3. **事故**:按升级器流程把线上从 v0.1.0 升到 v0.1.1 时,旧版本 `starter.sh stop` 报告"未运行"并返回成功,
   新版本随即启动——**同一数据目录、同一验证人密钥出现了两份进程**(约 20 秒)。
   - **原因**:节点最初经软链接 `/opt/deploy/zhixing-testnet/...` 启动;stop 从真实路径 `...-v0.1.0-967d1b2/...`
     执行,用路径字符串比对命令行认不出自己的进程,于是当作"PID 文件失效",删掉 PID 文件并返回成功。
     这违反了规范"无法确认退出时 stop 必须返回非零"。
   - **影响**:无。重复进程中 3 个仍在重放历史(重放期间不投票、不签名),第 4 个读到正在追加的日志后拒绝启动;
     链上无双签证据,4 个节点持续一致出块;4 份区块日志逐字节相同,证书日志只在迁移之后出现正常的各自差异。
   - **修复**(`979dda7`):`starter.sh` 改为按**数据目录**识别节点进程(扫描所有 `node run --config X`,读 X 的 `data_dir`
     并解析为真实路径),不再依赖 PID 文件与路径字符串;每个节点进程持有 `<data_dir>/.node.lock` 的 flock,
     同一数据目录的第二份进程由内核拒绝;数据目录已被占用时 start 拒绝启动。在新机用独立端口的临时链
     复现事故场景验证(经软链接启动、从另一版本真实路径停止;同数据目录启动第二份;绕过脚本直接启动;
     PID 文件缺失;PID 被复用)。
4. 用修复后的 v0.1.2 安装包重新升级:先把当前版本的 `starter.sh` 换成修复版(否则升级器调用的仍是有缺陷的 stop),
   stop 后**显式确认节点进程数为 0** 才启动新版本。升级前后高度 420641 的区块哈希相同(`498763ee…`),同一条链。
5. 升级中发现 `health-check.sh --retries` 参数被解析却从未生效(`66e6550` 修复),节点重放约 20 秒后 11/11 通过。
6. 清理新机迁移中间产物(其中有验证人密钥库与密码的副本)与临时测试链。

### 现状(2026-11-14 17:25)

- 新机:`/opt/deploy/zhixing-testnet` → `zhixing-testnet-service-v0.1.2-979dda7`;上一版本 `v0.1.0-967d1b2` 保留用于回退;
  安装包 `/opt/package/zhixing-testnet-service-v0.1.2-979dda7.tar.gz`(SHA-256 `7eeb9bb0…`)
- 冒烟:本机与公网(经通道)均 6/6 通过
- 运维文档:`deploy/README.md` 已按新拓扑重写

### 待办

- [ ] `v0.1.2` 包内的 `health-check.sh` 是手动替换的修复版;下次打包(v0.1.3)时随包发布,保持"部署目录 = 安装包内容"
- [x] 节点程序自身锁定数据目录(2026-11-15,`4878a2e`):`DataDirLock` 在打开日志前对 `<data_dir>/.node.lock`
      取独占锁,同一数据目录的第二个节点直接失败,覆盖手工启动。实测:手工在同一数据目录再启一个节点被拒绝,线上链不受影响。
      **升级中暴露的问题**:脚本层原有的 flock 锁的是**同一个文件**,两把锁互相冲突,4 个节点全部拒绝启动,链暂停约 2 分钟。
      修复(`b8864e1`):去掉脚本层的锁(节点自己的锁已覆盖全部启动方式),链恢复,升级前后同一高度区块哈希相同。
- [ ] 旧机备份 `/opt/backup/zhixing-testnet-out-20261114161812.tar.gz` 含验证人密钥库,确认不再需要回退后删除
- [ ] 接入社区 WebDAV 发布与 `operator/upgrade/upgrade.sh`(本次为本地打包、手动执行升级流程)
