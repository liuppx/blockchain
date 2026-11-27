#!/usr/bin/env python3
"""Regression tests for knowledge import source attribution."""

import unittest

import kgraph
from kimport import source_arg


class SourceArgTests(unittest.TestCase):
    def test_human_source(self):
        self.assertEqual(source_arg("人工撰写"), "human")

    def test_explicit_model_name_is_preserved(self):
        self.assertEqual(source_arg("模型生成(GPT-6)"), "model:gpt-6")

    def test_existing_claude_metadata_keeps_chain_source_id(self):
        self.assertEqual(source_arg("模型生成(Claude Opus 5.5)"), "model:claude-opus-5-5")

    def test_verified_model_name_is_preserved(self):
        self.assertEqual(source_arg("人工核验过的模型生成(GPT-6)"), "model-verified:gpt-6")

    def test_legacy_model_source_uses_existing_default(self):
        self.assertEqual(source_arg("模型生成"), "model:claude-opus-5-5")


class ModelIdTests(unittest.TestCase):
    """模型标识的规则:同一个模型在任何工具、任何入口下都是同一个标识。

    链上原样存 source 字符串、不做归一化,所以标识必须由一处产生。
    """

    def test_model_id_is_lowercase_dashed(self):
        self.assertEqual(kgraph.model_id("Claude Opus 5.5"), "claude-opus-5-5")
        self.assertEqual(kgraph.model_id("GPT-6"), "gpt-6")
        self.assertEqual(kgraph.model_id("OpenAI GPT-6"), "openai-gpt-6")

    def test_same_model_gets_same_id_in_every_tool(self):
        # 同一个模型名,zx 与 kimport 必须给出同一个标识
        self.assertEqual(source_arg("模型生成(GPT-6)"), kgraph.source_arg("模型生成(GPT-6)"))
        self.assertEqual(source_arg("模型生成(Claude Opus 5.5)"), kgraph.source_arg("模型生成(Claude Opus 5.5)"))

    def test_web_form_and_cli_agree(self):
        # 网页表单存的是链侧原样传的字符串;CLI 的标识规则必须与之相容
        self.assertEqual(source_arg("模型生成(GPT-6)"), "model:gpt-6")
        self.assertEqual(source_arg("人工核验过的模型生成(GPT-6)"), "model-verified:gpt-6")

    def test_unnamed_model_keeps_the_default(self):
        self.assertEqual(source_arg("模型生成"), "model:claude-opus-5-5")


if __name__ == "__main__":
    unittest.main()
