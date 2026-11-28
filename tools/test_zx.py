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
from unittest import mock

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


class SingleCreateTests(unittest.TestCase):
    def test_cli_parses_repeated_citations_for_create_handler(self):
        captured = {}

        def capture_args(args):
            captured["cites"] = args.cites
            return 0

        with tempfile.NamedTemporaryFile() as key_file, mock.patch.object(
            zx, "cmd_create", capture_args
        ):
            result = zx.main([
                "--account", "3", "--key", key_file.name, "--bin", key_file.name,
                "create", "document.md", "--domain", "1",
                "--cite", "16:1:see-also", "--cite", "12:2:basis",
            ])

        self.assertEqual(result, 0)
        self.assertEqual(captured["cites"], [(16, 1, "see-also"), (12, 2, "basis")])

    def test_parses_chain_citation(self):
        self.assertEqual(zx.parse_create_cite("16:1:see-also"), (16, 1, "see-also"))

    def test_rejects_invalid_chain_citation(self):
        for value in ("0:1:basis", "1:0:basis", "1:1:unknown", "one:1:basis"):
            with self.subTest(value=value), self.assertRaises(zx.argparse.ArgumentTypeError):
                zx.parse_create_cite(value)

    def test_explicit_chain_references_match_markdown_order_and_kind(self):
        document_cites = [
            kgraph.Cite("tool-calling.md", "参见", "v1"),
            kgraph.Cite("token.md", "依据", "v2"),
        ]
        zx.check_create_cites(
            document_cites,
            [(16, 1, "see-also"), (8, 2, "basis")],
        )

    def test_chain_references_must_cover_markdown_references(self):
        document_cites = [kgraph.Cite("tool-calling.md", "参见", "v1")]
        with self.assertRaisesRegex(SystemExit, "相同数量"):
            zx.check_create_cites(document_cites, [])

    def test_chain_reference_kind_must_match_markdown(self):
        document_cites = [kgraph.Cite("tool-calling.md", "参见", "v1")]
        with self.assertRaisesRegex(SystemExit, "引用类型"):
            zx.check_create_cites(document_cites, [(16, 1, "basis")])

    def test_body_lookup_follows_rpc_cursors(self):
        original = zx.rpc_get
        pages = []

        def fake_rpc_get(_rpc, path):
            pages.append(path)
            if len(pages) == 1:
                return {
                    "items": [{"id": "500", "author": "2", "body_hashes": []}],
                    "cursor": "500",
                }
            return {
                "items": [{"id": "501", "author": "3", "body_hashes": ["ab" * 32]}],
                "cursor": "501",
            }

        zx.rpc_get = fake_rpc_get
        try:
            found = zx.unit_for_body("https://example.test/rpc", "3", "ab" * 32)
        finally:
            zx.rpc_get = original
        self.assertEqual(found["id"], "501")
        self.assertIn("cursor=500", pages[1])


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


