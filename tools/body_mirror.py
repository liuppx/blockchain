#!/usr/bin/env python3
"""正文镜像:把链上引用到的每一份正文(含核验说明)复制一份到本地目录,逐份核对哈希。

链上只存正文的 SHA-256,正文本身在浏览器服务的正文仓库里。仓库只有一份时,那台机器的盘坏了,
链上的哈希就成了空壳——这个工具让任何人都能保存一份完整、可核对的副本。

    # 镜像:只取本地还没有(或哈希不对)的正文;可以反复跑
    python3 tools/body_mirror.py --dest /data/zhixing-testnet/bodies-mirror

    # 只检查:正文仓库是否还能取回链上引用的每一份正文(不写盘;缺了就非零退出,给监控用)
    python3 tools/body_mirror.py --check

    # 恢复:把本地副本里、正文仓库缺的那些传回去(需要写入令牌,放在环境变量 ZHIXING_BODY_TOKEN)
    python3 tools/body_mirror.py --dest <副本目录> --restore-to https://blockchain.yeying.pub

引用从哪来:遍历链上全部单元的**全部版本**(不只是最新版本),收集每个版本的正文哈希,
以及每条核验写了说明时的说明哈希(全 0 表示没写说明)。

每次镜像都在副本目录写 STATUS.json(时间、引用数、已有、缺失清单),监控读它判断副本是否完整、是否按时更新;
MANIFEST.json 记录每个哈希被谁引用,人工排查时用。

纯 Python 标准库,不依赖浏览器服务的代码,在节点机器上直接可用。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

RPC = "https://blockchain.yeying.pub/rpc"
SITE = "https://blockchain.yeying.pub"
HEX64 = re.compile(r"^[0-9a-f]{64}$")
ZERO = "0" * 64
MAX_BODY = 64 * 1024          # 与正文仓库的上限一致
UA = "zhixing-body-mirror/1"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def retry(fn, tries: int = 3, wait: float = 2.0):
    """网络偶尔超时不该让整次镜像失败:最多试 3 次。404 这类明确结果不重试。"""
    for i in range(tries):
        try:
            return fn()
        except urllib.error.HTTPError:
            raise
        except (urllib.error.URLError, TimeoutError, ConnectionError, OSError):
            if i == tries - 1:
                raise
            time.sleep(wait * (i + 1))


def get_json(url: str, timeout: float = 30):
    req = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": UA})
    def once():
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.load(r)
    return retry(once)


def get_bytes(url: str, timeout: float = 30) -> bytes | None:
    """取回一份正文;404 返回 None,其他错误抛出(网络问题不能当作"正文不存在")。"""
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    def once():
        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return r.read(MAX_BODY + 1)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            raise
    return retry(once)


# ---- 链上引用 -----------------------------------------------------------------

def unit_ids(rpc: str, fetch=get_json) -> list[int]:
    ids, cursor = [], None
    for _ in range(10000):
        q = "/units?limit=200" + ("&cursor=" + urllib.parse.quote(str(cursor)) if cursor else "")
        page = fetch(rpc + q)
        items = page.get("items") or []
        ids += [int(it["id"]) for it in items]
        cursor = page.get("cursor")
        if not cursor or not items:
            break
    return ids


def refs_of_unit(u: dict) -> dict[str, list[str]]:
    """一个单元引用的全部哈希 → 引用说明。"""
    out: dict[str, list[str]] = {}
    uid = u["id"]
    for v in u.get("versions") or []:
        n = v["n"]
        for b in v.get("bodies") or []:
            h = str(b.get("hash", "")).lower()
            if HEX64.match(h):
                out.setdefault(h, []).append(f"单元 #{uid} v{n} 正文({b.get('lang', '?')})")
        for r in v.get("verifications") or []:
            h = str(r.get("note_hash") or "").lower()
            if HEX64.match(h) and h != ZERO:
                out.setdefault(h, []).append(f"单元 #{uid} v{n} 核验说明(账户 #{r.get('verifier', '?')})")
    return out


def collect_refs(rpc: str, fetch=get_json) -> dict[str, list[str]]:
    refs: dict[str, list[str]] = {}
    for uid in unit_ids(rpc, fetch):
        for h, why in refs_of_unit(fetch(f"{rpc}/unit/{uid}")).items():
            refs.setdefault(h, []).extend(why)
    return refs


# ---- 本地副本 -----------------------------------------------------------------

def local_ok(dest: str, h: str) -> bool:
    p = os.path.join(dest, h)
    try:
        with open(p, "rb") as f:
            return sha256(f.read()) == h
    except OSError:
        return False


def write_atomic(dest: str, name: str, data: bytes) -> None:
    fd, tmp = tempfile.mkstemp(dir=dest, prefix="." + name[:8], suffix=".tmp")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.chmod(tmp, 0o644)
        os.replace(tmp, os.path.join(dest, name))
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def mirror(refs: dict[str, list[str]], sources: list[str], dest: str, fetch=get_bytes) -> dict:
    """把缺的(或哈希不对的)正文从各来源依次取回,核对哈希后写入副本。"""
    os.makedirs(dest, exist_ok=True)
    have, stored, missing, errors = 0, [], [], []
    for h in sorted(refs):
        if local_ok(dest, h):
            have += 1
            continue
        got = False
        for src in sources:
            try:
                data = fetch(f"{src.rstrip('/')}/bodies/{h}")
            except Exception as e:  # noqa: BLE001 — 换下一个来源,最后统一报告
                errors.append(f"{h[:12]} ← {src}: {e}")
                continue
            if data is None:
                continue
            if len(data) > MAX_BODY or sha256(data) != h:
                errors.append(f"{h[:12]} ← {src}: 内容与哈希不符,已丢弃")
                continue
            write_atomic(dest, h, data)
            stored.append(h)
            got = True
            break
        if not got:
            missing.append(h)
    return {"refs": len(refs), "have": have + len(stored), "stored": stored, "missing": missing, "errors": errors}


def check_remote(refs: dict[str, list[str]], site: str, fetch=get_bytes) -> dict:
    """不写盘:正文仓库能否取回每一份正文,且内容与哈希相符。"""
    ok, missing, errors = 0, [], []
    for h in sorted(refs):
        try:
            data = fetch(f"{site.rstrip('/')}/bodies/{h}")
        except Exception as e:  # noqa: BLE001
            errors.append(f"{h[:12]}: {e}")
            continue
        if data is None or sha256(data) != h:
            missing.append(h)
        else:
            ok += 1
    return {"refs": len(refs), "ok": ok, "missing": missing, "errors": errors}


def restore(dest: str, hashes: list[str], site: str, token: str, put=None) -> dict:
    """把本地副本里的正文传回正文仓库(仓库会再核对一次哈希)。"""
    def _put(url: str, data: bytes) -> int:
        req = urllib.request.Request(url, data=data, method="PUT", headers={
            "Authorization": "Bearer " + token, "Content-Type": "text/markdown; charset=utf-8", "User-Agent": UA})
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status
    put = put or _put
    done, failed = [], []
    for h in hashes:
        if not local_ok(dest, h):
            failed.append(f"{h[:12]}: 本地副本里没有或已损坏")
            continue
        with open(os.path.join(dest, h), "rb") as f:
            data = f.read()
        try:
            status = put(f"{site.rstrip('/')}/bodies/{h}", data)
            (done if status in (200, 201) else failed).append(h if status in (200, 201) else f"{h[:12]}: HTTP {status}")
        except Exception as e:  # noqa: BLE001
            failed.append(f"{h[:12]}: {e}")
    return {"restored": done, "failed": failed}


def write_reports(dest: str, refs: dict[str, list[str]], result: dict, sources: list[str],
                  source_check: dict | None = None) -> None:
    status = {
        "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "unix": int(time.time()),
        "sources": sources,
        "refs": result["refs"],
        "have": result["have"],
        "stored_now": len(result["stored"]),
        "missing": [{"hash": h, "refs": refs[h]} for h in result["missing"]],
        "errors": result["errors"][:50],
    }
    if source_check is not None:
        # 主仓库丢了副本里已有的正文时,镜像本身发现不了(它只取自己缺的),所以每次另外核一遍主仓库
        status["source"] = sources[0]
        status["source_ok"] = source_check["ok"]
        status["source_missing"] = [{"hash": h, "refs": refs[h]} for h in source_check["missing"]]
        status["source_errors"] = source_check["errors"][:50]
    write_atomic(dest, "STATUS.json", json.dumps(status, ensure_ascii=False, indent=2).encode())
    write_atomic(dest, "MANIFEST.json", json.dumps(refs, ensure_ascii=False, indent=2, sort_keys=True).encode())


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="知行图正文镜像:复制、检查、恢复")
    ap.add_argument("--rpc", default=os.environ.get("ZHIXING_RPC", RPC), help="读链上引用的 RPC")
    ap.add_argument("--source", action="append", help="取正文的站点(可多次给出,依次尝试;默认公开站点)")
    ap.add_argument("--dest", help="本地副本目录")
    ap.add_argument("--check", action="store_true", help="只检查正文仓库是否完整,不写盘")
    ap.add_argument("--restore-to", metavar="SITE", help="把副本里、该站点缺的正文传回去")
    ap.add_argument("--verify-source", action="store_true",
                    help="镜像后再完整核对一遍第一个来源(主仓库),结果写进 STATUS.json")
    args = ap.parse_args(argv)
    sources = args.source or [SITE]

    try:
        refs = collect_refs(args.rpc.rstrip("/"))
    except Exception as e:  # noqa: BLE001
        print(f"读不到链上引用({args.rpc}):{e}", file=sys.stderr)
        return 2
    print(f"链上引用的正文与核验说明:{len(refs)} 份")

    if args.check:
        site = sources[0]
        r = check_remote(refs, site)
        print(f"正文仓库 {site}:{r['ok']}/{r['refs']} 份可取回且哈希相符")
        for h in r["missing"]:
            print(f"  ✗ 缺失 {h}  ← {';'.join(refs[h])}")
        for e in r["errors"]:
            print(f"  ! {e}", file=sys.stderr)
        return 1 if r["missing"] or r["errors"] else 0

    if not args.dest:
        print("需要 --dest(副本目录),或用 --check 只检查", file=sys.stderr)
        return 2

    if args.restore_to:
        token = os.environ.get("ZHIXING_BODY_TOKEN", "")
        if not token:
            print("恢复需要环境变量 ZHIXING_BODY_TOKEN(正文仓库的写入令牌)", file=sys.stderr)
            return 2
        gap = check_remote(refs, args.restore_to)
        print(f"{args.restore_to} 缺 {len(gap['missing'])} 份")
        r = restore(args.dest, gap["missing"], args.restore_to, token)
        print(f"已传回 {len(r['restored'])} 份")
        for f in r["failed"]:
            print(f"  ✗ {f}", file=sys.stderr)
        return 1 if r["failed"] else 0

    r = mirror(refs, sources, args.dest)
    sc = check_remote(refs, sources[0]) if args.verify_source else None
    write_reports(args.dest, refs, r, sources, sc)
    print(f"副本 {args.dest}:{r['have']}/{r['refs']} 份齐全(本次新取 {len(r['stored'])} 份)")
    for h in r["missing"]:
        print(f"  ✗ 哪里都取不到 {h}  ← {';'.join(refs[h])}")
    for e in r["errors"]:
        print(f"  ! {e}", file=sys.stderr)
    bad = bool(r["missing"])
    if sc is not None:
        print(f"主仓库 {sources[0]}:{sc['ok']}/{sc['refs']} 份可取回")
        for h in sc["missing"]:
            mine = "有" if local_ok(args.dest, h) else "也没有"
            print(f"  ✗ 主仓库缺 {h}(副本{mine})  ← {';'.join(refs[h])}")
        bad = bad or bool(sc["missing"])
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
