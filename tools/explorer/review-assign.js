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
// 并把被指派者的结论单独标出。
//
// **知识经济 v1 激活以后**(链上 `/economy` 的 active=true),指派改由**协议**完成、写在链上状态里,
// 只有被指派者的结论计入确立与报酬。这时一律以链上指派为准(`source: "chain"`,`enforced: true`);
// 激活之前发布的版本链上没有指派记录,仍用这里的确定性算法提示(`source: "hint"`)。
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
          if (res.statusCode >= 400) {
            const err = new Error(p + " → HTTP " + res.statusCode);
            err.status = res.statusCode;
            return reject(err);
          }
          try { resolve(JSON.parse(b)); } catch (e) { reject(e); }
        });
      });
      req.on("error", reject);
      req.end();
    });
  }

  /**
   * 链上的知识经济状态(激活后才有指派)。
   *
   * **不做跨请求的时间缓存**:曾经缓存 5 秒,结果激活、确立刚发生的那几秒里,
   * 浏览器会按旧状态把"已确立"的版本仍列为待核验、把链上指派当成提示。
   * 一次 `pendingFor` 会对很多版本调 `forVersion`,所以只在**同一次**查询内复用(见 `withEconomy`)。
   */
  async economy() {
    if (this._scoped) return this._scoped;
    try {
      return await this.rpcJson("/economy");
    } catch (e) {
      // 只有"节点不认识这个路径"才算未激活:旧版本节点对未知路径一律回 200 "ok"
      // (解析 JSON 时报 SyntaxError),或者回 404。其他错误(网络不通、节点 5xx)**不能**当成未激活——
      // 否则节点一宕机,浏览器就悄悄退回"提示"模式,把不计入的版本当任务列给成员。
      if (e instanceof SyntaxError || e.status === 404) return { active: false, versions: [] };
      throw new Error("读不到链上的经济状态:" + e.message);
    }
  }

  /** 在一次查询内固定经济状态:所有版本看到的是同一份链上状态。 */
  async withEconomy(fn) {
    const outer = this._scoped;
    if (!outer) this._scoped = await this.economy();
    try {
      return await fn();
    } finally {
      if (!outer) this._scoped = null;
    }
  }

  /** 链上记录的指派(没有则返回 null)。 */
  async chainAssignment(unitId, versionN) {
    const e = await this.economy();
    if (!e || !e.active) return null;
    const hit = (e.versions || []).find(
      (x) => String(x.unit) === String(unitId) && String(x.version) === String(versionN));
    if (!hit) return null;
    return {
      assigned: (hit.assigned || []).map(String),
      established_height: hit.established_height == null ? null : String(hit.established_height),
      approvers: (hit.approvers || []).map(String),
    };
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
    const verifs = (v.verifications || []);
    // 先看链上:激活后发布的版本,指派以链为准
    const onChain = await this.chainAssignment(unitId, v.n);
    if (onChain) {
      const byAssigned = new Set(onChain.assigned);
      return {
        unit: String(unitId), version: String(v.n), published_height: String(height),
        source: "chain", enforced: true,
        assigned: onChain.assigned,
        established_height: onChain.established_height,
        approvers: onChain.approvers,
        verifications: verifs.map((x) => ({
          verifier: String(x.verifier), verdict: x.verdict, height: String(x.height),
          assigned: byAssigned.has(String(x.verifier)),
        })),
      };
    }
    // 链上没有(未激活,或激活前发布的版本):用确定性算法给出提示
    const blockHash = await this.blockHash(height);
    const members = await this.membersAt(height);
    const r = assign({ blockHash, unit: unitId, version: v.n, publisher: v.publisher, members, k });
    const byAssigned = new Set(r.assigned);
    return {
      unit: String(unitId), version: String(v.n), published_height: String(height),
      source: "hint", enforced: false,
      block_hash: blockHash, seed: r.seed, k,
      assigned: r.assigned,
      verifications: verifs.map((x) => ({
        verifier: String(x.verifier), verdict: x.verdict, height: String(x.height),
        assigned: byAssigned.has(String(x.verifier)),
      })),
    };
  }

  /**
   * 某成员被指派了哪些尚未核验的版本("待我核验")。
   *
   * 知识经济激活以后,只列**链上指派**的版本:激活前发布的版本只有浏览器提示,
   * 核验它们既不计入确立也不得分,列成"任务"会误导成员。激活之前,列提示(那时全都是提示)。
   */
  async pendingFor(memberId, k = DEFAULT_K) {
    return this.withEconomy(() => this._pendingFor(memberId, k));
  }

  async _pendingFor(memberId, k) {
    const econ = await this.economy();
    const active = !!(econ && econ.active);
    const out = [];
    let cursor = null;
    for (let i = 0; i < 20; i++) {
      const page = await this.rpcJson("/units?limit=200" + (cursor ? "&cursor=" + cursor : ""));
      for (const it of page.items || []) {
        if (it.lifecycle === "superseded") continue;
        try {
          const a = await this.forVersion(it.id, it.versions, k);
          const done = a.verifications.some((x) => x.verifier === String(memberId));
          // 已确立的版本不再需要核验(链上的确立不可逆;再核验也不计入)
          const settled = a.source === "chain" && a.established_height != null;
          // 激活后只列链上指派的版本(提示类不计入、不得分)
          const counts = !active || a.source === "chain";
          if (a.assigned.includes(String(memberId)) && !done && !settled && counts) out.push(a);
        } catch (_) { /* 跳过 */ }
      }
      cursor = page.cursor;
      if (!cursor || !(page.items || []).length) break;
    }
    return out;
  }
}

module.exports = { assign, ReviewAssigner, DEFAULT_K };
