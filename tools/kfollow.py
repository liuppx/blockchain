#!/usr/bin/env python3
"""跟进依据:为"依据已过时"的知识单元发布新版本,让依据指向被引用单元的当前版本。

为什么需要:"依据"锁定被引用单元的某个版本,可信度也按版本计算。被依据的单元发布新版本后,
下游单元仍指向旧版本;审查者核验新版本,下游的有效可信度依然被旧版本卡住(浏览器显示"引用已过时")。

**跟进是一个判断,不是机械操作**:发布前请确认被依据单元的新版本没有改变你这个单元赖以成立的内容。
本工具默认只列出要做的改动;加 `--yes` 才签名提交。正文与来源保持不变,只更新引用的版本号。
跟进本身会产生新版本,依据本单元的下游单元随之过时:跟进后再列一次,逐层往上处理。

    # 列出所有依据已过时的单元(只读,不需要密钥)
    python3 tools/kfollow.py --rpc https://blockchain.yeying.pub/rpc

    # 为单元 6 发布跟进版本(用你自己的密钥签名;令牌取自 ZHIXING_RPC_TOKEN)
    python3 tools/kfollow.py --rpc https://blockchain.yeying.pub/rpc --unit 6 --key 2=bob.json --yes

纯 Python 标准库;签名用 `node encode-knowledge-op`(密钥只在本机使用)。
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def get_json(rpc: str, path: str):
    req = urllib.request.Request(rpc + path, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read().decode())


def stale_units(rpc: str) -> list[dict]:
    out = []
    for it in get_json(rpc, "/units?limit=500").get("items", []):
        u = get_json(rpc, f"/unit/{it['id']}")
        if u["versions"][-1]["credibility"]["stale_cites"]:
            out.append(u)
    return out


def plan(rpc: str, u: dict) -> tuple[list[str], list[str]]:
    """→ (encode-knowledge-op 参数, 人能读懂的改动说明)。"""
    v = u["versions"][-1]
    args = ["--action", "publish", "--unit", u["id"], "--source", v["source"]]
    for b in v["bodies"]:
        args += ["--body", f"{b['lang']}:{b['hash']}"]
    changes = []
    for c in v["cites"]:
        cur = len(get_json(rpc, f"/unit/{c['unit']}")["versions"])
        if c["kind"] == "basis" and int(c["version"]) < cur:
            changes.append(f"依据 #{c['unit']}: v{c['version']} → v{cur}")
            ver = cur
        else:
            ver = int(c["version"])  # 参见不传递可信度,保持原样
        args += ["--cite", f"{c['unit']}:{ver}:{c['kind']}"]
    return args, changes


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--rpc", default="http://127.0.0.1:9221")
    ap.add_argument("--unit", type=int, help="要跟进的单元;不给则只列出所有依据已过时的单元")
    ap.add_argument("--key", help="你的账户:ID=密钥文件(keystore 需要 ZHIXING_KEYSTORE_PASSWORD 或交互输入)")
    ap.add_argument("--yes", action="store_true", help="确认签名并提交(默认只显示改动)")
    ap.add_argument("--bin", default=os.path.join(os.path.dirname(__file__), "..", "node", "target", "release", "node"))
    a = ap.parse_args()
    a.rpc = a.rpc.rstrip("/")

    if a.unit is None:
        units = stale_units(a.rpc)
        if not units:
            print("没有依据已过时的单元。")
        for u in units:
            _, changes = plan(a.rpc, u)
            print(f"#{u['id']} v{len(u['versions'])}: " + "; ".join(changes))
        return

    u = get_json(a.rpc, f"/unit/{a.unit}")
    args, changes = plan(a.rpc, u)
    if not changes:
        print(f"#{a.unit} 的依据都已是当前版本,无需跟进。")
        return
    n = len(u["versions"]) + 1
    print(f"#{a.unit} 将发布 v{n}(正文与来源不变):")
    for c in changes:
        print("  " + c)
    if not a.yes:
        print("请先确认被依据单元的新版本没有改变本单元赖以成立的内容;确认后加 --yes 提交。")
        return
    if not a.key or "=" not in a.key:
        sys.exit("--yes 需要 --key ID=密钥文件")
    actor, keyfile = a.key.split("=", 1)
    nonce = get_json(a.rpc, f"/account/{actor}")["nonce"]
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as t:
        out = t.name
    enc = subprocess.run([a.bin, "encode-knowledge-op", "--key-file", keyfile, "--out", out,
                          "--actor", actor, "--nonce", str(nonce), *args], text=True,
                         stdout=subprocess.PIPE)
    if enc.returncode != 0:
        sys.exit("签名失败")
    body = open(out, "rb").read()
    os.unlink(out)
    headers = {"Content-Type": "application/octet-stream"}
    token = os.environ.get("ZHIXING_RPC_TOKEN", "")
    if token:
        headers["Authorization"] = "Bearer " + token
    try:
        with urllib.request.urlopen(urllib.request.Request(a.rpc + "/knowledge", data=body, headers=headers,
                                                           method="POST"), timeout=15) as r:
            print("已提交:", r.read().decode().strip())
    except urllib.error.HTTPError as e:
        sys.exit(f"提交失败: HTTP {e.code} {e.read().decode(errors='replace').strip()}")
    for _ in range(60):
        if len(get_json(a.rpc, f"/unit/{a.unit}")["versions"]) >= n:
            left = get_json(a.rpc, f"/unit/{a.unit}")["versions"][-1]["credibility"]["stale_cites"]
            print(f"#{a.unit} v{n} 已入块;" + ("依据全部为当前版本。" if not left else f"仍有过时依据: {left}"))
            print("依据本单元的下游单元现在可能过时了,再运行一次不带 --unit 的命令查看。")
            return
        time.sleep(1)
    sys.exit("等待入块超时,请稍后在浏览器查看")


if __name__ == "__main__":
    main()
