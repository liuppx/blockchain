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

const { unitToJsonLd, memberToJsonLd, LICENSE_URL } = require("./ontology.js");

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
    // 单元详情(RPC /unit/{id}:每个版本的发布人、来源、正文、引用、核验、可信度)。
    // 只有单元的版本数或核验 / 可信度变化时才重新拉取。
    this.details = new Map();   // unitId → { sig, detail }
    this.bodyOwner = new Map(); // 正文哈希 → { unit, version, lang }
    this.members = new Map();   // 账户号 → { id, pubkey, established }
    this.membersFetchedAt = 0;
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

  /** 单元详情的变化签名:版本数、核验数、可信度、生命周期任一变化就重新拉详情。 */
  static detailSig(u) {
    const c = u.credibility || {};
    return [u.versions, c.passes, c.needs, c.self_bps, c.effective_bps, u.lifecycle].join("|");
  }

  async refreshDetails() {
    const stale = this.units.filter((u) => {
      const known = this.details.get(String(u.id));
      return !known || known.sig !== KnowledgeIndex.detailSig(u);
    });
    // 并发拉取,但不压垮节点
    for (let i = 0; i < stale.length; i += 8) {
      await Promise.all(stale.slice(i, i + 8).map(async (u) => {
        try {
          const detail = await this.rpcJson("/unit/" + u.id);
          if (detail && detail.id != null) {
            detail.lifecycle = u.lifecycle;
            this.details.set(String(u.id), { sig: KnowledgeIndex.detailSig(u), detail });
          }
        } catch (_) { /* 下次再拉 */ }
      }));
    }
    this.bodyOwner = new Map();
    for (const { detail } of this.details.values()) {
      for (const v of detail.versions || []) {
        for (const b of v.bodies || []) {
          // 同一份正文只能属于一个单元(U2);同一单元的多个版本可以复用同一份正文:
          // version 记最早出现的版本,versions 列出所有用到它的版本
          const known = this.bodyOwner.get(b.hash);
          if (!known) this.bodyOwner.set(b.hash, { unit: String(detail.id), version: String(v.n), versions: [String(v.n)], lang: b.lang });
          else if (known.unit === String(detail.id)) known.versions.push(String(v.n));
        }
      }
    }
  }

  /** 成员与公钥(换钥可能改变公钥,所以几分钟刷新一次)。 */
  async refreshMembers(maxAgeMs = 5 * 60 * 1000) {
    if (this.membersFetchedAt && Date.now() - this.membersFetchedAt < maxAgeMs) return;
    const page = await this.rpcJson("/members?limit=500");
    const next = new Map();
    for (const m of page.items || []) {
      try {
        const a = await this.rpcJson("/account/" + m.id);
        next.set(String(m.id), { id: String(m.id), pubkey: String(a.pubkey || "").toLowerCase(), established: !!m.established });
      } catch (_) { /* 跳过 */ }
    }
    this.members = next;
    this.membersFetchedAt = Date.now();
  }

  // 把正文挂到单元上,并建索引
  async build() {
    await this.refreshUnits();
    await this.refreshBodies();
    await this.refreshDetails();
    try { await this.refreshMembers(); } catch (_) { /* 成员查询失败不影响检索 */ }
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

  // 引用图的上下游:从某单元出发,沿"依据"向上(谁建立在它之上)与向下(它建立在谁之上)
  // ——这是"持续探索"需要的方向感:一条想法被谁继承,又站在谁的肩上。
  graph() {
    const byId = new Map(this.units.map((u) => [String(u.id), u]));
    const up = new Map(), down = new Map();
    for (const u of this.units) {
      const id = String(u.id);
      const cites = u.cites || [];
      for (const c of cites) {
        if (!down.has(id)) down.set(id, []);
        down.get(id).push(c);
        if (!up.has(String(c.unit))) up.set(String(c.unit), []);
        up.get(String(c.unit)).push({ from: id, kind: c.kind, version: c.version });
      }
    }
    return { byId, up, down };
  }

  /** 某单元的上游(谁建立在它之上),按距离分层。 */
  upstream(id, maxDepth = 3) {
    const { up } = this.graph();
    const out = [];
    let frontier = [String(id)];
    const seen = new Set(frontier);
    for (let depth = 1; depth <= maxDepth && frontier.length; depth++) {
      const next = [];
      for (const f of frontier) {
        for (const e of up.get(f) || []) {
          if (seen.has(e.from)) continue;
          seen.add(e.from);
          next.push(e.from);
          out.push({ unit: e.from, depth, via: f, kind: e.kind });
        }
      }
      frontier = next;
    }
    return out;
  }

  /** 某单元的地基(它建立在谁之上),按距离分层。 */
  downstream(id, maxDepth = 3) {
    const { down } = this.graph();
    const out = [];
    let frontier = [String(id)];
    const seen = new Set(frontier);
    for (let depth = 1; depth <= maxDepth && frontier.length; depth++) {
      const next = [];
      for (const f of frontier) {
        for (const c of down.get(f) || []) {
          if (seen.has(String(c.unit))) continue;
          seen.add(String(c.unit));
          next.push(String(c.unit));
          out.push({ unit: String(c.unit), depth, via: f, kind: c.kind });
        }
      }
      frontier = next;
    }
    return out;
  }

  /** 缺口:没有任何单元依据它、也没人核验它——可以填的"山谷"。 */
  gaps(opts = {}) {
    const { byId } = this.graph();
    const out = [];
    for (const u of this.units) {
      const id = String(u.id);
      const citedBy = (u.cited_by || []).filter((e) => e.kind === "basis").length;
      const passes = Number((u.credibility || {}).passes || 0);
      if (citedBy === 0 && passes === 0) {
        out.push({
          id, domain: String(u.domain), layer: u.layer,
          lifecycle: u.lifecycle || "active",
          versions: u.versions,
          // 已确立(有人核验过)但无人依据:地基扎实,还没有上层——最值得被引用的
          established: passes > 0,
        });
      }
    }
    // 注意:out 是 const,不能重新赋值(否则 /gaps?domain= 会抛 "Assignment to constant variable")
    return opts.domain ? out.filter((g) => g.domain === String(opts.domain)) : out;
  }

  /** 按领域的覆盖情况:每个领域有多少单元、多少已确立、多少无人依据。 */
  coverage() {
    const { byId } = this.graph();
    const domains = new Map();
    for (const u of this.units) {
      const d = String(u.domain);
      if (!domains.has(d)) domains.set(d, { domain: d, total: 0, established: 0, unreferenced: 0, layers: {} });
      const e = domains.get(d);
      e.total += 1;
      e.layers[u.layer] = (e.layers[u.layer] || 0) + 1;
      const passes = Number((u.credibility || {}).passes || 0);
      const citedBy = (u.cited_by || []).filter((x) => x.kind === "basis").length;
      if (passes > 0) e.established += 1;
      if (citedBy === 0 && passes === 0) e.unreferenced += 1;
    }
    return [...domains.values()].sort((a, b) => a.domain.localeCompare(b.domain));
  }

  // 导出:全部单元(JSONL),含正文
  async exportUnits() {
    await this.ensureFresh(0);
    const out = [];
    for (const u of this.units) {
      const bodies = [];
      const detail = (this.details.get(String(u.id)) || {}).detail;
      for (const h of u.body_hashes || []) {
        const b = this.bodies.get(h);
        // 语言取自链上记录(以前写死为 zh)
        const owner = this.bodyOwner.get(h);
        bodies.push({
          lang: owner ? owner.lang : "zh",
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
        // 每个版本的发布人、来源、正文、引用、核验与可信度(只增字段,与旧格式兼容)
        version_history: detail ? (detail.versions || []).map((v) => ({
          n: v.n, publisher: v.publisher, published_height: v.published_height, source: v.source,
          bodies: (v.bodies || []).map((b) => ({ lang: b.lang, sha256: b.hash })),
          cites: v.cites || [], verifications: v.verifications || [], credibility: v.credibility || null,
        })) : null,
      });
    }
    return out;
  }

  bodyText(hash) {
    const b = this.bodies.get(hash);
    return b ? { text: b.text, title: b.title } : null;
  }

  /** 单元的 JSON-LD(含全部版本;withBodies 时带正文全文)。找不到返回 null。 */
  unitJsonLd(id, { withBodies = false, withContext = true } = {}) {
    const known = this.details.get(String(id));
    if (!known) return null;
    return unitToJsonLd(known.detail, { withContext, bodyText: withBodies ? (h) => this.bodyText(h) : null });
  }

  /** 导出全部单元的 JSON-LD(每个单元一个自包含文档,带正文)。 */
  async exportJsonLd() {
    await this.ensureFresh(0);
    return this.units
      .map((u) => this.unitJsonLd(u.id, { withBodies: true }))
      .filter(Boolean);
  }

  /** 正文哈希 → 它属于哪个单元的哪个版本(同一份正文只能属于一个单元)。 */
  lookupBody(hash) {
    const h = String(hash || "").toLowerCase();
    return this.bodyOwner.get(h) || null;
  }

  /** 公钥 → 链上成员(账户号、是否已过见习期)。不是成员返回 null。 */
  lookupPubkey(pubkey) {
    const pk = String(pubkey || "").toLowerCase();
    for (const m of this.members.values()) if (m.pubkey === pk) return m;
    return null;
  }

  memberJsonLd(id) {
    const m = this.members.get(String(id));
    return m ? memberToJsonLd(m) : null;
  }
}

module.exports = { KnowledgeIndex, tokenize, excerpt, LICENSE, LICENSE_URL };
