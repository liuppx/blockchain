"use strict";
// 认知地图(浏览器视图)。
//
// 数据来自 /map:链上的引用关系、可信度,以及正文仓库里的标题。**不新增链上状态**,也不需要额外权限。
// 读法:列 = 领域,行 = 层级(自下而上 基础 → 解释 → 场景 → 应用);
//       点的颜色 = 有效可信度(越亮越可信),点的大小 = 有多少单元直接或间接依据它;
//       实线 = 依据(可信度沿它传递),虚线 = 参见,红线 = 取代。
// 用 SVG 画:文字清楚、悬停与点击可以落在每个点上。
(function () {
  const NS = "http://www.w3.org/2000/svg";
  const el = (id) => document.getElementById(id);
  const LAYERS = ["foundation", "explanation", "scenario", "application"];
  const LAYER_NAME = { foundation: "基础", explanation: "解释", scenario: "场景", application: "应用" };
  const KIND_NAME = { basis: "依据", "see-also": "参见", supersedes: "取代" };
  const LIFE_NAME = { active: "活跃", dormant: "休眠", contested: "竞争", superseded: "已取代" };
  const TAG_NAME = { peak: "峰", plateau: "高原", ridge: "山脊", valley: "山谷", fault: "断层", slope: "坡", plain: "平地" };
  // 有效可信度 → 单一蓝色色阶(暗底上越亮越可信)。已用 dataviz 校验器在 #0d1117 上校验:单色相、亮度单调、相邻可分辨。
  const CRED = [[0.15, "#184f95", "0.1"], [0.3, "#256abf", "0.2"], [0.5, "#3987e5", "0.4"], [0.7, "#6da7ec", "0.6"], [Infinity, "#9ec5f4", "0.8+"]];
  const credColor = (v) => CRED.find(([t]) => v < t)[1];
  const radius = (n) => 6 + Math.min(10, Math.sqrt(n.downstream || 0) * 3);
  const domainLabel = (d) => (typeof DOMAIN_ZH !== "undefined" && DOMAIN_ZH[d]) || ("领域 " + d);

  function svg(tag, attrs, parent) {
    const e = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v);
    if (parent) parent.appendChild(e);
    return e;
  }
  const clip = (s, max) => (s.length > max ? s.slice(0, max - 1) + "…" : s);
  const shortTitle = (t) => (t || "").replace(/[((].*?[))]/g, "").split(/[::,,]/)[0].trim();

  /**
   * 布局:每个(领域,层级)格子里的点横向排开,放不下就换行。
   * 先排基础层(按下游数从多到少),再自下而上按"所依据的点的平均横坐标"排上层,最后回头按上层重排基础层——
   * 两遍重心排序,减少连线交叉。
   */
  function layout(data, W) {
    const nodes = data.nodes || [];
    const domains = [...new Set(nodes.map((n) => n.domain))].sort((a, b) => Number(a) - Number(b));
    const layers = LAYERS.filter((l) => nodes.some((n) => n.layer === l));
    const padL = 64, padR = 16, padT = 40, colGap = 24;
    const colW = (W - padL - padR - colGap * (domains.length - 1)) / Math.max(1, domains.length);
    const slot = 128;                       // 每个点占的最小宽度(放得下一行短标题)
    const perLine = Math.max(1, Math.floor(colW / slot));
    const lineH = 76;
    const cell = (d, l) => nodes.filter((n) => n.domain === d && n.layer === l);
    const x = new Map();
    const byId = new Map(nodes.map((n) => [n.id, n]));
    const basisOut = (n) => n.cites.filter((e) => e.kind === "basis").map((e) => e.to).filter((t) => byId.has(t));
    const basisIn = (n) => n.cited_by.filter((e) => e.kind === "basis").map((e) => e.from);
    const mean = (ids) => { const xs = ids.map((i) => x.get(i)).filter((v) => v !== undefined); return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : null; };

    // 每层需要几行
    const linesOf = (l) => Math.max(1, ...domains.map((d) => Math.ceil(cell(d, l).length / perLine)));
    const rowH = layers.map((l) => linesOf(l) * lineH + 24);
    const H = padT + rowH.reduce((a, b) => a + b, 0) + 8;
    // 行的上边界:基础在最下
    const rowTop = new Map();
    let y = padT;
    for (let i = layers.length - 1; i >= 0; i--) { rowTop.set(layers[i], y); y += rowH[i]; }

    const place = (d, l, order) => {
      const colX = padL + domains.indexOf(d) * (colW + colGap);
      order.forEach((n, i) => {
        const line = Math.floor(i / perLine), k = i % perLine;
        const inLine = Math.min(perLine, order.length - line * perLine);
        const step = colW / inLine;
        n._x = colX + step * (k + 0.5);
        n._w = step;                         // 这个点能用的宽度(决定标题能显示几个字)
        n._y = rowTop.get(l) + 26 + line * lineH;
        x.set(n.id, n._x);
      });
    };
    const sortBy = (list, key) => list.slice().sort((a, b) => {
      const ka = key(a), kb = key(b);
      if (ka === null && kb === null) return Number(a.id) - Number(b.id);
      if (ka === null) return 1; if (kb === null) return -1;
      return ka - kb || Number(a.id) - Number(b.id);
    });
    // 第一遍:基础按下游数(重要的居中靠前),上层按依据的重心
    for (const d of domains) {
      const base = cell(d, "foundation").sort((a, b) => (b.downstream - a.downstream) || (Number(a.id) - Number(b.id)));
      place(d, "foundation", base);
    }
    for (const l of layers.filter((l) => l !== "foundation")) for (const d of domains) place(d, l, sortBy(cell(d, l), (n) => mean(basisOut(n))));
    // 第二遍:基础层按依据它的上层点的重心重排,再重排上层
    for (const d of domains) place(d, "foundation", sortBy(cell(d, "foundation"), (n) => mean(basisIn(n))));
    for (const l of layers.filter((l) => l !== "foundation")) for (const d of domains) place(d, l, sortBy(cell(d, l), (n) => mean(basisOut(n))));
    return { domains, layers, H, padL, padT, colW, colGap, rowTop, rowH, slot: Math.min(slot, colW) };
  }

  function draw(host, data, onPick) {
    host.querySelectorAll(":scope > svg").forEach((x) => x.remove());   // 只换图,保留提示框
    const W = Math.max(640, host.clientWidth);
    const L = layout(data, W);
    const root = svg("svg", { width: W, height: L.H, viewBox: `0 0 ${W} ${L.H}`, role: "img",
      "aria-label": "认知地图:按领域与层级排列的知识单元及其引用关系" });
    host.insertBefore(root, host.firstChild);
    root.style.display = "block";
    root.style.fontFamily = "ui-sans-serif,system-ui,-apple-system,'PingFang SC','Noto Sans CJK SC',sans-serif";

    // 层级带与名称
    L.layers.forEach((l, i) => {
      const top = L.rowTop.get(l), h = L.rowH[i];
      svg("rect", { x: 8, y: top, width: W - 16, height: h - 4, rx: 6, fill: i % 2 ? "rgba(255,255,255,.015)" : "rgba(255,255,255,.035)" }, root);
      const t = svg("text", { x: 20, y: top + h / 2, fill: "#8b949e", "font-size": 12, "dominant-baseline": "middle" }, root);
      t.textContent = LAYER_NAME[l];
    });
    // 领域列标题
    L.domains.forEach((d, i) => {
      const cx = L.padL + i * (L.colW + L.colGap) + L.colW / 2;
      const t = svg("text", { x: cx, y: 22, fill: "#e6edf3", "font-size": 13, "font-weight": 600, "text-anchor": "middle" }, root);
      t.textContent = domainLabel(d);
      if (i > 0) svg("line", { x1: cx - L.colW / 2 - L.colGap / 2, x2: cx - L.colW / 2 - L.colGap / 2, y1: 34, y2: L.H - 8,
        stroke: "#30363d", "stroke-dasharray": "2 4" }, root);
    });

    const byId = new Map(data.nodes.map((n) => [n.id, n]));
    // 连线:从依据者指向被依据者;依据在上层,弯向下方
    const edgeG = svg("g", { fill: "none" }, root);
    const edgeEls = [];
    for (const e of data.edges || []) {
      const a = byId.get(e.from), b = byId.get(e.to);
      if (!a || !b) continue;
      const my = (a._y + b._y) / 2;
      const p = svg("path", {
        d: `M${a._x},${a._y} C${a._x},${my} ${b._x},${my} ${b._x},${b._y}`,
        stroke: e.kind === "supersedes" ? "#f85149" : e.kind === "basis" ? "#6e7f96" : "#4a525e",
        "stroke-width": e.kind === "basis" ? 1.5 : 1,
        "stroke-dasharray": e.kind === "see-also" ? "3 4" : "",
        opacity: e.kind === "see-also" ? 0.6 : 0.8,
      }, edgeG);
      edgeEls.push({ el: p, e });
    }

    // 节点
    const nodeG = svg("g", {}, root);
    const nodeEls = new Map();
    for (const n of data.nodes) {
      const g = svg("g", { transform: `translate(${n._x},${n._y})`, tabindex: 0, role: "button",
        "aria-label": `#${n.id} ${n.title || ""},${LAYER_NAME[n.layer]},可信度 ${n.effective}` }, nodeG);
      g.style.cursor = "pointer";
      const r = radius(n);
      svg("circle", { r: r + 10, fill: "transparent" }, g);                         // 命中区比点大
      svg("circle", { r, fill: credColor(n.effective), stroke: "#0d1117", "stroke-width": 2,
        "stroke-dasharray": n.lifecycle === "superseded" ? "3 2" : "",
        opacity: n.lifecycle === "superseded" ? 0.5 : 1 }, g);
      // 标题加一圈底色描边:连线从文字下面穿过时字仍然清楚
      const label = svg("text", { y: r + 15, "text-anchor": "middle", "font-size": 12, fill: "#c9d1d9",
        stroke: "#11151b", "stroke-width": 4, "stroke-linejoin": "round", "paint-order": "stroke" }, g);
      const max = Math.max(4, Math.floor(((n._w || L.slot) - 12) / 12.5));
      label.textContent = clip(`#${n.id} ${shortTitle(n.title)}`.trim(), max);
      nodeEls.set(n.id, g);
      const focus = () => highlight(n.id);
      g.addEventListener("mouseenter", (ev) => { focus(); tip(ev, n); });
      g.addEventListener("mousemove", (ev) => tip(ev, n));
      g.addEventListener("mouseleave", () => { highlight(null); tip(null); });
      g.addEventListener("focus", focus);
      g.addEventListener("blur", () => highlight(null));
      g.addEventListener("click", () => onPick(n));
      g.addEventListener("keydown", (ev) => { if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); onPick(n); } });
    }

    // 悬停:只亮这个点、它连着的线和邻居,其余变淡
    function highlight(id) {
      const near = new Set(id ? [id] : []);
      for (const { e } of edgeEls) if (id && (e.from === id || e.to === id)) { near.add(e.from); near.add(e.to); }
      // 只淡出圆点,标题文字保持可读(不然整张图会"消失"一块)
    for (const [nid, g] of nodeEls) {
      const keep = !id || near.has(nid);
      const dot = g.querySelector("circle:nth-of-type(2)");
      if (dot) dot.style.opacity = keep ? 1 : 0.22;
      const lab = g.querySelector("text");
      if (lab) lab.style.opacity = keep ? 1 : 0.45;
    }
      for (const { el: p, e } of edgeEls) {
        const on = id && (e.from === id || e.to === id);
        p.style.opacity = !id ? "" : on ? 1 : 0.06;
        p.setAttribute("stroke-width", on ? 2.5 : e.kind === "basis" ? 1.5 : 1);
      }
    }
    // 悬停提示
    const tipEl = el("mapTip");
    function tip(ev, n) {
      if (!tipEl) return;
      if (!ev) { tipEl.style.display = "none"; return; }
      tipEl.innerHTML = `<b>#${esc(n.id)} ${esc(n.title || "(正文未上传)")}</b><br>
        ${esc(LAYER_NAME[n.layer])} · ${esc(domainLabel(n.domain))} · ${esc(LIFE_NAME[n.lifecycle] || n.lifecycle)}<br>
        有效可信度 <b>${esc(n.effective)}</b> · ${n.passes} 位核验通过<br>
        ${n.downstream ? `${n.downstream} 个单元直接或间接依据它` : "还没有单元依据它"}`;
      tipEl.style.display = "block";
      const box = host.getBoundingClientRect();
      const tw = tipEl.offsetWidth, th = tipEl.offsetHeight;
      let left = ev.clientX - box.left + 14;
      let top = ev.clientY - box.top + 14;
      if (left + tw > box.width - 4) left = Math.max(4, ev.clientX - box.left - tw - 14);
      if (top + th > box.height - 4) top = Math.max(4, ev.clientY - box.top - th - 14);
      tipEl.style.left = left + "px";
      tipEl.style.top = top + "px";
    }
    return root;
  }

  function legend(data) {
    const s = data.summary || {};
    const credSw = CRED.map(([, c, label]) =>
      `<span class='mapLg'><i style='background:${c}'></i>${label}</span>`).join("");
    return `<div class='mapLegend'>
      <span><b>颜色</b> 有效可信度 ${credSw}</span>
      <span><b>大小</b> 依据它的单元数</span>
      <span><b>连线</b>
        <span class='mapLg'><svg width='26' height='8'><line x1='0' y1='4' x2='26' y2='4' stroke='#6e7f96' stroke-width='1.5'/></svg>依据</span>
        <span class='mapLg'><svg width='26' height='8'><line x1='0' y1='4' x2='26' y2='4' stroke='#4a525e' stroke-dasharray='3 4'/></svg>参见</span>
        <span class='mapLg'><svg width='26' height='8'><line x1='0' y1='4' x2='26' y2='4' stroke='#f85149'/></svg>取代</span>
      </span>
      <span class='mut'>${s.nodes} 个单元 · ${s.edges} 条引用 · ${s.domains} 个领域</span>
    </div>`;
  }

  function table(data) {
    const rows = data.nodes.slice().sort((a, b) => (b.downstream - a.downstream) || (Number(a.id) - Number(b.id)));
    return `<table><tr><th>单元</th><th>标题</th><th>领域</th><th>层级</th><th>有效可信度</th><th>核验通过</th><th>依据它的单元</th><th>状态</th></tr>` +
      rows.map((n) => `<tr><td><a class='link' data-unit='${esc(n.id)}'>#${esc(n.id)}</a></td><td>${esc(n.title || "—")}</td>
        <td>${esc(domainLabel(n.domain))}</td><td>${esc(LAYER_NAME[n.layer])}</td><td>${esc(n.effective)}</td>
        <td>${n.passes}</td><td>${n.downstream}</td><td>${esc(LIFE_NAME[n.lifecycle] || n.lifecycle)}</td></tr>`).join("") + "</table>";
  }

  async function show() {
    setView("<span class='mut'>正在读取知识图…</span>");
    let data;
    try {
      if (typeof loadDomains === "function" && typeof DOMAIN_ZH !== "undefined" && !Object.keys(DOMAIN_ZH).length) await loadDomains();
      const r = await fetch("/map");
      if (!r.ok) throw new Error("HTTP " + r.status);
      data = await r.json();
    } catch (e) {
      setView(`<span class='err'>认知地图读取失败:${esc(e.message)}</span>`);
      return;
    }
    if (!(data.nodes || []).length) { setView("<h2 style='border:0;padding:0 0 4px'>认知地图</h2><p class='mut'>链上还没有知识单元。</p>"); return; }
    setView(`<h2 style='border:0;padding:0 0 4px'>认知地图</h2>
      <p class='mut'>每个点是一个知识单元。列是领域,行是层级(自下而上 基础 → 解释 → 场景 → 应用),
      连线是引用。<b>先核验大的、暗的点</b>:很多单元依据它,而它的可信度还低,核验它抬高的最多。
      数据全部来自链上的引用与核验记录。</p>
      ${legend(data)}
      <div id='mapHost' style='position:relative;margin-top:8px;border:1px solid var(--line);border-radius:6px;background:var(--bg);overflow:hidden'>
        <div id='mapTip' role='tooltip' style='display:none;position:absolute;z-index:2;max-width:320px;pointer-events:none;
          background:var(--panel);border:1px solid var(--line);border-radius:6px;padding:8px 10px;font-size:12px;line-height:1.6'></div>
      </div>
      <div id='mapPick' class='mut'></div>
      <details style='margin-top:12px'><summary class='mut'>表格视图</summary>${table(data)}</details>`);
    const host = el("mapHost");
    const pick = (n) => {
      const box = el("mapPick");
      const cites = n.cites.map((e) => `#${esc(e.to)}(${esc(KIND_NAME[e.kind] || e.kind)})`).join("、") || "—";
      const cited = n.cited_by.map((e) => `#${esc(e.from)}`).join("、") || "—";
      box.innerHTML = `<div style='margin-top:10px;border-top:1px solid var(--line);padding-top:8px'>
        <b>#${esc(n.id)} ${esc(n.title || "")}</b> · ${esc(LAYER_NAME[n.layer])} · ${esc(domainLabel(n.domain))}
        · 有效可信度 ${esc(n.effective)} · ${n.passes} 位核验通过 · ${esc(LIFE_NAME[n.lifecycle] || n.lifecycle)}
        · 图上的位置:${esc(TAG_NAME[n.tags[0]] || n.tags[0])}
        <br>它引用:${cites}<br>引用它的:${cited}
        <br><a class='link' data-unit='${esc(n.id)}'>打开这个单元 →</a></div>`;
      box.querySelector("a[data-unit]").addEventListener("click", () => showUnit(n.id));
    };
    const render = () => draw(host, data, pick);
    render();
    // 宽度变了就重排(只在这个视图还在时)
    let t = null;
    const onResize = () => { if (!document.body.contains(host)) return window.removeEventListener("resize", onResize); clearTimeout(t); t = setTimeout(render, 150); };
    window.addEventListener("resize", onResize);
    document.querySelectorAll("#view a[data-unit]").forEach((a) => a.addEventListener("click", () => showUnit(a.dataset.unit)));
  }

  window.ZXMap = { show, draw, layout };
  window.ZXTerrain = window.ZXMap;   // 旧名,兼容
})();
