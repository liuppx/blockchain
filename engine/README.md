# ZhixingGraph 性能引擎（Rust · Milestone 4）

对应白皮书 [`docs/WHITEPAPER.md`](../docs/WHITEPAPER.md) §7.3「性能架构与技术选型」。

认知图谱引擎（DAG + kNN 近邻检索 + ΔK 计算）是全系统最热的路径——每次提交、每个共识轮次都要跑。这里用 **Rust（纯 std、零依赖、可离线编译）** 实现该热路径，作为 Python 参考实现（[`sim/delta_k.py`](../sim/delta_k.py)）的高性能对应版本。**两者是同一份契约（白皮书 B.2.3）**，`compute_delta_k` 逐函数对应。

## 构建与测试

```bash
cd engine
cargo test --release            # 单元测试（契约边界：首节点/近重复/低分门控）
cargo run --release --bin bench # 吞吐基准，默认 N=50000 M=20000 D=8
cargo run --release --bin bench -- 20000 2000 8   # 自定义 N M D
```

## 基准结果（Apple Silicon, release+LTO）

同一工况下 Rust 引擎 vs Python 参考实现（`python3 engine/bench.py 20000 2000 8`）：

| 实现 | 工况 | 耗时 | 吞吐 | 校验和 |
|---|---|---|---|---|
| **Rust** | N=20000, M=2000 | 0.18s | **10,866 subs/sec** | 157.48 |
| Python | N=20000, M=2000 | 42.24s | 47 subs/sec | 157.40 |

> **加速比 ≈ 230×**。两侧校验和一致（157.48 vs 157.40，差异仅来自 f32/f64 舍入），证明 Rust 端口忠实于同一 ΔK 契约。
> 在更大工况 N=50000, M=20000 下 Rust 仍达 ~4,100 subs/sec；纯 Python 在该规模需数分钟。

## 为什么这部分用 Rust

- **热路径**：kNN 近邻检索是 `O(域内节点数)` 的向量点积扫描，随图谱增长被高频调用；这类紧凑数值循环正是 Rust（+ 未来 SIMD）的强项。
- **无 GC 抖动**：共识关键路径需要可预测延迟。
- **可嵌入**：作为 lib crate，未来可通过 FFI / WASM 暴露给上层（Python 仿真、TS 前端、链上节点）。
- **契约一致**：`compute_delta_k` 与 Python 版共享 B.2.3 定义，校验和交叉验证防止实现漂移。

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 引擎核心：`CognitiveGraph`、`cos_sim`、`compute_delta_k`（B.2.3 Rust 端口）+ 单元测试 |
| `src/bench.rs` | Rust 吞吐基准（种子化 xorshift RNG，确定性）|
| `bench.py` | 同工况 Python 基准（复用 `sim/delta_k.py`），用于对比 |
| `Cargo.toml` | 零依赖；release 开启 LTO |

## 局限与后续

- 当前 kNN 为暴力线性扫描；生产环境应换 HNSW / IVF 等 ANN 索引（可引入 `hnsw_rs` 等 crate）。
- 嵌入维度固定为 8（`DIM`）以利缓存与自动向量化；实际维度更高时需重新评估。
- 后续可加 `pyo3` 绑定，让 `sim/` 直接调用 Rust 引擎跑大规模参数扫描；或编译到 WASM 供前端认知地形图使用。
