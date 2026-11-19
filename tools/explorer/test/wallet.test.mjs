// 浏览器 ↔ 夜莺钱包的接口约定(不需要浏览器,CI 门禁里跑)。
//
//   node --test tools/explorer/test/wallet.test.mjs
//
// 约定(夜莺钱包 js/background/request-router.js 的 zhixing_* 方法):
//   - 网页只递"意图" { actor, nonce, action },钱包自己编码、给用户看摘要、签名;
//     网页绝不能把待签字节交给钱包——否则用户在弹窗里看到的不一定是实际签的内容;
//   - 不再用以太坊签名派生成员密钥(personal_sign 那条路已移除):同一个钱包不应产生两把不同的成员密钥。
// 端到端行为见 wallet.e2e.mjs。
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const member = readFileSync(join(here, "../public/member.js"), "utf8");
const signer = readFileSync(join(here, "../reference/signer.js"), "utf8");
const PUBLIC = join(here, "../public");

test("登录用 zhixing_requestAccounts,签名用 zhixing_signKnowledgeOp", () => {
  assert.match(member, /method:\s*"zhixing_requestAccounts"/);
  assert.match(member, /method:\s*"zhixing_signKnowledgeOp"/);
});

test("签名请求只递意图 { actor, nonce, action },不递待签字节", () => {
  const call = /method:\s*"zhixing_signKnowledgeOp",\s*params:\s*\[\{([^}]*)\}\]/.exec(member);
  assert.ok(call, "应找到 zhixing_signKnowledgeOp 调用");
  const keys = call[1].split(",").map((p) => p.trim().split(":")[0].trim()).filter(Boolean).sort();
  assert.deepEqual(keys, ["action", "actor", "nonce"]);
  assert.doesNotMatch(call[1], /encodeOp|bytes|signOp/);
});

test("不再用以太坊签名派生成员密钥", () => {
  for (const src of [member, signer]) {
    assert.doesNotMatch(src, /personal_sign/);
    assert.doesNotMatch(src, /seedFromWalletSignature|walletDeriveMessage/);
  }
});

test("钱包返回的公钥与登录时不同 ⇒ 拒绝提交(钱包里换过密钥)", () => {
  assert.match(member, /out\.address !== session\.pubkey/);
});

test("浏览器本地存密钥的旧登录方式已下线:网页不生成、不导入、不解锁私钥", () => {
  for (const id of ["idGen", "idImp", "idFile", "idUnlock", "rvUnlock", "impWords"]) {
    assert.doesNotMatch(member, new RegExp(`id='${id}'`), `不应再有 #${id}`);
  }
});

// 网页(被服务的 public/ 目录)里不能有任何私钥代码:不生成助记词、不派生密钥、不签名、不加解密。
// 参考签名器在 reference/,只给测试用;谁再把它或类似代码放回网页,这里直接失败。
test("被服务的 public/ 里没有私钥代码", () => {
  assert.ok(!existsSync(join(PUBLIC, "signer.js")), "public/signer.js 不应存在(参考签名器在 reference/)");
  assert.ok(!existsSync(join(PUBLIC, "bip39-en.js")), "public/bip39-en.js 不应存在");
  const FORBIDDEN = [/ZXSigner/, /BIP39_EN/, /Ed25519/i, /PBKDF2/i, /deriveBits/, /deriveKey/, /importKey/,
    /subtle\.(sign|encrypt|decrypt)/, /getRandomValues/, /mnemonic/i, /scrypt/i, /keccak/i, /\bseed\b/i];
  // 递归扫描网页会执行的代码(.js / .html);ns/v1/*.jsonld 是词表数据,里面描述"ed25519 公钥"不是代码
  const files = readdirSync(PUBLIC, { recursive: true, withFileTypes: true })
    .filter((d) => d.isFile() && /\.(js|html)$/.test(d.name))
    .map((d) => join(d.parentPath || d.path, d.name));
  assert.ok(files.length > 5, "应扫描到 public/ 下的文件");
  for (const f of files) {
    const src = readFileSync(f, "utf8");
    for (const re of FORBIDDEN) assert.doesNotMatch(src, re, `${f.slice(PUBLIC.length + 1)} 里出现了 ${re}`);
  }
});

test("页面只加载无密钥的 util.js,不加载签名器", () => {
  const html = readFileSync(join(PUBLIC, "index.html"), "utf8");
  assert.match(html, /<script src="util\.js"><\/script>/);
  assert.doesNotMatch(html, /signer\.js|bip39/);
});

test("util.js 的格式检查:合格的意图通过,越界的被拒", async () => {
  const require = (await import("node:module")).createRequire(import.meta.url);
  const U = require(join(PUBLIC, "util.js"));
  const h = "ab".repeat(32);
  U.checkAction({ type: "verify", unit: 6, version: 2, verdict: "pass" });
  U.checkAction({ type: "invite", pubkey: h });
  U.checkAction({ type: "create", layer: "foundation", domain: 2, bodies: [{ lang: "zh", hash: h }], source: { type: "human" }, cites: [] });
  U.checkAction({ type: "publish", unit: 6, bodies: [{ lang: "zh", hash: h }], source: { type: "model", id: "m" }, cites: [{ unit: 1, version: 2, kind: "supersedes" }] });
  assert.throws(() => U.checkAction({ type: "delete" }), /不认识的操作/);
  assert.throws(() => U.checkAction({ type: "create", layer: "theory", domain: 1 }), /层级/);
  assert.throws(() => U.checkAction({ type: "publish", unit: 1, bodies: [{ lang: "中文中文中文", hash: h }] }), /字节/);
  assert.throws(() => U.checkAction({ type: "publish", unit: 1, cites: [{ unit: 1, version: 1, kind: "depends" }] }), /引用类型/);
  assert.throws(() => U.checkAction({ type: "publish", unit: 1, cites: Array(33).fill({ unit: 1, version: 1, kind: "basis" }) }), /最多 32/);
  assert.throws(() => U.checkAction({ type: "publish", unit: 1, source: { type: "model", id: "x".repeat(65) } }), /模型标识/);
  assert.throws(() => U.checkAction({ type: "verify", unit: 1, version: 1, verdict: "maybe" }), /verdict/);
  assert.throws(() => U.checkAction({ type: "invite", pubkey: "abcd" }), /64 位/);
});
