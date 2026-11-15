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
- **Knowledge**(默认首页):知识单元列表(层级、核验通过数、自身 / 有效可信度、"被谁卡住");点进单元看全部版本、
  正文哈希、引用(依据 / 参见,可点击跳转)、核验记录、过时引用提示。
- **Account**(查询 + 列表)、**Validators**、**Staking**(活跃质押 + 解绑队列)、**Governance 提案**、**cNFTs**、**Supply**。

## 边界(L1)

- **只读**(不提交 tx/治理)。
- **无地址交易历史 / 全文搜索 / 图表** —— 需 indexer 后端(L2,独立项目)。
- 实时靠轮询 `/height`,无 WebSocket。
- 字段名对齐节点 `daemon.rs` 的 `json_*` 渲染器;列表走 `{total,cursor,items}` 分页信封;
  u64 为无损引号串原样显示。

## 成员功能:在浏览器里核验与邀请

`public/signer.js` + `public/member.js` 让成员不装任何程序,在浏览器里持有密钥、核验知识单元、邀请新成员。

- **私钥只在本机**:助记词(BIP-39)→ 种子 → ed25519 密钥,全部用浏览器内置的 WebCrypto 完成;
  种子用成员口令加密(PBKDF2-SHA256 60 万次 → AES-256-GCM)后存在 `localStorage`,解锁后的签名密钥只在内存里。
- **与节点逐字节一致**:`node --test tools/explorer/test/signer.test.mjs` 用同一助记词对照 Rust 程序
  (`node keygen` / `node encode-knowledge-op`)的公钥与签名后的 op,CI 中为必过项。
- **导入**:助记词,或 `node keygen --encrypt` 写的密钥文件(Web3 Secret Storage v3:scrypt + AES-128-CTR + Keccak-256 MAC;
  scrypt 与 Keccak 为纯 JS 实现,对照 RFC 7914 / 官方向量与 Rust 程序生成的文件检验)。
- **支持**:核验(通过 / 需修改,"通过"须先勾选单元的全部核验要点)、邀请、**写新单元与修订**(`public/author.js`:
  Markdown 编辑与预览 → 计算 SHA-256 → 上传到正文仓库 → 签名 create / publish)。批量导入目录仍用 `tools/kimport.py`。
- 本地开发链(`./cmd up`)的写入令牌是 `local-dev-token`。
- 提交需要写入令牌(组织者私下提供),保存在本机 `localStorage`。
- 需要较新的浏览器(WebCrypto Ed25519:Chrome / Edge 137+、Firefox 129+、Safari 17+)与 HTTPS(或 localhost)。
