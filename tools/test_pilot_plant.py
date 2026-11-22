"""tools/pilot_plant.py 的测试:同一单元合成一个版本、原文必须恰好出现一次、几处互不重叠。

    python3 -m unittest tools/test_pilot_plant.py
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import pilot_plant as pp  # noqa: E402

BODY = "组间休息 60–90 秒。\n用 4–8 公斤,3 组 × 8–10 次。\n## 核验要点\n1. 休息约 60–90 秒对吗?\n"


class Plant(unittest.TestCase):
    def test_groups_plants_on_the_same_unit(self):
        plans = {"plants": [{"id": "F1", "unit": 6, "old": "a", "new": "b"},
                            {"id": "F2", "unit": 2, "old": "c", "new": "d"},
                            {"id": "F5", "unit": 6, "old": "e", "new": "f"}]}
        g = pp.by_unit(plans)
        self.assertEqual([p["id"] for p in g[6]], ["F1", "F5"], "同一单元的几处合成一个版本,保持顺序")
        self.assertEqual(list(g), [6, 2])

    def test_apply_all_makes_one_version_with_every_plant(self):
        out = pp.apply_all("组间休息 60–90 秒。用 4–8 公斤。", [
            {"id": "F1", "old": "60–90 秒", "new": "10–15 秒"},
            {"id": "F5", "old": "4–8 公斤", "new": "40–80 公斤"}])
        self.assertEqual(out, "组间休息 10–15 秒。用 40–80 公斤。")

    def test_old_text_must_appear_exactly_once(self):
        # 「60–90 秒」在正文和核验要点里各出现一次:必须用更长的、唯一的原文,不能改到核验要点里
        with self.assertRaises(ValueError):
            pp.apply_all(BODY, [{"id": "X", "old": "60–90 秒", "new": "10 秒"}])
        out = pp.apply_all(BODY, [{"id": "X", "old": "组间休息 60–90 秒。", "new": "组间休息 10 秒。"}])
        self.assertIn("1. 休息约 60–90 秒对吗?", out, "核验要点保持不动")

    def test_missing_text_is_refused(self):
        with self.assertRaises(ValueError):
            pp.apply_all(BODY, [{"id": "X", "old": "不存在的句子", "new": "y"}])

    def test_no_op_is_refused(self):
        with self.assertRaises(ValueError):
            pp.apply("组间休息 60–90 秒。", "60–90", "60–90")


if __name__ == "__main__":
    unittest.main()
