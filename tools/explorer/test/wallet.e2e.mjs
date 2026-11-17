// 夜莺钱包 ↔ 知行图浏览器的端到端检验(注入一个模拟钱包,不需要真的装钱包插件)。
//
//   node --test tools/explorer/test/wallet.e2e.mjs
//
// 模拟钱包实现与真钱包相同的接口(夜莺钱包 js/background/request-router.js):
//   zhixing_requestAccounts           → [知行图公钥]
//   zhixing_signKnowledgeOp(意图)     → { signed: op‖签名 的十六进制, address }
// 签名用本仓库的 signer.js(已由 signer.test.mjs 对照 Rust 节点逐字节检验);钱包仓库那一侧的
// tests/zhixing-op.test.mjs 再用这份 signer.js 的输出检验钱包自己的编码——两边合起来闭环。
//
// 覆盖:登录 → 公钥与钱包里的密钥一致;本机有锁定身份时入口不消失;没装钱包时的提示;
//       网页只递"意图"(不递待签字节);旧版钱包不支持时提示升级;登录 → 邀请 → 核验入块。
//
// 用的是每次随机生成的助记词,不是任何真实成员的密钥。
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
require(join(here, "../public/bip39-en.js"));
const S = require(join(here, "../public/signer.js"));
const CHROME = process.env.CHROME_PATH || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const WEB = process.env.TEST_WEB || "http://127.0.0.1:8080";
const RPC = process.env.TEST_RPC || "http://127.0.0.1:9221";
const TOKEN = process.env.TEST_WRITE_TOKEN || "local-dev-token";

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

/**
 * 注入模拟钱包。mode:
 *   "native" — 支持 zhixing_*(当前版本钱包)
 *   "nokey"  — 支持 zhixing_*,但钱包里还没有知行图密钥(code 4900)
 *   "old"    — 旧版钱包:不认识 zhixing_*
 * 返回 { pubkey, requests }:requests 记录网页发给钱包的每个请求(用来检查网页只递意图)。
 */
async function injectFakeWallet(page, mode = "native") {
  const key = await S.keyFromSeed(await S.mnemonicToSeed(await S.generateMnemonic()));
  const requests = [];
  await page.exposeFunction("__zxWallet", async (method, params) => {
    requests.push({ method, params });
    if (mode === "old") return { error: { code: -32601, message: "Method not found" } };
    if (mode === "nokey") return { error: { code: 4900, message: "钱包里还没有知行图密钥" } };
    if (method === "zhixing_requestAccounts" || method === "zhixing_accounts") return { result: [key.pubkey] };
    if (method === "zhixing_signKnowledgeOp") {
      const { actor, nonce, action } = params[0];
      const signed = await S.signOp(key.signKey, Number(actor), action, Number(nonce));
      return { result: { signed: S.bytesToHex(signed), address: key.pubkey } };
    }
    return { error: { code: -32601, message: "Method not found: " + method } };
  });
  await page.evaluateOnNewDocument(() => {
    const info = Object.freeze({ uuid: "11111111-2222-4333-8444-555555555555", name: "YeYing Wallet", icon: "data:image/svg+xml;base64,", rdns: "io.github.yeying" });
    const provider = {
      isYeYing: true,
      async request({ method, params }) {
        const r = await window.__zxWallet(method, params === undefined ? null : params);
        if (r.error) { const e = new Error(r.error.message); e.code = r.error.code; throw e; }
        return r.result;
      },
      on() {},
    };
    // 与真钱包一样:注入时宣告一次,之后回应每次 requestProvider
    const announce = () => window.dispatchEvent(new CustomEvent("eip6963:announceProvider", { detail: { info, provider } }));
    window.addEventListener("eip6963:requestProvider", announce);
    announce();
  });
  return { pubkey: key.pubkey, requests };
}

async function login(page) {
  await page.click("#idBtn");
  await page.waitForSelector("#idWallet", { timeout: 10000 });
  await page.click("#idWallet");
  // 只在登录成功后出现的一行
  await page.waitForFunction(() => /由夜莺钱包保管/.test(document.getElementById("view").innerText), { timeout: 30000 });
}

test("用钱包登录:页面显示的就是钱包里那把密钥的公钥", { skip: skip() }, async () => {
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  const w = await injectFakeWallet(page);
  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  await login(page);
  assert.match(await page.$eval("#view", (e) => e.innerText), new RegExp(w.pubkey));
  assert.deepEqual(w.requests.map((r) => r.method), ["zhixing_requestAccounts"]);
  await ctx.close();
});

// 回归:本机已经保存了口令加密的身份(顶栏显示「已锁定」)时,钱包登录入口不能消失。
test("本机已有锁定的身份时,顶栏与身份页仍能用钱包登录", { skip: skip() }, async () => {
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  await injectFakeWallet(page);
  await page.evaluateOnNewDocument(() => {
    localStorage.setItem("zhixing.vault.v1", JSON.stringify({
      v: 1, kdf: "pbkdf2-sha256", iters: 600000, salt: "AAAA", iv: "AAAA", ct: "AAAA", pubkey: "aa".repeat(32),
    }));
  });
  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  assert.match(await page.$eval("#idStatus", (e) => e.textContent), /已锁定/);
  await page.waitForFunction(() => {
    const b = document.getElementById("idWalletTop");
    return b && b.style.display !== "none";
  }, { timeout: 10000 });
  await page.click("#idBtn");
  await page.waitForSelector("#idWallet", { timeout: 10000 });
  assert.match(await page.$eval("#view", (e) => e.innerText), /已检测到夜莺钱包/);

  await page.click("#idWalletTop");
  await page.waitForFunction(() => /由夜莺钱包保管/.test(document.getElementById("view").innerText), { timeout: 30000 });
  assert.equal(await page.$eval("#idWalletTop", (e) => e.style.display), "none");
  await ctx.close();
});

