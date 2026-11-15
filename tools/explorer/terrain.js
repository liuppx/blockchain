"use strict";
// 认知地形图:把知识图渲染成可导航的地形。
//
// 数据来源:链上的引用关系(依据 / 参见 / 取代)与可信度——**不新增任何链上状态**。
// 地形元素的定义(白皮书 §9):
//   峰 (Peak)      核心共识命题:引用数 × 验证人数 × 时间累积
//   高原 (Plateau)  成熟稳定领域:邻居密度高 + 边权重稳定
//   山脊 (Ridge)    跨领域桥梁:连接多个高密度子图的节点
//   山谷 (Valley)   认知空白:低密度 + 孤立节点群
//   陡坡 (Cliff)    范式转换:短期内边权重剧变
//   断层 (Fault)    认知冲突:高"取代"关系 + 并存的竞争单元
//
// 高度 = f(可信度 × 重要性),用对数压缩,避免一两条热门知识把整张图压平。
// 这是一张**投影图**:底层是知识单元与引用,地形只是它的一种读法。
const crypto = require("crypto");
const http = require("http");

const LAYER_ZH = { foundation: "基础", explanation: "解释", scenario: "场景", application: "应用" };
const KIND_ZH = { basis: "依据", "see-also": "参见", supersedes: "取代" };

function sha256(buf) { return crypto.createHash("sha256").update(buf).digest("hex"); }

class TerrainBuilder {
  constructor(rpcBase) { this.rpcBase = rpcBase; }

  rpcJson(p) {
    return new Promise((resolve, reject) => {
      const u = new URL(this.rpcBase + p);
      const req = http.request({ hostname: u.hostname, port: u.port || 80, path: u.pathname + u.search,
        headers: { Accept: "application/json" } }, (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () => {
          if (res.statusCode >= 400) return reject(new Error(p + " → HTTP " + res.statusCode));
          try { resolve(JSON.parse(b)); } catch (e) { reject(e); }
        });
      });
      req.on("error", reject);
      req.end();
    });
  }

  async allUnits() {
    const out = [];
    let cursor = null;
    for (let i = 0; i < 50; i++) {
      const page = await this.rpcJson("/units?limit=200" + (cursor ? "&cursor=" + encodeURIComponent(cursor) : ""));
      for (const it of page.items || []) out.push(it);
      cursor = page.cursor;
      if (!cursor || !(page.items || []).length) break;
    }
    return out;
  }

  /** 把单元目录 + 单元详情汇总成地形节点与边。 */
  async build() {
    const summaries = await this.allUnits();
    const details = [];
    for (const it of summaries) {
      try { details.push(await this.rpcJson("/unit/" + it.id)); } catch (_) { /* 跳过 */ }
    }
    const nodes = [];
    const edges = [];
    const byId = new Map();
    for (const u of details) {
      const v = u.versions[u.versions.length - 1];
      const cred = v.credibility || {};
      const passes = Number(cred.passes || 0);
      const eff = Number(cred.effective_bps || 0) / 10000;   // 0..1
      const title = (v.bodies && v.bodies[0] && v.bodies[0].hash) || "";
      const node = {
        id: String(u.id),
        layer: u.layer,
        layer_zh: LAYER_ZH[u.layer] || u.layer,
        domain: String(u.domain),
        author: String(u.author),
        versions: u.versions.length,
        passes,
        effective: eff,
        lifecycle: u.lifecycle || "active",
        body_hash: title,
        cites: [],
        cited_by: [],
        supersedes: [],
        superseded_by: [],
      };
      byId.set(node.id, node);
      nodes.push(node);
    }
    for (const u of details) {
      const from = byId.get(String(u.id));
      if (!from) continue;
      const v = u.versions[u.versions.length - 1];
      for (const c of v.cites || []) {
        const to = byId.get(String(c.unit));
        const e = { from: from.id, to: String(c.unit), kind: c.kind, version: String(c.version) };
        edges.push(e);
        from.cites.push(e);
        if (to) {
          to.cited_by.push(e);
          if (c.kind === "supersedes") { from.supersedes.push(String(c.unit)); to.superseded_by.push(from.id); }
        }
      }
    }
    // 重要度:被引越多、层级越基础,重要性越高
    for (const n of nodes) {
      const basisIn = n.cited_by.filter((e) => e.kind === "basis").length;
      const basisOut = n.cites.filter((e) => e.kind === "basis").length;
      n.basis_in = basisIn;
      n.basis_out = basisOut;
      n.degree = n.cited_by.length + n.cites.length;
      // 层级权重:基础层的依赖最多,重要性最高
      const layerW = { foundation: 1.0, explanation: 0.8, scenario: 0.65, application: 0.5 }[n.layer] || 0.5;
      n.importance = (1 + basisIn * 1.5 + n.passes * 0.6) * layerW;
      // 领域连通性:山脊要连接多个领域
      n.domains = [...new Set([n.domain, ...n.cited_by.map((e) => (byId.get(e.to) || {}).domain).filter(Boolean)])];
    }
    // 地形分类
    for (const n of nodes) {
      const tags = [];
      if (n.passes >= 2 && n.basis_in >= 2) tags.push("peak");
      if (n.basis_in >= 3 && n.degree >= 4) tags.push("plateau");
      if (n.domains.length >= 2) tags.push("ridge");
      if (n.degree === 0) tags.push("valley");
      if (n.superseded_by.length > 0 || n.supersedes.length > 0) tags.push("fault");
      if (!tags.length) tags.push(n.basis_in > 0 ? "slope" : "plain");
      n.tags = tags;
      // 高度:可信度 × 重要度,取对数压缩;孤立且无人核验的落在低谷
      const raw = Math.max(0.02, n.effective * (0.35 + n.importance));
      n.height = Math.log(1 + raw * 12) / Math.log(13); // 0..1
      if (n.degree === 0 && n.passes === 0) n.height *= 0.25;
    }
    // 布局:按领域分列,同域内按层级分行(确定性的力导向初始布局,便于人工核对)
    const domains = [...new Set(nodes.map((n) => n.domain))].sort();
    const layers = ["foundation", "explanation", "scenario", "application"];
    nodes.forEach((n) => {
      const di = Math.max(0, domains.indexOf(n.domain));
      const li = Math.max(0, layers.indexOf(n.layer));
      n.col = di;
      n.row = li;
    });
    const summary = {
      nodes: nodes.length,
      edges: edges.length,
      domains: domains.length,
      by_tag: nodes.reduce((a, n) => { a[n.tags[0]] = (a[n.tags[0]] || 0) + 1; return a; }, {}),
      by_layer: nodes.reduce((a, n) => { a[n.layer] = (a[n.layer] || 0) + 1; return a; }, {}),
    };
    return { nodes, edges, summary, generated_at: new Date().toISOString() };
  }
}

module.exports = { TerrainBuilder, LAYER_ZH, KIND_ZH };
