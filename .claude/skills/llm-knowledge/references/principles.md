# 大模型应用开发核心原理清单(起草参考)

这是起草时的参考清单,**不是**已核验的知识。每条原理最终都要作为一个基础单元经人核验后才算数。
"可查证出处"列出可以去核对的公开资料;引用前应核对原文,不确定就写"出处待补"。

## 模型原理

| 原理 | 要点 | 可查证出处 |
|---|---|---|
| 词元(token) | 模型读写的单位是词元而非字或词;常用子词分词(如 BPE),同样的文字在不同模型上切出的词元数不同 | Sennrich, Haddow, Birch (2016) *Neural Machine Translation of Rare Words with Subword Units*, ACL;Kudo & Richardson (2018) *SentencePiece*, EMNLP(演示) |
| 预测下一个词元 | 生成式语言模型的基本工作:根据前文给出下一个词元的概率分布,逐个生成 | Radford et al. (2018) *Improving Language Understanding by Generative Pre-Training*;Brown et al. (2020) *Language Models are Few-Shot Learners*, NeurIPS |
| 注意力与 Transformer | 每个位置按相关程度汇总其他位置的信息;当今主流大模型的基础结构 | Vaswani et al. (2017) *Attention Is All You Need*, NeurIPS |
| 上下文窗口 | 一次推理能处理的词元总数上限(输入 + 输出);窗口之外的内容模型看不到 | 各模型官方文档;Liu et al. (2024) *Lost in the Middle*, TACL(长上下文的利用) |
| 采样与温度 | 从概率分布中选词元的方式;温度调整分布的尖锐程度 | Holtzman et al. (2020) *The Curious Case of Neural Text Degeneration*, ICLR |
| 嵌入(embedding) | 把文字映射成向量,意思相近的文字向量也相近 | Mikolov et al. (2013) *Efficient Estimation of Word Representations in Vector Space*;Reimers & Gurevych (2019) *Sentence-BERT*, EMNLP |
| 预训练与指令微调 | 先在大量文本上学预测,再用指令数据与人类反馈学会"按要求回答" | Ouyang et al. (2022) *Training language models to follow instructions with human feedback*, NeurIPS |
| 无状态 | 模型本身不保存对话;"记忆"来自应用每次把历史重新放进上下文 | 各模型官方接口文档(待补具体出处) |

## 提示与上下文

| 原理 | 要点 | 可查证出处 |
|---|---|---|
| 少样本示例 | 在提示里给几个示例,模型能照着做,无需训练 | Brown et al. (2020) |
| 逐步推理 | 让模型先写出推理步骤,在多步问题上通常更准 | Wei et al. (2022) *Chain-of-Thought Prompting Elicits Reasoning in Large Language Models*, NeurIPS |
| 长上下文的位置效应 | 关键信息放在长上下文中间时,部分模型利用得更差 | Liu et al. (2024) *Lost in the Middle* |

## 检索与知识

| 原理 | 要点 | 可查证出处 |
|---|---|---|
| 检索增强生成(RAG) | 先检索相关资料放进上下文,再让模型据此回答 | Lewis et al. (2020) *Retrieval-Augmented Generation for Knowledge-Intensive NLP Tasks*, NeurIPS |
| 幻觉 | 模型会生成流畅但不符合事实的内容 | Ji et al. (2023) *Survey of Hallucination in Natural Language Generation*, ACM Computing Surveys |

## 工具与智能体

| 原理 | 要点 | 可查证出处 |
|---|---|---|
| 工具调用 | 模型输出结构化的调用请求,由程序执行并把结果放回上下文 | Schick et al. (2023) *Toolformer*;各模型官方文档 |
| 推理与行动交替 | 思考—行动—观察的循环 | Yao et al. (2023) *ReAct*, ICLR |

## 安全

| 原理 | 要点 | 可查证出处 |
|---|---|---|
| 提示注入 | 模型无法可靠区分"指令"和"数据";不可信内容进入上下文就可能改变模型行为 | Greshake et al. (2023) *Not what you've signed up for*(间接提示注入);OWASP Top 10 for LLM Applications |
