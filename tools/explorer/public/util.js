"use strict";
// 网页用的小工具:十六进制转换、写入令牌、提交前的格式检查、旧版本机密钥的清理。
//
// 这里**没有任何私钥代码**:不生成助记词、不派生密钥、不签名、不加解密。成员的密钥只在夜莺钱包里,
// 网页只把"意图"交给钱包签名(见 member.js)。op 编码与签名的参考实现在 tools/explorer/reference/signer.js,
// 只供测试对照 Rust 节点与钱包,不在网页里加载;test/wallet.test.mjs 会检查 public/ 里没有私钥代码。
(function (root) {
  const TOKEN_KEY = "zhixing.writeToken";
  const LEGACY_VAULT_KEY = "zhixing.vault.v1"; // 旧版网页自己保存的加密密钥(已下线)
  const enc = new TextEncoder();

  function hexToBytes(h) {
    if (!/^([0-9a-f]{2})*$/i.test(h)) throw new Error("bad hex");
    const out = new Uint8Array(h.length / 2);
    for (let i = 0; i < out.length; i++) out[i] = parseInt(h.substr(i * 2, 2), 16);
    return out;
  }

  // ---- 提交前的格式检查(与节点 codec.rs 的上限一致;钱包在弹窗前还会再校验一遍)----------------
  const LAYERS = ["foundation", "explanation", "scenario", "application"];
  const CITE_KINDS = ["basis", "see-also", "supersedes"];
  const VERDICTS = ["pass", "needs-revision"];
  const MAX_CITES = 32, MAX_BODIES = 16, MAX_LANG_LEN = 16, MAX_MODEL_ID_LEN = 64;
  const isHex32 = (s) => typeof s === "string" && /^[0-9a-f]{64}$/i.test(s);
  const isUint = (n, max) => Number.isInteger(n) && n >= 0 && n <= max;

  function checkVersionPayload(a) {
    const bodies = a.bodies || [];
    if (bodies.length > MAX_BODIES) throw new Error(`正文最多 ${MAX_BODIES} 个`);
    for (const b of bodies) {
      if (!b.lang || enc.encode(b.lang).length > MAX_LANG_LEN) throw new Error(`语言标签需 1–${MAX_LANG_LEN} 字节`);
      if (!isHex32(b.hash)) throw new Error("正文哈希应为 64 位十六进制");
    }
    const src = a.source || { type: "human" };
    if (src.type === "model" || src.type === "model-verified") {
      if (!src.id || enc.encode(src.id).length > MAX_MODEL_ID_LEN) throw new Error(`模型标识需 1–${MAX_MODEL_ID_LEN} 字节`);
    } else if (src.type !== "human") throw new Error("来源只能是 human / model / model-verified");
    const cites = a.cites || [];
    if (cites.length > MAX_CITES) throw new Error(`引用最多 ${MAX_CITES} 条`);
    for (const c of cites) {
      if (!CITE_KINDS.includes(c.kind)) throw new Error("引用类型只能是 basis / see-also / supersedes");
      if (!isUint(Number(c.unit), Number.MAX_SAFE_INTEGER) || !isUint(Number(c.version), 0xffffffff)) throw new Error("引用的单元或版本号不对");
    }
  }

  /** 检查一个知识操作的格式;不对就抛出可读的错误。 */
  function checkAction(a) {
    switch (a && a.type) {
      case "create":
        if (!LAYERS.includes(a.layer)) throw new Error("层级只能是 foundation / explanation / scenario / application");
        if (!isUint(a.domain, 0xffffffff)) throw new Error("领域编号不对");
        return checkVersionPayload(a);
      case "publish":
        if (!isUint(a.unit, Number.MAX_SAFE_INTEGER)) throw new Error("单元编号不对");
        return checkVersionPayload(a);
      case "verify":
        if (!isUint(a.unit, Number.MAX_SAFE_INTEGER) || !isUint(a.version, 0xffffffff)) throw new Error("单元或版本号不对");
        if (!VERDICTS.includes(a.verdict)) throw new Error("verdict 只能是 pass / needs-revision");
        if (a.noteHash && !isHex32(a.noteHash)) throw new Error("noteHash 应为 32 字节");
        return;
      case "invite":
        if (!isHex32(a.pubkey)) throw new Error("公钥应为 64 位十六进制");
        return;
      default:
        throw new Error("不认识的操作:" + (a && a.type));
    }
  }

  const api = {
    hexToBytes,
    checkAction,
    token() { return root.localStorage.getItem(TOKEN_KEY) || ""; },
    setToken(t) { t ? root.localStorage.setItem(TOKEN_KEY, t.trim()) : root.localStorage.removeItem(TOKEN_KEY); },
    // 只看旧密钥在不在、以及删掉它;不读取、不解密内容
    hasLegacyKey() { return root.localStorage.getItem(LEGACY_VAULT_KEY) !== null; },
    removeLegacyKey() { root.localStorage.removeItem(LEGACY_VAULT_KEY); },
  };
  root.ZXUtil = api;
  if (typeof module !== "undefined") module.exports = api;
})(globalThis);
