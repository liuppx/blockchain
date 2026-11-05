# zhixing 区块浏览器(L1 轻量,静态单页)

一个**零依赖**的区块浏览器:纯 `index.html`(内联 CSS + vanilla JS),直接读节点的 JSON RPC。
对应 [`docs/EXPLORER-ASSESSMENT-2026-11.md`](../../docs/EXPLORER-ASSESSMENT-2026-11.md) 的 **L1**。

## 用法

1. 节点开启 RPC 读面:`[rpc] enabled = true`(testnet 默认在 `92<id>`,如 n21 → `127.0.0.1:9221`)。
   读端点公开(GET 无需 token);跨源读依赖节点的 CORS(M223 起默认开)。
2. 打开浏览器:
   - 直接 `file://` 打开 `tools/explorer/index.html`,或
   - 静态托管:`python3 -m http.server -d tools/explorer 8080` → `http://localhost:8080`。
3. 顶部填节点 RPC 地址(默认 `http://127.0.0.1:9221`)→ **Connect**。

> 用 [`testnet/`](../../testnet/README.md) 起本地网后,`gen.sh → up.sh`,浏览器指向
> `http://127.0.0.1:9221` 即可。

## 覆盖

- **概览**(每 4s 轮询):高度、head、协议版本、供应、bonded、供应守恒、genesis。
- **Blocks**:最近区块列表 → 区块详情(头字段 + 交易列表)→ 交易详情。
- **Tx lookup**:按内容哈希查 `GET /tx/{hash}/detail`(author/domain/stake/fee/nonce/review·
  replication 计数/timestamp/embedding 维度 + 所在高度)。
- **Account**:按 id 查 + 账户列表。
- **Validators / Governance(提案)/ cNFTs / Supply**:各列表/明细。

## 边界(L1)

- **只读**:不提交 tx/治理(写面需 bearer + CORS 预检,不在 L1 范畴)。
- **无地址交易历史 / 全文搜索 / 图表**:这些需 indexer 后端(L2,独立项目)。
- **实时**:靠轮询 `/height`,无 WebSocket。
- 认知图谱(kNN/range/diff)未在 UI 展开,但 RPC 已具备,可后续加页。

## 实现说明

- 字段名对齐节点 `daemon.rs` 的 `json_*` 渲染器;列表走 `{total,next,items:[…]}` 分页信封。
- u64 值在 JSON 里是**无损引号字符串**(避免 JS 精度丢失),UI 原样显示。
- 无构建步骤、无 npm、无框架——改一个文件即可。
