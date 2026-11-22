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
    const tip = (ev, n) => showTip(host, ev, n);
    return root;
  }

  // 悬停提示(平面与立体共用):靠边时翻到另一侧,不被容器裁掉
  function showTip(host, ev, n) {
    const tipEl = el("mapTip");
    if (!tipEl) return;
    if (!ev || !n) { tipEl.style.display = "none"; return; }
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

  // ---- 立体视图 ----------------------------------------------------------------
  // 地面沿用平面图的布局(列 = 领域,行 = 层级):平面图里基础在下、应用在上,
  // 立体图里基础在近处、应用在远处,屏幕上的上下关系一致,两种视图能对上号。
  // 每个单元在地面上隆起一座山:山高 = 地形高度(链上算好的 可信度 × 重要性),
  // 山脚大小 = 依据它的单元数;引用从一个山顶拉到另一个山顶,画成弧线。
  // 相机、透视投影、画家算法都手写,不引入任何库。
  const rgbOf = (h) => { const n = parseInt(h.slice(1), 16); return [(n >> 16) & 255, (n >> 8) & 255, n & 255]; };
  const TERRAIN_LO = rgbOf("#141920"), TERRAIN_HI = rgbOf("#5a6678");

  function draw3d(host, data, onPick) {
    host.querySelectorAll(":scope > svg, :scope > canvas").forEach((x) => x.remove());
    const dpr = window.devicePixelRatio || 1;
    const W = Math.max(640, host.clientWidth);
    const H = Math.min(620, Math.max(420, Math.round(W * 0.52)));
    const cv = document.createElement("canvas");
    cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr);
    cv.style.width = W + "px"; cv.style.height = H + "px";
    cv.style.display = "block"; cv.style.touchAction = "none";
    cv.tabIndex = 0;
    cv.setAttribute("role", "img");
    cv.setAttribute("aria-label", "认知地图立体视图:山高是地形高度,山脚大小是依据它的单元数,弧线是引用。拖动旋转,滚轮缩放;也可切到平面视图或表格视图。");
    host.insertBefore(cv, host.firstChild);
    const ctx = cv.getContext("2d");

    // 地面:与平面图同一套布局
    const L = layout(data, 1100);
    const nodes = data.nodes;
    const ax = nodes.map((n) => n._x), ay = nodes.map((n) => n._y);
    const cX = (Math.min(...ax) + Math.max(...ax)) / 2;
    const cY = (Math.min(...ay) + Math.max(...ay)) / 2;

    // 山:峰顶 = 地形高度,山脚半径 = 被依赖的程度
    const peaks = nodes.map((n) => {
      const hn = n.height != null ? n.height : 0;
      return { n, X: n._x - cX, Z: n._y - cY, h: 8 + hn * 170, r: 58 + Math.sqrt(n.downstream || 0) * 24 };
    });
    const P = new Map(peaks.map((p) => [p.n.id, p]));
    const gx0 = Math.min(...peaks.map((p) => p.X)) - 60, gx1 = Math.max(...peaks.map((p) => p.X)) + 60;
    const gz0 = Math.min(...peaks.map((p) => p.Z)) - 60, gz1 = Math.max(...peaks.map((p) => p.Z)) + 60;

    // 高度场:每座山一个紧支撑抛物面,峰顶正好落在单元上
    const surf = (X, Z) => {
      let m = 0;
      for (const p of peaks) {
        const d2 = (X - p.X) * (X - p.X) + (Z - p.Z) * (Z - p.Z);
        if (d2 >= p.r * p.r) continue;
        const v = p.h * (1 - d2 / (p.r * p.r));
        if (v > m) m = v;
      }
      return m;
    };
    const NX = 44, NZ = 30;
    const dx = (gx1 - gx0) / NX, dz = (gz1 - gz0) / NZ;
    const G = [];
    for (let i = 0; i <= NX; i++) { G.push([]); for (let k = 0; k <= NZ; k++) G[i].push(surf(gx0 + i * dx, gz0 + k * dz)); }
    const hMax = Math.max(...G.flat()) || 1;
    // 受光:光源固定在世界坐标里,山脊会亮、山谷会暗
    const LX = -0.42, LY = 0.80, LZ = -0.43;
    const cells = [];
    for (let i = 0; i < NX; i++) for (let k = 0; k < NZ; k++) {
      const h00 = G[i][k], h10 = G[i + 1][k], h01 = G[i][k + 1], h11 = G[i + 1][k + 1];
      const gxx = (h10 + h11 - h00 - h01) / (2 * dx), gzz = (h01 + h11 - h00 - h10) / (2 * dz);
      const nl = Math.hypot(-gxx, 1, -gzz);
      cells.push({ i, k, h: (h00 + h10 + h01 + h11) / 4, lam: Math.max(0, (-gxx * LX + LY - gzz * LZ) / nl),
        X: gx0 + (i + 0.5) * dx, Z: gz0 + (k + 0.5) * dz });
    }

    // 相机:绕场景中心转,可俯仰;滚轮缩放。始终看向场景中心。
    const reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const view = { yaw: -0.34, pitch: 0.60, zoom: 1, auto: !reduce };
    const camD = 2400;
    function basis() {
      const cp = Math.cos(view.pitch), sp = Math.sin(view.pitch);
      const C = [camD * cp * Math.sin(view.yaw), camD * sp, camD * cp * Math.cos(view.yaw)];
      const F = [-C[0] / camD, -C[1] / camD, -C[2] / camD];       // 指向场景中心
      const R = [Math.cos(view.yaw), 0, -Math.sin(view.yaw)];
      const U = [R[1] * F[2] - R[2] * F[1], R[2] * F[0] - R[0] * F[2], R[0] * F[1] - R[1] * F[0]];
      return { C, F, R, U };
    }
    const cam = (B, X, Y, Z) => {
      const dx = X - B.C[0], dy = Y - B.C[1], dz = Z - B.C[2];
      return { xc: dx * B.R[0] + dy * B.R[1] + dz * B.R[2],
               yc: dx * B.U[0] + dy * B.U[1] + dz * B.U[2],
               zc: dx * B.F[0] + dy * B.F[1] + dz * B.F[2] };
    };
    // 取景:按当前朝向自动把场景装进画面(转起来不会转出画外)
    const keys = [[gx0, 0, gz0], [gx1, 0, gz0], [gx1, 0, gz1], [gx0, 0, gz1],
                  [gx0, 0, gz0 - 20], [gx1, 0, gz1 + 60], [gx0, 0, gz1 + 60]];
    for (const p of peaks) keys.push([p.X, p.h + 30, p.Z]);
    for (const e of data.edges || []) {
      const a = P.get(e.from), b = P.get(e.to);
      if (!a || !b) continue;
      const lift = 30 + Math.hypot(a.X - b.X, a.Z - b.Z) * 0.25;
      keys.push([(a.X + b.X) / 2, (a.h + b.h) / 2 + lift, (a.Z + b.Z) / 2]);
    }
    let focal = 1000, offX = 0, offY = 0;
    function fit(B) {
      const raw = keys.map((k) => cam(B, k[0], k[1], k[2]));
      const mx = Math.max(...raw.map((c) => Math.abs(c.xc / c.zc))) || 1;
      const my = Math.max(...raw.map((c) => Math.abs(c.yc / c.zc))) || 1;
      focal = Math.min((W * 0.46) / mx, (H * 0.46) / my) * view.zoom;
      const pts = raw.map((c) => ({ x: (c.xc / c.zc) * focal, y: -(c.yc / c.zc) * focal }));
      const bx = (Math.min(...pts.map((p) => p.x)) + Math.max(...pts.map((p) => p.x))) / 2;
      const by = (Math.min(...pts.map((p) => p.y)) + Math.max(...pts.map((p) => p.y))) / 2;
      offX = W / 2 - bx; offY = H / 2 - by;
    }
    const proj = (B, X, Y, Z) => {
      const c = cam(B, X, Y, Z);
      if (c.zc < 1) return null;
      return { x: (c.xc / c.zc) * focal + offX, y: -(c.yc / c.zc) * focal + offY, z: c.zc };
    };

    let hover = null, dirty = true, alive = true, last = 0;
    const near = () => {
      const s = new Set(hover ? [hover] : []);
      if (hover) for (const e of data.edges || []) if (e.from === hover || e.to === hover) { s.add(e.from); s.add(e.to); }
      return s;
    };
    const cellColor = (h, lam) => {
      const t = Math.min(1, (h / hMax) * 0.5 + lam * 0.55);
      const c = TERRAIN_LO.map((v, i) => Math.round(v + (TERRAIN_HI[i] - v) * t));
      return `rgb(${c[0]},${c[1]},${c[2]})`;
    };

    function frame() {
      const B = basis();
      fit(B);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.fillStyle = "#0d1117"; ctx.fillRect(0, 0, W, H);

      // 1. 地面底板
      const gq = [proj(B, gx0, 0, gz0), proj(B, gx1, 0, gz0), proj(B, gx1, 0, gz1), proj(B, gx0, 0, gz1)];
      if (gq.every(Boolean)) {
        ctx.beginPath(); ctx.moveTo(gq[0].x, gq[0].y);
        for (let i = 1; i < 4; i++) ctx.lineTo(gq[i].x, gq[i].y);
        ctx.closePath();
        ctx.fillStyle = "#10141a"; ctx.fill();
        ctx.strokeStyle = "#232b34"; ctx.lineWidth = 1; ctx.stroke();
      }
      // 2. 网格:领域之间竖着、层级之间横着(和平面图对应)
      const domX = [];
      for (let i = 1; i < L.domains.length; i++) domX.push(L.padL + i * (L.colW + L.colGap) - L.colGap / 2 - cX);
      domX.unshift(L.padL - cX);
      domX.push(L.padL + L.domains.length * (L.colW + L.colGap) - L.colGap - cX);
      const layZ = L.layers.map((l, i) => L.rowTop.get(l) - cY);
      const seg = (a, b) => { if (!a || !b) return; ctx.beginPath(); ctx.moveTo(a.x, a.y); ctx.lineTo(b.x, b.y); ctx.stroke(); };
      ctx.setLineDash([3, 5]); ctx.strokeStyle = "rgba(110,127,150,.45)"; ctx.lineWidth = 1;
      for (const X of domX) seg(proj(B, X, 0, gz0), proj(B, X, 0, gz1));
      for (const Z of layZ) seg(proj(B, gx0, 0, Z), proj(B, gx1, 0, Z));
      ctx.setLineDash([]);
      // 3. 地形:按相机深度从远到近一块一块填
      const order = cells.map((c) => ({ c, z: cam(B, c.X, 0, c.Z).zc })).sort((a, b) => b.z - a.z);
      for (const { c } of order) {
        const p00 = proj(B, gx0 + c.i * dx, G[c.i][c.k], gz0 + c.k * dz);
        const p10 = proj(B, gx0 + (c.i + 1) * dx, G[c.i + 1][c.k], gz0 + c.k * dz);
        const p11 = proj(B, gx0 + (c.i + 1) * dx, G[c.i + 1][c.k + 1], gz0 + (c.k + 1) * dz);
        const p01 = proj(B, gx0 + c.i * dx, G[c.i][c.k + 1], gz0 + (c.k + 1) * dz);
        if (!p00 || !p10 || !p11 || !p01) continue;
        ctx.beginPath(); ctx.moveTo(p00.x, p00.y); ctx.lineTo(p10.x, p10.y); ctx.lineTo(p11.x, p11.y); ctx.lineTo(p01.x, p01.y);
        ctx.closePath();
        ctx.fillStyle = cellColor(c.h, c.lam);
        ctx.strokeStyle = ctx.fillStyle; ctx.lineWidth = 1;   // 同时描边,消掉接缝
        ctx.fill(); ctx.stroke();
      }
      // 4. 引用:山顶之间的弧线
      const lit = near();
      for (const e of data.edges || []) {
        const a = P.get(e.from), b = P.get(e.to);
        if (!a || !b) continue;
        const on = hover && (e.from === hover || e.to === hover);
        const lift = 30 + Math.hypot(a.X - b.X, a.Z - b.Z) * 0.25;
        ctx.beginPath();
        let started = false;
        for (let t = 0; t <= 1.0001; t += 1 / 20) {
          const X = a.X + (b.X - a.X) * t, Z = a.Z + (b.Z - a.Z) * t;
          const Y = a.h + (b.h - a.h) * t + lift * 4 * t * (1 - t);
          const q = proj(B, X, Y, Z);
          if (!q) { started = false; continue; }
          if (!started) { ctx.moveTo(q.x, q.y); started = true; } else ctx.lineTo(q.x, q.y);
        }
        ctx.setLineDash(e.kind === "see-also" ? [3, 4] : []);
        ctx.strokeStyle = e.kind === "supersedes" ? "#f85149" : e.kind === "basis" ? "#8fa3bd" : "#525d69";
        ctx.globalAlpha = hover ? (on ? 1 : 0.07) : (e.kind === "see-also" ? 0.5 : 0.85);
        ctx.lineWidth = on ? 2.2 : e.kind === "basis" ? 1.3 : 1;
        ctx.stroke();
      }
      ctx.setLineDash([]); ctx.globalAlpha = 1;
      // 5. 山顶的点与标题:远的先画
      const order2 = peaks.map((p) => ({ p, n: p.n, top: proj(B, p.X, p.h, p.Z) }))
        .filter((o) => o.top).sort((a, b) => b.top.z - a.top.z);
      for (const o of order2) {
        const keep = !hover || lit.has(o.n.id);
        ctx.globalAlpha = keep ? 1 : 0.25;
        const base = proj(B, o.p.X, o.p.h - 18, o.p.Z);          // 标杆:插在山上的一根针
        ctx.strokeStyle = "rgba(185,195,210,.55)"; ctx.lineWidth = 1.2;
        ctx.beginPath(); ctx.moveTo(o.top.x, o.top.y);
        if (base) ctx.lineTo(base.x, base.y);
        ctx.stroke();
        const r = Math.max(3.5, (6 + Math.min(10, Math.sqrt(o.n.downstream || 0) * 3)) * (focal / o.top.z) * 1.6);
        ctx.beginPath(); ctx.arc(o.top.x, o.top.y, r, 0, Math.PI * 2);
        ctx.fillStyle = credColor(o.n.effective); ctx.fill();
        ctx.lineWidth = o.n.id === hover ? 2.5 : 1.5; ctx.strokeStyle = o.n.id === hover ? "#e6edf3" : "#0d1117";
        ctx.stroke();
        o.r = r;
      }
      ctx.font = "12px ui-sans-serif,system-ui,'PingFang SC',sans-serif";
      ctx.textAlign = "center"; ctx.textBaseline = "bottom"; ctx.lineJoin = "round";
      const placed = [];
      const must = (o) => hover && lit.has(o.n.id);
      const byPriority = [...order2].sort((a, b) => (must(b) - must(a)) || (a.top.z - b.top.z));
      for (const o of byPriority) {
        const keep = !hover || lit.has(o.n.id);
        const label = clip(`#${o.n.id} ${shortTitle(o.n.title)}`.trim(), 11);
        const w = ctx.measureText(label).width, y = o.top.y - o.r - 5;
        const box = { x0: o.top.x - w / 2 - 3, x1: o.top.x + w / 2 + 3, y0: y - 15, y1: y + 1 };
        const clash = placed.some((q) => box.x0 < q.x1 && box.x1 > q.x0 && box.y0 < q.y1 && box.y1 > q.y0);
        if (clash && !must(o)) continue;                 // 挤不下就不写,悬停时会出现
        placed.push(box);
        ctx.globalAlpha = keep ? 1 : 0.4;
        ctx.lineWidth = 4; ctx.strokeStyle = "#0d1117"; ctx.strokeText(label, o.top.x, y);
        ctx.fillStyle = "#c9d1d9"; ctx.fillText(label, o.top.x, y);
      }
      ctx.globalAlpha = 1;
      // 6. 轴上的字:层级在左边、领域在后边(和平面图一样)
      ctx.textBaseline = "middle"; ctx.textAlign = "right";
      ctx.font = "12px ui-sans-serif,system-ui,'PingFang SC',sans-serif"; ctx.fillStyle = "#8b949e";
      L.layers.forEach((l, i) => {
        const Z = layZ[i] + L.rowH[i] / 2;
        const q = proj(B, gx0 - 16, 0, Z);
        if (q) ctx.fillText(LAYER_NAME[l], q.x, q.y);
      });
      ctx.textAlign = "center"; ctx.fillStyle = "#e6edf3";
      ctx.font = "600 13px ui-sans-serif,system-ui,'PingFang SC',sans-serif";
      L.domains.forEach((d, i) => {
        const X = L.padL + i * (L.colW + L.colGap) + L.colW / 2 - cX;
        const q = proj(B, X, 0, gz1 + 36);
        if (q) ctx.fillText(domainLabel(d), q.x, q.y);
      });
      frame.order = order2;
    }
    function loop(now) {
      if (!alive || !document.body.contains(cv)) return;
      if (view.auto && (!last || now - last > 40)) { view.yaw += 0.005; dirty = true; last = now; }
      if (dirty) { frame(); dirty = false; }
      requestAnimationFrame(loop);
    }
    requestAnimationFrame(loop);

    // 交互:拖动旋转、滚轮缩放、悬停提示、点击打开;方向键也能转
    const hit = (ev) => {
      const b = cv.getBoundingClientRect(), x = ev.clientX - b.left, y = ev.clientY - b.top;
      let best = null, bd = 1e9;
      for (const o of [...(frame.order || [])].reverse()) {          // 近的优先
        const d = Math.hypot(o.top.x - x, o.top.y - y);
        if (d < Math.max(14, o.r + 7) && d < bd) { bd = d; best = o.n; }
      }
      return best;
    };
    let drag = null;
    cv.addEventListener("pointerdown", (ev) => { drag = { x: ev.clientX, y: ev.clientY, moved: 0 }; view.auto = false; cv.setPointerCapture(ev.pointerId); });
    cv.addEventListener("pointermove", (ev) => {
      if (drag) {
        const dx = ev.clientX - drag.x, dy = ev.clientY - drag.y;
        drag.moved += Math.abs(dx) + Math.abs(dy); drag.x = ev.clientX; drag.y = ev.clientY;
        view.yaw += dx * 0.006; view.pitch = Math.max(0.15, Math.min(1.35, view.pitch + dy * 0.005));
        showTip(host, null); dirty = true; return;
      }
      const n = hit(ev);
      const id = n ? n.id : null;
      if (id !== hover) { hover = id; dirty = true; }
      cv.style.cursor = n ? "pointer" : "grab";
      showTip(host, n ? ev : null, n);
    });
    cv.addEventListener("pointerup", (ev) => {
      const click = drag && drag.moved < 5;
      drag = null;
      if (click) { const n = hit(ev); if (n) onPick(n); }
    });
    cv.addEventListener("pointercancel", () => { drag = null; });
    cv.addEventListener("pointerleave", () => { if (!drag) { hover = null; dirty = true; showTip(host, null); } });
    cv.addEventListener("wheel", (ev) => { ev.preventDefault(); view.zoom = Math.max(0.5, Math.min(3, view.zoom * Math.exp(-ev.deltaY * 0.0015))); view.auto = false; dirty = true; }, { passive: false });
    cv.addEventListener("keydown", (ev) => {
      const k = { ArrowLeft: [-0.12, 0], ArrowRight: [0.12, 0], ArrowUp: [0, -0.08], ArrowDown: [0, 0.08] }[ev.key];
      if (!k) return;
      ev.preventDefault(); view.auto = false;
      view.yaw += k[0]; view.pitch = Math.max(0.15, Math.min(1.35, view.pitch + k[1])); dirty = true;
    });
    return {
      reset() { Object.assign(view, { yaw: -0.34, pitch: 0.60, zoom: 1 }); dirty = true; },
      toggleAuto() { view.auto = !view.auto; return view.auto; },
      get auto() { return view.auto; },
      stop() { alive = false; },
    };
  }

  function legend(data, mode) {
    const s = data.summary || {};
    const credSw = CRED.map(([, c, label]) =>
      `<span class='mapLg'><i style='background:${c}'></i>${label}</span>`).join("");
    return `<div class='mapLegend'>
      ${mode === "3d" ? "<span><b>高度</b> 有效可信度</span>" : ""}
      <span><b>颜色</b> 有效可信度 ${credSw}</span>
      <span><b>${mode === "3d" ? "山的宽度" : "大小"}</b> 依据它的单元数</span>
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
      <div class='mapBar'>
        <span class='mapSeg' role='group' aria-label='视图'>
          <button id='map2d' aria-pressed='true'>平面</button><button id='map3d' aria-pressed='false'>立体</button>
        </span>
        <span id='map3dCtl' style='display:none'>
          <button id='mapReset'>重置视角</button> <button id='mapSpin'>暂停旋转</button>
          <span class='mut'>拖动旋转 · 滚轮缩放 · 点一个点看详情</span>
        </span>
      </div>
      <div id='mapLegendBox'>${legend(data, "2d")}</div>
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
    let mode = "2d", ctl3d = null;
    const render = () => {
      if (ctl3d) { ctl3d.stop(); ctl3d = null; }
      showTip(host, null);
      if (mode === "3d") ctl3d = draw3d(host, data, pick);
      else { host.querySelectorAll(":scope > canvas").forEach((x) => x.remove()); draw(host, data, pick); }
      el("mapLegendBox").innerHTML = legend(data, mode);
      el("map2d").setAttribute("aria-pressed", String(mode === "2d"));
      el("map3d").setAttribute("aria-pressed", String(mode === "3d"));
      el("map3dCtl").style.display = mode === "3d" ? "" : "none";
      if (ctl3d) el("mapSpin").textContent = ctl3d.auto ? "暂停旋转" : "自动旋转";
    };
    el("map2d").addEventListener("click", () => { if (mode !== "2d") { mode = "2d"; render(); } });
    el("map3d").addEventListener("click", () => { if (mode !== "3d") { mode = "3d"; render(); } });
    el("mapReset").addEventListener("click", () => ctl3d && ctl3d.reset());
    el("mapSpin").addEventListener("click", () => { if (ctl3d) el("mapSpin").textContent = ctl3d.toggleAuto() ? "暂停旋转" : "自动旋转"; });
    render();
    // 宽度变了就重排(只在这个视图还在时)
    let t = null;
    const onResize = () => { if (!document.body.contains(host)) return window.removeEventListener("resize", onResize); clearTimeout(t); t = setTimeout(render, 150); };
    window.addEventListener("resize", onResize);
    document.querySelectorAll("#view a[data-unit]").forEach((a) => a.addEventListener("click", () => showUnit(a.dataset.unit)));
  }

  window.ZXMap = { show, draw, draw3d, layout };
  window.ZXTerrain = window.ZXMap;   // 旧名,兼容
})();
