"""tools/body_mirror.py 的测试:引用收集覆盖全部版本与核验说明、哈希不符的内容不落盘、可反复跑、恢复只传缺的。

    python3 -m unittest tools/test_body_mirror.py
"""
import hashlib
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import body_mirror as bm  # noqa: E402

A, B, NOTE = b"# v1 body", b"# v2 body", b"reviewer note"
H = lambda b: hashlib.sha256(b).hexdigest()  # noqa: E731

UNIT = {"id": "8", "versions": [
    {"n": 1, "bodies": [{"lang": "zh", "hash": H(A)}], "verifications": []},
    {"n": 2, "bodies": [{"lang": "zh", "hash": H(B)}], "verifications": [
        {"verifier": "3", "note_hash": H(NOTE)},
        {"verifier": "4", "note_hash": "0" * 64}]}]}


def chain(pages):
    """假 RPC:/units 分页 + /unit/<id>。"""
    def fetch(url):
        if "/units?" in url:
            cur = url.split("cursor=")[1] if "cursor=" in url else ""
            return pages[cur]
        return {"8": UNIT, "9": {"id": "9", "versions": [{"n": 1, "bodies": [{"lang": "en", "hash": H(A)}]}]}}[url.rsplit("/", 1)[1]]
    return fetch


class Refs(unittest.TestCase):
    def test_every_version_and_written_notes(self):
        refs = bm.refs_of_unit(UNIT)
        self.assertEqual(set(refs), {H(A), H(B), H(NOTE)}, "旧版本的正文也要保存;全 0 说明哈希表示没写,跳过")
        self.assertIn("核验说明(账户 #3)", refs[H(NOTE)][0])

    def test_paginates_and_merges_shared_bodies(self):
        pages = {"": {"items": [{"id": "8"}], "cursor": "1"}, "1": {"items": [{"id": "9"}], "cursor": None}}
        refs = bm.collect_refs("rpc", chain(pages))
        self.assertEqual(len(refs[H(A)]), 2, "同一份正文被两个地方引用时,两处都记下")


class Mirror(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.refs = bm.refs_of_unit(UNIT)

    def store(self, good):
        return lambda url: good.get(url.rsplit("/", 1)[1])

    def test_copies_verifies_and_is_idempotent(self):
        src = self.store({H(A): A, H(B): B, H(NOTE): NOTE})
        r = bm.mirror(self.refs, ["s"], self.d, src)
        self.assertEqual((r["have"], len(r["stored"]), r["missing"]), (3, 3, []))
        r2 = bm.mirror(self.refs, ["s"], self.d, src)
        self.assertEqual(len(r2["stored"]), 0, "第二次不再重取")

    def test_tampered_content_is_not_written(self):
        r = bm.mirror(self.refs, ["s"], self.d, self.store({H(A): b"tampered", H(B): B, H(NOTE): NOTE}))
        self.assertEqual(r["missing"], [H(A)])
        self.assertFalse(os.path.exists(os.path.join(self.d, H(A))), "哈希不符的内容一个字节都不落盘")

    def test_falls_back_to_next_source_and_repairs_corruption(self):
        with open(os.path.join(self.d, H(B)), "wb") as f:
            f.write(b"bitrot")                                   # 本地副本损坏
        def src(url):
            if url.startswith("down"):
                raise OSError("connection refused")
            return {H(A): A, H(B): B, H(NOTE): NOTE}[url.rsplit("/", 1)[1]]
        r = bm.mirror(self.refs, ["down", "up"], self.d, src)
        self.assertEqual(r["missing"], [])
        self.assertTrue(bm.local_ok(self.d, H(B)), "损坏的副本被修好")

    def test_status_report(self):
        r = bm.mirror(self.refs, ["s"], self.d, self.store({H(A): A}))
        bm.write_reports(self.d, self.refs, r, ["s"])
        st = json.load(open(os.path.join(self.d, "STATUS.json")))
        self.assertEqual(len(st["missing"]), 2)
        self.assertTrue(st["missing"][0]["refs"], "缺失项写明被谁引用")


class SourceCheck(unittest.TestCase):
    def test_primary_loss_is_reported_even_when_mirror_is_complete(self):
        d = tempfile.mkdtemp()
        refs = {H(A): ["单元 #1 v1 正文(zh)"]}
        r = bm.mirror(refs, ["s"], d, lambda url: A)
        sc = bm.check_remote(refs, "s", lambda url: None)          # 之后主仓库丢了这份
        bm.write_reports(d, refs, r, ["s"], sc)
        st = json.load(open(os.path.join(d, "STATUS.json")))
        self.assertEqual((st["have"], len(st["source_missing"])), (1, 1))


class Restore(unittest.TestCase):
    def test_only_uploads_intact_copies(self):
        d = tempfile.mkdtemp()
        bm.write_atomic(d, H(A), A)
        sent = []
        r = bm.restore(d, [H(A), H(B)], "site", "t", put=lambda url, data: sent.append((url, data)) or 201)
        self.assertEqual(r["restored"], [H(A)])
        self.assertEqual(len(r["failed"]), 1, "副本里没有的不能凭空传")
        self.assertEqual(sent, [(f"site/bodies/{H(A)}", A)])

    def test_check_remote(self):
        refs = {H(A): ["x"], H(B): ["y"]}
        r = bm.check_remote(refs, "site", lambda url: {H(A): A}.get(url.rsplit("/", 1)[1]))
        self.assertEqual((r["ok"], r["missing"]), (1, [H(B)]))


if __name__ == "__main__":
    unittest.main()
