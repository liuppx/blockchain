#!/usr/bin/env python3
"""把链上单元的 source 里的模型标识改成统一写法(小写连字符)。

链上原样存 source 字符串、不做归一化,所以「同一个模型」必须处处得到同一个
标识。已有单元写的是 `model:GPT-6`,而 CLI 侧统一产出 `model:gpt-6` ——
按来源统计时一个模型会被拆成两个。

链上记录不可改,所以只能**发新版本**:正文哈希原样带上(内容一个字不改),
引用原样带上,只把 source 换掉。

    python3 tools/fix_source.py --account 3 --key K --unit 16 --unit 28
    python3 tools/fix_source.py --account 3 --key K --all

`--dry-run` 只列出将要做什么。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)

TESTNET_RPC = "https://blockchain.yeying.pub/rpc"
TESTNET_BODIES = "https://blockchain.yeying.pub/bodies"


def rpc_get(rpc: str, path: str):
    req = urllib.request.Request(rpc + path, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def rpc_post(rpc: str, path: str, data: bytes, token: str = ""):
    headers = {"Content-Type": "application/octet-stream"}
    if token:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(rpc + path, data=data, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.read().decode().strip()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace").strip()
        raise SystemExit(f"提交失败: HTTP {e.code} {detail}")


def token() -> str:
    t = os.environ.get("ZHIXING_RPC_TOKEN", "")
    if t:
        return t
    raise SystemExit("需要写入令牌:请设置环境变量 ZHIXING_RPC_TOKEN。")


def account_nonce(rpc: str, actor: str) -> int:
    acct = rpc_get(rpc, f"/account/{actor}") or {}
    return int(acct.get("nonce", 0))


def all_units(rpc: str) -> list:
    return (rpc_get(rpc, "/units?limit=500") or {}).get("items", [])


def sign(bin_path: str, keyfile: str, actor: str, nonce: int, argv: list) -> bytes:
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as t:
        out = t.name
    try:
        enc = subprocess.run(
            [bin_path, "encode-knowledge-op", "--key-file", keyfile, "--out", out,
             "--actor", str(actor), "--nonce", str(nonce), *argv],
            capture_output=True, text=True,
        )
        if enc.returncode != 0:
            raise SystemExit("签名失败: " + (enc.stderr or enc.stdout).strip())
        return open(out, "rb").read()
    finally:
        if os.path.exists(out):
            os.unlink(out)


def normalized_model(source: str) -> str:
    """把 source 里的模型标识转成统一写法(小写连字符)。"""
    for prefix in ("model-verified:", "model:"):
        if source.startswith(prefix):
            ident = source[len(prefix):]
            fixed = "-".join(
                "".join(c if c.isalnum() and c.isascii() else " " for c in ident.lower()).split()
            ).strip("-")
            return prefix + fixed
    return source


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--account", required=True, help="发布者账户编号")
    p.add_argument("--key", required=True, help="该账户的密钥文件(加密 keystore 或裸种子)")
    p.add_argument("--bin", default=os.path.join(ROOT, "node", "target", "release", "node"))
    p.add_argument("--rpc", default=TESTNET_RPC)
    p.add_argument("--bodies", default=TESTNET_BODIES)
    p.add_argument("--unit", action="append", default=[], help="要修的单元编号,可多次指定")
    p.add_argument("--all", action="store_true", help="修该账户发布的全部单元")
    p.add_argument("--dry-run", action="store_true", help="只列出将要做什么")
    args = p.parse_args()

    units = all_units(args.rpc)
    targets = []
    for it in units:
        d = rpc_get(args.rpc, f"/unit/{it['id']}")
        if d is None:
            continue
        latest = d["versions"][-1]
        if str(latest["publisher"]) != str(args.account):
            continue
        if args.unit and it["id"] not in args.unit:
            continue
        fixed = normalized_model(latest.get("source") or "human")
        if fixed == (latest.get("source") or "human"):
            continue
        targets.append((it["id"], d, latest, fixed))

    if not targets:
        print("没有要改的单元。")
        return 0

    print(f"账户 {args.account} 有 {len(targets)} 个单元的来源标识要统一:\n")
    for cid, d, latest, fixed in targets:
        print(f"  #{cid} v{latest['n']}  {latest.get('source')} → {fixed}")
    print()

    if args.dry_run:
        print("(dry-run,没有签名也没有提交)")
        return 0

    nonce = account_nonce(args.rpc, args.account)
    for cid, d, latest, fixed in targets:
        body_hash = latest["bodies"][0]["hash"]
        # 正文原样带上:内容一个字不改,只是把 source 换掉。
        # 正文已经在正文仓库里(content-addressed),不用重传。
        argv = ["--action", "publish", "--unit", str(cid),
                "--body", f"zh:{body_hash}", "--source", fixed]
        for c in latest.get("cites", []):
            argv += ["--cite", f"{c['unit']}:{c['version']}:{c['kind']}"]
        print(f"── 单元 #{cid}:发布 v{len(d['versions']) + 1}(正文不变,来源 {fixed})")
        for c in latest.get("cites", []):
            print(f"     引用 #{c['unit']} v{c['version']} {c['kind']}")
        signed = sign(args.bin, args.key, args.account, nonce, argv)
        rpc_post(args.rpc, "/knowledge", signed, token())
        nonce += 1
        print(f"     已提交(nonce {nonce - 1}),等入块…")
        deadline = time.time() + 90
        while time.time() < deadline:
            if account_nonce(args.rpc, args.account) >= nonce:
                break
            time.sleep(0.5)
        else:
            print("   尚未确认入块,稍后用 zx follow 查看。")
            break
        print("     已入块。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
