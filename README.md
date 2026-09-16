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

同工况下 **Rust ≈ 230× 于 Python**（10,900 vs 47 submissions/sec），校验和一致。引擎还通过 **pyo3** 绑定导出为 Python 模块，让仿真直接调用 Rust 热路径——整条 ABM 仿真 **≈ 96×** 提速且结果按种子逐字节一致：

```bash
./engine/build_python.sh      # 构建 pyo3 扩展模块（cargo + abi3，无需 maturin）
python3 sim/run.py --compare  # baseline 场景 Python vs Rust 后端对比
```

详见 [engine/README.md](engine/README.md)。

## 参考节点（Rust · PoK 共识状态机）

- ⛓️ **[`node/`](node/)** — 把 PoK 规则落成一个**确定性的共识状态机**：区块 / 交易 / 账户 / 状态转移 / 铸造罚没 / 链上声誉 / ed25519 签名交易 / 追加式持久化 / 确定性 mempool 出块 / Merkle 认证状态与轻客户端证明 / BFT 最终性证书 / BFT 轮次状态机（超时·锁定·换轮）/ 内容寻址区块哈希链 + 状态根。ΔK 复用引擎，与白皮书 B.2.3 是同一份契约。纯 std、零外部依赖（签名用审计过的 `ed25519-dalek`）、可离线编译。

```bash
cd node && cargo run --release --bin node -- demo    # 内存演示链
cargo run --release --bin node -- build              # mempool 规范排序出块
cargo run --release --bin node -- prove              # 轻客户端 Merkle 证明
cargo run --release --bin node -- bft                # BFT 最终性证书
cargo run --release --bin node -- live               # BFT 活性：轮次状态机驱动出块（含换轮）
cargo run --release --bin node -- run --dir ./data   # 持久化链（落盘 + 重放）
cargo test --release                                 # 55 项测试（确定性/守恒/回滚/持久化/Merkle/BFT）
```

> 共识的前提是确定性：相同创世 + 相同区块 → 逐字节相同的 `state_root`；状态落盘为追加式区块日志，重启重放可完整重建；> 2/3 投票权的最终性证书 + Merkle 状态根让轻客户端可离线验证区块与账户；Tendermint 式轮次状态机在提议人宕机时仍能换轮出块（活性）。P2P gossip 与多高度链循环为后续里程碑，详见 [node/README.md](node/README.md)。

## 核心概念速查

| 术语 | 含义 |
|---|---|
| ΔK | 认知增量，PoK 中衡量新增认知的核心指标 |
| PoK / PoE / PoF | 认知证明 / 电力证明 / 有效算力证明 |
| $COG / $WATT / $FLOP | 认知币（主）/ 电力凭证 / 算力凭证 |
| cNFT | 认知贡献证书 |

## 状态

Draft v0.7 · RFC。所有参数、公式、经济模型均为待验证的初始设计，欢迎社区评审与 PR。

## License

见 [LICENSE](LICENSE)。
