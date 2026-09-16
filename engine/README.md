# ZhixingGraph 性能引擎（Rust · Milestone 4–5）

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

## Python 绑定（pyo3 · Milestone 5）

同一个 Rust 引擎通过 [`pyo3`](https://pyo3.rs) 暴露为 Python 扩展模块 `zhixing_engine`，让仿真（`sim/`）直接调用 Rust 跑热路径——**既能做大规模离线参数扫描，也能把 ABM 仿真本身提速**。不依赖 maturin：`build_python.sh` 用 `cargo build --features python` + abi3（一次构建适配任意 CPython ≥ 3.8）。

```bash
./engine/build_python.sh          # 生成 engine/zhixing_engine.abi3.so 并自检 import
python3 engine/sweep.py 4000 400  # 参数扫描：纯 Python vs Rust，校验校验和一致
python3 sim/run.py --compare      # 整条 ABM 仿真：Python vs Rust 后端对比
python3 sim/run.py --rust         # 用 Rust 后端跑全部场景矩阵
```

`sim/model.py` 里 `SimConfig.backend` 取 `"auto"`（默认，模块已构建则用 Rust）/ `"rust"` / `"python"`。Rust 图谱镜像 Python 图谱，ΔK 由所选后端计算——同一份 B.2.3 契约，结果按种子完全一致。

| 工作负载 | 纯 Python | Rust（pyo3） | 加速比 | 一致性 |
|---|---|---|---|---|
| 参数扫描（18 组 × 400 提交，`sweep.py`） | ~31.7s | ~0.14s | **≈ 225×** | 校验和相对误差 1.5e-7 |
| 整条 ABM 仿真（baseline，200 轮，`run.py --compare`） | ~11.8s | ~0.12s | **≈ 96×** | 逐指标完全一致 |

> ΔK 只是 ABM 每轮工作的一部分（还有评审抽样、复现、记账等仍在 Python 侧），所以整条仿真加速比低于纯 ΔK 热路径基准（~230×），但把最贵的那段搬到 Rust 后仍有近百倍收益，且结果零漂移。



- **热路径**：kNN 近邻检索是 `O(域内节点数)` 的向量点积扫描，随图谱增长被高频调用；这类紧凑数值循环正是 Rust（+ 未来 SIMD）的强项。
- **无 GC 抖动**：共识关键路径需要可预测延迟。
- **可嵌入**：作为 lib crate，未来可通过 FFI / WASM 暴露给上层（Python 仿真、TS 前端、链上节点）。
- **契约一致**：`compute_delta_k` 与 Python 版共享 B.2.3 定义，校验和交叉验证防止实现漂移。

## 文件

| 文件 | 作用 |
|---|---|
| `src/lib.rs` | 引擎核心：`CognitiveGraph`、`cos_sim`、`compute_delta_k`（B.2.3 Rust 端口）+ 单元测试 + `python` feature 下的 pyo3 绑定（`PyGraph`）|
| `src/bench.rs` | Rust 吞吐基准（种子化 xorshift RNG，确定性）|
| `bench.py` | 同工况 Python 基准（复用 `sim/delta_k.py`），用于对比 |
| `sweep.py` | 参数扫描基准：纯 Python vs Rust `PyGraph`，交叉验证校验和 |
| `build_python.sh` | 无需 maturin，用 cargo + abi3 构建 `zhixing_engine` 扩展模块 |
| `Cargo.toml` | 默认零依赖；`python` feature 才引入可选的 pyo3；release 开启 LTO |

## 局限与后续

- 当前 kNN 为暴力线性扫描；生产环境应换 HNSW / IVF 等 ANN 索引（可引入 `hnsw_rs` 等 crate）。
- 嵌入维度固定为 8（`DIM`）以利缓存与自动向量化；实际维度更高时需重新评估。
- pyo3 绑定已就绪（Milestone 5）；后续可进一步编译到 WASM 供前端认知地形图使用，或通过 FFI 暴露给链上节点。
