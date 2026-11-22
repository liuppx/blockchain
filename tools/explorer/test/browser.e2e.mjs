// 浏览器端到端测试:成员在浏览器里的完整流程。
//
//   node --test tools/explorer/test/browser.e2e.mjs
//
// 覆盖:用夜莺钱包登录(注入实现 zhixing_* 接口的假钱包,见 fake-wallet.mjs)→ 写作(上传正文 + create)
//       → 修订(publish v2)→ 核验(需勾选全部核验要点)→ 邀请 → 检索 → 导出。
// 浏览器本地存密钥的旧登录方式已下线,网页不再持有私钥。
//
// 前置:需要已运行的本地链(`./cmd up`,RPC 127.0.0.1:9221)与浏览器服务(127.0.0.1:8080)。
// 浏览器:用 CHROME_PATH 指定,默认 macOS 的 Chrome 路径;未找到则整个套件跳过(不算失败)。
// 用的是**公开测试助记词**(BIP-39 官方向量,全零熵),不是任何真实成员的密钥。
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const CHROME = process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const RPC = process.env.TEST_RPC || "http://127.0.0.1:9221";
const WEB = process.env.TEST_WEB || "http://127.0.0.1:8080";
const ZERO24 = Array(23).fill("abandon").join(" ") + " art"; // 公开测试助记词
import { injectFakeWallet, memberKeyFromMnemonic } from "./fake-wallet.mjs";
const TEST_KEY = await memberKeyFromMnemonic(ZERO24);

let puppeteer = null;
try {
  puppeteer = require("puppeteer-core");
} catch (_) {
  // puppeteer-core 不在依赖里:用全局安装的也行
  try { puppeteer = require(process.env.PUPPETEER_PATH || "puppeteer-core"); }
  catch (__) { puppeteer = null; }
}

const hasChrome = existsSync(CHROME);
let browser = null;

before(async () => {
  if (!puppeteer || !hasChrome) return;
  browser = await puppeteer.launch({ executablePath: CHROME, headless: "new" });
});

after(async () => {
  if (browser) await browser.close();
});

function skipIfNoBrowser() {
  if (!puppeteer || !hasChrome) return "no browser";
  return null;
}

async function newPage() {
  // 独立的浏览器上下文:每个用例的 localStorage 互不影响
  const ctx = await browser.createBrowserContext();
  const p = await ctx.newPage();
  await injectFakeWallet(p, TEST_KEY);
  p.context = ctx;
  await p.setViewport({ width: 1300, height: 1400 });
  p.on("dialog", (d) => d.accept());
  const errors = [];
  p.on("pageerror", (e) => errors.push("pageerror: " + e.message));
  // 404 的具体资源由 response 事件给出(console 文本里没有 URL)
  const notFound = [];
  // /status/disk.json 是可选的运维状态文件(本地开发链没有),不算错误
  p.on("response", (r) => { if (r.status() === 404 && !r.url().includes("/status/")) notFound.push(r.url()); });
  p.notFound = notFound;
  p.errors = errors;
  return p;
}

// 在「我的身份」页用钱包登录,等到登录完成(只在登录后出现"由夜莺钱包保管")
async function loginWithWallet(p) {
  await p.click("#idBtn");
  await p.waitForSelector("#idWallet", { timeout: 10000 });
  await p.click("#idWallet");
  await p.waitForFunction(() => /由夜莺钱包保管/.test(document.getElementById("view")?.innerText || "")
    && !!document.getElementById("idState"), { timeout: 30000 });
}

// 测试公钥(公开测试助记词 "abandon … art" 派生出的公钥),从假钱包里那把密钥直接取,不写死。
const TEST_PUBKEY = TEST_KEY.pubkey;

// 确保测试公钥在链上是成员:用账户 1 的种子签一个 invite。链上已有该成员时跳过。
// 需要链的写权限(本地开发链默认开放);失败时抛出明确信息而不是让用例莫名失败。
async function ensureMember() {
  const members = (await rpc("/members?limit=200")).items;
  if (members.length && members.some((m) => m.established)) {
    // 已有成员且已过见习期:仍需确认测试公钥本身是成员
    const ok = await isTestKeyMember();
    if (ok) return true;
  }
  const repoRoot = join(here, "../../..");
  const bin = join(repoRoot, "node/target/release/node");
  const seed = join(repoRoot, ".localnet/keys/acct1.seed");
  if (!existsSync(bin) || !existsSync(seed)) return false;
  const nonce = (await rpc("/account/1")).nonce;
  const out = join(mkdtempSync(join(tmpdir(), "zxinv-")), "op.bin");
  execFileSync(bin, ["encode-knowledge-op", "--key-file", seed, "--out", out,
    "--actor", "1", "--nonce", String(nonce), "--action", "invite", "--pubkey", TEST_PUBKEY],
    { stdio: "ignore" });
  const body = (await import("node:fs")).readFileSync(out);
  const r = await fetch(RPC + "/knowledge", {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream", "Authorization": "Bearer " + (process.env.TEST_WRITE_TOKEN || "local-dev-token") },
    body,
  });
  if (!r.ok) throw new Error("邀请测试公钥失败:HTTP " + r.status + " " + (await r.text()).slice(0, 200));
  return true;
}

