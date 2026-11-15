# 大模型应用开发 · 试运行草稿

这里是用 [`llm-knowledge`](../../../.claude/skills/llm-knowledge/SKILL.md) 技能起草的知识单元草稿,
面向**用 AI、但想真正理解 AI 的开发者**(VISION.md §4c)。

> ⚠️ **全部为模型生成、待核验。** 不代表知行图的已核验知识。
> 每个单元末尾的"核验要点"列出了需要懂行的人逐条确认的地方。
> 这个领域有一个特殊的陷阱:**模型在解释模型自己**。请特别留意把流行说法当成事实、把某一家产品的行为当成通用规律的地方。

| 单元 | 层级 | 状态 |
|---|---|---|
| [词元(token)](base-token.md) | 基础 | 模型生成 · 待核验 |
| [预测下一个词元](base-next-token-prediction.md) | 基础 | 模型生成 · 待核验 |
| [上下文窗口](base-context-window.md) | 基础 | 模型生成 · 待核验 |
| [采样与温度](base-sampling-temperature.md) | 基础 | 模型生成 · 待核验 |
| [模型本身不保存对话](base-stateless.md) | 基础 | 模型生成 · 待核验 |
| [嵌入(embedding)](base-embedding.md) | 基础 | 模型生成 · 待核验 |
| [为什么聊久了,模型会"忘记"前面说过的话](exp-why-model-forgets.md) | 解释 | 模型生成 · 待核验 |
| [资料太多放不进上下文:检索增强,还是长上下文](scn-rag-or-long-context.md) | 场景 | 模型生成 · 待核验 |
| [按词元预算管理对话历史](app-conversation-history-budget.md) | 应用 | 模型生成 · 待核验 |

**四层各有一例**,串成一条线:词元 / 上下文窗口 / 不保存对话(基础)→ 为什么会"忘记"(解释)
→ 检索增强还是长上下文(场景)→ 按词元预算管理历史(应用)。

**图谱与可信度**见 [`GRAPH.md`](GRAPH.md),由 `python3 tools/kgraph.py docs/pilot/llm` 生成。

## 每个单元都有"自己验证"

这个领域的对错大多能动手检验。每个单元都给出读者可以自己做的验证;起草时已实际运行的:

| 单元 | 验证 | 起草时是否运行 |
|---|---|---|
| 词元 | tiktoken 分词演示 | ✅ 已运行,结果写入单元 |
| 采样与温度 | 标准库 softmax 演示 | ✅ 已运行,与"预期"一致 |
| 按词元预算管理对话历史 | 标准库实现 + 自检 | ✅ 已运行,与"预期输出"一致 |
| 预测下一个词元 | GPT-2 本地演示 | ❌ 未运行(需下载约 500 MB 模型) |
| 其余单元 | 需要调用模型接口的对比实验 | ❌ 需审查者用自己的接口运行 |

## 建议核验顺序

先核验被依据最多的基础单元:**词元**(6 个单元直接或间接依据它)→ **上下文窗口** → **不保存对话**,
再核验上层单元。应用单元的有效可信度不会超过它所依据的基础单元。

## 导入测试网

领域编号 1(大模型应用开发):

```sh
export ZHIXING_RPC_TOKEN=...   # 组织者私下提供
python3 tools/kimport.py --dir docs/pilot/llm --domain 1 \
  --rpc https://blockchain.yeying.pub/rpc --bodies https://blockchain.yeying.pub/bodies \
  --key 你的编号=你的密钥文件.json
```
