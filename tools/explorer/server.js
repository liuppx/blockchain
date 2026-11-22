#!/usr/bin/env node
"use strict";
// zhixing block explorer — zero-dependency static server + RPC proxy.
//
//   node server.js              # serve ./public on :8080, proxy /rpc/* → http://127.0.0.1:9221
//   PORT=9000 NODE_RPC=http://host:9221 node server.js
//
// The proxy keeps the browser same-origin (no CORS / mixed-content concerns) and lets you
// point the explorer at any node without editing files. Uses only Node core (http, fs, path).
//
// Knowledge bodies (content-addressed store, optional — enabled when BODY_DIR is set):
//   GET /bodies/<sha256>   public; the browser re-hashes what it gets against the on-chain hash
//   PUT /bodies/<sha256>   needs `Authorization: Bearer $BODY_TOKEN`; rejected unless
//                          sha256(body) == <sha256>, so nobody can substitute content
const http = require("http");
const crypto = require("crypto");
const fs = require("fs");
const path = require("path");
const { URL } = require("url");

const PORT = Number(process.env.PORT || 8080);
const HOST = process.env.HOST || "127.0.0.1"; // behind a reverse proxy; set HOST=0.0.0.0 to expose directly
const NODE_RPC = (process.env.NODE_RPC || "http://127.0.0.1:9221").replace(/\/+$/, "");
const PUBLIC = path.join(__dirname, "public");
const BODY_DIR = process.env.BODY_DIR || "";
const STATUS_DIR = process.env.STATUS_DIR || ""; // 运维状态(如 deploy/diskwatch.sh 写的 disk.json),只读公开
const BODY_TOKEN = process.env.BODY_TOKEN || "";
const MAX_BODY = 64 * 1024; // matches the node's RPC body cap
const { KnowledgeIndex, LICENSE } = require("./knowledge-index.js");
const { ReviewAssigner } = require("./review-assign.js");
const { TerrainBuilder } = require("./terrain.js");
const TERRAIN = new TerrainBuilder(NODE_RPC);
const INDEX = new KnowledgeIndex(BODY_DIR, NODE_RPC);
const ASSIGNER = new ReviewAssigner(NODE_RPC);

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8", ".json": "application/json", ".ico": "image/x-icon",
  ".jsonld": "application/ld+json; charset=utf-8" };

function serveStatic(req, res) {
  // Map "/" → index.html; prevent path traversal by resolving under PUBLIC.
  let rel = decodeURIComponent(new URL(req.url, "http://x").pathname);
  if (rel === "/") rel = "/index.html";
  const file = path.normalize(path.join(PUBLIC, rel));
  if (!file.startsWith(PUBLIC)) { res.writeHead(403).end("forbidden"); return; }
  fs.readFile(file, (err, buf) => {
    if (err) { res.writeHead(404, { "Content-Type": "text/plain" }).end("not found"); return; }
    res.writeHead(200, { "Content-Type": MIME[path.extname(file)] || "application/octet-stream" });
    res.end(buf);
  });
}

function proxy(req, res) {
  // /rpc/<path>?<qs> → NODE_RPC/<path>?<qs>
  const sub = req.url.slice("/rpc".length) || "/";
  const target = new URL(NODE_RPC + sub);
  const opts = {
    hostname: target.hostname, port: target.port || 80, path: target.pathname + target.search,
    method: req.method,
    headers: Object.fromEntries(Object.entries({
      "Accept": req.headers["accept"] || "application/json", "Host": target.host,
      // 成员提交核验时需要:写入令牌与请求体类型 / 长度
      "Authorization": req.headers["authorization"], "Content-Type": req.headers["content-type"],
      "Content-Length": req.headers["content-length"],
    }).filter(([, v]) => v !== undefined)),
  };
  const up = http.request(opts, (upRes) => {
    res.writeHead(upRes.statusCode || 502, { "Content-Type": upRes.headers["content-type"] || "text/plain" });
    upRes.pipe(res);
  });
  up.on("error", (e) => { res.writeHead(502, { "Content-Type": "text/plain" }).end("rpc proxy error: " + e.message); });
  req.pipe(up);
}

