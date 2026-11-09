#!/usr/bin/env python3
"""把 docs/pilot/<领域>/ 下的知识单元导入一条运行中的链(测试网阶段 1,K6)。

    python3 tools/kimport.py --dir docs/pilot/fitness --domain 2 \\
        --config .localnet/node.toml --rpc http://127.0.0.1:9221 \\
        --key 1=.localnet/keys/acct1.seed [--store .localnet/bodies] [--map .localnet/kimport.json]

    # 远程测试网(HTTPS,经 nginx):不需要节点配置,令牌从环境变量读取,不出现在命令行
    ZHIXING_RPC_TOKEN=... python3 tools/kimport.py --dir docs/pilot/fitness --domain 2 \\
        --rpc https://blockchain.yeying.pub/rpc --bodies https://blockchain.yeying.pub/bodies \\
        --key 2=zhixing-member.json

做的事:
  1. 用 tools/kgraph.py 的同一个解析器读取单元文件(两个工具对文件的理解完全一致);
  2. 正文哈希 = 单元文件字节的 SHA-256(与 `node hash-content` 相同);`--store` 时把文件存为
     <store>/<哈希>,即测试网的内容寻址正文仓库;
  3. 按"依据"关系的拓扑序创建单元("依据"按规则无环)。链上一个版本只能引用已存在的版本,
     所以指向尚未创建单元的"参见"先不写入,全部创建完后,再为引用方**发布 v2** 把它补上
     ——这正是版本化(U1)的用法;
  4. 每条 op 用 `node encode-knowledge-op` 签名,用 `node rpc --path /knowledge --post` 提交
     (自动带上配置里的 RPC 令牌);不给 `--config` 时直接 HTTP POST 到 `--rpc`/knowledge,
     令牌取自环境变量 ZHIXING_RPC_TOKEN。等待入块后再提交下一条;序号每次从链上读取。

纯 Python 标准库。作者账户由 `--key ID=SEEDFILE` 指定(种子文件或加密 keystore 均可,后者需
ZHIXING_KEYSTORE_PASSWORD)。
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import kgraph  # noqa: E402  同目录的解析器

LAYERS = {"基础": "foundation", "解释": "explanation", "场景": "scenario", "应用": "application"}
DEFAULT_MODEL = "claude-opus-5-5"


def source_arg(src: str) -> str:
    """把单元头部的"来源"映射为 CLI 的 --source。"""
    if "人工核验过的模型生成" in src:
        return f"model-verified:{DEFAULT_MODEL}"
    if "模型生成" in src:
        return f"model:{DEFAULT_MODEL}"
    return "human"


def get_json(rpc: str, path: str):
    req = urllib.request.Request(rpc + path, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise


def wait_until(pred, what: str, timeout: float = 30.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        v = pred()
        if v:
            return v
        time.sleep(0.3)
    sys.exit(f"timeout waiting for {what}")


def topo_order(units: dict) -> list:
    """按"依据"引用的拓扑序;依据成环或缺失即报错(规则要求依据无环)。"""
    order, state = [], {}

    def visit(f, stack):
        if state.get(f) == 2:
            return
        if state.get(f) == 1:
            sys.exit("依据成环: " + " → ".join(stack + [f]))
        state[f] = 1
        for c in units[f].cites:
            if c.kind == "依据":
                if c.target not in units:
                    sys.exit(f"{f} 依据的 {c.target} 不存在")
                visit(c.target, stack + [f])
        state[f] = 2
        order.append(f)

    for f in sorted(units):
        visit(f, [])
    return order


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--dir", required=True)
    ap.add_argument("--domain", type=int, required=True)
    ap.add_argument("--config", help="本地节点配置(经 node rpc 提交);不给则直接 POST 到 --rpc,令牌取自 ZHIXING_RPC_TOKEN")
    ap.add_argument("--rpc", default="http://127.0.0.1:9221")
    ap.add_argument("--key", required=True, help="作者账户:ID=种子文件或 keystore")
    ap.add_argument("--store", help="内容寻址正文仓库目录")
    ap.add_argument("--bodies", help="远程正文仓库 URL(如 https://blockchain.yeying.pub/bodies),提交前先上传正文;令牌同 ZHIXING_RPC_TOKEN")
    ap.add_argument("--map", help="写出 单元文件 → 链上 id 的映射(JSON)")
    ap.add_argument("--bin", default=os.path.join(os.path.dirname(__file__), "..", "node", "target", "release", "node"))
    a = ap.parse_args()
    actor_s, keyfile = a.key.split("=", 1)
    actor = int(actor_s)

    files = [f for f in sorted(os.listdir(a.dir)) if f.endswith(".md") and f not in ("README.md", "GRAPH.md")]
    units = {}
    for f in files:
        u = kgraph.parse(os.path.join(a.dir, f))
        if "层级" in u.meta:
            units[u.file] = u
    order = topo_order(units)

    def nonce() -> int:
        acct = get_json(a.rpc, f"/account/{actor}")
        if acct is None:
            sys.exit(f"account {actor} not found on chain")
        return int(acct["nonce"])

    def upload_body(h: str, data: bytes) -> None:
        token = os.environ.get("ZHIXING_RPC_TOKEN", "")
        req = urllib.request.Request(a.bodies.rstrip("/") + "/" + h, data=data, method="PUT",
                                     headers={"Authorization": "Bearer " + token,
                                              "Content-Type": "text/markdown; charset=utf-8"})
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                print(f"  body {h[:12]}… {resp.read().decode().strip()}")
        except urllib.error.HTTPError as e:
            sys.exit(f"body upload failed: HTTP {e.code} {e.read().decode(errors='replace').strip()}")

    def submit(argv: list[str]) -> str:
        with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as t:
            out = t.name
        enc = subprocess.run([a.bin, "encode-knowledge-op", "--key-file", keyfile, "--out", out,
                              "--actor", str(actor), "--nonce", str(nonce()), *argv],
                             capture_output=True, text=True)
        if enc.returncode != 0:
            sys.exit("encode failed: " + enc.stderr.strip())
        if a.config:
            r = subprocess.run([a.bin, "rpc", "--config", a.config, "--path", "/knowledge", "--post", out],
                               capture_output=True, text=True)
            os.unlink(out)
            if r.returncode != 0:
                sys.exit("submit failed: " + (r.stderr or r.stdout).strip())
            return r.stdout.strip()
        with open(out, "rb") as fh:
            body = fh.read()
        os.unlink(out)
        headers = {"Content-Type": "application/octet-stream"}
        token = os.environ.get("ZHIXING_RPC_TOKEN", "")
        if token:
            headers["Authorization"] = "Bearer " + token
        req = urllib.request.Request(a.rpc + "/knowledge", data=body, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=15) as resp:
                return resp.read().decode().strip()
        except urllib.error.HTTPError as e:
            sys.exit(f"submit failed: HTTP {e.code} {e.read().decode(errors='replace').strip()}")

    ids: dict[str, int] = {}
    hashes: dict[str, str] = {}
    deferred: dict[str, list] = {}
    for f in order:
        u = units[f]
        data = open(os.path.join(a.dir, f), "rb").read()
        h = hashlib.sha256(data).hexdigest()
        hashes[f] = h
        if a.store:
            os.makedirs(a.store, exist_ok=True)
            open(os.path.join(a.store, h), "wb").write(data)
        if a.bodies:
            upload_body(h, data)
        cites, later = [], []
        for c in u.cites:
            kind = "basis" if c.kind == "依据" else "see-also"
            if c.target in ids:
                cites += ["--cite", f"{ids[c.target]}:1:{kind}"]
            else:
                later.append((c.target, kind))
        if later:
            deferred[f] = later
        layer = LAYERS.get(u.layer)
        if not layer:
            sys.exit(f"{f}: unknown layer {u.layer}")
        expect = (get_json(a.rpc, "/units?limit=1") or {}).get("total", "0")
        expect_id = int(expect) + 1
        submit(["--action", "create", "--layer", layer, "--domain", str(a.domain),
                "--body", f"zh:{h}", "--source", source_arg(u.source), *cites])
        wait_until(lambda: get_json(a.rpc, f"/unit/{expect_id}"), f"unit {expect_id} ({f})")
        ids[f] = expect_id
        print(f"created  unit {expect_id:>3}  {u.title}  ({layer})")

    # 补上先前无法写入的"参见":发布 v2,引用列表为完整列表
    for f, later in deferred.items():
        u = units[f]
        cites = []
        for c in u.cites:
            kind = "basis" if c.kind == "依据" else "see-also"
            cites += ["--cite", f"{ids[c.target]}:1:{kind}"]
        submit(["--action", "publish", "--unit", str(ids[f]), "--body", f"zh:{hashes[f]}",
                "--source", source_arg(u.source), *cites])
        wait_until(lambda: len((get_json(a.rpc, f"/unit/{ids[f]}") or {}).get("versions", [])) >= 2,
                   f"unit {ids[f]} v2")
        print(f"published unit {ids[f]:>3} v2  {u.title}  (补上参见: {', '.join(units[t].title for t, _ in later)})")

    if a.map:
        json.dump({"domain": a.domain, "units": ids, "body_sha256": hashes}, open(a.map, "w"),
                  ensure_ascii=False, indent=2)
        print(f"wrote {a.map}")


if __name__ == "__main__":
    main()