test("没装钱包时,身份页说明没有检测到并给出安装指引", { skip: skip() }, async () => {
  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  assert.equal(await page.$eval("#idWalletTop", (e) => e.style.display), "none");
  await page.click("#idBtn");
  await page.waitForFunction(() => /没有检测到夜莺钱包/.test(document.getElementById("view").innerText), { timeout: 10000 });
  await ctx.close();
});

test("钱包里还没有知行图密钥 / 旧版钱包:给出能照着做的提示", { skip: skip() }, async () => {
  for (const [mode, expect] of [["nokey", /设置 → 知行图/], ["old", /升级到最新版本/]]) {
    const ctx = await browser.createBrowserContext();
    const page = await ctx.newPage();
    await injectFakeWallet(page, mode);
    await page.goto(WEB + "/", { waitUntil: "networkidle0" });
    await page.click("#idBtn");
    await page.waitForSelector("#idWallet", { timeout: 10000 });
    await page.click("#idWallet");
    await page.waitForFunction((src) => new RegExp(src).test(document.getElementById("idWMsg").innerText),
      { timeout: 10000 }, expect.source);
    await ctx.close();
  }
});

// ---- 完整闭环:钱包签出的操作被节点接受 ------------------------------------------
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

test("钱包登录后核验:网页只递意图,钱包签出的操作被节点接受并入块", { skip: skip() }, async () => {
  const repo = join(here, "../../..");
  const bin = join(repo, "node/target/release/node"), seed = join(repo, ".localnet/keys/acct1.seed");
  if (!existsSync(bin) || !existsSync(seed)) return; // 没有本地链的密钥:跳过闭环部分

  const ctx = await browser.createBrowserContext();
  const page = await ctx.newPage();
  page.on("dialog", (d) => d.accept());
  const w = await injectFakeWallet(page);

  // 1. 登录(此时还不是成员)
  await page.goto(WEB + "/", { waitUntil: "networkidle0" });
  await login(page);
  assert.match(await page.$eval("#idState", (e) => e.textContent), /还不是测试网成员/);

  // 2. 由账户 1 邀请这个公钥,等它过见习期
  const out = join(mkdtempSync(join(tmpdir(), "zxwinv-")), "op.bin");
  await waitFor(async () => {
    const nonce = (await rpc("/account/1")).nonce;
    execFileSync(bin, ["encode-knowledge-op", "--key-file", seed, "--out", out, "--actor", "1",
      "--nonce", String(nonce), "--action", "invite", "--pubkey", w.pubkey], { stdio: "ignore" });
    const r = await fetch(RPC + "/knowledge", { method: "POST", body: readFileSync(out),
      headers: { "Content-Type": "application/octet-stream", Authorization: "Bearer " + TOKEN } });
    return r.ok || /cooldown|too recently/.test(await r.text()) ? r.ok : false;
  }, 60000);
  const m = await waitFor(async () => { const x = await memberOf(w.pubkey); return x && x.established ? x : null; }, 60000);
  assert.ok(m, "邀请后应在见习期结束时成为已确立成员");

  // 3. 重新加载 → 在单元页用钱包登录 → 核验
  await page.evaluate((t) => localStorage.setItem("zhixing.writeToken", t), TOKEN);
  const target = (await rpc("/units?limit=200")).items[0];
  assert.ok(target, "本地链上应至少有一个单元");
  const before = (await rpc("/account/" + m.id)).nonce;
  await page.goto(WEB + "/?r=1#unit/" + target.id, { waitUntil: "networkidle0" });
  await page.waitForSelector("#rvWallet", { timeout: 20000 });
  await page.click("#rvWallet");
  await page.waitForSelector("#rvNeeds", { timeout: 30000 });
  await page.click("#rvNeeds"); // "需修改"不需要勾选核验要点
  const after = await waitFor(async () => { const n = (await rpc("/account/" + m.id)).nonce; return Number(n) > Number(before) ? n : null; }, 30000);
  assert.ok(after, "钱包签出的核验应被节点接受并入块");

  // 网页递给钱包的是"意图",不是待签字节
  const sign = w.requests.find((r) => r.method === "zhixing_signKnowledgeOp");
  assert.ok(sign, "应通过 zhixing_signKnowledgeOp 请钱包签名");
  const intent = sign.params[0];
  assert.deepEqual(Object.keys(intent).sort(), ["action", "actor", "nonce"]);
  assert.equal(intent.actor, String(m.id));
  assert.deepEqual(intent.action, { type: "verify", unit: Number(target.id), version: intent.action.version, verdict: "needs-revision" });

  const detail = await rpc("/unit/" + target.id);
  const last = detail.versions[detail.versions.length - 1];
  assert.ok(last.verifications.some((r) => String(r.verifier) === String(m.id)), "链上应能查到这位成员的核验记录");
  await ctx.close();
});
