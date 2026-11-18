"use strict";
// 核验人随机指派(服务侧,确定性)。
//
// 设计稿 token-economy.md §6.2 要求"协议随机指派核验人,只有被指派者的结论计入"。真正的共识版需要
// 把指派结果写进链上状态(改 state_root,须按高度激活)。在那之前,这里用**确定性**算法计算指派:
//
//   种子 = SHA-256( 发布该版本那个区块的哈希 ‖ 单元号 ‖ 版本号 )
//   候选 = 发布时已过见习期的成员,排除发布者
//   按 SHA-256(种子 ‖ 成员号) 排序,取前 k 个
//
// 任何人用公开的链上数据都能算出同样的结果,所以它可验证、发布者事先无法预知(区块哈希在发布时
// 还不存在),但**不是共识强制的**:链目前仍接受所有成员的核验。浏览器据此提示"这一版指派给了谁",
// 并把被指派者的结论单独标出。等成员 ≥ 6 人、改为共识规则时,算法保持不变,只是从提示变成约束。
const crypto = require("crypto");
const http = require("http");

const DEFAULT_K = 4;

function sha256(buf) { return crypto.createHash("sha256").update(buf).digest(); }

function u64be(n) {
  const b = Buffer.alloc(8);
  b.writeBigUInt64BE(BigInt(n));
  return b;
}
function u32be(n) {
  const b = Buffer.alloc(4);
  b.writeUInt32BE(Number(n));
  return b;
}

/**
 * 纯函数:给定发布区块哈希、单元、版本、发布者与候选成员,算出指派结果。
 * 同样的输入永远得到同样的输出(这是可验证性的基础)。
 */
function assign({ blockHash, unit, version, publisher, members, k = DEFAULT_K }) {
  const seed = sha256(Buffer.concat([Buffer.from(blockHash, "hex"), u64be(unit), u32be(version)]));
  const candidates = members.filter((m) => String(m) !== String(publisher));
  const ranked = candidates
    .map((m) => ({ m: String(m), score: sha256(Buffer.concat([seed, u64be(m)])).toString("hex") }))
    .sort((a, b) => (a.score < b.score ? -1 : a.score > b.score ? 1 : 0));
  return {
    seed: seed.toString("hex"),
    assigned: ranked.slice(0, k).map((r) => r.m),
    candidates: candidates.map(String),
  };
}

class ReviewAssigner {
  constructor(rpcBase) { this.rpcBase = rpcBase; this.blockHashCache = new Map(); }

  rpcJson(p) {
    return new Promise((resolve, reject) => {
      const u = new URL(this.rpcBase + p);
      const req = http.request({ hostname: u.hostname, port: u.port || 80, path: u.pathname + u.search,
        headers: { Accept: "application/json" } }, (res) => {
        let b = "";
        res.on("data", (c) => (b += c));
        res.on("end", () => {
          if (res.statusCode >= 400) return reject(new Error(p + " → HTTP " + res.statusCode));
          try { resolve(JSON.parse(b)); } catch (e) { reject(e); }
        });
      });
      req.on("error", reject);
      req.end();
    });
  }

  async blockHash(height) {
    if (this.blockHashCache.has(height)) return this.blockHashCache.get(height);
    const b = await this.rpcJson("/block/" + height);
    const h = b.hash;
    this.blockHashCache.set(height, h);
    return h;
  }

  // 发布时已过见习期的成员(按发布高度判断,与可信度计算一致)
  async membersAt(height) {
    const ms = (await this.rpcJson("/members?limit=1000")).items || [];
    return ms.filter((m) => {
      if (String(m.inviter) === "0") return true; // 创世成员
      // 见习期以链参数为准;这里用成员目录给出的 established 作为当前状态的近似,
      // 并要求加入高度早于发布高度
      return Number(m.joined_height) < Number(height) && m.established;
    }).map((m) => m.id);
  }

  /** 某单元某版本的指派(以及哪些已有核验来自被指派者)。 */
  async forVersion(unitId, versionN, k = DEFAULT_K) {
    const u = await this.rpcJson("/unit/" + unitId);
    const v = u.versions.find((x) => String(x.n) === String(versionN)) || u.versions[u.versions.length - 1];
    const height = Number(v.published_height);
    const blockHash = await this.blockHash(height);
    const members = await this.membersAt(height);
    const r = assign({ blockHash, unit: unitId, version: v.n, publisher: v.publisher, members, k });
    const byAssigned = new Set(r.assigned);
    return {
      unit: String(unitId), version: String(v.n), published_height: String(height),
      block_hash: blockHash, seed: r.seed, k,
      assigned: r.assigned,
      verifications: (v.verifications || []).map((x) => ({
        verifier: String(x.verifier), verdict: x.verdict, height: String(x.height),
        assigned: byAssigned.has(String(x.verifier)),
      })),
      // 现阶段只是提示:链仍接受所有成员的核验
      enforced: false,
    };
  }

  /** 某成员被指派了哪些尚未核验的版本("待我核验")。 */
  async pendingFor(memberId, k = DEFAULT_K) {
    const out = [];
    let cursor = null;
    for (let i = 0; i < 20; i++) {
      const page = await this.rpcJson("/units?limit=200" + (cursor ? "&cursor=" + cursor : ""));
      for (const it of page.items || []) {
        if (it.lifecycle === "superseded") continue;
        try {
          const a = await this.forVersion(it.id, it.versions, k);
          const done = a.verifications.some((x) => x.verifier === String(memberId));
          if (a.assigned.includes(String(memberId)) && !done) out.push(a);
        } catch (_) { /* 跳过 */ }
      }
      cursor = page.cursor;
      if (!cursor || !(page.items || []).length) break;
    }
    return out;
  }
}

module.exports = { assign, ReviewAssigner, DEFAULT_K };
