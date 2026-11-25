# 协议升级:排期、激活与处理手册

> 适用于按高度激活的协议升级(`node/src/upgrade.rs`),第一个是知识经济 v1(`knowledge-economy-v1`)。
> 设计见 [`docs/design/knowledge-economy-v1.md`](../design/knowledge-economy-v1.md),
> 机制见 [`docs/design/rolling-upgrade.md`](../design/rolling-upgrade.md) §9。

## 1. 测试网的升级时刻表(以这里为准)

时刻表写在节点服务器的 `config/genesis.toml` 末尾的 `[[upgrades]]` 里,**不进创世哈希**。
所以仓库里必须另有一份记录:以后用仓库里的创世重建节点(灾备、新增验证人)时,要按这里补上,
否则新节点会在激活高度停下。

| 升级 | 激活高度 | 节奏(结算/锁定/追加,块) | 排期日期 | 状态 | `genesis-hash` 最后一行 |
|---|---|---|---|---|---|
| `knowledge-economy-v1` | **1259716** | 86400 / 15552000 / 259200 | 2026-11-25 | **已排期**(节点 v0.1.8,社区 main `d116584`) | `upgrades knowledge-economy-v1@1259716` |

**激活与结算的时间(按实测 1.204 秒/块推算,会随出块速率漂移)**:

| 高度 | 约在 | 是什么 |
|---|---|---|
| 1259716 | 11-26 10:06 | 激活:协议指派、确立、得分开始生效 |
| 1296000 | 11-26 22:14 | 第 1 个结算点(此后每 86400 块一次,约 28.6 小时) |
| 1370400 | 11-28 03:08 | 第 2 个结算点 |
| 1444800 | 11-29 08:02 | 第 3 个结算点(此时窗口已满,**首次增发**在这里) |
| 1519200 | 11-30 12:56 | 第 4 个结算点 |

创世哈希在加 `[[upgrades]]` 前后不变:`114326ee015a4817d9369b94d8bcad84060bad0a222b7a95412d61b5a2b5fd6d`。

### 1.1 新增领域(加领域不用改代码)

| 领域 | 编号 | 激活高度 | 排期日期 | 状态 | `genesis-hash` 最后一行 |
|---|---|---|---|---|---|
| 教育 | **3** | 待定 | — | 未排期 | — |

**新增领域是数据,不是代码。** 领域集合写在创世的 `[[domains]]` 里:

```toml
[[domains]]
id = 3      # 领域编号
height = H  # 从这个高度起可用(含该高度)
```

**不需要改节点代码、不需要发新版本**——创世里加一行就够。

**为什么要按高度激活**:领域登记在创世的 `[knowledge] domains` 里,而创世状态决定创世哈希,
创世哈希就是链的身份。直接改 `domains = [1, 2]` 会让已有的 120 多万个区块全部作废。
所以新增领域走同一套按高度激活:从 H\* 起,"这一刻能用哪些领域"里多一个编号;
`knowledge.domains` 本身不改,折进状态根的是"有效领域集合",所以 H\* 之前每个字节都与加领域前相同
(创世哈希也不变——实测过,见 `node/src/config.rs` 的 `domains_section_parses_and_rejects_duplicates`)。

领域编号:1 = 大模型应用开发,2 = 运动健身,**3 = 教育**。名称只在
`tools/explorer/public/ns/v1/domains.jsonld` 定义。网页只让成员选**链已接受**的领域
(读 `GET /domains` 的 `next`),所以词表里先加上教育、链还没激活时,网页不会让人选它。

排期:

```sh
deploy/activate-domain.sh --id 3 --height H
```

排期后把上面那张表改成实际值(高度、日期、`node genesis-hash` 的最后一行原样抄进来),提交到社区 main。

**测试网出块约 1.19 秒一块**(2026-11-24 实测)。所以"1 天"的结算周期(86400 块)实际约 28.6 小时,
"180 天"的锁定期约 248 天。时间换算一律按实测速率,不要按 1 块/秒。

