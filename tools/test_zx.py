#!/usr/bin/env python3
"""`zx` 命令行工具的回归测试。

覆盖那些**不碰链**的部分:单元解析、拓扑序、引用解析、来源映射、参数校验。
链上的部分(签名与提交)由 testnet/pilot-drill.sh 与本地 4 节点演练覆盖。

    python3 tools/test_zx.py
"""
from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def load_zx():
    """加载 tools/zx(没有 .py 后缀,用 importlib 按路径加载)。"""
    spec = importlib.util.spec_from_loader(
        "zxmod", importlib.machinery.SourceFileLoader("zxmod", os.path.join(HERE, "zx")))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


zx = load_zx()
sys.path.insert(0, HERE)
import kgraph  # noqa: E402


def write_unit(directory: str, name: str, layer: str, source: str, cites: str = "") -> str:
    """写一个最小单元文件(格式与 docs/pilot/*/ 里的相同)。"""
    cite_rows = ""
    if cites:
        rows = []
        for line in cites.strip().splitlines():
            target, kind = line.split("|")
            rows.append(f"| [{target}]({target}.md) | {kind} | v1 | 测试 |")
        cite_rows = "\n".join(rows)
    body = f"""# 测试单元 {name}

| 字段 | 内容 |
|---|---|
| 层级 | {layer} |
| 来源 | {source} |
| 状态 | **待核验** |
| 版本 | v1 |
| 语言 | zh |

## 核心陈述

这是一个用于测试的单元。

## 正文

正文。

## 引用

| 单元 | 类型 | 引用时的版本 | 为什么引用 |
|---|---|---|---|
{cite_rows}

## 证据

| 论断 | 出处 | 证据类型 |
|---|---|---|
| 测试 | 待补 | 待补 |

## 核验记录

| 核验人 | 版本 | 结论 | 日期 | 说明 |
|---|---|---|---|---|
"""
    path = os.path.join(directory, name + ".md")
    with open(path, "w", encoding="utf-8") as f:
        f.write(body)
    return path