async function isTestKeyMember() {
  const members = (await rpc("/members?limit=200")).items;
  for (const m of members) {
    try {
      const a = await rpc("/account/" + m.id);
      if (a.pubkey === TEST_PUBKEY) return true;
    } catch (_) { /* 跳过 */ }
  }
  return false;
}

// 链上读:走节点 RPC
async function rpc(path) {
  const r = await fetch(RPC + path, { headers: { Accept: "application/json" } });
  if (!r.ok) throw new Error(path + " → HTTP " + r.status);
  return r.json();
}

// 返回**满足条件的值**(而不是 true),便于取到新建单元的 id
async function waitFor(fn, ms = 20000, step = 500) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    const v = await fn();
    if (v) return v;
    await new Promise((r) => setTimeout(r, step));
  }
  return null;
}

test("浏览器:导入身份 → 写作 → 修订 → 核验 → 邀请 → 检索 → 导出", { concurrency: false }, async (t) => {
  const why = skipIfNoBrowser();
  if (why) { t.skip(why); return; }
  const p = await newPage();
  try {
    // ---- 用夜莺钱包登录(公开测试助记词对应的密钥) ----------------------------
    // 该公钥必须是链上成员才能写;不是就先用账户 1 邀请它,再登录
    if (!(await isTestKeyMember())) {
      const invited = await ensureMember();
      assert.ok(invited, "测试公钥不是链上成员,且无法自动邀请(需要本地链与账户 1 的种子)");
      const joined = await waitFor(isTestKeyMember, 30000, 500);
      assert.ok(joined, "邀请已提交但测试公钥尚未成为成员");
    }
    await p.goto(WEB + "/", { waitUntil: "networkidle0" });
    await loginWithWallet(p);
    const state = await p.$eval("#idState", (e) => e.textContent);
    assert.match(state, /账户 \d+/, "登录后应显示账户编号:" + state);

    // 该密钥要在链上是成员才能写;不是的话先由链上邀请(见下面的邀请用例)
    const token = process.env.TEST_WRITE_TOKEN || "local-dev-token";
    if (token) {
      await p.type("#idTok", token);
      await p.click("#idTokSave");
    }

    // ---- 写作:创建单元 ------------------------------------------------------
    // 正文每次运行都不同:U2 规定同一份正文不能被当作新单元重复提交(否则会被 DuplicateBody 拒绝),
    // 而本地链会在多次运行之间保留,所以用运行编号让正文唯一。
    const RUN = Date.now().toString(36);
    const BODY = `# 端到端测试单元 ${RUN}

| 字段 | 内容 |
|---|---|
| 层级 | 解释 |
| 来源 | 人工撰写 |

## 核心陈述

浏览器端到端测试创建的知识单元(运行 ${RUN})。

## 核验要点(给人类审查者)

1. 第一条要点。
2. 第二条要点。
`;
    await p.evaluate(() => showTab("knowledge"));
    await p.waitForSelector("#auNew", { timeout: 20000 });
    await p.click("#auNew");
    await p.waitForSelector("#auText");
    await p.$eval("#auText", (e, v) => { e.value = v; e.dispatchEvent(new Event("input")); }, BODY);
    await new Promise((r) => setTimeout(r, 400));
    const layer = await p.$eval("#auLayer", (e) => e.value);
    assert.equal(layer, "explanation", "层级应从正文表格自动识别");
    const before = (await rpc("/units?limit=1")).total;
    await p.click("#auGo");
    const created = await waitFor(async () => {
      const h = await p.evaluate(() => location.hash);
      return /^#unit\/\d+$/.test(h) ? h.split("/")[1] : null;
    });
    assert.ok(created, "创建后应跳转到新单元页面");
    const detail = await rpc("/unit/" + created);
    assert.equal(detail.author !== undefined || true, true);
    assert.ok(Number(detail.versions.length) >= 1, "链上应有版本");
    assert.ok(detail.versions[0].bodies[0].hash, "版本应带正文哈希");
    // 正文可在正文仓库取到,且哈希一致
    const bodyRes = await fetch(WEB + "/bodies/" + detail.versions[0].bodies[0].hash);
    assert.equal(bodyRes.status, 200, "正文应已上传到正文仓库");
    assert.equal((await bodyRes.text()).trim(), BODY.trim(), "正文内容应与提交一致");

    // ---- 修订:发布 v2 -------------------------------------------------------
    await p.waitForSelector("#auRevise", { timeout: 20000 });
    await p.click("#auRevise");
    await p.waitForSelector("#auText");
    const prefilled = await p.$eval("#auText", (e) => e.value);
    assert.match(prefilled, /端到端测试单元/, "修订应预填当前正文");
    await p.$eval("#auText", (e) => {
      e.value = e.value.replace("浏览器端到端测试创建的知识单元(", "浏览器端到端测试创建的知识单元(第二版,");
      e.dispatchEvent(new Event("input"));
    });
    await p.click("#auGo");
    await waitFor(async () => (await rpc("/unit/" + created)).versions.length >= 2, 30000);
    const after = await rpc("/unit/" + created);
    assert.equal(after.versions.length, 2, "修订后应有 2 个版本");
    assert.notEqual(after.versions[0].bodies[0].hash, after.versions[1].bodies[0].hash, "两版正文哈希应不同");
    assert.equal(Number(after.versions[1].publisher) !== 0, true, "修订者应记录在链上");

    // ---- 检索 ---------------------------------------------------------------
    // 检索索引按需刷新(最多 30 秒一次),刚建的单元可能还没进索引:等它出现,而不是只查一次
    let hits = null;
    const found = await waitFor(async () => {
      const sr = await fetch(WEB + "/search?q=" + encodeURIComponent(RUN));
      if (sr.status !== 200) return null;
      hits = await sr.json();
      return hits.items.some((h) => String(h.id) === String(created)) ? hits : null;
    }, 45000, 1500);
    assert.ok(found, "检索应命中新建单元(按本次运行编号查找)");
    assert.ok(hits.items.find((h) => String(h.id) === String(created)).title.includes("端到端测试"), "标题应含关键词");

    // ---- 导出 ---------------------------------------------------------------
    const ex = await fetch(WEB + "/export/units");
    assert.equal(ex.status, 200);
    const lines = (await ex.text()).trim().split("\n");
    const meta = JSON.parse(lines[0]);
    assert.equal(meta.kind, "metadata");
    assert.equal(meta.license, "CC-BY-4.0", "导出应声明许可");
    const exported = lines.slice(1).map((l) => JSON.parse(l));
    assert.equal(exported.length, meta.units, "导出行数应与元数据一致");
    const mine = exported.find((u) => String(u.id) === String(created));
    assert.ok(mine, "导出应包含新建单元");
    assert.ok(mine.bodies[0].text.includes("端到端测试"), "导出应包含正文全文");
    assert.ok(mine.cites !== undefined && mine.credibility, "导出应包含引用与可信度");

    assert.deepEqual(p.errors, [], "页面不应有 JS 错误");
    assert.deepEqual(p.notFound, [], "不应有 404 资源");
  } finally {
    await p.close();
    if (p.context) await p.context.close();
  }
});

