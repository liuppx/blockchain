// 钱包登录:从钱包的 personal_sign 签名派生成员密钥。
//
//   node --test tools/explorer/test/wallet.test.mjs
//
// 要证明的三件事:
//   1. 同一个钱包账户、同一条消息 ⇒ 同一把成员密钥(否则用户每次登录都变成另一个人);
//   2. 不同账户 ⇒ 不同的成员密钥;
//   3. 派生出的种子能正常生成 ed25519 密钥并签 op。
// 以太坊签名用的是 RFC 6979 确定性 k,这里用 ethers 模拟钱包签名,验证这一前提。
import { test } from "node:test";
import assert from "node:assert/strict";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { existsSync } from "node:fs";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
require(join(here, "../public/bip39-en.js"));
const S = require(join(here, "../public/signer.js"));

// 固定私钥(公开的测试值,不是任何真实账户),让测试结果可复现
const PK_A = "0x" + "11".repeat(32);
const PK_B = "0x" + "22".repeat(32);

// ethers 只用于在测试里模拟钱包签名;浏览器代码本身不依赖它
const ETHERS = process.env.ETHERS_PATH
  || join(here, "../../../../wallet/lib/ethers-6.16.esm.min.js");
const haveEthers = existsSync(ETHERS);

test("派生消息写明用途、地址与版本,地址统一小写", () => {
  const m = S.walletDeriveMessage("0xAbCdEf0000000000000000000000000000000001");
  assert.match(m, /知行图成员密钥派生/);
  assert.match(m, /Address: 0xabcdef0000000000000000000000000000000001/);
  assert.match(m, /Version: 1/);
  assert.match(m, /不是交易/);
});

test("签名长度不对时拒绝", async () => {
  await assert.rejects(() => S.seedFromWalletSignature("0x" + "ab".repeat(64), "0x1"), /65 字节/);
});

test("utf8ToHex 与 personal_sign 的十六进制参数格式一致", () => {
  assert.equal(S.utf8ToHex("hi"), "0x6869");
  assert.equal(S.utf8ToHex("知"), "0xe79fa5");
});

test("同一签名 ⇒ 同一种子;地址不同 ⇒ 种子不同(HKDF 绑定地址)", async () => {
  const sig = "0x" + "ab".repeat(65);
  const a1 = await S.seedFromWalletSignature(sig, "0xAAA");
  const a2 = await S.seedFromWalletSignature(sig, "0xaaa");
  const b = await S.seedFromWalletSignature(sig, "0xbbb");
  assert.equal(a1.length, 32);
  assert.deepEqual(a1, a2, "地址大小写不影响派生");
  assert.notDeepEqual(a1, b);
});

test("真实钱包签名:同一账户每次派生同一把成员密钥,不同账户不同", { skip: !haveEthers && "找不到 ethers(设置 ETHERS_PATH)" }, async () => {
  const { Wallet } = await import(ETHERS);
  const wa = new Wallet(PK_A), wb = new Wallet(PK_B);

  const derive = async (w) => {
    const msg = S.walletDeriveMessage(w.address);
    const sig = await w.signMessage(msg);              // 等价于钱包的 personal_sign
    return (await S.keyFromSeed(await S.seedFromWalletSignature(sig, w.address))).pubkey;
  };

  const a1 = await derive(wa), a2 = await derive(wa), b = await derive(wb);
  assert.equal(a1, a2, "同一账户两次登录必须得到同一个成员公钥");
  assert.notEqual(a1, b, "不同账户必须得到不同的成员公钥");
  assert.match(a1, /^[0-9a-f]{64}$/);
});

test("派生出的密钥能签 op", { skip: !haveEthers && "找不到 ethers" }, async () => {
  const { Wallet } = await import(ETHERS);
  const w = new Wallet(PK_A);
  const sig = await w.signMessage(S.walletDeriveMessage(w.address));
  const k = await S.keyFromSeed(await S.seedFromWalletSignature(sig, w.address));
  const op = await S.signOp(k.signKey, 7, { type: "verify", unit: 1, version: 1, verdict: "pass" }, 0);
  assert.ok(op instanceof Uint8Array && op.length > 64);
});
