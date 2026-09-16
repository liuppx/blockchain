# ZhixingGraph 经济仿真（Milestone 2）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) 附录 **B.3**（agent-based simulation）。
纯 Python 标准库实现，**无需安装任何依赖**。

## 运行

```bash
python3 sim/run.py           # 运行场景矩阵，打印结果表
python3 sim/run.py --md      # 同上，并写出 sim/RESULTS.md
```

## 文件

| 文件 | 作用 |
|---|---|
| `delta_k.py` | ΔK 计算的**共享契约**，逐行对应白皮书 B.2.3 参考伪代码 |
| `model.py` | ABM 主体：智能体、图谱演化、PoK 轮次（提交→评议→复现→定稿→铸造/罚没）、指标 |
| `run.py` | 场景矩阵（B.3.5）运行器 + Markdown 结果表 |
| `RESULTS.md` | 最近一次运行的结果（由 `run.py --md` 自动生成）|

## 设计要点

- **一份契约**：`model.py` 调用 `delta_k.compute_delta_k`，与白皮书 §5.2 / B.2.3 是同一实现，文档与代码不会漂移。
- **供应动力学**：`供应 = 铸造(+) − 需求销毁(−)`；罚没的质押**不销毁**，而是转入 treasury 再分配（忠于 §5.1「挑战成功者获得罚没的一部分」），因此罚没只惩罚攻击者，不通缩整体经济。
- **攻击者建模**：spammer 提交近重复/低质内容；colluder 属于一个合谋环，环内评议人互相抬分；随时间被声誉衰减机制识别。
- **指标**：见 `RESULTS.md` 表头说明。`AtkROI/HonROI` 为**净**质押回报 `(earned − slashed)/staked`，`<0` 表示净亏损。

## 结果解读（默认参数、seed=42）

| 场景 | 结论 |
|---|---|
| baseline | 供应温和增长、Gini≈0.04（高度均衡）、跨域均衡度 0.93、无伪贡献 |
| inflation_stress | base_emission 调高 → 供应 12× 膨胀 → 印证需要增发上限 |
| sybil_collusion | **攻击者净回报 −0.29（亏损）**，诚实者 +0.58；伪贡献通过率末期→0；Gini 升至 0.39（攻击者早期套利） |
| cold_start | 仅 8 名诚实贡献者仍能自举图谱，诚实者回报 +1.04 |
| demand_shock | 需求崩溃 → 供应膨胀但**不死锁**，诚实激励延续 |

## 局限与后续（诚实声明）

- 这是**方向性原型**，非标定过的央行级模型；结论对行为假设敏感，需与真实测试网数据校准（对应 B.3.6 开放问题）。
- 语义嵌入用随机单位向量代理，未建模对抗样本攻击 novelty（B.2 开放问题）。
- 需求端为简化的流量代理；后续可接入更真实的服务需求曲线。
- 可选升级：迁移到 [Mesa](https://mesa.readthedocs.io/) 以获得可视化与批量参数扫描。
