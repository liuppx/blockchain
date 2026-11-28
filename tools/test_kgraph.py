#!/usr/bin/env python3
"""kgraph 的引用版本检查:只有「依据」过时才算要处理的事。

链上 `stale_cites` 只统计「依据」(node/src/knowledge.rs 的 credibility_memo
只遍历 CiteKind::Basis)——「参见」不传递可信度,版本落后不卡任何指标。
本工具以前把两种混在一起报,让人以为有一堆要跟进的引用,而实际上跟不动
(互相引用的单元对会无限乒乓)。这里守住这个口径。

纯 Python 标准库,不依赖第三方包。
"""
import unittest

import kgraph


def mk(file, version, cites):
    return kgraph.Unit(file=file, title=file, meta={"版本": version}, cites=cites)


class StaleByKindTests(unittest.TestCase):
    def test_outdated_basis_is_flagged(self):
        # 「依据」指向旧版本(目标已是 v2,引用还写 v1)→ 要处理
        units = {
            "a.md": mk("a.md", "v2", []),
            "b.md": mk("b.md", "v1", [kgraph.Cite("a.md", "依据", "v1")]),
        }
        issues = kgraph.analyze(units)
        self.assertEqual(len(issues["stale"]), 1)
        self.assertEqual(issues["stale"][0][1], "a.md")
        self.assertEqual(issues["stale_see_also"], [])

    def test_outdated_see_also_is_not_a_stale_basis(self):
        # 「参见」指向旧版本 → 不算要处理的事,单独记一笔
        units = {
            "a.md": mk("a.md", "v2", []),
            "b.md": mk("b.md", "v1", [kgraph.Cite("a.md", "参见", "v1")]),
        }
        issues = kgraph.analyze(units)
        self.assertEqual(issues["stale"], [])
        self.assertEqual(len(issues["stale_see_also"]), 1)
        self.assertEqual(issues["stale_see_also"][0][1], "a.md")

    def test_current_versions_flag_nothing(self):
        units = {
            "a.md": mk("a.md", "v2", []),
            "b.md": mk("b.md", "v2", [
                kgraph.Cite("a.md", "依据", "v2"),
                kgraph.Cite("a.md", "参见", "v2"),
            ]),
        }
        issues = kgraph.analyze(units)
        self.assertEqual(issues["stale"], [])
        self.assertEqual(issues["stale_see_also"], [])

    def test_missing_target_is_still_an_error(self):
        units = {"b.md": mk("b.md", "v1", [kgraph.Cite("nope.md", "依据", "v1")])}
        issues = kgraph.analyze(units)
        self.assertEqual(len(issues["missing"]), 1)


if __name__ == "__main__":
    unittest.main()
