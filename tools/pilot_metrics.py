#!/usr/bin/env python3
"""知行图试运行指标:从链上读出核验记录,算出试运行要测的数(docs/pilot/PLAN.md)。

仿真(docs/VISION.md §4b)留下三个只能在真实运行中测的参数:
  1. 审查者发现错误的能力   ← 埋错单元(答案先承诺、后公开)被判「需修改」的比例
  2. 担保人把关的严格程度   ← 按邀请人汇总其邀请的人的核验准确率
  3. 攻击者能走多远         ← 红队账户的单元被判通过、红队互相核验通过的情况
另外报告运转指标:每个领域的核验覆盖、首次核验等了多少块、被指派者的完成率。

纯 Python 标准库,只读链,不签名、不提交:
    python3 tools/pilot_metrics.py --rpc https://blockchain.yeying.pub/rpc --site https://blockchain.yeying.pub
    python3 tools/pilot_metrics.py --rpc http://127.0.0.1:9221 --since-height 1000 \\
        --answer-key key.json --commitment <sha256>

先承诺、后公开(与 tools/pilot_plant.py 一致):
  - 试运行开始时公布的是**埋错清单 plants.json** 的 SHA-256——它在发布前就定稿,那时版本号还不知道;
  - 答案文件 key.json 由 pilot_plant.py 在发布后写出(带实际版本号),结束时与 plants.json 一起公开。
  核对分两步:① plants.json 的 SHA-256 等于公布的承诺;② key.json 里每个埋错版本的正文,
  确实等于前一版正文按清单做了那几处替换(从链上与正文仓库逐个重算)。两步都过才算指标。

    python3 tools/pilot_metrics.py --rpc … --site … --since-height H \\
        --plants plants.json --commitment <sha256> --answer-key key.json

答案文件格式(pilot_plant.py 写出):
    {"planted":  [{"unit": 3, "version": 2, "body_sha256": "…", "plants": [{"id": "F1", "note": "…"}]}],
     "red_team": [7, 8]}
"""
from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import sys
import os
import urllib.request

PASS, NEEDS = "pass", "needs-revision"


# ---- 读链 ------------------------------------------------------------------------

def get_json(url: str):
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def fetch(rpc: str, site: str | None):
    """返回 (units, members, assignments)。units 是 /unit/{id} 的完整 JSON。"""
    rpc = rpc.rstrip("/")
    ids = [u["id"] for u in get_json(f"{rpc}/units?limit=1000")["items"]]
    units = [get_json(f"{rpc}/unit/{i}") for i in ids]
    members = get_json(f"{rpc}/members?limit=1000")["items"]
    assignments = {}
    if site:
        for u in units:
            for v in u["versions"]:
                try:
                    a = get_json(f"{site.rstrip('/')}/assign?unit={u['id']}&version={v['n']}")
                    assignments[(u["id"], v["n"])] = [str(x) for x in a.get("assigned", [])]
                except Exception:  # 浏览器服务不可用时不报指派完成率
                    pass
    return units, members, assignments


# ---- 核对答案(先承诺、后公开)------------------------------------------------------

def verify_answer_key(units: list[dict], plans: dict, key: dict, fetch_body) -> list[str]:
    """核对 key.json 是按承诺过的清单发布的:每个埋错版本的正文 = 前一版正文做了清单里那几处替换。

    返回问题列表(空 = 全部对得上)。`fetch_body(sha256) -> str` 从正文仓库取正文。
    这一步防的是:组织者事后挑一个"有人恰好判了需修改"的版本,声称那就是埋错。
    """
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import pilot_plant  # 同一套替换规则(原文必须恰好出现一次)

    by_id = {str(u["id"]): u for u in units}
    by_unit = pilot_plant.by_unit(plans)
    problems = []
    for p in key.get("planted", []):
        uid, n = str(p["unit"]), int(p["version"])
        u = by_id.get(uid)
        if not u:
            problems.append(f"单元 #{uid} 在链上不存在")
            continue
        vs = {int(v["n"]): v for v in u["versions"]}
        if n not in vs or (n - 1) not in vs:
            problems.append(f"单元 #{uid} 没有 v{n} 或它的前一版")
            continue
        listed = by_unit.get(int(uid))
        if not listed:
            problems.append(f"单元 #{uid} v{n} 不在承诺过的清单里")
            continue
        try:
            prev = fetch_body(vs[n - 1]["bodies"][0]["hash"])
            want = pilot_plant.apply_all(prev, listed)
        except Exception as e:  # noqa: BLE001
            problems.append(f"单元 #{uid} v{n}:按清单重算失败({e})")
            continue
        got = vs[n]["bodies"][0]["hash"]
        if hashlib.sha256(want.encode("utf-8")).hexdigest() != got:
            problems.append(f"单元 #{uid} v{n} 的正文不是前一版按清单替换的结果")
    return problems


