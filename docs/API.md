# 知行图测试网 · 公开 API / Public API

> 面向外部开发者与 AI 应用。基址:`https://blockchain.yeying.pub`
> - 节点 RPC:`https://blockchain.yeying.pub/rpc/`
> - 浏览器服务(检索、导出、正文仓库):`https://blockchain.yeying.pub/`
>
> 所有读取接口**公开,无需令牌**;写入接口需要 `Authorization: Bearer <写入令牌>`(令牌由组织者私下提供)。
> 协议强制所有知识内容以 **CC BY 4.0** 许可开放:任何人可使用(含商用),只需署名。

## 1. 约定

| 项 | 说明 |
|---|---|
| 编码 | UTF-8;`Content-Type: application/json` 或 `text/plain` |
| 数值 | u64 以**字符串**返回(如 `"height":"419273"`),避免 JS 精度丢失 |
| 协商 | 加 `Accept: application/json` 得到 JSON;默认 `text/plain`(逐行键值,便于 grep) |
| 分页 | 列表接口带 `?limit=`(默认 100,上限 1000)与游标 `?cursor=`;响应含 `total` 与 `cursor` |
| 错误 | 非 2xx 时 body 为 `{"error":"..."}`(JSON)或纯文本;写入被拒常用 401(缺令牌)/409(重复)/422(规则不满足)/429(限流) |
| 链上不存正文 | 链上只有正文的 SHA-256 哈希;正文在**正文仓库**(`/bodies/<hash>`),浏览器取回后自行校验哈希 |

## 2. 链状态

| 端点 | 说明 |
|---|---|
| `GET /rpc/height` | 当前高度 |
| `GET /rpc/head` | 区块头哈希 |
| `GET /rpc/genesis` | 创世摘要(genesis_hash / state_root / validators) |
| `GET /rpc/info` | 创世摘要 + 当前高度 + head |
| `GET /rpc/version` | 协议版本 |
| `GET /rpc/routes` | 本节点支持的路由清单 |
| `GET /rpc/params` | 链参数(增发基准、衰减半衰期、罚没比例等) |
| `GET /rpc/supply` | 供应与守恒检查(`conserved`) |
| `GET /rpc/status` | 节点存活 |
| `GET /rpc/node` | 本节点身份与角色 |
| `GET /rpc/config` | 本节点配置(非敏感项) |

## 3. 知识(核心)

### 3.1 单元目录

```
GET /rpc/units?limit=50&cursor=<id>&domain=2&layer=foundation&author=7&min_effective_bps=2000
```

| 参数 | 说明 |
|---|---|
| `limit` | 每页条数(默认 100) |
| `cursor` | 上一页最后一个单元 id |
| `domain` | 按领域筛选(1 = 大模型应用开发,2 = 运动健身) |
| `layer` | 按层级筛选(`foundation` / `explanation` / `scenario` / `application`) |
| `author` | 按作者账户筛选 |
| `min_effective_bps` | 只看有效可信度不低于此值的单元(万分比,10000 = 1.0) |

每项字段:`id`、`layer`、`domain`、`author`、`created_height`、`versions`、`credibility`
(`passes` / `needs` / `self_bps` / `effective_bps` / `bottleneck` / `stale_cites`)、
`body_hashes`(当前版本正文哈希)、`cites`(当前版本的引用:`unit` / `version` / `kind`)、
`lifecycle`(生命周期状态,见下)。

> **生命周期状态**(由链上关系派生,不是新增的状态字段):
> | 值 | 含义 |
> |---|---|
> | `active` | 活跃:有核验通过,或仍被别的单元依据 |
> | `dormant` | 休眠:既无人核验、也无人依据(通常是图谱最上层、待核验的单元) |
> | `contested` | 竞争:它以"取代"引用挑战了另一个单元,新旧**并存**,链上不强行裁决 |
> | `superseded` | 已取代:被另一个单元取代;历史与署名保留,**不再向依据它的单元传递可信度** |
>
> **引用类型 `kind`**:`basis`(依据,传递可信度)、`see-also`(参见)、`supersedes`(取代)。

> **可信度语义**:`effective_bps` = min(自身可信度, 所依据的每个单元的有效可信度)。
> 每一位"需修改"抵消一位"通过"。`bottleneck` 指出卡住它的那条依据;`stale_cites` 指出已过时的依据。

### 3.2 单元详情

