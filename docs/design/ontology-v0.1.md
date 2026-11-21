# 知行图知识互操作词表 v0.1(JSON-LD)

> 状态:v0.1,已实现并上线(浏览器服务);接替白皮书附录 B.6 的早期草案。
> 词表命名空间:`https://blockchain.yeying.pub/ns/v1#`(前缀 `kdg:`,KnowDoGraph)
> 上下文:<https://blockchain.yeying.pub/ns/v1/context.jsonld>
> 代码:`tools/explorer/ontology.js`(转换)、`tools/explorer/public/ns/v1/`(词表与概念方案)、
> `tools/explorer/test/ontology.test.mjs`(测试)

## 1. 目的

知行图链上记录的是**知识单元**:谁写的、依据什么、谁核验过、可信度多少、后来有没有被取代。
这些事实要能被链以外的系统直接使用——检索系统、知识库、大模型训练与评测、其他组织的知识图谱。
本词表规定这些事实的公开表达方式:

- **能被通用工具读懂**:输出是 JSON-LD 1.1,任何 JSON-LD 处理器都能展开成 RDF,用 SPARQL 查询;
- **尽量不造新词**:能用 schema.org、W3C PROV-O、Dublin Core、SKOS 表达的都直接复用,只为它们没有的概念新造词
  (`kdg:` 下共 3 个类、约 20 个属性);
- **每个结论都能追到链上**:标识就是链上的编号,数值都能按链上记录重算。

不在本词表范围:链本身的共识、账户余额、质押与治理。

## 2. 标识(IRI)

全部以 `https://blockchain.yeying.pub/id` 开头。测试网的知识迁移到主网时(见 VISION §4d)标识不变。

| 对象 | IRI | 例 |
|---|---|---|
| 知识单元 | `/id/unit/{单元号}` | `https://blockchain.yeying.pub/id/unit/8` |
| 单元的某个版本 | `/id/unit/{单元号}/v/{版本号}` | `…/id/unit/8/v/2` |
| 成员 | `/id/member/{账户号}` | `…/id/member/2` |
| 领域 | `/id/domain/{领域号}` | `…/id/domain/1` |
| 核验 | `/id/verification/{单元号}/{版本号}/{核验人账户号}` | `…/id/verification/8/2/3` |
| 模型 | `/id/model/{模型标识}` | `…/id/model/claude-opus-5-5` |

标识可以直接打开:请求头带 `Accept: application/ld+json` 时返回 JSON-LD;浏览器访问则跳到单元页。

## 3. 词表

| 链上概念 | 表达 | 复用的标准 |
|---|---|---|
| 知识单元 | `kdg:KnowledgeUnit`(⊑ `schema:CreativeWork`) | schema.org |
| 版本 | `kdg:UnitVersion`(⊑ `prov:Entity`);`dcterms:isVersionOf` 指回单元;`schema:version` 版本号 | PROV-O、Dublin Core |
| 层级 | `kdg:layer` → 概念方案 `kdg:layers`(基础 / 解释 / 场景 / 应用) | SKOS |
| 领域 | `kdg:domain` → 概念方案 `/id/domain`(中英文名) | SKOS |
| 生命周期 | `kdg:lifecycle` → 概念方案 `kdg:lifecycleStates`(活跃 / 休眠 / 竞争 / 已取代;由链上关系派生) | SKOS |
| 作者 / 发布人 | `dcterms:creator`(单元)、`prov:wasAttributedTo`(版本) → 成员 `prov:Person` | Dublin Core、PROV-O |
| 正文 | `schema:encoding` → `schema:MediaObject`:`schema:inLanguage`、`schema:sha256`、`schema:contentUrl`、`schema:encodingFormat text/markdown` | schema.org |
| 来源 | `kdg:sourceType`:`kdg:HumanAuthored` / `kdg:ModelGenerated` / `kdg:ModelGeneratedHumanVerified`;模型生成的版本另有 `prov:wasGeneratedBy` → `prov:Activity`,`prov:wasAssociatedWith` → 模型 `prov:SoftwareAgent`(`schema:name` = 模型标识) | PROV-O |
| 引用:依据 | `kdg:basedOn`(⊑ `dcterms:references`):可信度沿它传递 | Dublin Core |
| 引用:参见 | `rdfs:seeAlso` | RDFS |
| 引用:取代 | `dcterms:replaces` | Dublin Core |
| 核验 | `kdg:Verification`(⊑ `prov:Activity`、`schema:Review`):`prov:wasAssociatedWith` 核验人、`prov:used` 被核验的版本、`kdg:verdict`(`kdg:Pass` / `kdg:NeedsRevision`)、`kdg:atHeight`、`kdg:noteHash` | PROV-O、schema.org |
| 可信度 | `kdg:selfCredibility`、`kdg:effectiveCredibility`(0–1)、`kdg:bottleneck`(拉低有效可信度的依据版本) | — |
| 成员 | `prov:Person`:`kdg:accountNumber`、`kdg:pubkey`(ed25519 公钥) | PROV-O |
| 许可 | `dcterms:license` → <https://creativecommons.org/licenses/by/4.0/> | Dublin Core |