# ---- 指标(纯函数,方便测试)--------------------------------------------------------

def reviews_of(version: dict, since: int = 0) -> list[dict]:
    """某版本的独立核验:不含发布者自己,只算 since 之后的。"""
    pub = str(version["publisher"])
    return [r for r in version.get("verifications", [])
            if str(r["verifier"]) != pub and int(r["height"]) >= since]


def coverage(units: list[dict], since: int = 0) -> dict:
    """每个领域:试运行期间发布的版本数、有独立核验的版本数、首次核验等了多少块。"""
    out: dict[str, dict] = {}
    for u in units:
        d = out.setdefault(str(u["domain"]), {"versions": 0, "reviewed": 0, "waits": [], "reviews": 0})
        for v in u["versions"]:
            if int(v["published_height"]) < since:
                continue
            d["versions"] += 1
            rs = reviews_of(v)
            d["reviews"] += len(rs)
            if rs:
                d["reviewed"] += 1
                d["waits"].append(min(int(r["height"]) for r in rs) - int(v["published_height"]))
    for d in out.values():
        d["median_wait"] = statistics.median(d["waits"]) if d["waits"] else None
    return out


def detection(units: list[dict], planted: list[dict], since: int = 0) -> dict:
    """埋错版本被发现的比例,以及没埋错的版本被误判「需修改」的比例,按领域分。"""
    keys = {(str(p["unit"]), str(p["version"])) for p in planted}
    out: dict[str, dict] = {}
    for u in units:
        d = out.setdefault(str(u["domain"]), {"planted": 0, "caught": 0, "planted_reviews": 0,
                                              "planted_needs": 0, "clean_reviews": 0, "clean_needs": 0})
        for v in u["versions"]:
            rs = reviews_of(v, since)
            needs = sum(1 for r in rs if r["verdict"] == NEEDS)
            if (str(u["id"]), str(v["n"])) in keys:
                d["planted"] += 1
                d["caught"] += 1 if needs else 0
                d["planted_reviews"] += len(rs)
                d["planted_needs"] += needs
            else:
                d["clean_reviews"] += len(rs)
                d["clean_needs"] += needs
    return out


def reviewer_accuracy(units: list[dict], planted: list[dict], since: int = 0) -> dict:
    """每位核验人:看过几个埋错版本、抓到几个;对干净版本判了几次「需修改」。"""
    keys = {(str(p["unit"]), str(p["version"])) for p in planted}
    acc: dict[str, dict] = {}
    for u in units:
        for v in u["versions"]:
            bad = (str(u["id"]), str(v["n"])) in keys
            for r in reviews_of(v, since):
                a = acc.setdefault(str(r["verifier"]), {"planted": 0, "caught": 0, "clean": 0, "false_alarm": 0})
                if bad:
                    a["planted"] += 1
                    a["caught"] += 1 if r["verdict"] == NEEDS else 0
                else:
                    a["clean"] += 1
                    a["false_alarm"] += 1 if r["verdict"] == NEEDS else 0
    return acc


def by_inviter(members: list[dict], acc: dict) -> dict:
    """担保人把关:每位邀请人邀请了谁、这些人在埋错版本上的准确率合计。"""
    out: dict[str, dict] = {}
    for m in members:
        inv = str(m["inviter"])
        if inv == "0":  # 创世成员没有邀请人
            continue
        o = out.setdefault(inv, {"invited": 0, "active": 0, "planted": 0, "caught": 0})
        o["invited"] += 1
        a = acc.get(str(m["id"]))
        if a:
            o["active"] += 1
            o["planted"] += a["planted"]
            o["caught"] += a["caught"]
    return out


