# 加入知行图测试网 · 成员指南

> 这份说明可以直接转发给第一批成员。

## 你需要做的只有一件事:生成你自己的密钥,把公钥发给组织者

你的密钥就是你在链上的身份。**只有你自己持有它**,组织者和其他任何人都拿不到。

### 第一步:获取节点程序

从组织者处获取 `node` 程序,或者自己从源码构建:

```sh
cd node && cargo build --release      # 生成 node/target/release/node
```

### 第二步:生成密钥

```sh
node keygen --new-mnemonic --encrypt --out zhixing-member.json
```

程序会让你设置一个密码,然后打印三样东西:

| 打印的内容 | 怎么处理 |
|---|---|
| `pubkey 03d3...`(64 位十六进制) | **发给组织者**。这是你的公钥,公开也没关系 |
| `keystore zhixing-member.json` | 加密后的密钥文件。**自己保存好**,以后签名要用 |
| `mnemonic: ...`(一串英文单词) | **抄在纸上,妥善保管,不要拍照,不要发给任何人**。密钥文件丢了,可以用它恢复 |

### 第三步:把这三项发给组织者

```
称呼:张三
方向:fitness(运动健身)或 llm(大模型应用开发)
公钥:03d330c69dabf7000b34af803bd9da850da0ac808d80d10e2952e84e5b74e35e
```

**绝不要发送**:密钥文件、密码、助记词。组织者不会、也不需要向你要这些。

## 测试网开启以后

### 你能做什么

- **写知识**:创建知识单元、为已有单元发布新版本(任何成员都可以修订别人的单元,你会被记录为修订者);
- **核验**:对别人发布的版本给出"通过"或"需修改"的结论。你不能核验自己发布的版本;
- **邀请**:邀请新成员加入。新成员需要经过 **7 天见习期**,期间可以写知识,但核验不计入可信度、也不能邀请别人;
  每位成员**每天最多邀请一人**。第一批成员不需要见习期。

### 怎么提交

测试网地址:**https://blockchain.yeying.pub**(浏览器)。提交接口是 `https://blockchain.yeying.pub/rpc/knowledge`,
需要一个**写入令牌**,由组织者私下发给你(它只用来防止刷写入,不代表你的身份;你的身份由你的密钥签名证明)。

所有操作都用你的密钥文件在**你自己的电脑上**签名,然后提交:

```sh
export ZHIXING_KEYSTORE_PASSWORD='你的密码'     # 或者不设置,运行时会提示输入
export ZHIXING_RPC_TOKEN='组织者发给你的写入令牌'
RPC=https://blockchain.yeying.pub/rpc

# 查你的当前序号(nonce):
curl -s $RPC/account/你的编号

# 例:核验 5 号单元的第 1 版,结论"通过"
node encode-knowledge-op --key-file zhixing-member.json --out op.bin \
  --actor 你的编号 --nonce 你的序号 --action verify --unit 5 --version 1 --verdict pass
curl -s -X POST -H "Authorization: Bearer $ZHIXING_RPC_TOKEN" \
  -H 'Content-Type: application/octet-stream' --data-binary @op.bin $RPC/knowledge
```

每提交一条,序号加一;等它入块(约 1 秒)再签下一条。

### 正文放在哪里

链上只记录正文的 **SHA-256 哈希**;正文本身放在测试网的**正文仓库** `https://blockchain.yeying.pub/bodies/<哈希>`。
浏览器打开单元时会取回正文、在你的浏览器里重新计算哈希,一致才显示("✓ 正文与链上哈希一致")。
所以**创建单元或发布新版本时,要先上传正文**(仓库只接受哈希相符的内容,别人无法替换你的正文):

```sh
H=$(shasum -a 256 单元文件.md | cut -c1-64)   # 与 node hash-content --file 单元文件.md 相同
curl -s -X PUT -H "Authorization: Bearer $ZHIXING_RPC_TOKEN" --data-binary @单元文件.md \
  https://blockchain.yeying.pub/bodies/$H
```

批量导入一个目录下的单元文件,用 `tools/kimport.py`,它会自动上传正文、取序号、等待入块:

```sh
python3 tools/kimport.py --rpc $RPC --bodies https://blockchain.yeying.pub/bodies \
  --key 你的编号=zhixing-member.json --dir 单元目录 --domain 领域编号   # 1 大模型应用开发,2 运动健身
```

### 怎么阅读与核验

在浏览器的 Knowledge 页点开单元即可阅读正文;每个单元有固定链接,例如
`https://blockchain.yeying.pub/#unit/3`,可以直接发给别人请他核验。

### 核验时请注意

- 先读单元末尾的"核验要点",逐条确认;
- 你只对**核验当时能知道的东西**负责。如果后来出现了新证据推翻了它,那是"诚实的错",不会受罚;
- 遇到真正有争议的问题(比如学界尚无定论),不要强行判对错,可以提交一个观点不同的新单元,两者并存。

## 内容许可

你在测试网提交的所有内容都以 **CC BY 4.0** 许可开放:任何人都可以使用(包括商用),只需署名。
提交即视为同意。测试网的知识、核验记录和贡献记录将作为主网的启动数据;测试网上没有有价值的代币,
也**不承诺**任何兑换关系。
