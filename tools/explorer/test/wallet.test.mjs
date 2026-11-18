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
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const member = readFileSync(join(here, "../public/member.js"), "utf8");
const signer = readFileSync(join(here, "../public/signer.js"), "utf8");

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
  for (const gone of ["generateMnemonic", "mnemonicToSeed", "sealSeed", "openSeed", "decryptKeystore", "keyFromSeed", "signOp"]) {
    assert.doesNotMatch(member, new RegExp("S\\." + gone + "\\("), `member.js 不应再调用 S.${gone}`);
  }
  for (const id of ["idGen", "idImp", "idFile", "idUnlock", "rvUnlock", "impWords"]) {
    assert.doesNotMatch(member, new RegExp(`id='${id}'`), `不应再有 #${id}`);
  }
});
