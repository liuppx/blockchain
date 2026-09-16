# 知行图 · ZhixingGraph

> **算力即电力，认知即资产，人类共识即地图。**

一个以认知为锚定物的图谱协议（cognition-anchored graph protocol）：把**电力 → 算力 → 认知**逐层映射并证明，代币 $COG 仅在产生**可验证的认知增量（ΔK）** 时增发——从根本上区别于"为挖矿而挖矿"的 PoW 与无价值锚定的空气币。

由**夜莺社区（Nightingale Community）** 发起，当前处于构想草稿（RFC）阶段。

## 文档

- 📄 **[白皮书 · WHITEPAPER.md](docs/WHITEPAPER.md)** — 哲学基础、三层价值映射、PoK 共识、代币经济学、技术架构与路线图。
  - 附录 B 深化：[B.1 博弈论与攻击面](docs/WHITEPAPER.md#b1-pok-共识的博弈论建模与攻击面分析)、[B.2 ΔK 形式化与规范](docs/WHITEPAPER.md#b2-δk-计算公式的形式化定义与参数标定)、[B.3 经济仿真](docs/WHITEPAPER.md#b3-cog-增发销毁的经济仿真agent-based-simulation)、B.4 绿证对接、B.5 zkML/opML、B.6 本体标准。

## 仿真

- 🧪 **[`sim/`](sim/)** — 零依赖（纯 Python 标准库）的经济学 agent-based 仿真，验证 PoK 激励与攻击抵抗性。

```bash
python3 sim/run.py          # 运行场景矩阵
python3 sim/run.py --md     # 并写出 sim/RESULTS.md
```

核心结论（详见 [sim/README.md](sim/README.md)）：在女巫 + 合谋攻击场景下，**攻击者净回报为负（亏损），诚实贡献者为正**，伪贡献通过率随声誉机制收敛到 0。

## 性能引擎（Rust）

- ⚙️ **[`engine/`](engine/)** — 认知图谱热路径（kNN + ΔK）的 Rust 实现（纯 std、零依赖），与 Python 参考实现共享同一份 B.2.3 契约。

```bash
cd engine && cargo run --release --bin bench
```

同工况下 **Rust ≈ 230× 于 Python**（10,900 vs 47 submissions/sec），校验和一致。详见 [engine/README.md](engine/README.md)。

## 核心概念速查

| 术语 | 含义 |
|---|---|
| ΔK | 认知增量，PoK 中衡量新增认知的核心指标 |
| PoK / PoE / PoF | 认知证明 / 电力证明 / 有效算力证明 |
| $COG / $WATT / $FLOP | 认知币（主）/ 电力凭证 / 算力凭证 |
| cNFT | 认知贡献证书 |

## 状态

Draft v0.3 · RFC。所有参数、公式、经济模型均为待验证的初始设计，欢迎社区评审与 PR。

## License

见 [LICENSE](LICENSE)。
