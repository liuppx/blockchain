#!/usr/bin/env python3
"""知行图知识单元的图谱检查与可信度计算(试运行规则 v0)。

读取一个目录下的知识单元 Markdown 文件,构建引用图,做三件事:
  1. 完整性检查:引用目标是否存在、"依据"关系是否成环、引用的版本是否落后于当前版本;
  2. 计算可信度:
       自身可信度  模型生成且未经人核验 0.1;人工撰写未核验 0.2;
                   每多一位独立的人对**当前版本**核验通过,+0.2,上限 0.9。
       有效可信度  = min(自身可信度, 它所"依据"的每个单元的有效可信度)。
                   一条知识不可能比它的地基更可信;"参见"不传递可信度。
  3. 给出优先核验顺序:哪个单元卡住了最多下游单元,先核验它收益最大。

纯 Python 标准库:
    python3 tools/kgraph.py docs/pilot/fitness        # 打印报告并写出 <目录>/GRAPH.md

与链上实现(node/src/knowledge.rs,K3)的关系:规则相同,链上用万分比整数计算。两点差别:
  - 链上"依据"锁定被引用的版本,用该版本的可信度;本工具读的是文件,只有当前版本,
    所以在引用都指向最新版本时两者一致(引用落后时本工具会给出提示);
  - 链上按核验发生时核验人是否已过见习期决定是否计入;本工具没有成员数据,所有核验都计入。
同一场景的一致性由 node 的 `knowledge::tests::matches_the_kgraph_pilot_scenario` 守护。

参数是试运行第 0 版,待试运行数据校准。规则依据见 docs/VISION.md §4a(知识单元)与 §4b(质量收敛)。
"""
from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass, field

LINK = re.compile(r"\[([^\]]+)\]\(([^)#\s]+\.md)\)")

C_MODEL = 0.1
C_HUMAN = 0.2
C_STEP = 0.2
C_MAX = 0.9


@dataclass
class Cite:
    target: str  # 目标文件名(不含路径)
    kind: str  # 依据 | 参见
    version: str


@dataclass
class Verification:
    who: str
    version: str
    verdict: str


@dataclass
class Unit:
    file: str
    title: str = ""
    meta: dict = field(default_factory=dict)
    cites: list = field(default_factory=list)
    verifs: list = field(default_factory=list)
    c_self: float = 0.0
    c_eff: float = 0.0
    bottleneck: str = ""  # 决定有效可信度的那个依据单元(为空表示取决于自身)

    @property
    def layer(self) -> str:
        return self.meta.get("层级", "?")

    @property
    def version(self) -> str:
        return self.meta.get("版本", "v1")

    @property
    def source(self) -> str:
        return self.meta.get("来源", "")


def table_rows(lines: list[str]) -> list[list[str]]:
    """解析 Markdown 表格的数据行(跳过表头与分隔行)。"""
    rows = []
    seen_header = False
    for ln in lines:
        ln = ln.strip()
        if not ln.startswith("|"):
            if seen_header:
                break
            continue
        cells = [c.strip() for c in ln.strip("|").split("|")]
        if all(re.fullmatch(r":?-+:?", c) for c in cells if c):
            continue
        if not seen_header:
            seen_header = True
            continue
        rows.append(cells)
    return rows


def section(text: str, title: str) -> list[str]:
    m = re.search(rf"^## {re.escape(title)}\s*$", text, re.M)
    if not m:
        return []
    rest = text[m.end():]
    nxt = re.search(r"^## ", rest, re.M)
    return (rest[: nxt.start()] if nxt else rest).splitlines()


def parse(path: str) -> Unit:
    text = open(path, encoding="utf-8").read()
    u = Unit(file=os.path.basename(path))
    m = re.search(r"^# (.+)$", text, re.M)
    u.title = m.group(1).strip() if m else u.file
    # 头部元数据表:第一张两列表
    head = text[: text.find("\n## ")] if "\n## " in text else text
    for row in table_rows(head.splitlines()):
        if len(row) >= 2:
            u.meta[row[0]] = re.sub(r"\*", "", row[1]).strip()
    for row in table_rows(section(text, "引用")):
        m = LINK.search(row[0]) if row else None
        if not m:
            continue
        kind = row[1] if len(row) >= 4 else "依据"
        version = row[2] if len(row) >= 4 else (row[1] if len(row) >= 2 else "v1")
        u.cites.append(Cite(os.path.basename(m.group(2)), kind, version))
    for row in table_rows(section(text, "核验记录")):
        if len(row) >= 3 and row[0]:
            u.verifs.append(Verification(row[0], row[1], row[2]))
    return u


def self_credibility(u: Unit) -> float:
    passed = {v.who for v in u.verifs if v.version == u.version and "通过" in v.verdict}
    base = C_MODEL if "模型生成" in u.source and not passed else C_HUMAN
    return min(C_MAX, base + C_STEP * len(passed))