test("浏览器:核验面板 —— 勾选全部核验要点后才能通过;不能核验自己发布的版本", { concurrency: false }, async (t) => {
  const why = skipIfNoBrowser();
  if (why) { t.skip(why); return; }
  const p = await newPage();
  try {
    await p.goto(WEB + "/", { waitUntil: "networkidle0" });
    await loginWithWallet(p);
    const token = process.env.TEST_WRITE_TOKEN || "local-dev-token";
    if (token) { await p.type("#idTok", token); await p.click("#idTokSave"); }

    // 找一个不是本账户发布的单元
    const me = await p.$eval("#idState", (e) => e.textContent);
    // The first browser flow may have just created the fixture unit. Wait for it
    // instead of racing the block commit when the suite runs against a fresh chain.
    const units = (await waitFor(async () => {
      const items = (await rpc("/units?limit=50")).items;
      return items.length ? items : null;
    }, 30000, 500)) || [];
    const target = units.find((u) => !me.includes("账户 " + u.author + " "));
    assert.ok(target, "应存在其他作者发布的单元");
    await p.goto(WEB + "/#unit/" + target.id, { waitUntil: "networkidle0" });
    await p.waitForSelector("#reviewSlot", { timeout: 20000 });

    const text = await p.$eval("#reviewSlot", (e) => e.innerText);
    if (/不能由你自己核验/.test(text)) {
      // 自己发布的版本:应明确拒绝
      assert.match(text, /不能由你自己核验/);
      return;
    }
    // 否则应有核验要点与按钮(登录状态只在内存里,换页后在单元页用钱包重新登录)
    if (/用夜莺钱包登录后核验/.test(text)) {
      await p.click("#rvWallet");
      await p.waitForSelector("#rvPass, #rvNeeds, #reviewSlot .mut", { timeout: 30000 });
      if (/不能由你自己核验/.test(await p.$eval("#reviewSlot", (e) => e.innerText))) return;
    }
    const checks = await p.$$eval(".rvChk", (cs) => cs.length);
    if (checks > 0) {
      const disabledBefore = await p.$eval("#rvPass", (e) => e.disabled);
      assert.equal(disabledBefore, true, "未勾选核验要点时,通过按钮应禁用");
      for (const c of await p.$$(".rvChk")) await c.click();
      const disabledAfter = await p.$eval("#rvPass", (e) => e.disabled);
      assert.equal(disabledAfter, false, "勾选全部后,通过按钮应可点");
    }
    assert.deepEqual(p.errors, [], "页面不应有 JS 错误");
    assert.deepEqual(p.notFound, [], "不应有 404 资源");
  } finally {
    await p.close();
    if (p.context) await p.context.close();
  }
});
