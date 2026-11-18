// 测试用的假夜莺钱包:实现真钱包的 zhixing_* 接口(见 wallet 仓库 docs/protocols/知行图协议.md)。
//
//   zhixing_requestAccounts  → [公钥]
//   zhixing_signKnowledgeOp  → { signed: op‖签名的十六进制, address }
//
// 签名用知行图浏览器签名器(signOp),它已由 signer.test.mjs 对照 Rust 节点逐字节检验过;
// 钱包仓库那一侧的 tests/zhixing-op.test.mjs 再用它检验钱包自己的编码——两边合起来闭环。
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
require(join(here, "../public/bip39-en.js"));
const S = require(join(here, "../public/signer.js"));

/** 用助记词生成一把"成员密钥",返回 { pubkey, signKey }。 */
export async function memberKeyFromMnemonic(mnemonic) {
  const seed = await S.mnemonicToSeed(mnemonic);
  const k = await S.keyFromSeed(seed);
  seed.fill(0);
  return { pubkey: k.pubkey, signKey: k.signKey };
}

/**
 * 在页面里注入假钱包。
 * @param {import('puppeteer-core').Page} page
 * @param {{ pubkey: string, signKey: CryptoKey }} key
 * @param {{ mode?: 'native'|'nokey'|'old' }} [opts] nokey: 钱包里没有知行图密钥;old: 不认识 zhixing_*
 * @returns {Promise<Array<{ method: string, params: any }>>} 记录网页发给钱包的请求
 */
export async function injectFakeWallet(page, key, opts = {}) {
  const mode = opts.mode || "native";
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
    const info = Object.freeze({
      uuid: "11111111-2222-4333-8444-555555555555",
      name: "YeYing Wallet",
      icon: "data:image/svg+xml;base64,",
      rdns: "io.github.yeying",
    });
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
  return requests;
}