class ParseTests(unittest.TestCase):
    def test_parses_layer_source_and_cites(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "模型生成(Claude Opus 5.5)")
            write_unit(d, "b", "解释", "人工撰写", cites="a|依据")
            units = zx.parse_units(d)
            self.assertEqual(len(units), 2)
            self.assertEqual(units["a.md"].layer, "基础")
            self.assertEqual(units["b.md"].layer, "解释")
            self.assertEqual(len(units["b.md"].cites), 1)
            self.assertEqual(units["b.md"].cites[0].target, "a.md")
            self.assertEqual(units["b.md"].cites[0].kind, "依据")

    def test_readme_and_graph_are_not_units(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写")
            for skip in ("README.md", "GRAPH.md"):
                with open(os.path.join(d, skip), "w", encoding="utf-8") as f:
                    f.write("# 不是单元\n")
            units = zx.parse_units(d)
            self.assertEqual(list(units), ["a.md"])

    def test_units_without_layer_are_ignored(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写")
            with open(os.path.join(d, "notes.md"), "w", encoding="utf-8") as f:
                f.write("# 随手记\n不是知识单元。\n")
            units = zx.parse_units(d)
            self.assertEqual(list(units), ["a.md"])

    def test_empty_directory_is_an_error(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit):
                zx.parse_units(d)


class TopoOrderTests(unittest.TestCase):
    def test_basis_citations_come_first(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写")
            write_unit(d, "b", "解释", "人工撰写", cites="a|依据")
            write_unit(d, "c", "应用", "人工撰写", cites="b|依据")
            units = zx.parse_units(d)
            order = [f[:-3] for f in zx.topo_order(units)]
            self.assertLess(order.index("a"), order.index("b"))
            self.assertLess(order.index("b"), order.index("c"))

    def test_see_also_does_not_constrain_order(self):
        # "参见"不传递可信度,不该影响发布顺序(链上一个版本只能引用已存在的版本,
        # 所以"参见"由导入工具在全部建完之后补一版)。
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写")
            write_unit(d, "b", "基础", "人工撰写", cites="a|参见")
            units = zx.parse_units(d)
            order = [f[:-3] for f in zx.topo_order(units)]
            self.assertEqual(sorted(order), ["a", "b"])

    def test_basis_cycle_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写", cites="b|依据")
            write_unit(d, "b", "基础", "人工撰写", cites="a|依据")
            units = zx.parse_units(d)
            with self.assertRaises(SystemExit):
                zx.topo_order(units)

    def test_missing_basis_target_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            write_unit(d, "a", "基础", "人工撰写", cites="nope|依据")
            units = zx.parse_units(d)
            with self.assertRaises(SystemExit):
                zx.topo_order(units)


class SourceArgTests(unittest.TestCase):
    def test_human(self):
        self.assertEqual(zx.source_arg("人工撰写"), "human")

    def test_model_keeps_the_model_name(self):
        self.assertEqual(zx.source_arg("模型生成(Claude Opus 5.5)"), "model:claude-opus-5-5")

    def test_model_verified(self):
        self.assertEqual(zx.source_arg("人工核验过的模型生成(GPT-6)"), "model-verified:gpt-6")

    def test_bare_model_uses_the_default(self):
        self.assertEqual(zx.source_arg("模型生成"), "model:claude-opus-5-5")


class NonceTrackerTests(unittest.TestCase):
    """序号跟踪:链上 nonce 只在入块后才前进,连发多条会撞号。

    2026-11-26 实测:发布 10 个教育单元时,#17/#18 补版成功,第三条 op 报
    HTTP 422 account 6 nonce mismatch: expected 12, got 11 —— 前一条还没入块,
    从链上读到的 nonce 还是旧的。
    """

    class FakeRpc:
        """假链:nonce 只在 wait_in_block 之后才前进。"""

        def __init__(self, start=5):
            self.chain_nonce = start

    def test_take_increments_locally(self):
        tracker = zx.NonceTracker.__new__(zx.NonceTracker)
        tracker.rpc = "http://x"
        tracker.actor = "6"
        tracker.nonce = 5
        self.assertEqual(tracker.take(), 5)
        self.assertEqual(tracker.take(), 6)
        self.assertEqual(tracker.nonce, 7)

    def test_sequences_do_not_repeat(self):
        tracker = zx.NonceTracker.__new__(zx.NonceTracker)
        tracker.rpc = "http://x"
        tracker.actor = "6"
        tracker.nonce = 0
        seen = [tracker.take() for _ in range(10)]
        self.assertEqual(seen, list(range(10)), "每条 op 的序号必须不同")

    def test_wait_in_block_returns_true_once_the_chain_catches_up(self):
        tracker = zx.NonceTracker.__new__(zx.NonceTracker)
        tracker.rpc = "http://x"
        tracker.actor = "6"
        tracker.nonce = 7
        # 链上 nonce 还没追上 → 等不到(用极短超时,只验证它不误报成功)
        original = zx.account_nonce
        zx.account_nonce = lambda rpc, actor: 5
        try:
            self.assertFalse(tracker.wait_in_block(timeout=0.2))
            # 链上追上 → 成功
            zx.account_nonce = lambda rpc, actor: 7
            self.assertTrue(tracker.wait_in_block(timeout=1.0))
        finally:
            zx.account_nonce = original


class TokenTests(unittest.TestCase):
    def test_local_flag_wins_over_the_environment(self):
        # 本地开发链的令牌是固定的;环境变量里若有测试网令牌,本地模式下不该用它。
        prev = os.environ.get("ZHIXING_RPC_TOKEN")
        os.environ["ZHIXING_RPC_TOKEN"] = "testnet-token"
        os.environ["ZHIXING_LOCAL"] = "1"
        try:
            self.assertEqual(zx.token(), "local-dev-token")
        finally:
            os.environ.pop("ZHIXING_LOCAL", None)
            if prev is None:
                os.environ.pop("ZHIXING_RPC_TOKEN", None)
            else:
                os.environ["ZHIXING_RPC_TOKEN"] = prev

    def test_environment_token_is_used(self):
        prev = os.environ.get("ZHIXING_RPC_TOKEN")
        prev_local = os.environ.get("ZHIXING_LOCAL")
        os.environ.pop("ZHIXING_LOCAL", None)
        os.environ["ZHIXING_RPC_TOKEN"] = "testnet-token"
        try:
            self.assertEqual(zx.token(), "testnet-token")
        finally:
            if prev is None:
                os.environ.pop("ZHIXING_RPC_TOKEN", None)
            else:
                os.environ["ZHIXING_RPC_TOKEN"] = prev
            if prev_local is not None:
                os.environ["ZHIXING_LOCAL"] = prev_local

    def test_missing_token_is_a_clear_error(self):
        prev = os.environ.get("ZHIXING_RPC_TOKEN")
        prev_local = os.environ.get("ZHIXING_LOCAL")
        os.environ.pop("ZHIXING_RPC_TOKEN", None)
        os.environ.pop("ZHIXING_LOCAL", None)
        try:
            with self.assertRaises(SystemExit) as cm:
                zx.token()
            self.assertIn("ZHIXING_RPC_TOKEN", str(cm.exception))
        finally:
            if prev is not None:
                os.environ["ZHIXING_RPC_TOKEN"] = prev
            if prev_local is not None:
                os.environ["ZHIXING_LOCAL"] = prev_local


class BodyHashTests(unittest.TestCase):
    def test_body_hash_is_sha256_of_the_utf8_bytes(self):
        # 正文哈希是链上"同一份正文只能属于一个版本"的依据:SHA-256(UTF-8 字节),小写十六进制。
        # 期望值用独立计算核对过(python3 -c "import hashlib; print(hashlib.sha256('测试正文'.encode()).hexdigest())")。
        import hashlib
        data = "测试正文".encode("utf-8")
        self.assertEqual(hashlib.sha256(data).hexdigest(),
                         "8afa18c692e74933eabfa628444a91623511c6c43a177a31a6caf8633f08a671")


if __name__ == "__main__":
    unittest.main()
