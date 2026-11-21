"""tools/pilot_metrics.py 的测试:手搭一个小场景,每个数都能手算核对。

    python3 -m unittest tools/test_pilot_metrics.py
"""
import hashlib
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import pilot_metrics as pm  # noqa: E402


def v(n, publisher, height, reviews):
    return {"n": str(n), "publisher": str(publisher), "published_height": str(height),
            "verifications": [{"verifier": str(w), "verdict": d, "height": str(h)} for w, d, h in reviews]}


# 单元 1(健身):v1 干净,v2 埋了错
# 单元 2(大模型):红队 #9 发布的错误单元,红队 #8 帮着通过,诚实的 #3 判需修改
UNITS = [
    {"id": "1", "domain": "2", "versions": [
        v(1, 1, 100, [(2, "pass", 110), (3, "pass", 130), (1, "pass", 101)]),   # #1 是发布者,不算
        v(2, 1, 200, [(2, "pass", 220), (3, "needs-revision", 205)]),
    ]},
    {"id": "2", "domain": "1", "versions": [
        v(1, 9, 300, [(8, "pass", 301), (3, "needs-revision", 340)]),
    ]},
]
MEMBERS = [
    {"id": "1", "inviter": "0"}, {"id": "2", "inviter": "1"}, {"id": "3", "inviter": "1"},
    {"id": "8", "inviter": "2"}, {"id": "9", "inviter": "2"},
]
KEY = {"planted": [{"unit": 1, "version": 2}], "red_team": [8, 9]}


class Metrics(unittest.TestCase):
    def test_reviews_exclude_the_publisher_and_honour_since(self):
        self.assertEqual(len(pm.reviews_of(UNITS[0]["versions"][0])), 2)
        self.assertEqual(len(pm.reviews_of(UNITS[0]["versions"][0], since=120)), 1)

    def test_coverage(self):
        c = pm.coverage(UNITS)
        self.assertEqual((c["2"]["versions"], c["2"]["reviewed"], c["2"]["reviews"]), (2, 2, 4))
        self.assertEqual(c["2"]["median_wait"], 7.5)  # v1 等 10 块,v2 等 5 块
        self.assertEqual(c["1"]["median_wait"], 1)

    def test_detection_and_false_alarms(self):
        d = pm.detection(UNITS, KEY["planted"])["2"]
        self.assertEqual((d["planted"], d["caught"]), (1, 1))
        self.assertEqual((d["planted_needs"], d["planted_reviews"]), (1, 2))  # #2 漏了,#3 抓到
        self.assertEqual((d["clean_needs"], d["clean_reviews"]), (0, 2))

    def test_reviewer_accuracy_and_inviter(self):
        acc = pm.reviewer_accuracy(UNITS, KEY["planted"])
        self.assertEqual(acc["2"], {"planted": 1, "caught": 0, "clean": 1, "false_alarm": 0})
        self.assertEqual(acc["3"]["caught"], 1)
        inv = pm.by_inviter(MEMBERS, acc)
        self.assertEqual(inv["1"], {"invited": 2, "active": 2, "planted": 2, "caught": 1})
        self.assertEqual(inv["2"]["invited"], 2)
        self.assertNotIn("0", inv, "创世成员没有邀请人")

    def test_red_team(self):
        rt = pm.red_team(UNITS, KEY["red_team"])
        self.assertEqual(rt["versions"], 1)
        self.assertEqual(rt["escaped"], 0, "红队互相通过不算数,诚实的 #3 判了需修改")
        self.assertEqual(rt["collusive_passes"], 1)
        self.assertEqual(rt["caught_after"], [40])

    def test_assignment_compliance(self):
        done, total = pm.assignment_compliance(UNITS, {("1", "2"): ["2", "4"]})
        self.assertEqual((done, total), (1, 2))

    def test_report_without_key_says_what_is_missing(self):
        text = pm.report(UNITS, MEMBERS, {}, None, 0, 1.0)
        self.assertIn("运动健身", text)
        self.assertIn("没有答案文件", text)

    def test_commitment_mismatch_is_refused(self):
        with tempfile.NamedTemporaryFile("wb", suffix=".json", delete=False) as f:
            f.write(json.dumps(KEY).encode())
        try:
            bad = "00" * 32
            self.assertEqual(pm.main(["--answer-key", f.name, "--commitment", bad, "--rpc", "http://127.0.0.1:1"]), 1)
            self.assertNotEqual(hashlib.sha256(open(f.name, "rb").read()).hexdigest(), bad)
        finally:
            os.unlink(f.name)


if __name__ == "__main__":
    unittest.main()
