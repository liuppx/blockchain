"use strict";
// 知识检索与导出(浏览器服务侧)。
//
// 链上只有正文哈希,正文在内容寻址的正文仓库里,所以**全文检索与导出只能在这一层做**:
//   GET /search?q=…&domain=…&layer=…   跨标题与正文的检索(带摘要)
//   GET /export/units                    全部单元的 JSONL 导出(每行一个单元,含正文)
//
// 检索索引按需构建并缓存:扫描正文仓库里的每个正文文件,分词后建立"词 → 单元"倒排索引。
// 正文以内容哈希为文件名,内容不可变,所以索引只需在正文变化时增量更新(按 mtime 判断)。
// 导出遵守协议强制的 CC BY 4.0:随文件给出许可与来源说明。
//
// 无依赖:只用 Node 内置模块。
const fs = require("fs");
const path = require("path");
const http = require("http");
const crypto = require("crypto");

const LICENSE = "CC-BY-4.0";
const CJK = /[㐀-鿿豈-﫿]/;

function tokenize(text) {
  // 英文/数字按词切分(小写),CJK 逐字切分并额外保留双字组合(中文短语检索)
  const out = new Set();
  const lower = text.toLowerCase();
  for (const m of lower.matchAll(/[a-z0-9_]+/g)) out.add(m[0]);
  const cjk = lower.match(/[㐀-鿿豈-﫿]/g) || [];
  for (const c of cjk) out.add(c);
  for (let i = 0; i + 1 < cjk.length; i++) out.add(cjk[i] + cjk[i + 1]);
  return out;
}

// 摘要:取命中词附近的片段
function excerpt(text, terms, len = 220) {
  const flat = text.replace(/\s+/g, " ").trim();
  if (!terms.length) return flat.slice(0, len);
  const lower = flat.toLowerCase();
  let at = -1;
  for (const t of terms) {
    const i = lower.indexOf(t);
    if (i >= 0 && (at < 0 || i < at)) at = i;
  }
  if (at < 0) return flat.slice(0, len);
  const start = Math.max(0, at - Math.floor(len / 3));
  return (start > 0 ? "…" : "") + flat.slice(start, start + len) + (start + len < flat.length ? "…" : "");
}

class KnowledgeIndex {
  constructor(bodyDir, rpcBase) {
    this.bodyDir = bodyDir;
    this.rpcBase = rpcBase;
    this.units = [];          // 单元摘要(来自链)
    this.bodies = new Map();  // hash → {text, title, mtime}
    this.postings = new Map();// term → Set(unitId)
    this.unitTerms = new Map();// unitId → Set(term)
    this.builtAt = 0;
    this.unitsFetchedAt = 0;
  }