引用一律指向**被引单元的具体版本**:依据锁定版本,被引单元以后修订,引用不会悄悄改变含义。

完整的类与属性定义(中英文说明)见 [`ns/v1/vocab.jsonld`](../../tools/explorer/public/ns/v1/vocab.jsonld)。

## 4. 例:一个单元

```json
{
  "@context": "https://blockchain.yeying.pub/ns/v1/context.jsonld",
  "id": "https://blockchain.yeying.pub/id/unit/2",
  "type": "KnowledgeUnit",
  "layer": "kdg:Foundation",
  "domain": "https://blockchain.yeying.pub/id/domain/2",
  "author": "https://blockchain.yeying.pub/id/member/1",
  "createdAtHeight": 5,
  "license": "https://creativecommons.org/licenses/by/4.0/",
  "lifecycle": "kdg:Active",
  "latestVersion": "https://blockchain.yeying.pub/id/unit/2/v/1",
  "versions": [{
    "id": "https://blockchain.yeying.pub/id/unit/2/v/1",
    "type": "UnitVersion",
    "isVersionOf": "https://blockchain.yeying.pub/id/unit/2",
    "versionNumber": 1,
    "publisher": "https://blockchain.yeying.pub/id/member/1",
    "publishedAtHeight": 5,
    "sourceType": "model",
    "generatedBy": {
      "id": "https://blockchain.yeying.pub/id/unit/2/v/1#generation",
      "type": "Generation",
      "associatedWith": { "id": "https://blockchain.yeying.pub/id/model/claude-opus-5-5", "type": "Model", "name": "claude-opus-5-5" }
    },
    "bodies": [{
      "type": "Body", "inLanguage": "zh", "encodingFormat": "text/markdown",
      "sha256": "56ac8191bb56d2017de534c455f1929bd2e175d5dcab97c85664af51d293cd8a",
      "contentUrl": "https://blockchain.yeying.pub/bodies/56ac8191bb56d2017de534c455f1929bd2e175d5dcab97c85664af51d293cd8a"
    }],
    "seeAlso": ["https://blockchain.yeying.pub/id/unit/1/v/1"],
    "verifications": [],
    "selfCredibility": 0.1,
    "effectiveCredibility": 0.1
  }]
}
```

## 5. 接口

| 接口 | 内容 |
|---|---|
| `GET /export/units?format=jsonld` | 全部单元(一个 `@graph`),含全部版本、来源、引用、核验、可信度与**正文全文**,以及一个 `schema:Dataset` 节点写明许可 |
| `GET /unit/{id}.jsonld` | 单个单元(不含正文全文,正文按 `contentUrl` 取) |
| `GET /id/unit/{id}[/v/{n}]`、`/id/member/{id}`、`/id/domain[/{n}]` | 按标识取(见 §2) |
| `GET /ns/v1/context.jsonld`、`vocab.jsonld`、`domains.jsonld`、`layers.jsonld`、`lifecycle.jsonld` | 上下文、词表、概念方案 |
| `GET /lookup?body=<sha256>` | 正文属于哪个单元的哪些版本(同一份正文只能属于一个单元) |
| `GET /lookup?pubkey=<hex>` | 公钥对应的成员(账户号、是否已过见习期) |

