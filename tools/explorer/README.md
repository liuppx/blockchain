# zhixing 区块浏览器(L1)

一个**独立的轻量区块浏览器项目**:静态 UI + **零依赖** Node 服务(静态托管 + RPC 反向代理)。
对应 [`docs/EXPLORER-ASSESSMENT-2026-11.md`](../../docs/EXPLORER-ASSESSMENT-2026-11.md) 的 **L1**。

## 项目结构

```
tools/explorer/
  package.json        # name/scripts(start=node server.js);无运行期依赖
  server.js           # 零依赖 Node 服务:托管 public/,并把 /rpc/* 代理到节点 RPC
  public/
    index.html        # 页面骨架
    styles.css        # 样式
    app.js            # 逻辑(vanilla JS fetch,无框架、无构建)
```

## 运行

```sh
cd tools/explorer
PORT=8080 NODE_RPC=http://127.0.0.1:9221 npm start      # 或: node server.js
# 打开 http://localhost:8080
```

- 默认 `NODE_RPC=http://127.0.0.1:9221`、`PORT=8080`,经 `/rpc` **同源代理**到节点
  (无需 CORS / 无混合内容问题)。
- 也可在页面顶部把 RPC 直接填成节点地址(如 `http://127.0.0.1:9221`),此时走节点的 CORS(M223)。
- **最省事**:仓库根运行 [`./cmd`](../../cmd) —— 一键起单节点网络 + 本浏览器,打印
  `http://localhost:8080`。

## 覆盖

- **概览**(4s 轮询):高度、head、协议版本、供应、bonded、供应守恒、genesis。
- **Blocks**:最近区块 → 区块详情(头字段 + 交易列表)→ 交易详情。
- **Tx lookup**:按内容哈希查 `/tx/{hash}/detail`。
- **Graph**:认知图谱节点(PoK 核心)——node_id / domain / dim / embedding 向量。
- **Account**(查询 + 列表)、**Validators**、**Staking**(活跃质押 + 解绑队列)、**Governance 提案**、**cNFTs**、**Supply**。

## 边界(L1)

- **只读**(不提交 tx/治理)。
- **无地址交易历史 / 全文搜索 / 图表** —— 需 indexer 后端(L2,独立项目)。
- 实时靠轮询 `/height`,无 WebSocket。
- 字段名对齐节点 `daemon.rs` 的 `json_*` 渲染器;列表走 `{total,cursor,items}` 分页信封;
  u64 为无损引号串原样显示。