  rpcJson(p) {
    return new Promise((resolve, reject) => {
      const u = new URL(this.rpcBase + p);
      const req = http.request({
        hostname: u.hostname, port: u.port || 80, path: u.pathname + u.search,
        headers: { Accept: "application/json" },
      }, (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () => {
          try { resolve(JSON.parse(b)); } catch (e) { reject(e); }
        });
      });
      req.on("error", reject);
      req.end();
    });
  }

  // 拉取链上单元目录(分页拉完)。节点返回 {"total","cursor","items":[...]} 信封。
  async refreshUnits() {
    const out = [];
    let cursor = null;
    for (let i = 0; i < 50; i++) {
      const q = "/units?limit=200" + (cursor ? "&cursor=" + encodeURIComponent(cursor) : "");
      const page = await this.rpcJson(q);
      const items = page.items || [];
      for (const it of items) out.push(it);
      cursor = page.cursor;
      if (!cursor || !items.length) break;
    }
    this.units = out;
    this.unitsFetchedAt = Date.now();
  }

  // 扫描正文仓库,建立倒排索引
  async refreshBodies() {
    if (!this.bodyDir) return;
    let names = [];
    try { names = await fs.promises.readdir(this.bodyDir); } catch (_) { return; }
    for (const name of names) {
      if (!/^[0-9a-f]{64}$/.test(name)) continue;
      const file = path.join(this.bodyDir, name);
      let st;
      try { st = await fs.promises.stat(file); } catch (_) { continue; }
      const known = this.bodies.get(name);
      if (known && known.mtime === st.mtimeMs) continue;
      let text;
      try { text = await fs.promises.readFile(file, "utf8"); } catch (_) { continue; }
      // 与链上一致性:正文哈希必须等于文件名(内容寻址的保证)
      const h = crypto.createHash("sha256").update(Buffer.from(text, "utf8")).digest("hex");
      if (h !== name) continue;
      const title = (/^#\s+(.+)$/m.exec(text) || [, ""])[1].trim();
      this.bodies.set(name, { text, title, mtime: st.mtimeMs });
    }
  }

  // 把正文挂到单元上,并建索引
  async build() {
    await this.refreshUnits();
    await this.refreshBodies();
    this.postings = new Map();
    this.unitTerms = new Map();
    for (const u of this.units) {
      const terms = new Set();
      for (const h of u.body_hashes || []) {
        const b = this.bodies.get(h);
        if (!b) continue;
        if (b.title) for (const t of tokenize(b.title)) terms.add(t);
        for (const t of tokenize(b.text)) terms.add(t);
      }
      this.unitTerms.set(Number(u.id), terms);
      for (const t of terms) {
        if (!this.postings.has(t)) this.postings.set(t, new Set());
        this.postings.get(t).add(Number(u.id));
      }
    }
    this.builtAt = Date.now();
  }

  async ensureFresh(maxAgeMs = 30000) {
    if (!this.builtAt || Date.now() - this.builtAt > maxAgeMs) {
      try { await this.build(); } catch (_) { /* 保留旧索引 */ }
    }
  }

  // 检索:命中标题或正文的单元
  search(q, opts = {}) {
    const terms = tokenize(q || "");
    const scored = [];
    for (const u of this.units) {
      const id = Number(u.id);
      if (opts.domain != null && Number(u.domain) !== Number(opts.domain)) continue;
      if (opts.layer && u.layer !== opts.layer) continue;
      const uTerms = this.unitTerms.get(id) || new Set();
      let score = 0;
      const hits = [];
      for (const t of terms) {
        if (uTerms.has(t)) { score += t.length > 1 ? 2 : 1; hits.push(t); }
      }
      if (!terms.size) { score = 1; }               // 空查询:按列表返回
      else if (!score) continue;
      // 标题命中加权
      for (const h of u.body_hashes || []) {
        const b = this.bodies.get(h);
        if (!b) continue;
        for (const t of terms) if (b.title.toLowerCase().includes(t)) score += 3;
      }
      scored.push({ unit: u, score, hits });
    }
    scored.sort((a, b) => (b.score - a.score) || (Number(a.unit.id) - Number(b.unit.id)));
    const limit = Math.min(Number(opts.limit) || 50, 200);
    return scored.slice(0, limit).map(({ unit, score, hits }) => {
      const h = (unit.body_hashes || [])[0];
      const b = h ? this.bodies.get(h) : null;
      return {
        id: unit.id, layer: unit.layer, domain: unit.domain, author: unit.author,
        versions: unit.versions, title: b ? b.title : "",
        excerpt: b ? excerpt(b.text, hits) : "",
        credibility: unit.credibility || null,
        body_url: h ? "/bodies/" + h : null,
        score,
      };
    });
  }

  // 导出:全部单元(JSONL),含正文
  async exportUnits() {
    await this.ensureFresh(0);
    const out = [];
    for (const u of this.units) {
      const bodies = [];
      for (const h of u.body_hashes || []) {
        const b = this.bodies.get(h);
        bodies.push({
          lang: "zh",
          sha256: h,
          url: "/bodies/" + h,
          text: b ? b.text : null,     // 正文缺失时为 null(链上哈希仍在)
          title: b ? b.title : "",
        });
      }
      out.push({
        id: u.id, layer: u.layer, domain: u.domain, author: u.author,
        created_height: u.created_height, versions: u.versions,
        credibility: u.credibility || null,
        cites: u.cites || [],
        license: LICENSE,
        bodies,
      });
    }
    return out;
  }
}

module.exports = { KnowledgeIndex, tokenize, excerpt, LICENSE };