原来的 `GET /export/units`(JSONL)保持兼容,另外补上了每个版本的来源与核验(`version_history`),
正文语言也改为取自链上记录(以前一律写成 `zh`)。

## 6. 版本与兼容

- **只增不改**:v1 内只增加词、不改已有词的含义;已有标识永不改变。
- 需要改变含义时发布 v2(新的命名空间 `…/ns/v2#`),v1 继续可用。
- 领域只在 `domains.jsonld` 定义:网页与导出都读它。增加领域 = 改创世参数 + 在这里加一个概念。

## 7. 校验

自动测试(CI 门禁):`node --test tools/explorer/test/ontology.test.mjs`——转换结果、每个词都在上下文里定义、
概念方案覆盖链上全部取值、导出与查询接口。

用标准处理器的完整校验(上线前做过一次,结果:6 份文档共展开约 1800 条三元组,无未定义词、无相对 IRI,
SPARQL 能查出每条核验记录、每个版本的来源与模型、依据关系,并跨文档关联到领域名称):

```sh
python3 -m venv /tmp/kdg && /tmp/kdg/bin/pip install pyld rdflib
# 把上下文 URL 映射到本地文件后:jsonld.expand → to_rdf → rdflib.Graph → SPARQL
```

## 7b. 密钥派生与换密钥

知行图账户的密钥是 ed25519,助记词按 SLIP-0010 ed25519 派生,路径 `m/44'/1105'/account'/0'/0'`
(1105 = KnowDoGraph 在 SLIP-0044 的编号,satoshilabs/slips#2102)。`node keygen` 与夜莺钱包
「创建钱包 → 知行图钱包」「导入钱包 → 知行图 → 助记词」都用这一条规则。

两边的实现必须逐字节一致:链上有一条测试用钱包(JS)编码并签名的知识 op,链(Rust)解码并验签通过。

**换密钥**(`POST /rpc/key-rotation`):账户用**当前**密钥签名,把账户的公钥换成新的。
账户编号、成员身份、见习期与全部历史都不变;旧密钥立刻失效。成员换设备或换备份方式时用,
步骤见 [`testnet/MEMBERS.md`](../../testnet/MEMBERS.md)。

对互操作的影响:`kdg:pubkey` 指向的是**当前**公钥;换密钥后同一 IRI(`/id/member/2`)指向的公钥会变。
词表本身不用改。

## 8. 从 Knowledge 发布到知行图(映射)

Knowledge(社区的知识生产工作台)里的条目可以「上链到知行图」。签名在浏览器里由夜莺钱包完成,
Knowledge 不接触私钥;正文放进知行图的正文仓库(链协议要求正文按哈希可取),Knowledge 只记哈希与单元号。

| Knowledge | 知行图 |
|---|---|
| 条目第一次上链 | `create`:新单元 |
| 条目的后续修订上链 | `publish`:同一单元的新版本 |
| 修订的 `title` + `statement` + `structured_payload_json`(按 fact / rule / procedure / faq / reference 渲染) | Markdown 正文(第一行为标题),`schema:sha256` 为其哈希 |
| 修订的语言(新增字段,默认 zh) | `schema:inLanguage` |
| 条目的领域、层级(新增字段) | `kdg:domain`、`kdg:layer` |
| 条目之间的引用(新增:依据 / 参见 / 取代) | `kdg:basedOn` / `rdfs:seeAlso` / `dcterms:replaces`,指向被引条目**已上链的**最新版本 |
| 人工创建的条目 | `kdg:HumanAuthored` |
| 由模型生成、未经修改(修订记有模型标识) | `kdg:ModelGenerated` + 模型 |
| 由模型生成、经人工修改 | `kdg:ModelGeneratedHumanVerified` + 模型 |
| 上链人的钱包里的知行图账户 | 发布人(必须是测试网成员) |

**不映射核验**:Knowledge 里的"评审人"目前就是作者本人,而链上的核验必须由其他成员做,两者含义不同。
条目上链后,由其他成员在知行图上核验。
