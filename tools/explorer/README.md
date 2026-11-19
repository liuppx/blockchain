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
    util.js           # 十六进制、写入令牌、格式检查、旧密钥清理(无任何私钥代码)
    member.js         # 成员功能:用夜莺钱包登录、邀请、核验
    author.js         # 写作与修订
  reference/
    signer.js         # op 编码与签名的参考实现(只给测试用;不被服务、不部署)
    bip39-en.js       # BIP-39 词表(同上)
  test/               # 契约测试与浏览器端到端测试
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

## 成员功能:用夜莺钱包核验、邀请、写作

`public/member.js` 让成员用[夜莺钱包](https://wallet.yeying.pub/)登录后核验知识单元、邀请新成员;
`public/author.js` 负责写新单元与修订(Markdown 编辑与预览 → 计算 SHA-256 → 上传到正文仓库 → 请钱包签名 create / publish)。

- **网页不持有私钥**:密钥在钱包里;网页只递"意图",钱包编码、显示摘要、确认后签名(接口见下一节)。
- **旧的浏览器本地身份已下线**:以前网页可以自己生成/导入助记词、把种子用口令加密存在 `localStorage`。
  这条路已移除——同一个身份不应同时存在于浏览器和钱包两处。浏览器里留有旧密钥的成员,页面会提示把它移进钱包
  (在钱包里导入同一组助记词或同一个密钥文件,就是同一个链上账户),并提供「清除本机旧密钥」。
- **网页里没有任何私钥代码**:`public/` 里不生成助记词、不派生密钥、不签名、不加解密;页面只加载 `public/util.js`
  (十六进制转换、写入令牌、提交前的格式检查、旧版本机密钥的检测与删除)。`test/wallet.test.mjs` 会扫描 `public/`,
  一旦出现这类代码 CI 就失败。
- **参考签名器在 `reference/`**:`reference/signer.js`(连同 `bip39-en.js`)是 op 编码与签名的 JavaScript 参考实现,
  **不被服务、不部署**(`deploy/deploy-explorer.sh` 排除它,并检查线上 `/signer.js` 返回 404)。`test/signer.test.mjs`
  用它对照 Rust 程序逐字节检验,`test/fake-wallet.mjs` 用它扮演钱包。
- **支持**:核验(通过 / 需修改,"通过"须先勾选单元的全部核验要点)、邀请、写新单元与修订。批量导入目录仍用 `tools/kimport.py`。
- 提交需要写入令牌(组织者私下提供),保存在本机 `localStorage`。本地开发链(`./cmd up`)的写入令牌是 `local-dev-token`。

## 用夜莺钱包登录

[夜莺钱包](https://wallet.yeying.pub/)原生支持知行图:知行图账户就是钱包里的普通账户(「账户管理 → 导入钱包」里网络选「知行图」,用助记词 / `node keygen` 的密钥文件导入——
派生与 `node keygen` 相同,导入后就是成员原来那个链上账户),网页只拿公钥、只递"意图",钱包自己编码、
在弹窗里给用户看可读摘要、确认后签名。

检测到钱包时,顶栏出现 **「用夜莺钱包登录」**;「我的身份」页也有,
并明确显示"✓ 已检测到夜莺钱包"或"✗ 没有检测到"。

**接口**(EIP-1193 `provider.request`,钱包侧实现见 wallet 仓库 `js/background/request-router.js`):

| 方法 | 参数 | 返回 | 说明 |
|---|---|---|---|
| `zhixing_accounts` | — | `[公钥]` 或 `[]` | 本站已授权时返回公钥;不弹窗 |
| `zhixing_requestAccounts` | — | `[公钥]` | 首次弹窗请用户确认连接;按网站单独授权 |
| `zhixing_signKnowledgeOp` | `[{ actor, nonce, action }]` | `{ signed, address }` | `action` 的字段见 `reference/signer.js` 的 `encodeOp`(网页用 `util.js` 的 `checkAction` 预检同样的格式);`signed` 是 op ‖ 签名的十六进制,直接 POST `/rpc/knowledge` |

错误码:`4001` 用户拒绝;`4100` 本站未连接;`4900` 钱包里还没有知行图密钥;旧版钱包不认识这些方法(页面提示升级)。

**为什么只递意图**:如果网页把待签字节交给钱包,弹窗里显示的内容就不一定是实际签的内容。钱包自己从意图编码,
显示与签名必然一致。钱包的编码由 wallet 仓库 `tests/zhixing-op.test.mjs` 对照本仓库 `reference/signer.js` 的输出逐字节检验,
`reference/signer.js` 又由 `test/signer.test.mjs` 对照 Rust 节点检验——三方一致。

(早先曾用以太坊 `personal_sign` 的签名派生成员密钥;钱包原生支持后已移除,避免同一个钱包产生两把不同的成员密钥。)

检验:`node --test tools/explorer/test/wallet.test.mjs`(接口约定,CI 门禁)与
`node --test tools/explorer/test/wallet.e2e.mjs`(注入实现同样接口的模拟钱包:登录、本机留有旧版密钥时提示移进钱包、
没装钱包 / 钱包里无密钥 / 旧版钱包的提示、登录 → 邀请 → 核验入块;需要本地链)。