class ReviseCitesTests(unittest.TestCase):
    """`zx revise-cites`:发新版本只改引用,正文与来源原样。

    为什么单独一个命令:`revise` 只发正文与来源、不带引用——那会把单元的引用
    全部清空;`follow` 会带上全部引用,但只在有依据过时的时候才动,改不了类型。
    """

    def _unit(self):
        # /unit/{id} 的形状:versions 是**当前版本**的数组(链上 /unit 返回最新版本)
        return {
            "id": "15",
            "versions": [self._latest()],
        }

    def _latest(self):
        return {
            "n": "2",
            "publisher": "2",
            "source": "model:claude-opus-5-5",
            "bodies": [{"lang": "zh", "hash": "8e835142fd63d63f8095dff74ca85cf8ddb2a77d691b303aef3550b7b5980217"}],
            "cites": [
                {"unit": "9", "version": "2", "kind": "basis"},
                {"unit": "11", "version": "1", "kind": "basis"},
                {"unit": "7", "version": "2", "kind": "see-also"},
            ],
        }

    def _run(self, change, drop=None):
        captured = {}

        def fake_sign(bin_path, keyfile, actor, nonce, argv):
            captured["argv"] = list(argv)
            return b"\x00"

        with mock.patch.object(zx, "rpc_get", lambda rpc, path: self._unit()), \
             mock.patch.object(zx, "account_nonce", lambda rpc, actor: 39), \
             mock.patch.object(zx, "sign", fake_sign), \
             mock.patch.object(zx, "submit_knowledge", lambda rpc, signed: None), \
             mock.patch.object(zx, "confirm", lambda what, assume_yes=False: True), \
             mock.patch.object(zx.NonceTracker, "wait_in_block", lambda self, timeout=90.0: True):
            args = mock.Mock(rpc="http://x", unit=15, change=change, drop=drop or [],
                             account="2", yes=True, key="k", bin="b")
            zx.cmd_revise_cites(args)
        return captured["argv"]

    def test_flips_only_the_named_citation(self):
        argv = self._run([(11, 1, "see-also")])
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertEqual(cites, ["9:2:basis", "11:1:see-also", "7:2:see-also"])

    def test_body_and_source_are_carried_over_unchanged(self):
        argv = self._run([(11, 1, "see-also")])
        self.assertIn("--body", argv)
        body = argv[argv.index("--body") + 1]
        self.assertEqual(body, "zh:8e835142fd63d63f8095dff74ca85cf8ddb2a77d691b303aef3550b7b5980217")
        source = argv[argv.index("--source") + 1]
        self.assertEqual(source, "model:claude-opus-5-5")

    def test_citation_order_follows_the_current_version(self):
        # 顺序变了就看不出只改了一处,所以按当前版本的顺序来
        argv = self._run([(11, 1, "see-also")])
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertEqual(cites[0], "9:2:basis")
        self.assertEqual(cites[-1], "7:2:see-also")

    def test_no_change_requested_is_an_error(self):
        with self.assertRaises(SystemExit):
            self._run([])

    def test_cli_parses_the_change_argument(self):
        captured = {}

        def capture_args(args):
            captured["change"] = args.change
            return 0

        with tempfile.NamedTemporaryFile() as key_file, \
             mock.patch.object(zx, "cmd_revise_cites", capture_args):
            zx.main(["--account", "2", "--key", key_file.name, "revise-cites", "--unit", "15",
                     "--cite", "11:1:see-also"])
        self.assertEqual(captured["change"], [(11, 1, "see-also")])

    def test_adds_a_citation_the_unit_did_not_have(self):
        # 「参见 → #24」这类漏发的引用,`follow` 做不到(它只改已有的),这里能加
        argv = self._run([(24, 1, "see-also")])
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertIn("24:1:see-also", cites)
        self.assertEqual(len(cites), 4, "原有的三条 + 新增一条")

    def test_drops_a_citation(self):
        argv = self._run([], drop=[11])
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertNotIn("11:1:basis", cites)
        self.assertEqual(cites, ["9:2:basis", "7:2:see-also"])

    def test_drop_and_add_together(self):
        argv = self._run([(24, 1, "see-also")], drop=[11])
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertEqual(cites, ["9:2:basis", "7:2:see-also", "24:1:see-also"])

    def test_cli_parses_the_drop_argument(self):
        captured = {}

        def capture_args(args):
            captured["drop"] = args.drop
            return 0

        with tempfile.NamedTemporaryFile() as key_file, \
             mock.patch.object(zx, "cmd_revise_cites", capture_args):
            zx.main(["--account", "2", "--key", key_file.name, "revise-cites", "--unit", "15",
                     "--drop", "11"])
        self.assertEqual(captured["drop"], [11])


class ReviseKeepsCitesTests(unittest.TestCase):
    """`zx revise` 必须把当前版本的引用原样带上。

    链上的引用是**这一版自己的**:不带 `--cite` 发出去的新版本引用是空的,
    原单元的引用关系就断了。这是个静默的破坏——单元还在,引用没了。
    """

    def _run(self, body):
        captured = {}

        def fake_sign(bin_path, keyfile, actor, nonce, argv):
            captured["argv"] = list(argv)
            return b"\x00"

        unit = {
            "id": "20",
            "versions": [{
                "n": "3",
                "publisher": "6",
                "source": "model:claude-opus-5-5",
                "bodies": [{"lang": "zh", "hash": "old"}],
                "cites": [
                    {"unit": "17", "version": "2", "kind": "basis"},
                    {"unit": "24", "version": "1", "kind": "see-also"},
                ],
            }],
        }
        with tempfile.NamedTemporaryFile(suffix=".md", mode="w+", encoding="utf-8") as md, \
             mock.patch.object(zx, "rpc_get", lambda rpc, path: unit), \
             mock.patch.object(zx, "account_nonce", lambda rpc, actor: 5), \
             mock.patch.object(zx, "upload_body", lambda bodies, data, token="": "new"), \
             mock.patch.object(zx, "token", lambda: "test-token"), \
             mock.patch.object(zx, "sign", fake_sign), \
             mock.patch.object(zx, "submit_knowledge", lambda rpc, signed: None), \
             mock.patch.object(zx, "confirm", lambda what, assume_yes=False: True), \
             mock.patch.object(zx.NonceTracker, "wait_in_block", lambda self, timeout=90.0: True):
            md.write(body); md.flush()
            args = mock.Mock(rpc="http://x", unit=20, file=md.name,
                             source="model:claude-opus-5-5", account="6",
                             yes=True, key="k", bin="b")
            zx.cmd_revise(args)
        return captured["argv"]

    def test_cites_are_carried_over(self):
        argv = self._run("新正文")
        cites = [argv[i + 1] for i, v in enumerate(argv) if v == "--cite"]
        self.assertEqual(cites, ["17:2:basis", "24:1:see-also"])

    def test_body_and_source_are_the_new_ones(self):
        import hashlib
        argv = self._run("新正文")
        # 正文哈希是新正文的哈希(不是旧版本的)
        want = hashlib.sha256("新正文".encode("utf-8")).hexdigest()
        self.assertEqual(argv[argv.index("--body") + 1], f"zh:{want}")
        self.assertEqual(argv[argv.index("--source") + 1], "model:claude-opus-5-5")
