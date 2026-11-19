"use strict";
// 知行图知识互操作词表 v1:把链上的单元详情转成 JSON-LD(规范见 docs/design/ontology-v0.1.md)。
//
// 输入是节点 RPC `GET /unit/{id}` 的 JSON(含每个版本的发布人、来源、正文、引用、核验、可信度),
// 输出是一个自包含的 JSON-LD 节点;上下文在 public/ns/v1/context.jsonld。
// 纯函数、无依赖,导出、单元查询与测试共用。
const BASE = "https://blockchain.yeying.pub/id";
const NS = "https://blockchain.yeying.pub/ns/v1";
const CONTEXT_URL = NS + "/context.jsonld";
const LICENSE_URL = "https://creativecommons.org/licenses/by/4.0/";

const LAYER = { foundation: "kdg:Foundation", explanation: "kdg:Explanation", scenario: "kdg:Scenario", application: "kdg:Application" };
const LIFECYCLE = { active: "kdg:Active", dormant: "kdg:Dormant", superseded: "kdg:Superseded", contested: "kdg:Contested" };
const CITE_PROP = { basis: "basedOn", "see-also": "seeAlso", supersedes: "replaces" };

const iri = {
  unit: (id) => `${BASE}/unit/${id}`,
  version: (id, n) => `${BASE}/unit/${id}/v/${n}`,
  member: (id) => `${BASE}/member/${id}`,
  domain: (id) => `${BASE}/domain/${id}`,
  verification: (id, n, verifier) => `${BASE}/verification/${id}/${n}/${verifier}`,
  generation: (id, n) => `${BASE}/unit/${id}/v/${n}#generation`,
  model: (modelId) => `${BASE}/model/${encodeURIComponent(modelId)}`,
};

/** bps(万分比字符串)→ 0–1 的小数;保留 4 位,避免浮点噪声。 */
const bpsToDecimal = (bps) => Math.round(Number(bps || 0)) / 10000;

/** 链上的来源字段:"human" | "model:<id>" | "model-verified:<id>" */
function parseSource(source) {
  const s = String(source || "human");
  const i = s.indexOf(":");
  if (i < 0) return { type: s, model: null };
  return { type: s.slice(0, i), model: s.slice(i + 1) };
}

/**
 * 一个版本 → JSON-LD 节点。
 * @param {object} unit 单元详情(RPC /unit/{id})
 * @param {object} v 其中一个版本
 * @param {(hash:string)=>({text:string,title:string}|null)} [bodyText] 取正文全文(导出时带上)
 */
function versionNode(unit, v, bodyText) {
  const id = String(unit.id);
  const n = String(v.n);
  const src = parseSource(v.source);
  const node = {
    id: iri.version(id, n),
    type: "UnitVersion",
    isVersionOf: iri.unit(id),
    versionNumber: Number(n),
    publisher: iri.member(v.publisher),
    publishedAtHeight: Number(v.published_height),
    sourceType: src.type,
  };
  if (src.model) {
    // 模型生成:记下生成它的活动与模型(PROV-O);发布者自报,链上不验证模型身份
    node.generatedBy = {
      id: iri.generation(id, n),
      type: "Generation",
      associatedWith: { id: iri.model(src.model), type: "Model", name: src.model },
    };
  }
  node.bodies = (v.bodies || []).map((b) => {
    const body = {
      type: "Body",
      inLanguage: b.lang,
      sha256: b.hash,
      contentUrl: `https://blockchain.yeying.pub/bodies/${b.hash}`,
      encodingFormat: "text/markdown",
    };
    const t = bodyText ? bodyText(b.hash) : null;
    if (t) {
      if (t.title) body.title = t.title;
      body.text = t.text;
    }
    return body;
  });
  for (const c of v.cites || []) {
    const prop = CITE_PROP[c.kind];
    if (!prop) continue;
    (node[prop] = node[prop] || []).push(iri.version(c.unit, c.version));
  }
  node.verifications = (v.verifications || []).map((r) => {
    const out = {
      id: iri.verification(id, n, r.verifier),
      type: "Verification",
      verifier: iri.member(r.verifier),
      verified: iri.version(id, n),
      verdict: r.verdict,
      atHeight: Number(r.height),
    };
    if (r.note_hash && !/^0+$/.test(r.note_hash)) out.noteHash = r.note_hash;
    return out;
  });
  if (v.credibility) {
    node.selfCredibility = bpsToDecimal(v.credibility.self_bps);
    node.effectiveCredibility = bpsToDecimal(v.credibility.effective_bps);
    const b = v.credibility.bottleneck;
    if (b && b.unit != null) node.bottleneck = iri.version(b.unit, b.version);
  }
  return node;
}

/**
 * 单元详情 → 自包含的 JSON-LD 文档。
 * @param {object} unit RPC /unit/{id} 的结果(另可带 lifecycle,来自 /units 列表)
 * @param {{bodyText?:Function, withContext?:boolean}} [opts]
 */
function unitToJsonLd(unit, opts = {}) {
  const id = String(unit.id);
  const versions = (unit.versions || []).map((v) => versionNode(unit, v, opts.bodyText));
  const doc = {};
  if (opts.withContext !== false) doc["@context"] = CONTEXT_URL;
  Object.assign(doc, {
    id: iri.unit(id),
    type: "KnowledgeUnit",
    layer: LAYER[unit.layer] || unit.layer,
    domain: iri.domain(unit.domain),
    author: iri.member(unit.author),
    createdAtHeight: Number(unit.created_height),
    license: LICENSE_URL,
  });
  if (unit.lifecycle) doc.lifecycle = LIFECYCLE[unit.lifecycle] || unit.lifecycle;
  if (versions.length) doc.latestVersion = versions[versions.length - 1].id;
  doc.versions = versions;
  return doc;
}

/** 成员 → JSON-LD。 */
function memberToJsonLd(m, opts = {}) {
  const doc = {};
  if (opts.withContext !== false) doc["@context"] = CONTEXT_URL;
  Object.assign(doc, { id: iri.member(m.id), type: "Member", accountNumber: Number(m.id) });
  if (m.pubkey) doc.pubkey = m.pubkey;
  return doc;
}

module.exports = { BASE, NS, CONTEXT_URL, LICENSE_URL, iri, parseSource, bpsToDecimal, unitToJsonLd, memberToJsonLd };
