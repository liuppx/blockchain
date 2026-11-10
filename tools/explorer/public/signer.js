"use strict";
// 知行图浏览器签名器:成员在自己的浏览器里持有密钥、签名知识 op,私钥从不离开浏览器。
//
// 与节点(node/src)逐字节一致:
//   - 助记词 → 种子:BIP-39,PBKDF2-HMAC-SHA512(助记词, "mnemonic"+口令, 2048),取前 32 字节(crypto.rs)
//   - 密钥:ed25519,种子即私钥(Keypair::from_seed)
//   - 签名:纯 ed25519,签在 "zhixing-knowledge-op-v1" ‖ op 编码(不含签名)上(codec.rs)
//   - op 编码:大端整数,见 encodeOp
// 一致性由 tools/explorer/test/signer.test.mjs 对照 Rust 程序的输出逐字节检验。
//
// 本机保存:种子用口令加密(PBKDF2-SHA256 60 万次 → AES-256-GCM)后存入 localStorage,
// 只有本机、只有知道口令的人能解开。不依赖任何第三方库,只用浏览器内置的 WebCrypto。
(function (root) {
  const subtle = root.crypto.subtle;
  const enc = new TextEncoder();
  const SIGN_DOMAIN = enc.encode("zhixing-knowledge-op-v1");
  const PKCS8_ED25519_PREFIX = hexToBytes("302e020100300506032b657004220420");
  const VAULT_KEY = "zhixing.vault.v1";
  const TOKEN_KEY = "zhixing.writeToken";
  const KDF_ITERS = 600000;

  function hexToBytes(h) {
    if (!/^([0-9a-f]{2})*$/i.test(h)) throw new Error("bad hex");
    const out = new Uint8Array(h.length / 2);
    for (let i = 0; i < out.length; i++) out[i] = parseInt(h.substr(i * 2, 2), 16);
    return out;
  }
  const bytesToHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  const b64 = (b) => btoa(String.fromCharCode(...b));
  const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
  function concat(...parts) {
    const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
    let o = 0;
    for (const p of parts) { out.set(p, o); o += p.length; }
    return out;
  }
  function u8(v) { return Uint8Array.of(v); }
  function u32(v) { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, v); return b; }
  function u64(v) { const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(v)); return b; }

  // ---- BIP-39 ---------------------------------------------------------------
  const words = () => root.BIP39_EN;

  async function sha256(b) { return new Uint8Array(await subtle.digest("SHA-256", b)); }

  /** 32 字节随机熵 → 24 个词(与 `node keygen --new-mnemonic` 相同的词数)。 */
  async function generateMnemonic() {
    const ent = root.crypto.getRandomValues(new Uint8Array(32));
    return entropyToMnemonic(ent);
  }

  async function entropyToMnemonic(ent) {
    const cs = (await sha256(ent))[0]; // 32 字节熵的校验和 = SHA-256 首字节(8 位)
    let bits = "";
    for (const b of ent) bits += b.toString(2).padStart(8, "0");
    bits += cs.toString(2).padStart(8, "0").slice(0, ent.length / 4);
    const out = [];
    for (let i = 0; i < bits.length; i += 11) out.push(words()[parseInt(bits.slice(i, i + 11), 2)]);
    return out.join(" ");
  }

  /** 校验词数、词表与校验和;返回规范化(单空格、小写)的助记词。 */
  async function validateMnemonic(m) {
    const parts = m.trim().toLowerCase().split(/\s+/);
    if (![12, 15, 18, 21, 24].includes(parts.length)) throw new Error(`助记词应为 12/15/18/21/24 个词,收到 ${parts.length} 个`);
    let bits = "";
    for (const w of parts) {
      const i = words().indexOf(w);
      if (i < 0) throw new Error(`不认识的词:${w}`);
      bits += i.toString(2).padStart(11, "0");
    }
    const csBits = parts.length / 3;
    const entBits = bits.length - csBits;
    const ent = new Uint8Array(entBits / 8);
    for (let i = 0; i < ent.length; i++) ent[i] = parseInt(bits.slice(i * 8, i * 8 + 8), 2);
    const want = (await sha256(ent))[0].toString(2).padStart(8, "0").slice(0, csBits);
    if (want !== bits.slice(entBits)) throw new Error("助记词校验失败:可能有词抄错或顺序不对");
    return parts.join(" ");
  }

  async function mnemonicToSeed(m, passphrase = "") {
    const norm = await validateMnemonic(m);
    if (!/^[\x00-\x7f]*$/.test(passphrase)) throw new Error("口令只支持 ASCII(与节点一致)");
    const key = await subtle.importKey("raw", enc.encode(norm), "PBKDF2", false, ["deriveBits"]);
    const bits = await subtle.deriveBits(
      { name: "PBKDF2", hash: "SHA-512", salt: enc.encode("mnemonic" + passphrase), iterations: 2048 }, key, 512);
    return new Uint8Array(bits).slice(0, 32);
  }

  // ---- ed25519 ----------------------------------------------------------------
  async function keyFromSeed(seed) {
    if (seed.length !== 32) throw new Error("种子应为 32 字节");
    const priv = await subtle.importKey("pkcs8", concat(PKCS8_ED25519_PREFIX, seed), { name: "Ed25519" }, true, ["sign"]);
    const jwk = await subtle.exportKey("jwk", priv);
    const pub = Uint8Array.from(atob(jwk.x.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
    // 只留下不可导出的签名密钥
    const signKey = await subtle.importKey("pkcs8", concat(PKCS8_ED25519_PREFIX, seed), { name: "Ed25519" }, false, ["sign"]);
    return { signKey, pubkey: bytesToHex(pub) };
  }

  // ---- 知识 op 编码(codec.rs enc_knowledge_op)----------------------------------
  const VERDICT = { pass: 0, "needs-revision": 1 };

  function encodeAction(a) {
    switch (a.type) {
      case "verify": {
        if (!(a.verdict in VERDICT)) throw new Error("verdict 只能是 pass / needs-revision");
        const note = a.noteHash ? hexToBytes(a.noteHash) : new Uint8Array(32);
        if (note.length !== 32) throw new Error("noteHash 应为 32 字节");
        return concat(u8(2), u64(a.unit), u32(a.version), u8(VERDICT[a.verdict]), note);
      }
      case "invite": {
        const pk = hexToBytes(a.pubkey);
        if (pk.length !== 32) throw new Error("公钥应为 64 位十六进制");
        return concat(u8(3), pk);
      }
      default:
        throw new Error("浏览器签名器目前只支持 verify 与 invite");
    }
  }

  function encodeOp(actor, action, nonce) {
    return concat(u64(actor), encodeAction(action), u64(nonce));
  }

  /** 签名并返回完整编码(可直接 POST /knowledge)。 */
  async function signOp(signKey, actor, action, nonce) {
    const body = encodeOp(actor, action, nonce);
    const sig = new Uint8Array(await subtle.sign({ name: "Ed25519" }, signKey, concat(SIGN_DOMAIN, body)));
    return concat(body, sig);
  }

  // ---- 本机加密保存 ------------------------------------------------------------
  async function wrapKey(password, salt) {
    const k = await subtle.importKey("raw", enc.encode(password), "PBKDF2", false, ["deriveKey"]);
    return subtle.deriveKey({ name: "PBKDF2", hash: "SHA-256", salt, iterations: KDF_ITERS },
      k, { name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
  }

  async function sealSeed(seed, password, pubkey) {
    if (!password || password.length < 8) throw new Error("口令至少 8 个字符");
    const salt = root.crypto.getRandomValues(new Uint8Array(16));
    const iv = root.crypto.getRandomValues(new Uint8Array(12));
    const ct = new Uint8Array(await subtle.encrypt({ name: "AES-GCM", iv }, await wrapKey(password, salt), seed));
    return { v: 1, kdf: "pbkdf2-sha256", iters: KDF_ITERS, salt: b64(salt), iv: b64(iv), ct: b64(ct), pubkey };
  }

  async function openSeed(vault, password) {
    try {
      const pt = await subtle.decrypt({ name: "AES-GCM", iv: unb64(vault.iv) },
        await wrapKey(password, unb64(vault.salt)), unb64(vault.ct));
      return new Uint8Array(pt);
    } catch (_) {
      throw new Error("口令不对");
    }
  }

  const store = {
    loadVault() { try { return JSON.parse(root.localStorage.getItem(VAULT_KEY) || "null"); } catch (_) { return null; } },
    saveVault(v) { root.localStorage.setItem(VAULT_KEY, JSON.stringify(v)); },
    forget() { root.localStorage.removeItem(VAULT_KEY); },
    token() { return root.localStorage.getItem(TOKEN_KEY) || ""; },
    setToken(t) { t ? root.localStorage.setItem(TOKEN_KEY, t.trim()) : root.localStorage.removeItem(TOKEN_KEY); },
  };

  const api = {
    generateMnemonic, entropyToMnemonic, validateMnemonic, mnemonicToSeed, keyFromSeed,
    encodeOp, signOp, sealSeed, openSeed, store, bytesToHex, hexToBytes,
  };
  root.ZXSigner = api;
  if (typeof module !== "undefined") module.exports = api;
})(globalThis);
