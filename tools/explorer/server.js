#!/usr/bin/env node
"use strict";
// zhixing block explorer — zero-dependency static server + RPC proxy.
//
//   node server.js              # serve ./public on :8080, proxy /rpc/* → http://127.0.0.1:9221
//   PORT=9000 NODE_RPC=http://host:9221 node server.js
//
// The proxy keeps the browser same-origin (no CORS / mixed-content concerns) and lets you
// point the explorer at any node without editing files. Uses only Node core (http, fs, path).
const http = require("http");
const fs = require("fs");
const path = require("path");
const { URL } = require("url");

const PORT = Number(process.env.PORT || 8080);
const NODE_RPC = (process.env.NODE_RPC || "http://127.0.0.1:9221").replace(/\/+$/, "");
const PUBLIC = path.join(__dirname, "public");

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

http.createServer((req, res) => {
  if (req.url === "/rpc" || req.url.startsWith("/rpc/") || req.url.startsWith("/rpc?")) return proxy(req, res);
  return serveStatic(req, res);
}).listen(PORT, () => {
  console.log(`zhixing explorer → http://localhost:${PORT}  (proxying /rpc → ${NODE_RPC})`);
});
