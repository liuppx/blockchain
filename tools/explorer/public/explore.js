"use strict";
// 探索:从一条知识出发,看它站在谁的肩上、又被谁继承;以及整张图的缺口在哪里。
// 数据来自 /explore、/gaps、/coverage(全部基于链上已有的引用与核验记录)。
(function () {
  const el = (id) => document.getElementById(id);
  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  const LAYER = { foundation: "基础", explanation: "解释", scenario: "场景", application: "应用" };
  const DOMAIN = { "1": "大模型应用开发", "2": "运动健身" };
  const layerZh = (l) => LAYER[l] || l;
  const domainZh = (d) => DOMAIN[String(d)] ? `${DOMAIN[String(d)]} (${d})` : String(d);

  function unitLink(id, label) {
    return `<a class='link' data-unit='${esc(id)}'>#${esc(id)}${label ? " " + esc(label) : ""}</a>`;
  }

  /** 单元页上的"探索"面板:上游(谁建立在它之上)与地基。 */
  async function attachExplore(u) {
    const slot = el("exploreSlot");
    if (!slot) return;
    slot.innerHTML = "<span class='mut'>读取引用关系…</span>";
    try {
      const r = await fetch("/explore?unit=" + encodeURIComponent(u.id));
      if (!r.ok) throw new Error("HTTP " + r.status);
      const d = await r.json();
      const up = d.upstream || [], down = d.downstream || [];
      const kinds = { basis: "依据", "see-also": "参见", supersedes: "取代" };
      const group = (list, dir) => {
        if (!list.length) return "<p class='mut'>—</p>";
        const byDepth = new Map();
        for (const e of list) {
          if (!byDepth.has(e.depth)) byDepth.set(e.depth, []);
          byDepth.get(e.depth).push(e);
        }
        return [...byDepth.entries()].sort((a, b) => a[0] - b[0]).map(([depth, es]) =>
          `<div><span class='mut'>${depth === 1 ? "直接" : "间接(第 " + depth + " 层)"}</span>: ` +
          es.map((e) => `${unitLink(e.unit)} <span class='mut'>(${kinds[e.kind] || e.kind},经 #${esc(e.via)})</span>`).join("、") +
          `</div>`).join("");
      };
      slot.innerHTML = `<h3 class='md'>探索</h3>
        <p class='mut'>这条知识站在谁的肩上、又被谁继承——想法的流向。</p>
        <kv><b>地基(它依据)</b>${group(down, "down")}</kv>
        <kv><b>继承(谁依据它)</b>${group(up, "up")}</kv>
        <p class='mut'>共 ${down.length} 条地基、${up.length} 条继承路径。</p>`;
      slot.querySelectorAll("a[data-unit]").forEach((a) =>
        a.addEventListener("click", () => showUnit(a.dataset.unit)));
    } catch (e) {
      slot.innerHTML = `<span class='mut'>探索数据不可用:${esc(e.message)}</span>`;
    }
  }

  /** 缺口视图:哪些单元还没有被任何人依据、也还没有人核验。 */
  async function showGaps() {
    setView("<span class='mut'>读取缺口…</span>");
    try {
      const [g, c] = await Promise.all([
        (await fetch("/gaps")).json(),
        (await fetch("/coverage")).json(),
      ]);
      const cov = c.coverage || [];
      const gaps = g.gaps || [];
      const domOf = {};
      for (const x of cov) domOf[x.domain] = x;
      const rows = gaps.map((x) => {
        const c2 = domOf[x.domain] || {};
        return `<tr><td>${unitLink(x.id)}</td><td>${esc(domainZh(x.domain))}</td><td>${esc(layerZh(x.layer))}</td>
          <td>${x.established ? "<span class='ok'>已有人核验</span>" : "<span class='mut'>无人核验</span>"}</td>
          <td class='mut'>${esc(x.versions)} 个版本</td></tr>`;
      }).join("");
      setView(`<h2 style='border:0;padding:0 0 8px'>缺口与覆盖</h2>
        <p class='mut'>"缺口"= 没有任何单元依据它、也还没有人核验它。这些是图里的<strong>山谷</strong>:
        写一条新的、或者去核验现有的,都能填上它们。</p>
        <table><tr><th>单元</th><th>领域</th><th>层级</th><th>状态</th><th>版本</th></tr>${rows || "<tr><td colspan='5' class='mut'>没有缺口</td></tr>"}</table>
        <h3 class='md'>按领域</h3>
        <table><tr><th>领域</th><th>单元</th><th>已确立</th><th>缺口</th><th>层级分布</th></tr>` +
        cov.map((x) => `<tr><td>${esc(domainZh(x.domain))}</td><td>${x.total}</td><td>${x.established}</td>
          <td>${x.unreferenced}</td><td class='mut'>${Object.entries(x.layers).map(([k, v]) => layerZh(k) + " " + v).join(" · ")}</td></tr>`).join("") +
        `</table>`);
      el("view").querySelectorAll("a[data-unit]").forEach((a) =>
        a.addEventListener("click", () => showUnit(a.dataset.unit)));
    } catch (e) {
      setView(`<span class='err'>读取失败:${esc(e.message)}</span>`);
    }
  }

  window.ZXExplore = { attachExplore, showGaps };
})();
