"use strict";
// 认知地形图(浏览器视图)。
//
// 数据来自 /terrain:链上引用关系与可信度的投影。**不新增链上状态**,也不需要额外权限。
// 地形元素(白皮书 §9):峰 / 高原 / 山脊 / 山谷 / 断层 / 陡坡。
(function () {
  const TAG_ZH = {
    peak: "峰", plateau: "高原", ridge: "山脊",
    valley: "山谷", fault: "断层", slope: "坡", plain: "平地",
  };
  const TAG_COLOR = {
    peak: "#d9a441", plateau: "#7d9b76", ridge: "#8b6fc4",
    valley: "#3b4a5a", fault: "#c4483f", slope: "#4a6d8c", plain: "#39414d",
  };
  const el = (id) => document.getElementById(id);

  function draw(canvas, data, opts) {
    const dpr = window.devicePixelRatio || 1;
    const W = canvas.clientWidth, H = canvas.clientHeight;
    canvas.width = W * dpr; canvas.height = H * dpr;
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    ctx.fillStyle = "#0d1117";
    ctx.fillRect(0, 0, W, H);

    const nodes = data.nodes || [];
    if (!nodes.length) return;
    const cols = Math.max(1, ...nodes.map((n) => n.col)) + 1;
    const rows = Math.max(1, ...nodes.map((n) => n.row)) + 1;
    const padX = 70, padY = 40, padB = 46;
    const cw = (W - padX * 2) / cols;
    const usableH = H - padY - padB;
    // 每层的带高:层数少时给足高度(地形要有起伏),层数多时压缩
    const ch = usableH / Math.max(1, rows);

    // 节点位置:每个层级一条带(基础在下),带内按领域分列、同一格里再散开。
    // 层级带的高度按该层单元数分配(人多的层更宽),避免所有单元挤在一列。
    const byRow = new Map();
    for (const n of nodes) {
      if (!byRow.has(n.row)) byRow.set(n.row, []);
      byRow.get(n.row).push(n);
    }
    const rowHeights = [];
    let totalUnits = 0;
    for (const [, list] of byRow) totalUnits += list.length;
    for (let r = 0; r < rows; r++) {
      const cnt = (byRow.get(r) || []).length;
      rowHeights.push(totalUnits ? cnt / totalUnits : 1 / rows);
    }
    const pos = new Map();
    for (let r = 0; r < rows; r++) {
      const list = (byRow.get(r) || []).sort((a, b) => Number(a.id) - Number(b.id));
      if (!list.length) continue;
      const bandH = rowHeights[r] * usableH;
      const bandBottom = H - padB - rowHeights.slice(0, r).reduce((a, b) => a + b, 0) * usableH;
      // 带内按领域分组,每组横向排开
      const byCol = new Map();
      for (const n of list) {
        if (!byCol.has(n.col)) byCol.set(n.col, []);
        byCol.get(n.col).push(n);
      }
      const groups = [...byCol.values()];
      const groupW = (W - padX * 2) / groups.length;
      groups.forEach((grp, gi) => {
        const cx = padX + gi * groupW + groupW / 2;
        const per = Math.max(1, Math.ceil(Math.sqrt(grp.length)));
        grp.forEach((n, i) => {
          const gx = Math.floor(i / per), gy = i % per;
          const spread = Math.min(groupW * 0.75, bandH * 0.75);
          const ox = (gx - (per - 1) / 2) * (spread / per) * 1.8;
          const oy = (gy - (per - 1) / 2) * (spread / per) * 1.4;
          const lift = n.height * (bandH * 0.5);
          pos.set(n.id, { x: cx + ox, y: bandBottom - lift + oy, n });
        });
      });
    }

    // 层带:底色与名称,让人读得出"这是哪一层"
    const LAYERS = [["application", "应用"], ["scenario", "场景"], ["explanation", "解释"], ["foundation", "基础"]];
    ctx.textAlign = "right";
    ctx.font = "11px ui-monospace,Menlo,monospace";
    for (let r = 0; r < rows; r++) {
      const cnt = (byRow.get(r) || []).length;
      if (!cnt) continue;
      const bandH = rowHeights[r] * usableH;
      const bandBottom = H - padB - rowHeights.slice(0, r).reduce((a, b) => a + b, 0) * usableH;
      ctx.fillStyle = "rgba(255,255,255,.025)";
      ctx.fillRect(padX - 20, bandBottom - bandH, W - padX * 2 + 40, bandH);
      ctx.fillStyle = "#6e7681";
      const name = (LAYERS.find((l) => l[0] === ["foundation", "explanation", "scenario", "application"][r]) || [, ""])[1];
      ctx.fillText(name + " " + cnt, padX - 26, bandBottom - bandH / 2);
    }
    ctx.textAlign = "left";

    // 连线:依据(实线)/参见(虚线)/取代(红)
    ctx.lineWidth = 1;
    for (const e of data.edges || []) {
      const a = pos.get(e.from), b = pos.get(e.to);
      if (!a || !b) continue;
      ctx.strokeStyle = e.kind === "supersedes" ? "rgba(196,72,63,.65)" :
        e.kind === "basis" ? "rgba(120,150,190,.34)" : "rgba(140,140,160,.18)";
      ctx.setLineDash(e.kind === "see-also" ? [3, 3] : []);
      ctx.beginPath();
      ctx.moveTo(a.x, a.y);
      ctx.bezierCurveTo(a.x, (a.y + b.y) / 2, b.x, (a.y + b.y) / 2, b.x, b.y);
      ctx.stroke();
    }
    ctx.setLineDash([]);

    // 节点:半径按被引数,颜色按地形类型,高度已经体现在 y 上
    for (const [, p] of pos) {
      const n = p.n;
      const r = 3 + Math.min(9, Math.sqrt(n.cited_by.length) * 2.2);
      ctx.beginPath();
      ctx.fillStyle = TAG_COLOR[n.tags[0]] || TAG_COLOR.plain;
      ctx.globalAlpha = 0.35 + 0.65 * (n.effective || 0);
      ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
      ctx.fill();
      ctx.globalAlpha = 1;
      if (opts && opts.labels && (n.tags[0] === "peak" || n.tags[0] === "ridge" || n.tags[0] === "fault")) {
        ctx.fillStyle = "#c9d1d9";
        ctx.font = "11px ui-monospace,Menlo,monospace";
        ctx.textAlign = "center";
        ctx.fillText("#" + n.id, p.x, p.y - r - 4);
      }
    }

    // 图例(横向一行)
    ctx.textAlign = "left";
    ctx.font = "12px ui-monospace,Menlo,monospace";
    const shown = Object.entries(TAG_ZH).filter(([tag]) => (data.summary.by_tag || {})[tag]);
    const w = Math.max(80, (W - 40) / Math.max(1, shown.length));
    shown.forEach(([tag, zh], i) => {
      const x = 20 + i * w, y = H - 22;
      ctx.fillStyle = TAG_COLOR[tag];
      ctx.beginPath(); ctx.arc(x, y - 4, 5, 0, Math.PI * 2); ctx.fill();
      ctx.fillStyle = "#8b949e";
      ctx.fillText(zh + " " + ((data.summary.by_tag || {})[tag] || 0), x + 12, y);
    });
    return pos;
  }

  async function show() {
    setView("<span class='mut'>正在读取知识图…</span>");
    let data;
    try {
      const r = await fetch("/terrain");
      if (!r.ok) throw new Error("HTTP " + r.status);
      data = await r.json();
    } catch (e) {
      setView(`<span class='err'>地形数据读取失败:${esc(e.message)}</span>`);
      return;
    }
    const s = data.summary || {};
    setView(`<h2 style='border:0;padding:0 0 4px'>认知地形图</h2>
      <p class='mut'>这是知识图的一种读法:横轴是领域,纵轴是层级(自下而上 基础 → 解释 → 场景 → 应用),
      高度是可信度与被引用程度,颜色是地形类型。<b>数据全部来自链上已有的引用与核验记录</b>,不新增任何链上状态。</p>
      <p class='mut'>${s.nodes} 个单元 · ${s.edges} 条引用 · ${s.domains} 个领域 ·
      标签:${Object.entries(s.by_tag || {}).map(([k, v]) => (TAG_ZH[k] || k) + " " + v).join(" · ")}</p>
      <canvas id='terrainCv' style='width:100%;height:520px;border:1px solid var(--line);border-radius:6px'></canvas>
      <p class='mut'><label><input type='checkbox' id='terrainLabels' checked> 标出峰 / 山脊 / 断层</label></p>
      <div id='terrainPick' class='mut'></div>`);
    const cv = el("terrainCv");
    let pos = null;
    const render = () => {
      pos = draw(cv, data, { labels: el("terrainLabels").checked });
    };
    el("terrainLabels").addEventListener("change", render);
    setTimeout(render, 30);
    // 点击节点:显示它的地形属性与引用
    cv.addEventListener("click", (ev) => {
      const rect = cv.getBoundingClientRect();
      const x = ev.clientX - rect.left, y = ev.clientY - rect.top;
      let best = null, bestD = 1e9;
      for (const [id, p] of pos || []) {
        const d = Math.hypot(p.x - x, p.y - y);
        if (d < bestD && d < 18) { bestD = d; best = { id, p }; }
      }
      const box = el("terrainPick");
      if (!best) { box.innerHTML = ""; return; }
      const n = best.p.n;
      const cites = n.cites.map((e) => `#${e.to}(${KIND_ZH[e.kind] || e.kind})`).join("、") || "—";
      const cited = n.cited_by.map((e) => `#${e.from}`).join("、") || "—";
      box.innerHTML = `<div style='margin-top:10px;border-top:1px solid var(--line);padding-top:8px'>
        <b>#${esc(n.id)}</b> · ${esc(n.layer_zh)} · 领域 ${esc(n.domain)} · ${esc(TAG_ZH[n.tags[0]] || n.tags[0])}
        · 可信度 ${esc(n.effective)} · ${n.passes} 位核验通过 · 被引 ${n.cited_by.length} 次
        <br>它依据:${esc(cites)}<br>它被引用于:${esc(cited)}
        <br><a class='link' data-unit='${esc(n.id)}'>打开这个单元 →</a></div>`;
      box.querySelector("a[data-unit]").addEventListener("click", () => showUnit(n.id));
    });
  }

  window.ZXTerrain = { show, draw };
})();
