"use strict";
// 知行图 op 编码与签名的**参考实现**(JavaScript)。不在网页里加载,也不部署到服务器:
// 成员的密钥只在夜莺钱包里,网页只把"意图"交给钱包签名(见 public/member.js)。
// 这份代码只给测试用:test/signer.test.mjs 用它对照 Rust 节点逐字节检验;test/fake-wallet.mjs 用它扮演钱包;
// 钱包仓库的 tests/zhixing-op.test.mjs 固定了它的输出作为向量。
//
// (下面的"本机保存"是早先网页自己保存密钥时的做法,已下线;保留只为测试旧数据的格式。)
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
    // 知行图成员密钥:SLIP-0010 ed25519,路径 m/44'/1105'/0'/0'/0'(与节点 keygen、夜莺钱包一致)
    return slip10Ed25519(new Uint8Array(bits), [44, 1105, 0, 0, 0]);
  }

  async function hmacSha512(key, data) {
    const k = await subtle.importKey("raw", key, { name: "HMAC", hash: "SHA-512" }, false, ["sign"]);
    return new Uint8Array(await subtle.sign("HMAC", k, data));
  }

  /** SLIP-0010 ed25519(各级硬化):返回路径末端的 32 字节种子。 */
  async function slip10Ed25519(seed, path) {
    let I = await hmacSha512(enc.encode("ed25519 seed"), seed);
    let key = I.slice(0, 32), chain = I.slice(32);
    for (const index of path) {
      const data = new Uint8Array(37);
      data.set(key, 1);
      new DataView(data.buffer).setUint32(33, (index | 0x80000000) >>> 0, false);
      I = await hmacSha512(chain, data);
      key = I.slice(0, 32); chain = I.slice(32);
    }
    return key;
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
  const LAYER = { foundation: 0, explanation: 1, scenario: 2, application: 3 };
  const CITE_KIND = { basis: 0, "see-also": 1, supersedes: 2 };
  const MAX_CITES = 32, MAX_BODIES = 16, MAX_LANG_LEN = 16, MAX_MODEL_ID_LEN = 64;

  function encodeVersionPayload(p) {
    const bodies = p.bodies || [];
    if (bodies.length > MAX_BODIES) throw new Error(`正文最多 ${MAX_BODIES} 个`);
    const parts = [u64(bodies.length)];
    for (const b of bodies) {
      if (!b.lang || enc.encode(b.lang).length > MAX_LANG_LEN) throw new Error(`语言标签需 1–${MAX_LANG_LEN} 字节`);
      parts.push(u32(enc.encode(b.lang).length), enc.encode(b.lang));
      parts.push(hexToBytes(b.hash));
    }
    // 来源:0 = 人工撰写;1 = 模型生成(带标识);2 = 自报的"人工核验过的模型生成"
    const src = p.source || { type: "human" };
    if (src.type === "human") parts.push(u8(0));
    else if (src.type === "model") {
      if (!src.id || enc.encode(src.id).length > MAX_MODEL_ID_LEN) throw new Error(`模型标识需 1–${MAX_MODEL_ID_LEN} 字节`);
      parts.push(u8(1), u32(enc.encode(src.id).length), enc.encode(src.id));
    } else if (src.type === "model-verified") {
      if (!src.id || enc.encode(src.id).length > MAX_MODEL_ID_LEN) throw new Error(`模型标识需 1–${MAX_MODEL_ID_LEN} 字节`);
      parts.push(u8(2), u32(enc.encode(src.id).length), enc.encode(src.id));
    } else throw new Error("来源只能是 human / model / model-verified");
    const cites = p.cites || [];
    if (cites.length > MAX_CITES) throw new Error(`引用最多 ${MAX_CITES} 条`);
    parts.push(u64(cites.length));
    for (const c of cites) {
      if (!(c.kind in CITE_KIND)) throw new Error("引用类型只能是 basis / see-also");
      parts.push(u64(c.unit), u32(c.version), u8(CITE_KIND[c.kind]));
    }
    return concat(...parts);
  }

  function encodeAction(a) {
    switch (a.type) {
      case "create":
        if (!(a.layer in LAYER)) throw new Error("层级只能是 foundation / explanation / scenario / application");
        return concat(u8(0), u8(LAYER[a.layer]), u32(a.domain), encodeVersionPayload(a));
      case "publish":
        return concat(u8(1), u64(a.unit), encodeVersionPayload(a));
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
        throw new Error("不认识的操作:" + a.type);
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

  // ---- 导入节点的加密密钥文件(Web3 Secret Storage v3,keystore.rs)------------------
  // scrypt(n=2^18, r=8, p=1)→ 32 字节;前 16 字节是 AES-128-CTR 的密钥,
  // 后 16 字节与密文一起做 Keccak-256 得到 MAC。浏览器没有内置 scrypt 与 Keccak,这里用纯 JS 实现,
  // 由 tools/explorer/test/signer.test.mjs 对照官方测试向量与 Rust 程序生成的密钥文件检验。

  const KECCAK_RC = [
    "0x0000000000000001", "0x0000000000008082", "0x800000000000808a", "0x8000000080008000",
    "0x000000000000808b", "0x0000000080000001", "0x8000000080008081", "0x8000000000008009",
    "0x000000000000008a", "0x0000000000000088", "0x0000000080008009", "0x000000008000000a",
    "0x000000008000808b", "0x800000000000008b", "0x8000000000008089", "0x8000000000008003",
    "0x8000000000008002", "0x8000000000000080", "0x000000000000800a", "0x800000008000000a",
    "0x8000000080008081", "0x8000000000008080", "0x0000000080000001", "0x8000000080008008",
  ].map(BigInt);
  // 旋转位数 ROT[x + 5y]
  const KECCAK_ROT = [0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8, 18, 2, 61, 56, 14];
  const M64 = (1n << 64n) - 1n;
  const rot64 = (v, n) => (n === 0 ? v : ((v << BigInt(n)) | (v >> BigInt(64 - n))) & M64);

  function keccakF(s) {
    for (let round = 0; round < 24; round++) {
      const C = [0, 1, 2, 3, 4].map((x) => s[x] ^ s[x + 5] ^ s[x + 10] ^ s[x + 15] ^ s[x + 20]);
      for (let x = 0; x < 5; x++) {
        const D = C[(x + 4) % 5] ^ rot64(C[(x + 1) % 5], 1);
        for (let y = 0; y < 25; y += 5) s[x + y] ^= D;
      }
      const B = new Array(25);
      for (let x = 0; x < 5; x++)
        for (let y = 0; y < 5; y++) B[y + 5 * ((2 * x + 3 * y) % 5)] = rot64(s[x + 5 * y], KECCAK_ROT[x + 5 * y]);
      for (let x = 0; x < 5; x++)
        for (let y = 0; y < 5; y++)
          s[x + 5 * y] = B[x + 5 * y] ^ ((M64 ^ B[(x + 1) % 5 + 5 * y]) & B[(x + 2) % 5 + 5 * y]);
      s[0] ^= KECCAK_RC[round];
    }
  }

  /** Keccak-256(以太坊 / Web3 密钥文件用的原始 Keccak,填充 0x01,不是 SHA3-256 的 0x06)。 */
  function keccak256(msg) {
    const rate = 136;
    const padded = new Uint8Array(Math.floor(msg.length / rate) * rate + rate);
    padded.set(msg);
    padded[msg.length] ^= 0x01;
    padded[padded.length - 1] ^= 0x80;
    const s = new Array(25).fill(0n);
    for (let off = 0; off < padded.length; off += rate) {
      for (let i = 0; i < rate / 8; i++) {
        let lane = 0n;
        for (let b = 7; b >= 0; b--) lane = (lane << 8n) | BigInt(padded[off + i * 8 + b]);
        s[i] ^= lane;
      }
      keccakF(s);
    }
    const out = new Uint8Array(32);
    for (let i = 0; i < 4; i++) for (let b = 0; b < 8; b++) out[i * 8 + b] = Number((s[i] >> BigInt(8 * b)) & 0xffn);
    return out;
  }

  function salsa20_8(B) {
    let x0 = B[0], x1 = B[1], x2 = B[2], x3 = B[3], x4 = B[4], x5 = B[5], x6 = B[6], x7 = B[7],
      x8 = B[8], x9 = B[9], x10 = B[10], x11 = B[11], x12 = B[12], x13 = B[13], x14 = B[14], x15 = B[15], t;
    for (let i = 0; i < 8; i += 2) {
      t = (x0 + x12) | 0; x4 ^= (t << 7) | (t >>> 25); t = (x4 + x0) | 0; x8 ^= (t << 9) | (t >>> 23);
      t = (x8 + x4) | 0; x12 ^= (t << 13) | (t >>> 19); t = (x12 + x8) | 0; x0 ^= (t << 18) | (t >>> 14);
      t = (x5 + x1) | 0; x9 ^= (t << 7) | (t >>> 25); t = (x9 + x5) | 0; x13 ^= (t << 9) | (t >>> 23);
      t = (x13 + x9) | 0; x1 ^= (t << 13) | (t >>> 19); t = (x1 + x13) | 0; x5 ^= (t << 18) | (t >>> 14);
      t = (x10 + x6) | 0; x14 ^= (t << 7) | (t >>> 25); t = (x14 + x10) | 0; x2 ^= (t << 9) | (t >>> 23);
      t = (x2 + x14) | 0; x6 ^= (t << 13) | (t >>> 19); t = (x6 + x2) | 0; x10 ^= (t << 18) | (t >>> 14);
      t = (x15 + x11) | 0; x3 ^= (t << 7) | (t >>> 25); t = (x3 + x15) | 0; x7 ^= (t << 9) | (t >>> 23);
      t = (x7 + x3) | 0; x11 ^= (t << 13) | (t >>> 19); t = (x11 + x7) | 0; x15 ^= (t << 18) | (t >>> 14);
      t = (x0 + x3) | 0; x1 ^= (t << 7) | (t >>> 25); t = (x1 + x0) | 0; x2 ^= (t << 9) | (t >>> 23);
      t = (x2 + x1) | 0; x3 ^= (t << 13) | (t >>> 19); t = (x3 + x2) | 0; x0 ^= (t << 18) | (t >>> 14);
      t = (x5 + x4) | 0; x6 ^= (t << 7) | (t >>> 25); t = (x6 + x5) | 0; x7 ^= (t << 9) | (t >>> 23);
      t = (x7 + x6) | 0; x4 ^= (t << 13) | (t >>> 19); t = (x4 + x7) | 0; x5 ^= (t << 18) | (t >>> 14);
      t = (x10 + x9) | 0; x11 ^= (t << 7) | (t >>> 25); t = (x11 + x10) | 0; x8 ^= (t << 9) | (t >>> 23);
      t = (x8 + x11) | 0; x9 ^= (t << 13) | (t >>> 19); t = (x9 + x8) | 0; x10 ^= (t << 18) | (t >>> 14);
      t = (x15 + x14) | 0; x12 ^= (t << 7) | (t >>> 25); t = (x12 + x15) | 0; x13 ^= (t << 9) | (t >>> 23);
      t = (x13 + x12) | 0; x14 ^= (t << 13) | (t >>> 19); t = (x14 + x13) | 0; x15 ^= (t << 18) | (t >>> 14);
    }
    B[0] += x0; B[1] += x1; B[2] += x2; B[3] += x3; B[4] += x4; B[5] += x5; B[6] += x6; B[7] += x7;
    B[8] += x8; B[9] += x9; B[10] += x10; B[11] += x11; B[12] += x12; B[13] += x13; B[14] += x14; B[15] += x15;
  }

  // XY 的前 w 个字是当前块 B,后 w 个字是暂存区 Y
  function blockMix(XY, r, x) {
    const w = 32 * r;
    x.set(XY.subarray((2 * r - 1) * 16, 2 * r * 16));
    for (let i = 0; i < 2 * r; i++) {
      for (let k = 0; k < 16; k++) x[k] ^= XY[i * 16 + k];
      salsa20_8(x);
      XY.set(x, w + ((i & 1) ? r + (i >> 1) : i >> 1) * 16); // 偶数块在前、奇数块在后
    }
    XY.copyWithin(0, w, 2 * w);
  }

  async function pbkdf2Sha256(pass, salt, len) {
    const k = await subtle.importKey("raw", pass, "PBKDF2", false, ["deriveBits"]);
    return new Uint8Array(await subtle.deriveBits({ name: "PBKDF2", hash: "SHA-256", salt, iterations: 1 }, k, len * 8));
  }

  /** scrypt(RFC 7914)。n=2^18、r=8 时需要约 256 MB 内存,桌面浏览器需几秒到十几秒。 */
  async function scrypt(pass, salt, N, r, p, dkLen) {
    const w = 32 * r;
    const B = await pbkdf2Sha256(pass, salt, p * 128 * r);
    const dv = new DataView(B.buffer);
    const XY = new Uint32Array(2 * w);
    const V = new Uint32Array(w * N);
    const x = new Uint32Array(16);
    for (let k = 0; k < p; k++) {
      const off = k * 128 * r;
      for (let i = 0; i < w; i++) XY[i] = dv.getUint32(off + i * 4, true);
      for (let i = 0; i < N; i++) { V.set(XY.subarray(0, w), i * w); blockMix(XY, r, x); }
      for (let i = 0; i < N; i++) {
        const vo = (XY[(2 * r - 1) * 16] & (N - 1)) * w;
        for (let t = 0; t < w; t++) XY[t] ^= V[vo + t];
        blockMix(XY, r, x);
      }
      for (let i = 0; i < w; i++) dv.setUint32(off + i * 4, XY[i], true);
    }
    return pbkdf2Sha256(pass, B, dkLen);
  }

  /** 解开 `node keygen --encrypt` / `node encrypt-key` 写的密钥文件,返回 32 字节种子。 */
  async function decryptKeystore(jsonText, password) {
    let ks;
    try { ks = JSON.parse(jsonText); } catch (_) { throw new Error("这不是一个密钥文件(无法解析 JSON)"); }
    const c = ks && (ks.crypto || ks.Crypto);
    if (!c || ks.version !== 3) throw new Error("不支持的密钥文件:只支持 version 3");
    if (c.kdf !== "scrypt" || c.cipher !== "aes-128-ctr") throw new Error("不支持的密钥文件:只支持 scrypt + aes-128-ctr");
    const { n, r, p, dklen, salt } = c.kdfparams || {};
    // 与节点同样的参数检查,并限制内存,防止恶意文件把浏览器拖垮
    if (dklen !== 32 || !Number.isInteger(n) || n < 2 || (n & (n - 1)) !== 0 || n > 2 ** 20 ||
        !Number.isInteger(r) || r < 1 || r > 16 || !Number.isInteger(p) || p < 1 || p > 4 || 128 * r * n > 512 * 2 ** 20)
      throw new Error("密钥文件的 scrypt 参数超出支持范围");
    const iv = hexToBytes(c.cipherparams.iv), ct = hexToBytes(c.ciphertext), mac = hexToBytes(c.mac);
    if (iv.length !== 16 || ct.length !== 32 || mac.length !== 32) throw new Error("密钥文件内容不完整");
    const dk = await scrypt(enc.encode(password), hexToBytes(salt), n, r, p, 32);
    const want = keccak256(concat(dk.slice(16, 32), ct));
    if (bytesToHex(want) !== bytesToHex(mac)) throw new Error("密钥文件的口令不对");
    const aes = await subtle.importKey("raw", dk.slice(0, 16), "AES-CTR", false, ["decrypt"]);
    return new Uint8Array(await subtle.decrypt({ name: "AES-CTR", counter: iv, length: 128 }, aes, ct));
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
    keccak256, scrypt, decryptKeystore,
  };
  root.ZXSigner = api;
  if (typeof module !== "undefined") module.exports = api;
})(globalThis);
