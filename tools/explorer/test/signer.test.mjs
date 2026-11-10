// 浏览器签名器与节点逐字节一致的检验。
//
//   node tools/explorer/test/signer.test.mjs [path/to/node-binary]
//
// 对同一个助记词,分别用 signer.js(浏览器同款代码,在 Node 的 WebCrypto 上运行)和 Rust 程序
// (`node keygen` / `node encode-knowledge-op`)生成公钥与签名后的 op,要求完全相同。
// ed25519 签名是确定性的,同一密钥对同一消息的签名唯一,所以可以逐字节比较。
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
require(join(here, "../public/bip39-en.js"));
const S = require(join(here, "../public/signer.js"));
const BIN = process.argv[2] || join(here, "../../../node/target/release/node");

// 公开的测试助记词(BIP-39 官方测试向量用的全零熵),不是任何真实成员的密钥
const ZERO24 = Array(23).fill("abandon").join(" ") + " art";
const ZERO12 = Array(11).fill("abandon").join(" ") + " about";

function rust(args, cwd) {
  return execFileSync(BIN, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
}

function withRustKey(mnemonic, fn) {
  const dir = mkdtempSync(join(tmpdir(), "zxsig-"));
  try {
    const out = rust(["keygen", "--mnemonic", mnemonic, "--out", "k.seed"], dir);
    const pubkey = /pubkey ([0-9a-f]{64})/.exec(out)[1];
    // keygen 写的种子文件带 # 注释行(助记词、公钥),--key-file 直接读取它
    const seedHex = readFileSync(join(dir, "k.seed"), "utf8").trim().split("\n").pop();
    return fn({ dir, pubkey, seedHex });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

test("BIP-39 生成:全零熵得到官方测试向量的助记词", async () => {
  assert.equal(await S.entropyToMnemonic(new Uint8Array(32)), ZERO24);
  assert.equal(await S.entropyToMnemonic(new Uint8Array(16)), ZERO12);
});

test("BIP-39 校验:词数、未知词、校验和", async () => {
  assert.equal(await S.validateMnemonic("  " + ZERO12.toUpperCase() + " "), ZERO12);
  await assert.rejects(S.validateMnemonic("abandon abandon"), /12\/15\/18\/21\/24/);
  await assert.rejects(S.validateMnemonic(ZERO12.replace("about", "zebra1")), /不认识/);
  await assert.rejects(S.validateMnemonic(ZERO12.replace("about", "abandon")), /校验失败/);
  const fresh = await S.generateMnemonic();
  assert.equal(fresh.split(" ").length, 24);
  assert.equal(await S.validateMnemonic(fresh), fresh);
});

for (const [name, m] of [["24 词", ZERO24], ["12 词", ZERO12]]) {
  test(`助记词 → 种子 → 公钥 与节点一致(${name})`, async () => {
    await withRustKey(m, async ({ pubkey, seedHex }) => {
      const seed = await S.mnemonicToSeed(m);
      assert.equal(S.bytesToHex(seed), seedHex);
      assert.equal((await S.keyFromSeed(seed)).pubkey, pubkey);
    });
  });
}

const CASES = [
  { action: { type: "verify", unit: 6, version: 2, verdict: "pass" }, actor: 3, nonce: 7,
    args: ["--action", "verify", "--unit", "6", "--version", "2", "--verdict", "pass"] },
  { action: { type: "verify", unit: 123456789012, version: 4000000000, verdict: "needs-revision",
      noteHash: "ab".repeat(32) }, actor: 2 ** 40, nonce: 2 ** 33,
    args: ["--action", "verify", "--unit", "123456789012", "--version", "4000000000",
      "--verdict", "needs-revision", "--note-hash", "ab".repeat(32)] },
  { action: { type: "invite", pubkey: "1de352e44cd333672593f2334a730e180aaf290de89aa16d480de594e34e2961" },
    actor: 1, nonce: 0,
    args: ["--action", "invite", "--pubkey", "1de352e44cd333672593f2334a730e180aaf290de89aa16d480de594e34e2961"] },
];

for (const c of CASES) {
  test(`签名后的 op 与节点逐字节一致:${c.action.type} actor=${c.actor} nonce=${c.nonce}`, async () => {
    await withRustKey(ZERO24, async ({ dir }) => {
      rust(["encode-knowledge-op", "--key-file", "k.seed", "--out", "op.bin",
        "--actor", String(c.actor), "--nonce", String(c.nonce), ...c.args], dir);
      const want = readFileSync(join(dir, "op.bin"));
      const { signKey } = await S.keyFromSeed(await S.mnemonicToSeed(ZERO24));
      const got = await S.signOp(signKey, c.actor, c.action, c.nonce);
      assert.equal(S.bytesToHex(got), want.toString("hex"));
    });
  });
}

test("本机加密保存:口令正确才能解开,错误口令被拒", async () => {
  const seed = await S.mnemonicToSeed(ZERO24);
  const vault = await S.sealSeed(seed, "correct horse battery", "pk");
  assert.ok(!JSON.stringify(vault).includes(S.bytesToHex(seed)), "密文中不含种子明文");
  assert.equal(S.bytesToHex(await S.openSeed(vault, "correct horse battery")), S.bytesToHex(seed));
  await assert.rejects(S.openSeed(vault, "wrong password"), /口令不对/);
  await assert.rejects(S.sealSeed(seed, "short", "pk"), /至少 8/);
});

test("不支持的操作与错误参数被拒绝", async () => {
  assert.throws(() => S.encodeOp(1, { type: "create" }, 0), /只支持/);
  assert.throws(() => S.encodeOp(1, { type: "verify", unit: 1, version: 1, verdict: "maybe" }, 0), /verdict/);
  assert.throws(() => S.encodeOp(1, { type: "invite", pubkey: "abcd" }, 0), /64 位/);
});

// ---- 导入节点的加密密钥文件 ---------------------------------------------------------

test("Keccak-256 官方向量(以太坊用的原始 Keccak,不是 SHA3-256)", () => {
  const h = (s) => S.bytesToHex(S.keccak256(new TextEncoder().encode(s)));
  assert.equal(h(""), "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470");
  assert.equal(h("abc"), "4e03657aea45a94fc7d47ba826c8d667c0d1e6e33a64a036ec44f58fa12d6c45");
  // 跨越一个分组(136 字节)的输入
  assert.equal(S.bytesToHex(S.keccak256(new Uint8Array(200))).length, 64);
});

test("scrypt RFC 7914 测试向量", async () => {
  const e = new TextEncoder();
  const v = await S.scrypt(e.encode("password"), e.encode("NaCl"), 1024, 8, 16, 64);
  assert.equal(S.bytesToHex(v),
    "fdbabe1c9d3472007856e7190d01e9fe7c6ad7cbc8237830e77376634b3731622eaf30d92e22a3886ff109279d9830dac727afb94a83ee6d8360cbdfa2cc0640");
  const v2 = await S.scrypt(e.encode("pleaseletmein"), e.encode("SodiumChloride"), 16384, 8, 1, 64);
  assert.equal(S.bytesToHex(v2),
    "7023bdcb3afd7348461c06cd81fd38ebfda8fbba904f8e3ea9b543f6545da1f2d5432955613f0fcf62d49705242a9af9e61e85dc0d651e40dfcf017b45575887");
});

test("解开 Rust 程序写的密钥文件(keygen --encrypt),种子与助记词推出的一致", async () => {
  const dir = mkdtempSync(join(tmpdir(), "zxks-"));
  try {
    execFileSync(BIN, ["keygen", "--mnemonic", ZERO24, "--encrypt", "--out", "ks.json"],
      { cwd: dir, env: { ...process.env, ZHIXING_KEYSTORE_PASSWORD: "ks-password-123" }, stdio: "ignore" });
    const json = readFileSync(join(dir, "ks.json"), "utf8");
    const seed = await S.decryptKeystore(json, "ks-password-123");
    assert.equal(S.bytesToHex(seed), S.bytesToHex(await S.mnemonicToSeed(ZERO24)));
    await assert.rejects(S.decryptKeystore(json, "wrong-password"), /口令不对/);
    await assert.rejects(S.decryptKeystore("{not json", "x"), /无法解析/);
    const evil = JSON.parse(json);
    evil.crypto.kdfparams.n = 2 ** 24; // 恶意文件:要求 4 GB 内存
    await assert.rejects(S.decryptKeystore(JSON.stringify(evil), "x"), /超出支持范围/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
