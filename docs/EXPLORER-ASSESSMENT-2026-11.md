# 区块链浏览器评估 / Block explorer assessment — 2026-11

> 评估这条链「浏览器」(区块浏览器 / block explorer)的现状、可行性与差距。结论先行:
> **目前没有浏览器 UI,但节点 RPC 的只读 API 已相当完整且原生支持 JSON** —— 浏览器是
> 「可直接在现有 API 上搭建」的,不需改共识/wire,主要缺的是前端本身与少数几个便利端点。
>
> **更新(M223)**:本仓已补齐 L1 的两个节点侧前置——**RPC 读面全局 CORS**(`Access-Control-Allow-Origin: *`,
> 浏览器可跨源直连读 API)与 **`GET /tx/{hash}/detail`**(交易完整字段)。§3 差距 #1、#2 已消除;
> 纯静态前端现在可直连节点 RPC(无需代理)。

## 1. 现状

- **无任何前端 / Web UI / explorer 代码**(仓库无 `web/`、`frontend/`、`explorer/`)。
- **有完整的只读 RPC**(`node run` 的 `[rpc]` ingress,`daemon.rs`),且:
  - **内容协商**:每个读端点同时能返回 `text/plain` 与 `application/json`(按 `Accept:` 头,M68/69)
    —— 对 JS 前端友好。
  - **端点发现**:`GET /routes` 列出所有可用路由。
  - **分页**:列表端点支持 `?cursor=/?offset=/?limit=`。
  - **鉴权**:读面公开,写面(`/submit_tx`、`/gov/*`)可配 bearer token。

## 2. 浏览器所需数据 vs 现有端点

| 浏览器页面 | 需要 | 现有端点 | 覆盖 |
|---|---|---|---|
| 链概览 | 高度、head、供应、版本、genesis | `/height` `/head` `/supply` `/version` `/info` `/genesis` `/config` `/status` | ✅ |
| 最新区块列表 | 分页区块头 | `/headers?from=&limit=`、`/height` | ✅ |
| 区块详情 | 头 + 交易列表 | `/block/{h}`、`/block/{h}/txs` | ✅(txs 为 hash+author 引用,见差距) |
| 交易查询 | 按哈希定位 | `/tx/{hash}`(→ 所在高度,M112) | ⚠️ 仅定位,无完整字段(见差距) |
| 账户页 | 余额/nonce/统计 | `/account/{id}`、`/accounts` | ✅(无交易历史,见差距) |
| 验证人 | 集合 + 权重 | `/validators`、`/validator/{id}` | ✅ |
| 质押/解绑 | bonds/unbonding | `/bonds`、`/unbonding`、`/stake_ops` | ✅ |
| 治理 | 提案 + 票数 | `/proposals` | ✅ |
| 跨链桥 | 锁定/证明 | `/bridge/locks`、`/bridge/lock/{id}/proof` | ✅ |
| 认知图谱(PoK 特色) | 节点/kNN/范围/时序 diff | `/graph`、`/graph/{id}`、kNN/range/diff | ✅ |
| cNFT(贡献证书) | 列表/详情 | `/cnfts`、`/cnft/{id}` | ✅ |
| SPV 校验 | 账户/图/验证人包含证明 | `/*/proof`、`/leaf/*`、`/batch` | ✅(浏览器可**自证**读到的数据被 cert 签名头背书) |
| 罚没事件 | 双签证据 | `/evidence` | ✅ |
| 监控 | 指标 | `/metrics`(另端口) | ✅ |

**覆盖面:链状态的几乎所有维度都有只读端点,且 PoK 特色(图谱/cNFT/贡献)也已暴露。**

## 3. 差距(搭浏览器前要补的)

1. **CORS 缺失**(`daemon.rs` 零 `Access-Control-Allow-*`)。纯浏览器前端跨源直连 RPC 会被浏览器
   拦。→ **M223 已修**:RPC 读面统一回 `Access-Control-Allow-Origin: *`(读面本就公开;写面仍服务端
   bearer 把关,CORS 不绕过鉴权)。纯静态前端现可直连;生产若要缓存/限流仍可选加反向代理。
2. **无完整交易详情端点** → **M223 已补** `GET /tx/{hash}/detail`:返回 author/domain/stake/fee/
   nonce/review 数/replication 数/timestamp/embedding 维度 + 所在高度(text 与 JSON);未知哈希 404。
   原始 embedding 向量与签名不在内(前者重、后者经 SPV 自证)。
3. **无按账户的交易历史**:无法列「某账户的历史交易」而不全链扫描。浏览器要做地址页历史需:
   ① 节点侧加作者→tx 索引(改动较大,涉持久索引);或 ② 浏览器后端自建索引(扫块入库,explorer
   常规做法)。
4. **无实时推送**(无 WebSocket/SSE):浏览器需**轮询** `/height` 增量拉新块。对低频出块(默认 1s)
   可接受。
5. **纯前端即可,但生产浏览器通常要一个索引后端**:上面 #3/#2 的完整体验(地址历史、全文搜索、
   统计图表)标准做法是一个 **indexer**(订阅/轮询节点 → 入库 Postgres → 自有 API + 前端),而非
   前端直打节点 RPC。节点 RPC 足以支撑一个**轻量**浏览器(区块/交易定位/账户/验证人/治理/图谱),
   但「全功能」浏览器建议走 indexer。

## 4. 建议(分层)

- **L0 诊断面(已可用,零开发)**:`node rpc --config F --path /...` 或 `curl` 直接读 JSON;运维/开发
  即刻可用,无需浏览器。
- **L1 轻量浏览器(小)**:静态单页前端(任意框架)**直连节点 RPC**(M223 起 CORS 已开,无需代理)。
  覆盖 §2 全部「✅」端点 + `/tx/{hash}/detail`:链概览、区块/头列表与详情、交易详情、账户/验证人/
  治理/桥/图谱/cNFT 查询、SPV 自证。**不**含地址交易历史(需 indexer)。性价比最高,先做它。
- **L2 全功能浏览器(大,独立项目)**:indexer(轮询节点 → DB)+ 自有 API + 前端。补地址历史、全文
  搜索、统计/图表、完整 tx 详情。属独立产品工程,不在本链节点仓范畴。
- **节点侧可选小改**(让 L1 更顺):~~① RPC 读面加 CORS 头;② `/tx/{hash}/detail`~~ **均已在 M223 完成**。
  后续可选:`/tx/{hash}/detail` 之外的 indexer 支撑(地址历史),属 L2。

## 5. 结论

- **浏览器现在能做吗?** 能——节点只读 RPC + JSON 已足以支撑一个**轻量区块浏览器**(L1);**M223 起**
  已加 CORS + `/tx/{hash}/detail`,纯静态前端可直连节点、无需代理,剩余工作在前端本身。
- **全功能浏览器**(地址历史/搜索/图表)需一个独立 **indexer** 后端(L2),属独立项目(差距 #3 地址
  交易历史、全文搜索仍需索引层)。
- 这些都与「是否可生产部署」解耦:浏览器是**观测/生态**件,不影响共识安全;可在链上线前后并行推进。