def red_team(units: list[dict], red: list, since: int = 0) -> dict:
    """红队:他们发布的版本有几个被判通过多于需修改(逃过);他们之间互相核验通过几次。"""
    red = {str(x) for x in red}
    out = {"versions": 0, "escaped": 0, "collusive_passes": 0, "caught_after": []}
    for u in units:
        for v in u["versions"]:
            rs = reviews_of(v, since)
            if str(v["publisher"]) in red:
                out["versions"] += 1
                honest = [r for r in rs if str(r["verifier"]) not in red]
                passes = sum(1 for r in honest if r["verdict"] == PASS)
                needs = [r for r in honest if r["verdict"] == NEEDS]
                if passes > len(needs):
                    out["escaped"] += 1
                if needs:
                    out["caught_after"].append(min(int(r["height"]) for r in needs) - int(v["published_height"]))
                out["collusive_passes"] += sum(1 for r in rs if str(r["verifier"]) in red and r["verdict"] == PASS)
    return out


ZERO = "0" * 64


def note_quality(units: list[dict], since: int = 0) -> dict:
    """每个领域:「需修改」里写了说明的比例。没写说明的「需修改」修订者无从改起,也统计不了发现了什么错。"""
    out: dict[str, dict] = {}
    for u in units:
        d = out.setdefault(str(u["domain"]), {"needs": 0, "with_note": 0, "notes": []})
        for v in u["versions"]:
            for r in reviews_of(v, since):
                if r["verdict"] != NEEDS:
                    continue
                d["needs"] += 1
                h = str(r.get("note_hash") or ZERO)
                if h != ZERO:
                    d["with_note"] += 1
                    d["notes"].append({"unit": str(u["id"]), "version": str(v["n"]),
                                       "verifier": str(r["verifier"]), "note_hash": h})
    return out


def assignment_compliance(units: list[dict], assignments: dict) -> tuple[int, int]:
    """被指派的人里,有几位真的核验了这个版本。"""
    assigned = done = 0
    for u in units:
        for v in u["versions"]:
            who = assignments.get((str(u["id"]), str(v["n"])))
            if not who:
                continue
            verifiers = {str(r["verifier"]) for r in v.get("verifications", [])}
            assigned += len(who)
            done += sum(1 for w in who if w in verifiers)
    return done, assigned


# ---- 报告 ------------------------------------------------------------------------

DOMAIN = {"1": "大模型应用开发", "2": "运动健身"}


def pct(a: int, b: int) -> str:
    return f"{a}/{b}({100 * a / b:.0f}%)" if b else "—"


def blocks(n, sec_per_block: float) -> str:
    return "—" if n is None else f"{n:.0f} 块(≈{n * sec_per_block / 3600:.1f} 小时)"


