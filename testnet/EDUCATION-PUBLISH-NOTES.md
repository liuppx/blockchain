# 教育领域首批单元 · 发布说明

> 给 **jack**(账户 6)——教育领域的第一批 10 个知识单元由你发布。
> 其他核验人(alice、rose、tom)按 [`REVIEWER-GUIDE-EDUCATION.md`](REVIEWER-GUIDE-EDUCATION.md) 核验。

## 1. 领域什么时候能用

教育是**领域编号 3**,按高度激活:高度 **1260000** 起可用(社区 main `c72f20b` 部署的节点,
时刻表 `domain-3@1260000`)。在那之前,节点会以 422 拒绝领域 3 的单元。

确认链上已经接受:

```sh
curl -s -H 'Accept: application/json' https://blockchain.yeying.pub/rpc/domains
```

`"next"` 里有 `"3"` 就说明可以发布了。没有的话等一会儿再试。

## 2. 单元在哪

`docs/pilot/education/`(在区块链仓库里,`git clone` 下来或直接取社区 main 的源码):

| 文件 | 单元 | 层级 |
|---|---|---|
| `base-retrieval-practice.md` | 提取练习:合上书回忆比重读更有效 | 基础 |
| `base-transfer-cues.md` | 迁移需要线索:旧知识不会自动用上 | 基础 |
| `base-cognitive-load.md` | 认知负荷:一次能处理的新信息有限 | 基础 |
| `base-spacing-effect.md` | 间隔重复:把复习分散开 | 基础 |
| `base-interleaving.md` | 交错练习:把不同类型的问题混着练 | 基础 |
| `base-goal-self-regulation.md` | 目标与自我调节:把"多学点"变成能检查的目标 | 基础 |
| `exp-why-failed-retrieval-helps.md` | 为什么会"记住了又忘":提取失败不是白费 | 解释 |
| `exp-why-understanding-isnt-ability.md` | 为什么"看懂了"却做不出题 | 解释 |
| `scn-exam-prep-schedule.md` | 备考:把复习摊到几个月,而不是考前一周 | 场景 |
| `app-self-teaching-a-skill.md` | 自学一门新技能:从"看教程"到"能独立做出来" | 应用 |

## 3. 怎么发布

发布需要**你的私钥**——它只在你的钱包/机器上,组织者和你都拿不到对方的私钥。所以这一步由你做。

先拿写入令牌:组织者私下给你(同其他领域一样,不要发在群里)。

然后:

```sh
# 1. 取仓库(只要 tools/ 与 docs/pilot/education/ 两处)
git clone https://github.com/yeying-community/blockchain.git
cd blockchain

# 2. 准备你的密钥文件
#    钱包导出的知行图账户密钥,或 node keygen --encrypt --out zhixing-member.json 生成的
#    (助记词按 SLIP-0010 派生,与钱包里的账户是同一个)

# 3. 导入(会自动上传正文、取序号、等入块)
export ZHIXING_RPC_TOKEN=<组织者给你的写入令牌>
python3 tools/kimport.py \
  --rpc https://blockchain.yeying.pub/rpc \
  --bodies https://blockchain.yeying.pub/bodies \
  --key 6=zhixing-member.json \
  --dir docs/pilot/education \
  --domain 3
```

输出里会看到每个单元的 `created unit N`,以及几个单元的 `published unit N v2`
——那是导入工具在补"参见"引用:链上一个版本只能引用已存在的版本,
所以指向还没建的单元的"参见"要等全部建完再发 v2 补上。这是正常的。

## 4. 导入之后

1. **在浏览器上看一眼**:打开 `https://blockchain.yeying.pub/#unit/N`,确认正文、层级、领域都对;
2. **核验要点要留好**:每个单元末尾的「核验要点」是给核验人的检查清单,
   页面上核验时要逐条勾选才能点「通过」。这 10 个单元的要点已经列好了,不用另外准备;
3. **告诉 alice、rose、tom 可以核验了**,并把单元链接发给他们。

## 5. 两件要提醒你的事

**这些单元全部是模型生成的,还没有人核验过。** 页面上它们显示"待核验"。
你发布它们不代表你认可其内容——发布者不能核验自己发布的版本,所以这批单元的核验
由另外三位做。这一点在链上是自动的。

**第一批草稿的出处做过一轮核查,查出 7 条不存在的文献,已经删掉或改正。**
每一处改动都写在对应单元的「核验要点」里(改了什么、为什么、删掉的那条原来想支撑什么)。
核验人核验时要**按新写的出处重新核对**,不要只看结论。
详细情况见 [`docs/pilot/education/README.md`](../docs/pilot/education/README.md) 开头一节。

## 6. 出问题找谁

- 导入报 422:多半是领域 3 还没激活(见 §1),或者 `--domain` 写错了;
- 导入报签名相关错误:密钥文件与账户 6 的公钥对不上(见 `testnet/members.toml` 里 jack 的公钥);
- 其他:找组织者。