function tokenOk(req) {
  const m = /^Bearer (.+)$/.exec(req.headers["authorization"] || "");
  if (!BODY_TOKEN || !m) return false;
  const a = Buffer.from(m[1]), b = Buffer.from(BODY_TOKEN);
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

function bodies(req, res) {
  const m = /^\/bodies\/([0-9a-f]{64})$/.exec(new URL(req.url, "http://x").pathname);
  const text = (code, msg) => res.writeHead(code, { "Content-Type": "text/plain; charset=utf-8" }).end(msg + "\n");
  if (!BODY_DIR) return text(404, "body store disabled");
  if (!m) return text(400, "expected /bodies/<64 lowercase hex sha256>");
  const file = path.join(BODY_DIR, m[1]);
  if (req.method === "GET" || req.method === "HEAD") {
    return fs.readFile(file, (err, buf) => {
      if (err) return text(404, "unknown body");
      res.writeHead(200, { "Content-Type": "text/markdown; charset=utf-8",
        "Cache-Control": "public, max-age=31536000, immutable" }); // content-addressed: never changes
      res.end(req.method === "HEAD" ? undefined : buf);
    });
  }
  if (req.method !== "PUT") return res.writeHead(405, { Allow: "GET, HEAD, PUT" }).end();
  if (!tokenOk(req)) return text(401, "unauthorized");
  const chunks = []; let size = 0, aborted = false;
  req.on("data", (c) => {
    size += c.length;
    if (size > MAX_BODY && !aborted) { aborted = true; text(413, "body too large (max 64 KiB)"); req.destroy(); }
    else if (!aborted) chunks.push(c);
  });
  req.on("end", () => {
    if (aborted) return;
    const buf = Buffer.concat(chunks);
    if (crypto.createHash("sha256").update(buf).digest("hex") !== m[1]) return text(422, "sha256 mismatch");
    if (fs.existsSync(file)) return text(200, "exists");
    const tmp = file + ".tmp" + process.pid;
    fs.writeFile(tmp, buf, { flag: "wx" }, (err) => {
      if (err) return text(500, "write failed");
      fs.rename(tmp, file, (e2) => e2 ? text(500, "write failed") : text(201, "stored"));
    });
  });
}

// GET /search?q=…&domain=…&layer=…   跨标题与正文的检索
async function search(req, res) {
  const u = new URL(req.url, "http://x");
  const q = u.searchParams.get("q") || "";
  const domain = u.searchParams.get("domain");
  const layer = u.searchParams.get("layer") || undefined;
  const limit = u.searchParams.get("limit");
  try {
    // 索引最多 15 秒刷新一次;新单元通常在一个刷新周期内即可被搜到
    await INDEX.ensureFresh(15000);
    const hits = INDEX.search(q, { domain, layer, limit });
    const body = JSON.stringify({
      query: q, total: hits.length,
      // 命中的单元 id 与摘要;正文本身用 /bodies/<sha256> 取
      items: hits.map((h) => ({
        id: h.id, title: h.title, layer: h.layer, domain: h.domain, author: h.author,
        versions: h.versions, excerpt: h.excerpt, body_url: h.body_url,
        credibility: h.credibility, score: h.score,
      })),
    });
    res.writeHead(200, { "Content-Type": "application/json; charset=utf-8",
      "Cache-Control": "no-store" }).end(body);
  } catch (e) {
    res.writeHead(500, { "Content-Type": "application/json" })
      .end(JSON.stringify({ error: "search failed: " + e.message }));
  }
}

// GET /assign?unit=N[&version=M]   该版本的核验人指派(确定性,可验证)
// GET /assign/pending?member=N      指派给该成员、尚未核验的版本
async function assign(req, res) {
  const u = new URL(req.url, "http://x");
  const json = (obj, code = 200) => {
    res.writeHead(code, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" })
      .end(JSON.stringify(obj));
  };
  try {
    const member = u.searchParams.get("member");
    if (member) {
      const list = await ASSIGNER.pendingFor(member);
      return json({ member, count: list.length, items: list, enforced: false });
    }
    const unit = u.searchParams.get("unit");
    if (!unit) return json({ error: "missing unit" }, 400);
    const version = u.searchParams.get("version") || undefined;
    const a = await ASSIGNER.forVersion(unit, version);
    return json(a);
  } catch (e) {
    return json({ error: "assign failed: " + e.message }, 500);
  }
}

// GET /explore?unit=N      该单元的上游(谁建立在它之上)与地基(它站在谁之上)
// GET /gaps[?domain=N]     缺口:无人依据、也无人核验的单元(可以填的"山谷")
// GET /coverage            按领域的覆盖情况
async function explore(req, res) {
  const u = new URL(req.url, "http://x");
  const json = (obj, code = 200) => res.writeHead(code, {
    "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store",
  }).end(JSON.stringify(obj));
  try {
    await INDEX.ensureFresh(15000);
    if (req.url.startsWith("/gaps")) {
      const domain = u.searchParams.get("domain");
      return json({ gaps: INDEX.gaps(domain ? { domain } : {}) });
    }
    if (req.url.startsWith("/coverage")) {
      return json({ coverage: INDEX.coverage() });
    }
    const unit = u.searchParams.get("unit");
    if (!unit) return json({ error: "missing unit" }, 400);
    return json({
      unit: String(unit),
      upstream: INDEX.upstream(unit),
      downstream: INDEX.downstream(unit),
    });
  } catch (e) {
    return json({ error: "explore failed: " + e.message }, 500);
  }
}

// GET /map(旧名 /terrain)  认知地图:引用关系、可信度与标题,供浏览器画图
async function terrain(req, res) {
  try {
    const t = await TERRAIN.build();
    // 标题在正文仓库里(链上只有哈希):有就带上,图上才能直接看到每个单元讲什么
    try {
      await INDEX.ensureFresh();
      for (const n of t.nodes) {
        const b = INDEX.bodies.get(n.body_hash);
        n.title = b ? b.title : "";
      }
    } catch (_) { /* 正文仓库不可用时只是没有标题 */ }
    res.writeHead(200, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store" })
      .end(JSON.stringify(t));
  } catch (e) {
    res.writeHead(500, { "Content-Type": "application/json" })
      .end(JSON.stringify({ error: "terrain failed: " + e.message }));
  }
}

// GET /export/units   全部单元的 JSONL 导出(CC BY 4.0)
async function exportUnits(req, res) {
  try {
    const units = await INDEX.exportUnits();
    res.writeHead(200, {
      "Content-Type": "application/x-ndjson; charset=utf-8",
      "Content-Disposition": 'attachment; filename="zhixing-units.jsonl"',
      "Cache-Control": "no-store",
    });
    // 先写一行元数据(许可与来源),再逐行写单元
    res.write(JSON.stringify({
      kind: "metadata", license: LICENSE,
      source: "知行图测试网 blockchain.yeying.pub",
      note: "内容以 CC BY 4.0 许可开放:任何人可使用(含商用),只需署名。链上只存正文哈希,正文在此文件中。",
      units: units.length,
    }) + "\n");
    for (const u of units) res.write(JSON.stringify(u) + "\n");
    res.end();
  } catch (e) {
    res.writeHead(500, { "Content-Type": "application/json" })
      .end(JSON.stringify({ error: "export failed: " + e.message }));
  }
}

function status(req, res) {
  const m = /^\/status\/([a-z]+\.json)$/.exec(new URL(req.url, "http://x").pathname);
  if (!STATUS_DIR || !m || req.method !== "GET") return res.writeHead(404).end();
  fs.readFile(path.join(STATUS_DIR, m[1]), (err, buf) => {
    if (err) return res.writeHead(404).end();
    res.writeHead(200, { "Content-Type": "application/json", "Cache-Control": "no-store" }).end(buf);
  });
}

// ---- 知识互操作词表 v1(JSON-LD,规范见 docs/design/ontology-v0.1.md)-------------------------
// GET /export/units?format=jsonld    全部单元的 JSON-LD(一个 @graph,含全部版本、来源、引用、核验与正文)
// GET /unit/{id}.jsonld              单个单元的 JSON-LD(不含正文全文)
// GET /id/unit/{id}[/v/{n}]          全局标识:要 JSON-LD 就返回 JSON-LD,浏览器访问则跳到单元页
// GET /id/member/{id}                成员(账户号、公钥)
// GET /id/domain[/{n}]               领域概念方案
// GET /lookup?body=<sha256>          正文属于哪个单元的哪个版本
// GET /lookup?pubkey=<hex>           公钥对应的成员
const { CONTEXT_URL } = require("./ontology.js");
const NS_DIR = path.join(PUBLIC, "ns", "v1");

function sendJsonLd(res, obj, code = 200) {
  res.writeHead(code, { "Content-Type": "application/ld+json; charset=utf-8", "Cache-Control": "no-store",
    "Access-Control-Allow-Origin": "*" }).end(JSON.stringify(obj));
}
function sendJson(res, obj, code = 200) {
  res.writeHead(code, { "Content-Type": "application/json; charset=utf-8", "Cache-Control": "no-store",
    "Access-Control-Allow-Origin": "*" }).end(JSON.stringify(obj));
}
function wantsJsonLd(req) {
  const a = String(req.headers.accept || "");
  return /application\/(ld\+json|json)/.test(a) && !/text\/html/.test(a);
}

async function exportJsonLd(req, res) {
  try {
    const graph = (await INDEX.exportJsonLd()).map((d) => { const { ["@context"]: _c, ...rest } = d; return rest; });
    res.writeHead(200, {
      "Content-Type": "application/ld+json; charset=utf-8",
      "Content-Disposition": 'attachment; filename="zhixing-units.jsonld"',
      "Cache-Control": "no-store",
    });
    // 数据集说明放进 @graph 里的一个节点:顶层若带 id,@graph 会变成"命名图",
    // 只读默认图的工具(如 rdflib 的 Graph)就看不到任何单元
    const dataset = {
      id: "https://blockchain.yeying.pub/id/export",
      type: "schema:Dataset",
      name: "知行图测试网知识单元",
      license: "https://creativecommons.org/licenses/by/4.0/",
    };
    res.end(JSON.stringify({ "@context": CONTEXT_URL, "@graph": [dataset, ...graph] }));
  } catch (e) {
    sendJson(res, { error: "export failed: " + e.message }, 500);
  }
}

async function identity(req, res, apiPath) {
  try {
    let m;
    if ((m = /^\/unit\/(\d+)\.jsonld$/.exec(apiPath))) {
      await INDEX.ensureFresh(15000);
      const doc = INDEX.unitJsonLd(m[1]);
      return doc ? sendJsonLd(res, doc) : sendJson(res, { error: "no such unit" }, 404);
    }
    if ((m = /^\/id\/unit\/(\d+)(?:\/v\/(\d+))?$/.exec(apiPath))) {
      if (!wantsJsonLd(req)) {
        return res.writeHead(303, { Location: "/#unit/" + m[1] }).end();
      }
      await INDEX.ensureFresh(15000);
      const doc = INDEX.unitJsonLd(m[1]);
      if (!doc) return sendJson(res, { error: "no such unit" }, 404);
      if (m[2]) {
        const v = doc.versions.find((x) => x.versionNumber === Number(m[2]));
        return v ? sendJsonLd(res, { "@context": CONTEXT_URL, ...v }) : sendJson(res, { error: "no such version" }, 404);
      }
      return sendJsonLd(res, doc);
    }
    if ((m = /^\/id\/member\/(\d+)$/.exec(apiPath))) {
      await INDEX.ensureFresh(15000);
      const doc = INDEX.memberJsonLd(m[1]);
      return doc ? sendJsonLd(res, doc) : sendJson(res, { error: "no such member" }, 404);
    }
    if (/^\/id\/domain(?:\/\d+)?$/.test(apiPath)) {
      const scheme = JSON.parse(await fs.promises.readFile(path.join(NS_DIR, "domains.jsonld"), "utf8"));
      const n = /^\/id\/domain\/(\d+)$/.exec(apiPath);
      if (!n) return sendJsonLd(res, scheme);
      const c = (scheme.concepts || []).find((x) => x.notation === n[1]);
      return c ? sendJsonLd(res, { "@context": CONTEXT_URL, ...c }) : sendJson(res, { error: "no such domain" }, 404);
    }
    return sendJson(res, { error: "not found" }, 404);
  } catch (e) {
    return sendJson(res, { error: "identity failed: " + e.message }, 500);
  }
}

async function lookup(req, res) {
  const u = new URL(req.url, "http://x");
  try {
    const body = u.searchParams.get("body");
    const pubkey = u.searchParams.get("pubkey");
    if (body) {
      if (!/^[0-9a-fA-F]{64}$/.test(body)) return sendJson(res, { error: "body must be a sha256 hex" }, 400);
      await INDEX.ensureFresh(5000);
      const hit = INDEX.lookupBody(body);
      return hit ? sendJson(res, { found: true, ...hit }) : sendJson(res, { found: false });
    }
    if (pubkey) {
      if (!/^[0-9a-fA-F]{64}$/.test(pubkey)) return sendJson(res, { error: "pubkey must be 64 hex" }, 400);
      await INDEX.ensureFresh(15000);
      await INDEX.refreshMembers(0).catch(() => {});
      const m = INDEX.lookupPubkey(pubkey);
      return m ? sendJson(res, { found: true, account: m.id, established: m.established })
        : sendJson(res, { found: false });
    }
    return sendJson(res, { error: "need body= or pubkey=" }, 400);
  } catch (e) {
    return sendJson(res, { error: "lookup failed: " + e.message }, 500);
  }
}

function apiPathOf(req) { return new URL(req.url, "http://x").pathname; }

http.createServer((req, res) => {
  if (req.url.startsWith("/bodies/")) return bodies(req, res);
  if (req.url.startsWith("/status/")) return status(req, res);
  if (req.url === "/search" || req.url.startsWith("/search?")) return search(req, res);
  if (apiPathOf(req) === "/assign" || apiPathOf(req) === "/assign/pending") return assign(req, res);
  if (req.url === "/terrain" || req.url.startsWith("/terrain?") ||
      req.url === "/map" || req.url.startsWith("/map?")) return terrain(req, res);
  // 精确匹配 API 路径:不能用 startsWith("/explore"),否则会吞掉静态文件 /explore.js
  const apiPath = new URL(req.url, "http://x").pathname;
  if (apiPath === "/explore" || apiPath === "/gaps" || apiPath === "/coverage") {
    return explore(req, res);
  }
  if (apiPath === "/export/units") {
    return new URL(req.url, "http://x").searchParams.get("format") === "jsonld" ? exportJsonLd(req, res) : exportUnits(req, res);
  }
  if (apiPath === "/lookup") return lookup(req, res);
  if (apiPath.startsWith("/id/") || apiPath === "/id/domain" || /^\/unit\/\d+\.jsonld$/.test(apiPath)) {
    return identity(req, res, apiPath);
  }
  if (req.url === "/rpc" || req.url.startsWith("/rpc/") || req.url.startsWith("/rpc?")) return proxy(req, res);
  return serveStatic(req, res);
}).listen(PORT, HOST, () => {
  console.log(`zhixing explorer → http://${HOST}:${PORT}  (proxying /rpc → ${NODE_RPC})`);
});
