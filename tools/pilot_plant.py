#!/usr/bin/env python3
"""试运行埋错的发布工具:把埋错清单里的每一处改动,发布为对应单元的新版本,并写出答案文件。

    # 1. 只检查(默认):每处「原文」在链上当前版本正文里是否恰好出现一次,不签名、不提交
    python3 tools/pilot_plant.py --plants .pilot/plants.json

    # 2. 发布(组织者执行):签名、上传正文、提交,等入块,写出答案文件
    python3 tools/pilot_plant.py --plants .pilot/plants.json --publish \\
        --key <账户号>=<组织者的种子文件> --out .pilot/key.json

埋错清单(JSON,不进 git,放在 .pilot/):
    {"plants": [
       {"id": "F1", "unit": 6, "old": "组间休息 60–90 秒。", "new": "组间休息 10–15 秒。"},
       ...],
     "red_team": [7, 8]}

答案文件按**发布后的实际版本号**写出,形如 tools/pilot_metrics.py 读的格式:
    {"planted": [{"id": "F1", "unit": 6, "version": 3, "note": "..."}], "red_team": [...]}

先承诺、后公开:**试运行开始时公布埋错清单 plants.json 的 SHA-256**(它在发布前就定稿,
版本号还不知道);结束时公开 plants.json 与 key.json。任何人都能核对 plants.json 没被改过,
再从链上核对 key.json 里的版本确实是按清单发布的(每个版本的正文 = 前一版正文做了清单里那一处替换)。

令牌:写入令牌取自环境变量 ZHIXING_RPC_TOKEN,不出现在命令行。纯 Python 标准库。
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
import urllib.request

RPC = "https://blockchain.yeying.pub/rpc"
SITE = "https://blockchain.yeying.pub"


def get(url: str, as_json: bool = True):
    req = urllib.request.Request(url, headers={"Accept": "application/json"} if as_json else {})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r) if as_json else r.read()


def current(rpc: str, site: str, unit: int) -> tuple[dict, dict, str]:
    """单元、它的当前版本、当前版本的正文(只取第一份正文;试运行单元都是单语言)。"""
    u = get(f"{rpc}/unit/{unit}")
    v = u["versions"][-1]
    body = get(f"{site}/bodies/{v['bodies'][0]['hash']}", as_json=False).decode("utf-8")
    return u, v, body


def apply(body: str, old: str, new: str) -> str:
    """把正文里的 old 换成 new;old 必须恰好出现一次,否则拒绝(避免改错地方、或改到核验要点里)。"""
    n = body.count(old)
    if n != 1:
        raise ValueError(f"原文在正文里出现 {n} 次(必须恰好 1 次)")
    if old == new:
        raise ValueError("改动前后相同")
    return body.replace(old, new)


def by_unit(plans: dict) -> dict[int, list[dict]]:
    """同一单元的几处埋错合成**一个**新版本(否则会出现只含一部分埋错的中间版本,
    核验了中间版本的人会被按不完整的答案打分)。保持清单里的顺序。"""
    groups: dict[int, list[dict]] = {}
    for p in plans["plants"]:
        groups.setdefault(int(p["unit"]), []).append(p)
    return groups


def apply_all(body: str, plants: list[dict]) -> str:
    """依次应用同一单元的几处埋错;每处的原文都必须在**原始**正文里恰好出现一次,且几处互不重叠。"""
    for p in plants:
        if body.count(p["old"]) != 1:
            raise ValueError(f"{p['id']}:原文在正文里出现 {body.count(p['old'])} 次(必须恰好 1 次)")
    out = body
    for p in plants:
        out = apply(out, p["old"], p["new"])
    return out


def check(plans: dict, rpc: str, site: str) -> list[dict]:
    """按单元检查,不提交。返回每处埋错的状态。"""
    out = []
    for unit, plants in by_unit(plans).items():
        try:
            u, v, body = current(rpc, site, unit)
            apply_all(body, plants)
            for p in plants:
                out.append({"id": p["id"], "unit": unit, "from_version": v["n"], "ok": True, "error": ""})
        except Exception as e:  # noqa: BLE001 — 每个单元单独报告
            for p in plants:
                out.append({"id": p["id"], "unit": unit, "from_version": "?", "ok": False, "error": str(e)})
    return out


def publish(plans: dict, rpc: str, site: str, actor: int, key_file: str, node_bin: str, token: str) -> dict:
    """逐条发布:新正文 → 上传正文仓库 → 签 publish(沿用当前版本的引用与来源)→ 提交 → 等入块。"""
    planted = []
    for unit, plants in by_unit(plans).items():
        u, v, body = current(rpc, site, unit)
        ids = "+".join(p["id"] for p in plants)
        new_body = apply_all(body, plants).encode("utf-8")
        h = hashlib.sha256(new_body).hexdigest()
        put = urllib.request.Request(f"{site}/bodies/{h}", data=new_body, method="PUT",
                                     headers={"Authorization": "Bearer " + token,
                                              "Content-Type": "text/markdown; charset=utf-8"})
        with urllib.request.urlopen(put, timeout=30) as r:
            if r.status not in (200, 201):
                raise RuntimeError(f"{ids}:上传正文失败 HTTP {r.status}")
        nonce = int(get(f"{rpc}/account/{actor}")["nonce"])
        args = ["--action", "publish", "--unit", str(unit), "--body", f"{v['bodies'][0]['lang']}:{h}",
                "--source", v["source"]]
        for c in v.get("cites", []):
            args += ["--cite", f"{c['unit']}:{c['version']}:{c['kind']}"]
        with tempfile.TemporaryDirectory() as d:
            op = os.path.join(d, "op.bin")
            subprocess.run([node_bin, "encode-knowledge-op", "--key-file", key_file, "--out", op,
                            "--actor", str(actor), "--nonce", str(nonce), *args], check=True, capture_output=True)
            data = open(op, "rb").read()
        req = urllib.request.Request(f"{rpc}/knowledge", data=data, method="POST",
                                     headers={"Authorization": "Bearer " + token,
                                              "Content-Type": "application/octet-stream"})
        with urllib.request.urlopen(req, timeout=30) as r:
            resp = r.read().decode().strip()
        if len(resp) != 64:
            raise RuntimeError(f"{ids}:链拒绝了:{resp}")
        want = int(v["n"]) + 1
        for _ in range(60):  # 等新版本入块
            if int(get(f"{rpc}/unit/{unit}")["versions"][-1]["n"]) >= want:
                break
            time.sleep(2)
        else:
            raise RuntimeError(f"{ids}:等不到单元 #{unit} 的 v{want} 入块")
        # 一个版本一条记录(指标按版本算「被发现没有」);plants 列出它含的几处埋错
        planted.append({"unit": unit, "version": want, "body_sha256": h,
                        "plants": [{"id": p["id"], "note": p.get("note", "")} for p in plants]})
        print(f"  {ids}:单元 #{unit} 发布为 v{want}")
    return {"planted": planted, "red_team": plans.get("red_team", [])}


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="试运行埋错:检查或发布")
    ap.add_argument("--plants", required=True, help="埋错清单 JSON(放在 .pilot/,不进 git)")
    ap.add_argument("--rpc", default=RPC)
    ap.add_argument("--site", default=SITE)
    ap.add_argument("--publish", action="store_true", help="真的签名并提交(默认只检查)")
    ap.add_argument("--key", help="组织者账户:ID=种子文件")
    ap.add_argument("--out", help="写出答案文件(发布后,带实际版本号)")
    ap.add_argument("--bin", default=os.path.join(os.path.dirname(__file__), "..", "node", "target", "release", "node"))
    args = ap.parse_args(argv)

    raw = open(args.plants, "rb").read()
    plans = json.loads(raw)
    print(f"埋错清单 SHA-256:{hashlib.sha256(raw).hexdigest()}(试运行开始时公布这个)")

    results = check(plans, args.rpc.rstrip("/"), args.site.rstrip("/"))
    for r in results:
        print(f"  {r['id']} 单元 #{r['unit']} v{r['from_version']}:{'可以发布' if r['ok'] else '✗ ' + r['error']}")
    if not all(r["ok"] for r in results):
        print("有埋错不能干净应用:先修正清单再发布", file=sys.stderr)
        return 1
    if not args.publish:
        return 0

    if not args.key or "=" not in args.key or not args.out:
        print("发布需要 --key ID=种子文件 与 --out 答案文件", file=sys.stderr)
        return 2
    token = os.environ.get("ZHIXING_RPC_TOKEN", "")
    if not token:
        print("发布需要环境变量 ZHIXING_RPC_TOKEN(写入令牌)", file=sys.stderr)
        return 2
    actor, key_file = args.key.split("=", 1)
    key = publish(plans, args.rpc.rstrip("/"), args.site.rstrip("/"), int(actor), key_file, args.bin, token)
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(key, f, ensure_ascii=False, indent=2)
    os.chmod(args.out, 0o600)
    print(f"答案文件:{args.out}(试运行结束时公开)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
