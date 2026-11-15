// 钱包登录的端到端检验(用注入的假钱包,不需要真的装钱包插件)。
//
//   node tools/explorer/test/wallet.e2e.mjs
//
// 覆盖:浏览器里注入一个模拟夜莺钱包(EIP-6963)→ 在「我的身份」页用钱包登录 →
//       派生出的成员公钥与离线派生一致 → 能找到成员账户 → 核验能提交。
//
// 用的是固定的假私钥,不是任何真实账户。
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const CHROME = process.env.CHROME_PATH || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const WEB = process.env.TEST_WEB || "http://127.0.0.1:8080";
const ETHERS = process.env.ETHERS_PATH || join(here, "../../../../wallet/lib/ethers-6.16.esm.min.js");

let puppeteer = null;
try { puppeteer = require("puppeteer-core"); } catch (_) { puppeteer = null; }
const hasChrome = existsSync(CHROME);
let browser = null;

before(async () => {
  if (!puppeteer || !hasChrome) return;
  browser = await puppeteer.launch({ executablePath: CHROME, headless: "new" });
});
after(async () => { if (browser) await browser.close(); });

const skip = () => (!puppeteer || !hasChrome ? "no browser" : false);

async function injectFakeWallet(page, privKey) {
  const { Wallet } = await import(ETHERS);
  const w = new Wallet(privKey);
  await page.evaluateOnNewDocument((addr) => {
    const info = Object.freeze({
      uuid: "11111111-2222-4333-8444-555555555555",
      name: "YeYing Wallet",
      icon: "data:image/svg+xml;base64,",
      rdns: "io.github.yeying",
    });
    const listeners = {};
    const provider = {
      isYeYing: true,
      async request({ method, params }) {
        if (method === "eth_requestAccounts") return [addr];
        if (method === "eth_accounts") return [addr];
        if (method === "personal_sign") return window.__realPersonalSign(params[0]);
        throw new Error("fake wallet: 不支持 " + method);
      },
      on(ev, cb) { (listeners[ev] ||= []).push(cb); },
    };
    window.__FAKE_WALLET__ = { provider, listeners };
    // 与真钱包一样:注入时宣告一次,之后回应每次 requestProvider
    const announce = () => window.dispatchEvent(new CustomEvent("eip6963:announceProvider", { detail: { info, provider } }));
    window.addEventListener("eip6963:requestProvider", announce);
    announce();
  }, w.address);
  // personal_sign 用 ethers 真签名(页面里没有 ethers);参数是 UTF-8 的十六进制
  await page.exposeFunction("__realPersonalSign", async (hex) => {
    const hexs = hex.replace(/^0x/, "");
    const bytes = new Uint8Array(hexs.length / 2);
    for (let i = 0; i < bytes.length; i++) bytes[i] = parseInt(hexs.slice(i * 2, i * 2 + 2), 16);
    return w.signMessage(new TextDecoder().decode(bytes));
  });
  return w;
}

test("用钱包登录:派生出的成员公钥与离线派生一致,并找到成员账户", { skip: skip() }, async () => {
  const priv = "0x" + "11".repeat(32);
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  page.on("dialog", (d) => d.accept());
  const w = await injectFakeWallet(page, priv);

  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  await page.click("#idBtn");
  await page.waitForSelector("#idWallet", { timeout: 10000 });
  await page.click("#idWallet");
  // 等到登录完成后的身份页(而不是过程中的提示)
  await page.waitForSelector("#idState", { timeout: 30000 });

  const shown = await page.$eval("#view", (e) => e.innerText);
  assert.match(shown, /钱包/);

  // 页面显示的公钥应与离线按同样规则派生的一致
  const { Wallet } = await import(ETHERS);
  const sig = await w.signMessage(
    ["知行图成员密钥派生 / ZhixingGraph member key derivation", "",
      "签署这条消息会生成你在知行图上的成员密钥。",
      "只在 blockchain.yeying.pub 上签署它;任何拿到这个签名的人都能以你的身份核验和发布知识。",
      "这不是交易,不会花费任何资产。", "",
      `Address: ${w.address.toLowerCase()}`, "Version: 1"].join("\n"));
  const S = require(join(here, "../public/signer.js"));
  const seed = await S.seedFromWalletSignature(sig, w.address);
  const k = await S.keyFromSeed(seed);
  const expect = k.pubkey;
  assert.match(shown, new RegExp(expect.slice(0, 16)), "页面显示的公钥应与离线派生的一致");

  await ctx.close();
});