```
GET /rpc/unit/{id}
```

返回该单元的全部版本:每版含 `n`、`publisher`、`published_height`、`source`
(`human` / `model:<id>` / `model-verified:<id>`)、`bodies`(语言与哈希)、`cites`、
`verifications`(核验人、结论、高度)、以及该版可信度。

### 3.3 成员

```
GET /rpc/members?limit=100
```

每项:`id`、`joined_height`、`inviter`(0 = 创世成员)、`last_invite_height`、`established`
(是否已过见习期)。**已过见习期**的成员其核验才计入可信度。

### 3.4 全文检索(浏览器服务)

```
GET /search?q=<关键词>&domain=2&layer=foundation&limit=50
```

跨**标题与正文**检索(正文在正文仓库,链上只有哈希)。中文按单字与双字组合匹配,英文按词匹配。
返回:`items[]`(`id`、`title`、`excerpt` 摘要、`body_url`、`credibility`、`score` 命中分,标题命中加权)。

### 3.5 认知地形图(浏览器服务)

```
GET /terrain
```

把链上的引用关系与可信度投影成地形数据:每个单元给出层级、领域、核验通过数、有效可信度、被引/引用的次数、
重要性、**地形类型**(峰 / 高原 / 山脊 / 山谷 / 断层 / 坡 / 平地)与高度;另有全部引用边(依据 / 参见 / 取代)。
数据全部来自链上已有的记录,**不新增任何链上状态**。

### 3.6 探索与缺口(浏览器服务)

```
GET /explore?unit=N      该单元的上下游:谁建立在它之上(继承)、它站在谁之上(地基),按距离分层
GET /gaps[?domain=N]     缺口:没有任何单元依据它、也还没人核验它的单元(图里的"山谷")
GET /coverage            按领域的覆盖:单元数、已确立数、缺口数、层级分布
```

`/explore` 沿着链上的引用关系双向遍历,返回 `{unit, upstream[], downstream[]}`,
每项含 `{unit, depth, via, kind}`:`depth` 是距离(1 = 直接),`via` 是中间那一条,`kind` 是引用类型。
这是"持续探索"的支撑:从任意一条知识,能立刻知道**它支撑了谁、又被谁支撑**。

### 3.7 导出(浏览器服务)

```
GET /export/units
```

JSONL:第一行是元数据(许可、来源、单元数),其后每行一个单元,含**正文全文**与链上信息
(`id`、`layer`、`domain`、`author`、`cites`、`credibility`、`license`、`bodies[]`)。
正文缺失时 `text` 为 `null`(链上哈希仍在,可据此去正文仓库取)。

每个单元还带 `version_history`:每个版本的发布人、来源(`human` / `model:<模型>` / `model-verified:<模型>`)、
正文(语言与哈希)、引用、核验记录与可信度。正文的 `lang` 取自链上记录。

> 用途:喂给大模型训练、做检索索引、做离线分析。许可为 CC BY 4.0,署名即可。

```
GET /export/units?format=jsonld      同样的内容,用知识互操作词表 v1 表达(JSON-LD,一个 @graph)
GET /unit/{id}.jsonld                单个单元的 JSON-LD(不含正文全文)
GET /id/unit/{id}[/v/{n}]            全局标识;Accept: application/ld+json 返回 JSON-LD,浏览器访问跳到单元页
GET /id/member/{id}   GET /id/domain[/{n}]
GET /ns/v1/context.jsonld            上下文;另有 vocab / domains / layers / lifecycle 四个 .jsonld
```

JSON-LD 可被任何 JSON-LD 处理器展开成 RDF、用 SPARQL 查询。词表与标识规则见
[`docs/design/ontology-v0.1.md`](design/ontology-v0.1.md)。

### 3.7.1 查询(浏览器服务)

```
GET /lookup?body=<sha256>     → {found, unit, version, versions[], lang}   正文属于哪个单元的哪些版本
GET /lookup?pubkey=<64 hex>   → {found, account, established}             公钥对应的成员
```

同一份正文只能属于一个单元,所以提交 `create` / `publish` 后,可以用正文哈希查到它落在哪个单元、哪个版本
(`POST /rpc/knowledge` 只返回操作哈希)。

### 3.8 正文仓库

```
GET  /bodies/<sha256>          公开;内容寻址,永不变化
PUT  /bodies/<sha256>          需写入令牌;服务端校验 sha256(body) == <sha256>,不符则 422
```

