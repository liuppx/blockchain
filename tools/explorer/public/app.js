"use strict";
// zhixing block explorer (L1) — dependency-free. Reads a node's JSON RPC. The RPC base
// defaults to "/rpc" (this project's server proxies it to the node, avoiding CORS/mixed
// content); you can also point it straight at a node (e.g. http://127.0.0.1:9221) when the
// node's CORS is on (M223). Field names track `daemon.rs`'s `json_*` renderers; list
// endpoints use the {total,cursor,items:[…]} page envelope; u64s are lossless quoted strings.
let RPC = "/rpc";
let timer = null;

async function j(path) {
  const r = await fetch(RPC + path, { headers: { "Accept": "application/json" } });
  if (!r.ok) throw new Error(path + " → HTTP " + r.status);
  return r.json();
}
const el = (id) => document.getElementById(id);
const esc = (s) => String(s).replace(/[&<>"]/g, c => ({ "&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;" }[c]));
const short = (h) => h && h.length > 16 ? h.slice(0,10) + "…" + h.slice(-6) : (h||"");
function setView(html) { el("view").innerHTML = "<div class='body'>" + html + "</div>"; }
function card(k, v) { return `<div class='card'><div class='k'>${k}</div><div class='v'>${v}</div></div>`; }

async function connect() {
  RPC = el("rpc").value.replace(/\/+$/, "") || "/rpc";
  try {
    await refreshOverview();
    el("conn").textContent = "connected"; el("conn").className = "mut";
    renderTabs();
    const deep = /^#unit\/(\d+)$/.exec(location.hash);   // shareable link: …/#unit/3
    if (deep) showUnit(deep[1]); else showTab("knowledge");
    if (timer) clearInterval(timer);
    timer = setInterval(refreshOverview, 4000);  // poll chain tip
  } catch (e) {
    el("conn").textContent = "error: " + e.message; el("conn").className = "err";
  }
}

async function refreshOverview() {
  const [h, sup, ver, info] = await Promise.all([ j("/height"), j("/supply"), j("/version"), j("/info") ]);
  el("overview").innerHTML =
    card("height", esc(h.height)) +
    card("head", `<span class='hash'>${esc(short(info.head))}</span>`) +
    card("protocol version", esc(ver.version)) +
    card("supply (µ$COG)", esc(sup.supply)) +
    card("bonded", esc(sup.bonded)) +
    card("supply conserved", sup.conserved ? "<span style='color:var(--ok)'>yes</span>" : "<span class='err'>NO</span>") +
    card("genesis", `<span class='hash'>${esc(short(info.genesis_hash))}</span>`);
}

const TABS = [
  ["knowledge", "Knowledge"], ["blocks", "Blocks"], ["tx", "Tx lookup"], ["account", "Account"], ["graph", "Graph"],
  ["cnfts", "cNFTs"], ["validators", "Validators"], ["staking", "Staking"],
  ["proposals", "Governance"], ["supply", "Supply"],
];
function renderTabs() {
  el("tabs").innerHTML = TABS.map(([k,l]) => `<button id='tab-${k}' data-tab='${k}'>${l}</button>`).join("");
  el("tabs").querySelectorAll("button").forEach(b => b.addEventListener("click", () => showTab(b.dataset.tab)));
}
function showTab(k) {
  TABS.forEach(([t]) => { const b = el("tab-"+t); if (b) b.className = (t===k ? "on" : ""); });
  ({ knowledge:tabKnowledge, blocks:tabBlocks, tx:tabTx, account:tabAccount, graph:tabGraph, validators:tabValidators,
     staking:tabStaking, proposals:tabProposals, cnfts:tabCnfts, supply:tabSupply })[k]();
}

async function tabBlocks() {
  setView("<span class='mut'>loading…</span>");
  const tip = Number((await j("/height")).height);
  const from = Math.max(1, tip - 14);
  const rows = [];
  for (let hgt = tip; hgt >= from; hgt--) {
    try {
      const b = await j("/block/" + hgt);
      rows.push(`<tr><td><a class='link' data-blk='${hgt}'>#${esc(b.height)}</a></td>
        <td class='hash'>${esc(short(b.hash))}</td><td>${esc(b.proposer)}</td><td>${esc(b.txs)}</td>
        <td class='mut'>${esc(b.timestamp_days)}</td></tr>`);
    } catch (e) { /* pruned/absent: skip */ }
  }
  setView(`<h2 style='border:0;padding:0 0 8px'>Latest blocks (tip #${tip})</h2>
    <table><tr><th>height</th><th>hash</th><th>proposer</th><th>txs</th><th>ts(days)</th></tr>${rows.join("")}</table>`);
  el("view").querySelectorAll("a[data-blk]").forEach(a => a.addEventListener("click", () => showBlock(Number(a.dataset.blk))));
}

async function showBlock(hgt) {
  setView("<span class='mut'>loading…</span>");
  try {
    const b = await j("/block/" + hgt);
    let txs = [];
    try { txs = (await j(`/block/${hgt}/txs`)).items || []; } catch (e) {}
    const txrows = txs.length ? txs.map(t =>
      `<tr><td><a class='link' data-tx='${esc(t.hash)}'>${esc(short(t.hash))}</a></td><td>${esc(t.author)}</td></tr>`
    ).join("") : "<tr><td class='mut' colspan=2>(empty block)</td></tr>";
    setView(`<p><a class='link' id='back'>← blocks</a></p>
      <h2 style='border:0;padding:0 0 8px'>Block #${esc(b.height)}</h2>
      <kv><b>hash</b><span class='mono'>${esc(b.hash)}</span></kv>
      <kv><b>prev_hash</b><span class='mono hash'>${esc(b.prev_hash)}</span></kv>
      <kv><b>proposer</b>${esc(b.proposer)}</kv>
      <kv><b>timestamp_days</b>${esc(b.timestamp_days)}</kv>
      <kv><b>state_root</b><span class='mono hash'>${esc(b.state_root)}</span></kv>
      <kv><b>accounts_root</b><span class='mono hash'>${esc(b.accounts_root)}</span></kv>
      <kv><b>txs / fees</b>${esc(b.txs)} / ${esc(b.fees)}</kv>
      <kv><b>stake_ops / evidence / val_updates</b>${esc(b.stake_ops)} / ${esc(b.evidence)} / ${esc(b.validator_updates)}</kv>
      <h2 style='border:0;padding:12px 0 8px'>Transactions</h2>
      <table><tr><th>hash</th><th>author</th></tr>${txrows}</table>`);
    el("back").addEventListener("click", () => showTab("blocks"));
    el("view").querySelectorAll("a[data-tx]").forEach(a => a.addEventListener("click", () => showTxHash(a.dataset.tx)));
  } catch (e) { setView("<span class='err'>" + esc(e.message) + "</span>"); }
}

function tabTx() {
  setView(`<kv><b>tx hash (64-hex)</b><input id='txq' size='70' placeholder='content hash'></kv>
    <p><button id='txgo'>Look up</button></p>
    <div id='txout' class='mut'>Enter a committed tx's content hash.</div>`);
  el("txgo").addEventListener("click", () => showTxHash(el("txq").value.trim()));
}
async function showTxHash(h) {
  try {
    const d = await j(`/tx/${h}/detail`);
    const html = `<kv><b>hash</b><span class='mono'>${esc(d.hash)}</span></kv>
      <kv><b>height</b>#${esc(d.height)}</kv>
      <kv><b>author</b>${esc(d.author)}</kv>
      <kv><b>domain</b>${esc(d.domain)}</kv>
      <kv><b>stake / fee</b>${esc(d.stake)} / ${esc(d.fee)}</kv>
      <kv><b>nonce</b>${esc(d.nonce)}</kv>
      <kv><b>reviews</b>${esc(d.reviews)}</kv>
      <kv><b>replication</b>${esc(d.repl_success)} / ${esc(d.repl_total)}</kv>
      <kv><b>timestamp_days</b>${esc(d.timestamp_days)}</kv>
      <kv><b>embedding_dims</b>${esc(d.embedding_dims)}</kv>`;
    if (el("txout")) el("txout").innerHTML = html; else setView(html);
  } catch (e) {
    const msg = `<span class='err'>not found / not committed (${esc(e.message)})</span>`;
    if (el("txout")) el("txout").innerHTML = msg; else setView(msg);
  }
}

function tabAccount() {
  setView(`<kv><b>account id (u64)</b><input id='acq' size='20' placeholder='e.g. 1'></kv>
    <p><button id='acgo'>Look up</button></p>
    <div id='acout' class='mut'>Enter an account id. Or see the accounts list below.</div>
    <h2 style='border:0;padding:12px 0 8px'>Accounts</h2><div id='aclist' class='mut'>loading…</div>`);
  el("acgo").addEventListener("click", () => showAccount(el("acq").value.trim()));
  j("/accounts").then(p => {
    const items = p.items || [];
    el("aclist").innerHTML = `<table><tr><th>id</th><th>balance</th><th>accepted</th><th>earned</th></tr>` +
      items.map(a => `<tr><td><a class='link' data-acc='${esc(a.id)}'>${esc(a.id)}</a></td>
        <td>${esc(a.balance)}</td><td>${esc(a.accepted)}</td><td>${esc(a.earned_total)}</td></tr>`).join("") + `</table>`;
    el("aclist").querySelectorAll("a[data-acc]").forEach(a => a.addEventListener("click", () => showAccount(a.dataset.acc)));
  }).catch(e => el("aclist").innerHTML = `<span class='err'>${esc(e.message)}</span>`);
}
async function showAccount(id) {
  try {
    const a = await j("/account/" + id);
    el("acout").innerHTML = `<kv><b>id</b>${esc(a.id)}</kv>
      <kv><b>balance</b>${esc(a.balance)}</kv>
      <kv><b>staked_total</b>${esc(a.staked_total)}</kv>
      <kv><b>earned_total</b>${esc(a.earned_total)}</kv>
      <kv><b>slashed_total</b>${esc(a.slashed_total)}</kv>
      <kv><b>submissions / accepted</b>${esc(a.submissions)} / ${esc(a.accepted)}</kv>
      <kv><b>pubkey</b><span class='mono hash'>${esc(a.pubkey)}</span></kv>`;
  } catch (e) { el("acout").innerHTML = `<span class='err'>${esc(e.message)}</span>`; }
}

async function listTab(path, title, cols, row) {
  setView("<span class='mut'>loading…</span>");
  try {
    const items = (await j(path)).items || [];
    const head = cols.map(c => `<th>${c}</th>`).join("");
    const body = items.length ? items.map(row).join("") : `<tr><td class='mut' colspan=${cols.length}>(none)</td></tr>`;
    setView(`<h2 style='border:0;padding:0 0 8px'>${title}</h2><table><tr>${head}</tr>${body}</table>`);
  } catch (e) { setView(`<span class='err'>${esc(e.message)}</span>`); }
}
const tabGraph = () => listTab("/graph", "Cognitive graph nodes (PoK)", ["node_id","domain","dim","embedding"],
  g => `<tr><td>${esc(g.node_id)}</td><td>${esc(g.domain)}</td><td>${esc(g.dim)}</td><td class='mono'>[${(g.embedding||[]).map(x => esc(x)).join(", ")}]</td></tr>`);
const tabValidators = () => listTab("/validators", "Validators", ["id","power","pubkey"],
  v => `<tr><td>${esc(v.validator_id)}</td><td>${esc(v.power)}</td><td class='mono hash'>${esc(short(v.pubkey))}</td></tr>`);
const tabProposals = () => listTab("/proposals", "Governance proposals", ["id","param","value","created","period","yes/votes"],
  p => `<tr><td>${esc(p.id)}</td><td>${esc(p.param)}</td><td>${esc(p.value)}</td><td>${esc(p.created_height)}</td><td>${esc(p.voting_period)}</td><td>${esc(p.yes)}/${esc(p.votes)}</td></tr>`);
const tabCnfts = () => listTab("/cnfts", "cNFT contribution certificates", ["node_id","contributor","domain","reward(µ)","height"],
  c => `<tr><td>${esc(c.node_id)}</td><td>${esc(c.contributor)}</td><td>${esc(c.domain)}</td><td>${esc(c.reward_micro)}</td><td>${esc(c.height)}</td></tr>`);

// Staking lifecycle: live bonds (voting-power stake) + the unbonding queue (bonds in
// their cooldown, released at mature_height). Fields track daemon.rs json_bonds/json_unbonding.
async function tabStaking() {
  setView("<span class='mut'>loading…</span>");
  try {
    const [bp, up, info] = await Promise.all([ j("/bonds"), j("/unbonding"), j("/info") ]);
    const bonds = bp.items || [], unb = up.items || [], tip = Number(info.height);
    const brows = bonds.length ? bonds.map(b =>
      `<tr><td>${esc(b.validator_id)}</td><td>${esc(b.bonded)}</td></tr>`).join("")
      : "<tr><td class='mut' colspan=2>(no active bonds — ./cmd bond)</td></tr>";
    const urows = unb.length ? unb.map(u => {
      const left = Number(u.mature_height) - tip;
      return `<tr><td>${esc(u.account)}</td><td>${esc(u.amount)}</td><td>${esc(u.mature_height)}</td>
        <td class='mut'>${left > 0 ? "in " + left + " blocks" : "matured"}</td></tr>`;
    }).join("") : "<tr><td class='mut' colspan=4>(unbonding queue empty — ./cmd unbond)</td></tr>";
    setView(`<h2 style='border:0;padding:0 0 8px'>Active bonds (total ${esc(bp.total_bonded)} µ$COG)</h2>
      <table><tr><th>validator_id</th><th>bonded(µ)</th></tr>${brows}</table>
      <h2 style='border:0;padding:16px 0 8px'>Unbonding queue (tip #${tip})</h2>
      <table><tr><th>account</th><th>amount(µ)</th><th>mature_height</th><th>status</th></tr>${urows}</table>`);
  } catch (e) { setView(`<span class='err'>${esc(e.message)}</span>`); }
}

async function tabSupply() {
  setView("<span class='mut'>loading…</span>");
  try {
    const s = await j("/supply");
    setView(`<h2 style='border:0;padding:0 0 8px'>Money supply (µ$COG)</h2>
      <kv><b>supply</b>${esc(s.supply)}</kv>
      <kv><b>accounts_balance</b>${esc(s.accounts_balance)}</kv>
      <kv><b>treasury</b>${esc(s.treasury)}</kv>
      <kv><b>bonded</b>${esc(s.bonded)}</kv>
      <kv><b>unbonding</b>${esc(s.unbonding)}</kv>
      <kv><b>bridge_locked</b>${esc(s.bridge_locked)}</kv>
      <kv><b>bridge_minted</b>${esc(s.bridge_minted)}</kv>
      <kv><b>conserved</b>${s.conserved ? "<span style='color:var(--ok)'>yes</span>" : "<span class='err'>NO</span>"}</kv>`);
  } catch (e) { setView(`<span class='err'>${esc(e.message)}</span>`); }
}

// K7: knowledge units (testnet stage 1). Credibility is in basis points (10000 = 1.0);
// effective credibility = min(own, every "basis" citation's effective credibility), so the
// bottleneck column names the weakest foundation a unit stands on.
const LAYER_ZH = { foundation: "基础", explanation: "解释", scenario: "场景", application: "应用" };
const DOMAIN_ZH = { 1: "大模型应用开发", 2: "运动健身" }; // testnet genesis [knowledge] domains
const domainName = d => DOMAIN_ZH[d] ? `${DOMAIN_ZH[d]} (${d})` : String(d);
function credBar(bps) {
  const v = Number(bps) / 10000;
  const pct = Math.round(v * 100);
  const color = v >= 0.6 ? "var(--ok)" : v >= 0.3 ? "#d9a400" : "#c0392b";
  return `<span style='display:inline-block;width:80px;height:8px;background:#ddd;vertical-align:middle'>` +
    `<span style='display:block;width:${pct}%;height:8px;background:${color}'></span></span> ${v.toFixed(1)}`;
}

// ---- knowledge bodies: fetched from the content-addressed store, re-hashed in the browser ----
const bodyCache = new Map();
async function sha256hex(buf) {
  const d = await crypto.subtle.digest("SHA-256", buf);
  return Array.from(new Uint8Array(d)).map(b => b.toString(16).padStart(2, "0")).join("");
}
// → {text, ok} | null. ok=false ⇒ bytes don't match the on-chain hash (never shown as content).
async function fetchBody(hash) {
  if (bodyCache.has(hash)) return bodyCache.get(hash);
  const p = (async () => {
    const r = await fetch("/bodies/" + hash);
    if (!r.ok) return null;
    const buf = await r.arrayBuffer();
    return { ok: (await sha256hex(buf)) === hash, text: new TextDecoder().decode(buf) };
  })().catch(() => null);
  bodyCache.set(hash, p);
  return p;
}
function bodyTitle(text) { const m = /^#\s+(.+)$/m.exec(text); return m ? m[1].trim() : ""; }
// Minimal, escape-first markdown (headings, tables, lists, quotes, code, **bold**, `code`, links).
function mdInline(s) {
  return esc(s)
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\*\*([^*]+)\*\*/g, "<b>$1</b>")
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, "<a href='$2' rel='noopener noreferrer' target='_blank'>$1</a>");
}
function renderMd(text) {
  const lines = text.replace(/\r\n/g, "\n").split("\n"), out = [];
  let i = 0;
  const cells = l => l.trim().replace(/^\||\|$/g, "").split("|").map(c => c.trim());
  while (i < lines.length) {
    const l = lines[i];
    if (/^```/.test(l)) {
      const buf = []; i++;
      while (i < lines.length && !/^```/.test(lines[i])) buf.push(lines[i++]);
      i++; out.push(`<pre>${esc(buf.join("\n"))}</pre>`); continue;
    }
    const h = /^(#{1,6})\s+(.*)$/.exec(l);
    if (h) { const n = Math.min(h[1].length + 2, 6); out.push(`<h${n} class='md'>${mdInline(h[2])}</h${n}>`); i++; continue; }
    if (/^\s*\|/.test(l)) {
      const rows = [];
      while (i < lines.length && /^\s*\|/.test(lines[i])) rows.push(lines[i++]);
      const body = rows.filter((r, k) => !(k === 1 && /^[\s|:-]+$/.test(r)));
      const [head, ...rest] = body;
      out.push(`<table class='md'><tr>${cells(head).map(c => `<th>${mdInline(c)}</th>`).join("")}</tr>` +
        rest.map(r => `<tr>${cells(r).map(c => `<td>${mdInline(c)}</td>`).join("")}</tr>`).join("") + "</table>");
      continue;
    }
    if (/^\s*([-*]|\d+\.)\s+/.test(l)) {
      const ordered = /^\s*\d+\./.test(l), items = [];
      while (i < lines.length && /^\s*([-*]|\d+\.)\s+/.test(lines[i])) items.push(lines[i++].replace(/^\s*([-*]|\d+\.)\s+/, ""));
      const t = ordered ? "ol" : "ul";
      out.push(`<${t} class='md'>${items.map(x => `<li>${mdInline(x)}</li>`).join("")}</${t}>`); continue;
    }
    if (/^>\s?/.test(l)) {
      const buf = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) buf.push(lines[i++].replace(/^>\s?/, ""));
      out.push(`<blockquote class='md'>${mdInline(buf.join(" "))}</blockquote>`); continue;
    }
    if (!l.trim()) { i++; continue; }
    const buf = [];
    while (i < lines.length && lines[i].trim() && !/^(#{1,6}\s|```|\s*\||\s*([-*]|\d+\.)\s|>)/.test(lines[i])) buf.push(lines[i++]);
    out.push(`<p class='md'>${buf.map(mdInline).join("<br>")}</p>`);
  }
  return out.join("");
}
async function fillBody(slot, hash) {
  const b = await fetchBody(hash);
  if (!b) { slot.innerHTML = "<span class='mut'>正文未上传到正文仓库(链上只有哈希)</span>"; return; }
  if (!b.ok) { slot.innerHTML = "<span class='err'>正文与链上哈希不一致,已拒绝显示</span>"; return; }
  slot.innerHTML = "<div class='ok'>✓ 正文与链上哈希一致</div><div class='mdbody'>" + renderMd(b.text) + "</div>";
}

async function tabKnowledge() {
  setView("<span class='mut'>loading…</span>");
  try {
    const items = (await j("/units?limit=500")).items || [];
    const rows = items.length ? items.map(u => {
      const c = u.credibility, b = c.bottleneck;
      const neck = b ? `<a class='link' data-unit='${esc(b.unit)}'>#${esc(b.unit)} v${esc(b.version)}</a>` : "<span class='mut'>—</span>";
      return `<tr><td><a class='link' data-unit='${esc(u.id)}'>#${esc(u.id)}</a></td>
        <td><a class='link' data-unit='${esc(u.id)}' data-title='${esc(u.id)}'><span class='mut'>…</span></a></td>
        <td>${esc(LAYER_ZH[u.layer] || u.layer)}</td><td>${esc(domainName(u.domain))}</td><td>${esc(u.versions)}</td>
        <td>${esc(c.passes)}</td><td>${credBar(c.self_bps)}</td><td>${credBar(c.effective_bps)}</td><td>${neck}</td></tr>`;
    }).join("") : "<tr><td class='mut' colspan=9>(no knowledge units yet — ./cmd knowledge)</td></tr>";
    setView(`<h2 style='border:0;padding:0 0 8px'>Knowledge units</h2>
      <p class='mut'>有效可信度 = 自身可信度与所依据单元有效可信度的最小值。"被谁卡住"指出它最薄弱的地基。</p>
      <table><tr><th>unit</th><th>标题</th><th>层级</th><th>领域</th><th>版本数</th><th>核验通过</th><th>自身</th><th>有效</th><th>被谁卡住</th></tr>${rows}</table>`);
    el("view").querySelectorAll("a[data-unit]").forEach(a => a.addEventListener("click", () => showUnit(a.dataset.unit)));
    // titles live in the bodies: latest version's first body, verified against its hash
    el("view").querySelectorAll("a[data-title]").forEach(async a => {
      try {
        const u = await j("/unit/" + a.dataset.title), v = u.versions[u.versions.length - 1];
        const b = v && v.bodies[0] ? await fetchBody(v.bodies[0].hash) : null;
        a.innerHTML = b && b.ok && bodyTitle(b.text) ? esc(bodyTitle(b.text)) : "<span class='mut'>(正文未上传)</span>";
      } catch (_) { a.innerHTML = "<span class='mut'>?</span>"; }
    });
  } catch (e) { setView(`<span class='err'>${esc(e.message)}</span>`); }
}
async function showUnit(id) {
  history.replaceState(null, "", "#unit/" + id);
  setView("<span class='mut'>loading…</span>");
  try {
    const u = await j("/unit/" + id);
    const vers = u.versions.slice().reverse().map(v => {
      const c = v.credibility;
      const cites = v.cites.length ? v.cites.map(x =>
        `<a class='link' data-unit='${esc(x.unit)}'>#${esc(x.unit)} v${esc(x.version)}</a> <span class='mut'>(${x.kind === "basis" ? "依据" : "参见"})</span>`).join("<br>") : "<span class='mut'>—</span>";
      const stale = c.stale_cites.map(s => `<div class='err'>引用已过时:#${esc(s.unit)} 引用 v${esc(s.cited)},当前 v${esc(s.current)}</div>`).join("");
      const ver = v.verifications.length ? v.verifications.map(r =>
        `account ${esc(r.verifier)} · ${r.verdict === "pass" ? "通过" : "需修改"} · height ${esc(r.height)}`).join("<br>") : "<span class='mut'>尚无核验</span>";
      const bodies = v.bodies.map(b => `${esc(b.lang)}: <span class='mono hash'>${esc(short(b.hash))}</span>`).join("<br>");
      return `<h2 style='border:0;padding:12px 0 6px'>v${esc(v.n)}</h2>
        <kv><b>publisher / height</b>account ${esc(v.publisher)} · ${esc(v.published_height)}</kv>
        <kv><b>来源</b>${esc(v.source)}</kv>
        <kv><b>正文哈希</b>${bodies}</kv>
        ${v.bodies.map(b => `<div class='bodyslot' data-hash='${esc(b.hash)}'><span class='mut'>正文加载中…</span></div>`).join("")}
        <kv><b>可信度</b>自身 ${credBar(c.self_bps)} · 有效 ${credBar(c.effective_bps)} · 核验通过 ${esc(c.passes)}</kv>
        <kv><b>引用</b>${cites}</kv>${stale}
        <kv><b>核验记录</b>${ver}</kv>`;
    }).join("");
    setView(`<p><a class='link' id='kback'>← knowledge</a></p>
      <h2 style='border:0;padding:0 0 8px'>Unit #${esc(u.id)} · ${esc(LAYER_ZH[u.layer] || u.layer)}</h2>
      <kv><b>author / created</b>account ${esc(u.author)} · height ${esc(u.created_height)}</kv>
      <kv><b>domain</b>${esc(domainName(u.domain))}</kv><kv><b>license</b>${esc(u.license)}</kv>${vers}`);
    el("kback").addEventListener("click", () => { history.replaceState(null, "", location.pathname); showTab("knowledge"); });
    // latest version expanded; older versions' bodies load too (they are usually the same text)
    el("view").querySelectorAll(".bodyslot").forEach(sl => fillBody(sl, sl.dataset.hash));
    el("view").querySelectorAll("a[data-unit]").forEach(a => a.addEventListener("click", () => showUnit(a.dataset.unit)));
  } catch (e) { setView(`<span class='err'>${esc(e.message)}</span>`); }
}

// Wire up the Connect button and auto-connect on load (default "/rpc" proxy).
document.getElementById("connectBtn").addEventListener("click", connect);
connect();