// ---- 完整闭环:钱包派生的密钥签出的 op 被节点接受 --------------------------------
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
const RPC = process.env.TEST_RPC || "http://127.0.0.1:9221";
const TOKEN = process.env.TEST_WRITE_TOKEN || "local-dev-token";
const rpc = async (p) => { const r = await fetch(RPC + p, { headers: { Accept: "application/json" } }); if (!r.ok) throw new Error(p + " → " + r.status); return r.json(); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(fn, ms = 30000) { const t0 = Date.now(); while (Date.now() - t0 < ms) { const v = await fn(); if (v) return v; await sleep(500); } return null; }
async function memberOf(pubkey) {
  for (const m of (await rpc("/members?limit=500")).items) {
    const a = await rpc("/account/" + m.id).catch(() => null);
    if (a && a.pubkey === pubkey) return m;
  }
  return null;
}

test("钱包登录后核验:节点接受钱包派生密钥签出的 op", { skip: skip() }, async () => {
  const repo = join(here, "../../..");
  const bin = join(repo, "node/target/release/node"), seed = join(repo, ".localnet/keys/acct1.seed");
  if (!existsSync(bin) || !existsSync(seed)) return; // 没有本地链的密钥:跳过闭环部分

  // 每次运行用新私钥,避免与上次运行的成员冲突
  const priv = "0x" + Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => b.toString(16).padStart(2, "0")).join("");
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  page.on("dialog", (d) => d.accept());
  await injectFakeWallet(page, priv);

  // 1. 登录,拿到派生公钥(此时还不是成员)
  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  await page.click("#idBtn");
  await page.waitForSelector("#idWallet", { timeout: 10000 });
  await page.click("#idWallet");
  await page.waitForSelector("#idState", { timeout: 30000 });
  const pub = (await page.$eval("#view kv .mono", (e) => e.textContent)).trim();
  assert.match(pub, /^[0-9a-f]{64}$/);
  assert.match(await page.$eval("#idState", (e) => e.textContent), /还不是测试网成员/);

  // 2. 由账户 1 邀请这个公钥,等它过见习期
  const out = join(mkdtempSync(join(tmpdir(), "zxwinv-")), "op.bin");
  await waitFor(async () => {
    const nonce = (await rpc("/account/1")).nonce;
    execFileSync(bin, ["encode-knowledge-op", "--key-file", seed, "--out", out, "--actor", "1",
      "--nonce", String(nonce), "--action", "invite", "--pubkey", pub], { stdio: "ignore" });
    const r = await fetch(RPC + "/knowledge", { method: "POST", body: readFileSync(out),
      headers: { "Content-Type": "application/octet-stream", Authorization: "Bearer " + TOKEN } });
    return r.ok || /cooldown|too recently/.test(await r.text()) ? r.ok : false;
  }, 60000);
  const m = await waitFor(async () => { const x = await memberOf(pub); return x && x.established ? x : null; }, 60000);
  assert.ok(m, "邀请后应在见习期结束时成为已确立成员");

  // 3. 重新加载(此时已是成员)→ 登录 → 核验一个别人发布的单元
  await page.evaluate((t) => localStorage.setItem("zhixing.writeToken", t), TOKEN);
  const units = (await rpc("/units?limit=200")).items;
  const target = units.find((u) => true);
  assert.ok(target, "本地链上应至少有一个单元");
  const before = (await rpc("/account/" + m.id)).nonce;
  // 带一个不同的查询串,强制整页重新加载(session 只在内存里,必须重来)
  await page.goto(WEB + "/?r=1#unit/" + target.id, { waitUntil: "networkidle0" });
  await page.waitForSelector("#rvWallet", { timeout: 20000 });
  await page.click("#rvWallet");
  await page.waitForSelector("#rvNeeds", { timeout: 30000 });
  await page.click("#rvNeeds");           // "需修改"不需要勾选核验要点
  const after = await waitFor(async () => { const n = (await rpc("/account/" + m.id)).nonce; return Number(n) > Number(before) ? n : null; }, 30000);
  assert.ok(after, "钱包派生密钥签出的核验应被节点接受并入块");

  const detail = await rpc("/unit/" + target.id);
  const last = detail.versions[detail.versions.length - 1];
  assert.ok(last.verifications.some((r) => String(r.verifier) === String(m.id)), "链上应能查到这位成员的核验记录");
  await ctx.close();
});