def report(units, members, assignments, key, since, sec_per_block) -> str:
    planted = key.get("planted", []) if key else []
    lines = ["# 知行图试运行指标", "", f"统计起点:高度 {since}。只算独立核验(不含发布者自己)。", ""]
    lines += ["## 运转", "", "| 领域 | 发布的版本 | 有独立核验 | 核验次数 | 首次核验等待(中位数) |", "|---|---|---|---|---|"]
    for dom, d in sorted(coverage(units, since).items()):
        lines.append(f"| {DOMAIN.get(dom, dom)} | {d['versions']} | {pct(d['reviewed'], d['versions'])} | "
                     f"{d['reviews']} | {blocks(d['median_wait'], sec_per_block)} |")
    done, total = assignment_compliance(units, assignments)
    lines += ["", f"被指派者完成率:{pct(done, total)}" + ("" if total else "(没有读到指派数据:加 --site)"), ""]
    nq = note_quality(units, since)
    lines += ["### 「需修改」的说明", "", "| 领域 | 写了说明的「需修改」 |", "|---|---|"]
    for dom, d in sorted(nq.items()):
        lines.append(f"| {DOMAIN.get(dom, dom)} | {pct(d['with_note'], d['needs'])} |")
    listed = [n for d in nq.values() for n in d["notes"]]
    if listed:
        lines += ["", "说明原文在正文仓库:`<浏览器>/bodies/<哈希>`。", ""]
        for n in listed:
            lines.append(f"- 单元 #{n['unit']} v{n['version']} · 核验人 #{n['verifier']} · `{n['note_hash'][:16]}…`")
    lines.append("")

    if not key:
        lines += ["## 发现错误的能力 / 担保 / 红队", "",
                  "没有答案文件(--answer-key):试运行结束、公开答案后再算这三项。", ""]
        return "\n".join(lines)

    lines += ["## 1. 审查者发现错误的能力", "",
              "| 领域 | 埋错版本被发现 | 埋错版本上判「需修改」的比例 | 干净版本被误判「需修改」 |", "|---|---|---|---|"]
    for dom, d in sorted(detection(units, planted, since).items()):
        lines.append(f"| {DOMAIN.get(dom, dom)} | {pct(d['caught'], d['planted'])} | "
                     f"{pct(d['planted_needs'], d['planted_reviews'])} | {pct(d['clean_needs'], d['clean_reviews'])} |")
    acc = reviewer_accuracy(units, planted, since)
    lines += ["", "| 核验人 | 抓到的埋错 | 误判 |", "|---|---|---|"]
    for who, a in sorted(acc.items(), key=lambda x: int(x[0])):
        lines.append(f"| #{who} | {pct(a['caught'], a['planted'])} | {pct(a['false_alarm'], a['clean'])} |")

    lines += ["", "## 2. 担保人把关", "", "| 邀请人 | 邀请 | 参与了核验 | 被邀请者抓到的埋错 |", "|---|---|---|---|"]
    for inv, o in sorted(by_inviter(members, acc).items(), key=lambda x: int(x[0])):
        lines.append(f"| #{inv} | {o['invited']} | {o['active']} | {pct(o['caught'], o['planted'])} |")

    rt = red_team(units, key.get("red_team", []), since)
    wait = statistics.median(rt["caught_after"]) if rt["caught_after"] else None
    lines += ["", "## 3. 红队", "",
              f"- 红队发布的版本:{rt['versions']},逃过(诚实核验里通过多于需修改):{pct(rt['escaped'], rt['versions'])}",
              f"- 红队之间互相核验通过:{rt['collusive_passes']} 次",
              f"- 被抓到时已过去(中位数):{blocks(wait, sec_per_block)}", ""]
    return "\n".join(lines)


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="知行图试运行指标(只读链)")
    ap.add_argument("--rpc", default="http://127.0.0.1:9221", help="节点 RPC(线上:https://blockchain.yeying.pub/rpc)")
    ap.add_argument("--site", help="浏览器服务地址,用来读核验人指派(可选)")
    ap.add_argument("--since-height", type=int, default=0, help="只算这个高度之后的活动(试运行开始的高度)")
    ap.add_argument("--answer-key", help="埋错与红队的答案文件 key.json(pilot_plant.py 发布后写出)")
    ap.add_argument("--plants", help="埋错清单 plants.json(试运行开始时公布了它的 SHA-256)")
    ap.add_argument("--commitment", help="试运行开始时公布的埋错清单 SHA-256;给了就核对 --plants")
    ap.add_argument("--seconds-per-block", type=float, default=1.0, help="把块数换算成时间(测试网 1 秒一块)")
    args = ap.parse_args(argv)

    key = None
    if args.answer_key:
        key = json.loads(open(args.answer_key, "rb").read())
    # 参数与本地文件先核对完,再去读链(给错参数不该先打一遍网络)
    raw = None
    if args.commitment:
        # 承诺的是清单,不是答案文件:答案文件带版本号,发布前根本不存在
        if not args.plants:
            print("给了 --commitment 就要给 --plants(承诺的是埋错清单的 SHA-256)", file=sys.stderr)
            return 2
        raw = open(args.plants, "rb").read()
        if hashlib.sha256(raw).hexdigest() != args.commitment.lower():
            print("埋错清单与公布的 SHA-256 不一致:清单被改过,不能用它算指标", file=sys.stderr)
            return 1
        if key is not None and not args.site:
            print("核对答案文件要从正文仓库取正文:请给 --site", file=sys.stderr)
            return 2
    units, members, assignments = fetch(args.rpc, args.site)
    if args.commitment:
        if key is not None:
            site = args.site.rstrip("/")

            def fetch_body(h: str) -> str:
                with urllib.request.urlopen(f"{site}/bodies/{h}", timeout=30) as r:
                    return r.read().decode("utf-8")

            problems = verify_answer_key(units, json.loads(raw), key, fetch_body)
            if problems:
                print("答案文件与承诺的清单对不上,不能用它算指标:", file=sys.stderr)
                for p in problems:
                    print("  - " + p, file=sys.stderr)
                return 1
        print("承诺核对通过:清单未被改过,答案文件里的每个埋错版本都是按清单发布的。\n")
    print(report(units, members, assignments, key, args.since_height, args.seconds_per_block))
    return 0


if __name__ == "__main__":
    sys.exit(main())
