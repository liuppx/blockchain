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
const BODY_TOKEN = process.env.BODY_TOKEN || "";
const MAX_BODY = 64 * 1024; // matches the node's RPC body cap

const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8", ".json": "application/json", ".ico": "image/x-icon" };

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
    method: req.method, headers: { "Accept": req.headers["accept"] || "application/json", "Host": target.host },
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

http.createServer((req, res) => {
  if (req.url.startsWith("/bodies/")) return bodies(req, res);
  if (req.url === "/rpc" || req.url.startsWith("/rpc/") || req.url.startsWith("/rpc?")) return proxy(req, res);
  return serveStatic(req, res);
}).listen(PORT, HOST, () => {
  console.log(`zhixing explorer → http://${HOST}:${PORT}  (proxying /rpc → ${NODE_RPC})`);
});