## 2. 排期前的检查

1. 代码已合并到社区 main,节点已经用社区 main 编译部署(`deploy/deploy-nodes.sh`)——
   **只有实现了经济规则的版本才能排期**:0.1.7 认识升级名、但没有规则,用它排期到了激活高度会"激活"一套空规则。
2. `deploy/activate-upgrade.sh --height H --dry-run`:检查 4 个节点都提供 `/economy`(带结算字段)、
   H 至少领先当前高度 1 小时、加 `[[upgrades]]` 后创世哈希不变。
   (新增领域用 `deploy/activate-domain.sh --id N --height H --dry-run`:它检查 4 个节点都提供 `/domains`。)
3. 激活高度选在**人醒着**的时候到达(激活当块会第一次执行新规则)。

## 3. 排期

```sh
deploy/activate-upgrade.sh --height H
```

脚本会:备份创世 → 追加 `[[upgrades]]` → 逐个节点核对时刻表一致 → 重启 → 健康检查 →
**逐个节点**核对 `/economy` 报的激活高度都是 H → 确认重新出块。
任何一步失败都会打印回退命令(见 §5.1)。

排期后:
- 按 §1 更新时刻表记录并提交;
- `deploy/monitor-testnet.sh` 的 `economy-schedule` 检查应显示"4 个节点激活高度一致(H)"。
  它在激活**之前**就能发现"有节点没换上时刻表"——那是最需要早发现的情况。

## 4. 激活当天要看什么

| 时间点 | 看什么 | 正常 |
|---|---|---|
| H 之前 | 监控 `economy-schedule` | 4 个节点激活高度一致 |
| H | 4 个节点高度继续增长;`/economy` 的 `active=true` | 出块不停,无告警 |
| H 之后第一个周期边界(高度为周期的整数倍) | `/economy` 的 `period settled` | 从 0 变成 1 |
| 再过 2 个周期 | `period issued` | 有知识量增长时开始大于 0 |

监控的 `economy-settlement` 会在结算落后两期以上时告警。

## 5. 出问题时

### 5.1 激活之前发现问题 → 回退

激活高度之前随时可以安全回退:

```sh
ssh -p 10022 root@221.130.30.75
cd /opt/deploy/zhixing-testnet
cp config/genesis.toml.bak-<时间> config/genesis.toml
scripts/starter.sh restart
```

然后 `deploy/activate-upgrade.sh --check` 确认 4 个节点都回到 `upgrades none`。

### 5.2 激活之后,**一个**节点停在 H

症状:3 个节点继续出块,1 个停在 H−1;它的日志里有
`certified block does not apply locally — this node's rules differ from the quorum`。
原因:它的时刻表或版本与其他节点不同。

处理:3 个节点仍是法定数,链不停。给这个节点换上与其他节点相同的版本与创世,重启即可;
它会重放自己的日志并追上(`testnet/upgrade-drill.sh` 演练过这个过程)。

### 5.3 激活之后,**两个或以上**节点停在 H

症状:链停摆(4 个验证人里不足 3 个能出块)。
处理:把停下的节点按 5.2 修好;法定数恢复后链继续。**不要**把时刻表回退——
H 之后回退时刻表,节点重放到 H 会算出与证书不同的状态根而启动失败。

### 5.4 激活之后发现规则有错

不要在原升级上改规则(那会改已经被证书签过的状态根)。正确做法是**再排一次升级**
(例如 `knowledge-economy-v2`),在新的激活高度切换到修正后的规则。

## 6. 快照与备份恢复

快照里没有升级时刻表。激活以后,离线检查快照必须带上创世文件:

```sh
node inspect-snapshot --snapshot F --genesis config/genesis.toml
node check-snapshot --snapshot F --header HEX --genesis config/genesis.toml
```

`testnet/recover.sh` 已经这样做了。节点从快照启动时会自动从创世配置补上时刻表。