def analyze(units: dict[str, Unit]) -> dict:
    issues = {"missing": [], "cycles": [], "stale": []}
    deps = {f: [c.target for c in u.cites if c.kind == "依据"] for f, u in units.items()}
    for f, u in units.items():
        for c in u.cites:
            if c.target not in units:
                issues["missing"].append((f, c.target))
            elif c.version != units[c.target].version:
                issues["stale"].append((f, c.target, c.version, units[c.target].version))
        u.c_self = self_credibility(u)

    # 有效可信度:沿"依据"做记忆化 DFS,同时检测环
    state: dict[str, int] = {}  # 0 未访问 1 访问中 2 完成

    def visit(f: str, stack: list[str]) -> float:
        u = units[f]
        if state.get(f) == 2:
            return u.c_eff
        if state.get(f) == 1:
            cyc = stack[stack.index(f):] + [f]
            issues["cycles"].append(cyc)
            return 0.0  # 成环的依据不可信
        state[f] = 1
        eff, neck = u.c_self, ""
        for d in deps[f]:
            if d not in units:
                eff, neck = 0.0, d  # 依据的单元不存在
                continue
            de = visit(d, stack + [f])
            if de < eff:
                eff, neck = de, d
        u.c_eff, u.bottleneck = eff, neck
        state[f] = 2
        return eff

    for f in units:
        visit(f, [])

    # 优先核验:一个单元若被核验通过(+0.2),能抬高多少个下游单元的有效可信度
    leverage = {}
    for f in units:
        down = [g for g in units if reaches(g, f, deps)]
        leverage[f] = len(down)
    issues["leverage"] = leverage
    return issues


def reaches(src: str, dst: str, deps: dict) -> bool:
    """src 是否(直接或间接)依据 dst。"""
    seen, todo = set(), list(deps.get(src, []))
    while todo:
        x = todo.pop()
        if x == dst:
            return True
        if x in seen:
            continue
        seen.add(x)
        todo.extend(deps.get(x, []))
    return False


def report(units: dict[str, Unit], issues: dict) -> str:
    order = {"基础": 0, "解释": 1, "场景": 2, "应用": 3}
    us = sorted(units.values(), key=lambda u: (order.get(u.layer, 9), u.file))
    out = ["<!-- 由 tools/kgraph.py 自动生成,勿手改 -->", "# 知识图谱与可信度(试运行规则 v0)", ""]
    out += [
        "自身可信度:模型生成未核验 0.1,人工撰写未核验 0.2,每多一位独立的人对当前版本核验通过 +0.2,上限 0.9。",
        "有效可信度:自身可信度与所依据单元的有效可信度取最小。**一条知识不可能比它的地基更可信。**",
        "",
        "## 图",
        "",
        "实线为\"依据\"(传递可信度),虚线为\"参见\"。节点内为有效可信度。",
        "",
        "```mermaid",
        "graph BT",
    ]
    ids = {u.file: f"u{i}" for i, u in enumerate(us)}
    for u in us:
        out.append(f'    {ids[u.file]}["{u.title}<br/>{u.layer} · {u.c_eff:.1f}"]')
    for u in us:
        for c in u.cites:
            if c.target in ids:
                arrow = "-->" if c.kind == "依据" else "-.->"
                out.append(f"    {ids[u.file]} {arrow} {ids[c.target]}")
    out += ["```", "", "## 单元一览", ""]
    out += ["| 单元 | 层级 | 来源 | 当前版本 | 核验通过人数 | 自身可信度 | 有效可信度 | 被谁卡住 |",
            "|---|---|---|---|---|---|---|---|"]
    for u in us:
        passed = len({v.who for v in u.verifs if v.version == u.version and "通过" in v.verdict})
        neck = units[u.bottleneck].title if u.bottleneck in units else (u.bottleneck or "—(取决于自身)")
        out.append(f"| [{u.title}]({u.file}) | {u.layer} | {u.source} | {u.version} | {passed} "
                   f"| {u.c_self:.1f} | **{u.c_eff:.1f}** | {neck} |")
    out += ["", "## 优先核验顺序", "",
            "按\"有多少单元直接或间接依据它\"排序。越靠前,核验它能抬高的下游单元越多。", "",
            "| 顺序 | 单元 | 层级 | 依据它的下游单元数 | 当前自身可信度 |", "|---|---|---|---|---|"]
    rank = sorted(us, key=lambda u: (-issues["leverage"][u.file], order.get(u.layer, 9), u.file))
    for i, u in enumerate(rank, 1):
        out.append(f"| {i} | {u.title} | {u.layer} | {issues['leverage'][u.file]} | {u.c_self:.1f} |")
    out += ["", "## 完整性检查", ""]
    if not any(issues[k] for k in ("missing", "cycles", "stale")):
        out.append("- 无问题:引用目标齐全,\"依据\"无环,引用版本均为最新。")
    for f, t in issues["missing"]:
        out.append(f"- ❌ 引用目标不存在:`{f}` → `{t}`")
    for cyc in issues["cycles"]:
        out.append("- ❌ \"依据\"成环:" + " → ".join(f"`{x}`" for x in cyc))
    for f, t, cv, tv in issues["stale"]:
        out.append(f"- ⚠️ 引用版本落后:`{f}` 引用 `{t}` 的 {cv},当前为 {tv},请确认是否跟进")
    return "\n".join(out) + "\n"


def main():
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    d = sys.argv[1]
    files = [f for f in sorted(os.listdir(d)) if f.endswith(".md") and f not in ("README.md", "GRAPH.md")]
    units = {}
    for f in files:
        u = parse(os.path.join(d, f))
        if "层级" in u.meta:
            units[u.file] = u
    issues = analyze(units)
    text = report(units, issues)
    path = os.path.join(d, "GRAPH.md")
    open(path, "w", encoding="utf-8").write(text)
    print(text)
    print(f"wrote {path}")
    bad = issues["missing"] or issues["cycles"]
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
