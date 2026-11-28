# zx · 命令行签名提交工具

> 在终端里签名并提交知行图的知识操作:发布、修订、核验、邀请、跟进依据。
> 私钥只在本机——工具调 `node encode-knowledge-op` 签名,口令在终端无回显输入,
> **只把签好的字节发到节点**,私钥从不离开本机、也不经过网络。

## 为什么要有它

网页版的签名在夜莺钱包里做,适合"人在浏览器前"的场景。但下面这些事在终端里更顺手:

- 一次发布一批单元(教育领域第一批就是 10 个);
- 在脚本里跑"跟进依据"这类机械操作;
- 没有图形界面、或者不想开浏览器的时候。

`zx` 不改变安全模型——它用的是同一套密钥、同一套签名格式、同一套链上规则。
**它只是把"网页表单"换成了"命令行"。**

## 用法

```sh
# 先看看这把密钥是链上哪个账户
export ZHIXING_RPC_TOKEN=<写入令牌>     # 向组织者要
./tools/zx --account 2 --key zhixing-member.json whoami

# 发布一个目录下的单元(按"依据"的拓扑序;指向未建单元的"参见"会自动补一版)
./tools/zx --account 2 --key zhixing-member.json publish docs/pilot/education --domain 3

# 创建单个单元;Markdown 的引用顺序和类型须与 --cite 一一对应
./tools/zx --account 3 --key zhixing-keystore-mary.json create docs/pilot/llm/app-agent-evaluation.md \
  --domain 1 --source model:GPT-6 --cite 16:1:see-also

# 为已有单元发布新版本
./tools/zx --account 2 --key zhixing-member.json revise 新版本.md --unit 8 --source model:claude-opus-5-5

# 核验(判"需修改"必须写说明)
./tools/zx --account 2 --key zhixing-member.json verify --unit 8 --version 1 --needs-revision --note 说明.txt

# 邀请新成员
./tools/zx --account 1 --key zhixing-member.json invite --pubkey <64位公钥>

# 列出依据已过时的单元;带 --unit 是跟进(发布新版本,让依据指向当前版本)
./tools/zx --account 2 --key zhixing-member.json follow
./tools/zx --account 2 --key zhixing-member.json follow --unit 8
```

## 每次签名前都确认

链上记录不可撤销,所以 `zx` 在签名前**打印要签的内容并等回车确认**:

```
  提取练习:合上书回忆比重读更有效
    层级 基础 · 正文 8320 字节 · SHA-256 bf1e4edb52a314b9…
    引用: #base-spacing-effect v1 参见

──────
共 10 个单元,作者为账户 6
──────
以上内容将签名并永久写入链上。确认发布?输入 yes 回车:
```

批量发布时**一次确认整批清单**,而不是一个一个问——但清单上每个单元的正文哈希、
层级、引用都在里面。输入别的内容就取消,**没有签任何东西**。

## 密钥

`--key` 接受两种:

| 形式 | 说明 |
|---|---|
| 加密 keystore(Web3 Secret Storage v3) | **推荐**。口令在终端无回显输入,或用 `ZHIXING_KEYSTORE_PASSWORD`(无人值守时) |
| 明文种子(64 位十六进制,可带 `0x` 前缀) | 本地开发用。生产环境别用 |

两种形式的来源:

- **从钱包导出**:夜莺钱包的知行图首页有「导出密钥文件」,产出的文件与
  `node keygen --encrypt` 同格式,可直接给 `--key`。口令是**那个文件自己的**,
  与钱包密码无关——钱包不知道链侧的口令,链侧也不知道钱包密码;
- **自己生成**:`ZHIXING_KEYSTORE_PASSWORD=... node keygen --encrypt --out zhixing.json`。

密钥文件不要进版本库、不要发给别人。成员自己生成密钥、只把公钥给组织者——
这是链的设计意图,`zx` 也不改变它。

> 钱包存私钥用的是 ethers 的 `0x` 前缀惯例,而节点原来不认这个前缀(会当成加密
> keystore 然后报一个和真实原因无关的"读不到口令")。链侧已修:带不带 `0x` 都认。

## 与浏览器的关系

两者可以混用:网页上写的单元,命令行核验;命令行发布的单元,网页上读。
**发布者不能核验自己发布的版本**(链上规则,`zx` 在签名前会先查并拒绝)。

`zx` 不做的事:
- 不碰钱包(那是浏览器路径);
- 不替你判断"这个版本该不该核验通过"——那是人的判断,核验要点在单元末尾;
- 不自动跟进依据——跟进是**一个判断**:被依据单元的新版本可能已经改变了你的单元赖以成立的内容,
  所以 `zx follow` 默认只列出,带 `--unit` 才发布。

## 与其他工具的关系

| 工具 | 干什么 | 什么时候用 |
|---|---|---|
| `tools/zx` | 签名提交(发布/修订/核验/邀请/跟进) | 日常操作 |
| `tools/kimport.py` | 批量导入一个目录(早期的做法) | 已被 `zx publish` 取代 |
| `tools/kfollow.py` | 只做跟进依据 | 已被 `zx follow` 取代 |
| `tools/pilot_plant.py` | 试运行埋错 | 试运行专用 |
| `tools/fill-zhixing.py` | 在 Edge 里代填网页表单 | 想用网页的写作体验时 |
| `tools/fix_source.py` | 把链上单元的模型标识改成统一写法 | 早期单元的标识写法不一致时 |

`zx create` 用于单个新单元。Markdown 中每条引用都须用命令行参数映射为链上单元编号、版本和类型。工具会检查引用版本,展示正文哈希与操作摘要,再等待确认。它能保留对目录外既有单元的引用。

`tools/fix_source.py` 修的是**来源字段里的模型标识**:链上原样存这个字符串、不做归一化,
所以「同一个模型」必须处处得到同一个标识(规则在 `tools/kgraph.py` 的 `model_id()`)。
早期用网页表单发布的单元写成了 `model:GPT-6`,而命令行产出的是 `model:gpt-6`——
按来源统计时一个模型会被拆成两个。链上记录不可改,所以只能**发新版本**:正文哈希
原样带上(内容一个字不改),引用原样带上,只把 source 换掉。

```bash
# 先看要改什么(不签名、不提交)
python3 tools/fix_source.py --account 3 --key zhixing-keystore-mary.json --dry-run
# 真的改
python3 tools/fix_source.py --account 3 --key zhixing-keystore-mary.json
```

`zx` 与 `kimport.py` 用**同一套单元解析规则**(`tools/kgraph.py`),所以对同一个目录读出的
单元、引用、层级完全一致。

## 实现要点

- 纯 Python 标准库,不依赖第三方包;
- 签名交给 `node encode-knowledge-op`,它认明文种子也认加密 keystore;
- 发布顺序按"依据"的拓扑序("依据"成环或目标缺失直接报错,链上规则:依据无环);
- 指向尚未创建单元的"参见"要等全部建完再发一版补上——链上一个版本只能引用已存在的版本;
- 单元与正文哈希的对应关系由 `tools/test_zx.py` 守着。

## 测试

```sh
python3 tools/test_zx.py          # 不碰链的部分:解析、拓扑序、来源映射、令牌
```

链上的部分(签名与提交)在本地 4 节点链上实测过:发布(含"参见"补版)、修订、
核验(通过/需修改)、邀请、跟进依据,以及自核验被拒。