浏览器取回正文后会**重新计算哈希**并与链上比对,一致才显示。

## 4. 链上其他只读目录

| 端点 | 内容 |
|---|---|
| `GET /rpc/validators` | 验证人集(id、权力、公钥) |
| `GET /rpc/accounts` | 账户目录(余额、质押、收益、提交数) |
| `GET /rpc/account/{id}` | 单个账户 |
| `GET /rpc/reviewers` | 审阅人目录(声誉) |
| `GET /rpc/cnfts` | cNFT 贡献证书 |
| `GET /rpc/proposals` | 治理提案 |
| `GET /rpc/bonds` | 质押绑定 |
| `GET /rpc/unbonding` | 解绑队列 |
| `GET /rpc/stake-ops` | 待处理质押操作 |
| `GET /rpc/evidence` | 待处理的双签证据 |
| `GET /rpc/peers` | 已连接对端 |
| `GET /rpc/mempool` | 内存池 |
| `GET /rpc/graph` | 旧版认知图谱(嵌入向量图;测试网已关闭 ΔK 提交,通常为空) |
| `GET /rpc/bridge/locks` | 跨链锁 |

## 5. 区块与证明

| 端点 | 说明 |
|---|---|
| `GET /rpc/block/{height}` | 区块摘要 |
| `GET /rpc/block/{height}/txs` | 区块内交易 |
| `GET /rpc/tx/{hash}` | 交易是否已上链 |
| `GET /rpc/tx/{hash}/detail` | 交易详情 |
| `GET /rpc/header/{height}` | 认证区块头(certified header) |
| `GET /rpc/headers?limit=&from=` | 批量取头(轻客户端同步) |
| `GET /rpc/account/{id}/proof` | 账户的 Merkle 包含证明 |
| `GET /rpc/leaf/account/{id}` | 证明叶 |
| `GET /rpc/graph/{id}/proof`、`/rpc/cnft/{id}/proof`、`/rpc/reviewer/{id}/proof`、`/rpc/bridge/lock/{id}/proof` | 各实体的包含证明 |

轻客户端可用 `header + cert + proof` 离线验证:头由 2/3 投票权证书背书,证明可对着头里的
`accounts_root` / `next_validators_root` 复核,无需重放整条链。

## 6. 写入(需要令牌)

```
POST /rpc/knowledge        知识操作(create / publish / verify / invite)
POST /rpc/key-rotation     账户换密钥(用**当前**密钥签名,把账户的公钥换成新的)
```

请求体是 op 的规范编码(二进制),不是 JSON;`Authorization: Bearer <写入令牌>`。
**每个操作必须用你自己的密钥签名**,令牌只防刷写入,不能代替签名。

各操作的可执行工具(在仓库里):
- 浏览器:https://blockchain.yeying.pub 的「我的身份」与单元页(核验、邀请、写作、修订);
  签名一律在[夜莺钱包](https://wallet.yeying.pub/)里完成,网页不持有、不保存私钥(钱包接口 `zhixing_requestAccounts` /
  `zhixing_signKnowledgeOp`,见 [`tools/explorer/README.md`](../tools/explorer/README.md));
- 命令行:`node encode-knowledge-op` + `node rpc --path /knowledge --post`。

**换密钥**(成员换设备或换备份方式时用):账户编号、成员身份、见习期与全部历史都不变,
只是之后的知识 op 改用新密钥签名。旧密钥立刻失效(再用它签的任何操作都会被拒)。

```sh
node keygen --out new.seed --new-mnemonic          # 新密钥(标准派生,m/44'/1105'/0'/0'/0')
node encode-key-rotation --key-file old.seed --out rot.bin \
    --account <账户号> --nonce <当前序号> --new-pubkey <新公钥>
node rpc --path /key-rotation --post rot.bin
```

换完之后把新密钥放进钱包、删掉旧的知行图账户即可。

## 7. 限流

nginx 按读写分开限流:**写入 5 次/秒/IP**(突发 20),**读取 50 次/秒/IP**(突发 300)。
超限返回 `429`。打开一个知识页会同时发出几十个读请求,读额度为此留足。

## 8. 稳定性

接口形状在测试网期间**可能变**(这是测试网)。破坏性变更会写进本文件与
`docs/design/`。依赖读取接口的应用请用 `?format=json` 并对未知字段保持宽容。
