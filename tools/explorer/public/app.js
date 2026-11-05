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
    renderTabs(); showTab("blocks");
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
  ["blocks", "Blocks"], ["tx", "Tx lookup"], ["account", "Account"],
  ["validators", "Validators"], ["proposals", "Governance"], ["cnfts", "cNFTs"], ["supply", "Supply"],
];
function renderTabs() {
  el("tabs").innerHTML = TABS.map(([k,l]) => `<button id='tab-${k}' data-tab='${k}'>${l}</button>`).join("");
  el("tabs").querySelectorAll("button").forEach(b => b.addEventListener("click", () => showTab(b.dataset.tab)));
}
function showTab(k) {
  TABS.forEach(([t]) => { const b = el("tab-"+t); if (b) b.className = (t===k ? "on" : ""); });
  ({ blocks:tabBlocks, tx:tabTx, account:tabAccount, validators:tabValidators,
     proposals:tabProposals, cnfts:tabCnfts, supply:tabSupply })[k]();
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
const tabValidators = () => listTab("/validators", "Validators", ["id","power","pubkey"],
  v => `<tr><td>${esc(v.validator_id)}</td><td>${esc(v.power)}</td><td class='mono hash'>${esc(short(v.pubkey))}</td></tr>`);
const tabProposals = () => listTab("/proposals", "Governance proposals", ["id","param","value","created","period","yes/votes"],
  p => `<tr><td>${esc(p.id)}</td><td>${esc(p.param)}</td><td>${esc(p.value)}</td><td>${esc(p.created_height)}</td><td>${esc(p.voting_period)}</td><td>${esc(p.yes)}/${esc(p.votes)}</td></tr>`);
const tabCnfts = () => listTab("/cnfts", "cNFT contribution certificates", ["node_id","contributor","domain","reward(µ)","height"],
  c => `<tr><td>${esc(c.node_id)}</td><td>${esc(c.contributor)}</td><td>${esc(c.domain)}</td><td>${esc(c.reward_micro)}</td><td>${esc(c.height)}</td></tr>`);

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

// Wire up the Connect button and auto-connect on load (default "/rpc" proxy).
document.getElementById("connectBtn").addEventListener("click", connect);
connect();
