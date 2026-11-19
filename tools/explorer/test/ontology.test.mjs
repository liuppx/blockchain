// 知识互操作词表 v1(JSON-LD)与浏览器服务的导出 / 查询。零依赖,不需要真实的链:
//
//   node --test tools/explorer/test/ontology.test.mjs
//
// 覆盖:
//   - ontology.js:链上单元详情 → JSON-LD(标识、来源与模型、引用、核验、可信度、正文语言);
//   - 输出里用到的每个词都在 public/ns/v1/context.jsonld 里定义(否则 JSON-LD 处理器会静默丢弃);
//   - 层级 / 生命周期 / 领域概念方案覆盖链上的全部取值;
//   - KnowledgeIndex(对着一个假节点):按正文哈希查单元与版本、按公钥查成员、旧格式导出保持兼容且
//     正文语言取自链上、JSON-LD 导出、/gaps 按领域筛选不再抛错。
// 用标准处理器(pyld + rdflib)的完整校验步骤见 docs/design/ontology-v0.1.md §7。
import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { createHash } from "node:crypto";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const O = require(join(here, "../ontology.js"));
const { KnowledgeIndex } = require(join(here, "../knowledge-index.js"));
const NS = join(here, "../public/ns/v1");
const readJson = (f) => JSON.parse(readFileSync(join(NS, f), "utf8"));
const CONTEXT = readJson("context.jsonld")["@context"];

const sha = (t) => createHash("sha256").update(Buffer.from(t, "utf8")).digest("hex");
const BODY_A = "# 渐进超负荷\n\n身体只会对超过它已经适应的负荷产生适应。\n";
const BODY_B = "# Progressive overload\n\nThe body adapts only to loads beyond what it is used to.\n";
const BODY_C = "# 深蹲\n\n依据渐进超负荷。\n";
const ZERO = "0".repeat(64);

// 与节点 RPC /unit/{id} 同形的单元详情
const UNIT_1 = {
  id: "1", layer: "foundation", domain: "2", author: "2", created_height: "441", license: "CC-BY-4.0",
  versions: [
    { n: "1", publisher: "2", published_height: "441", source: "model:claude-opus-5-5",
      bodies: [{ lang: "zh", hash: sha(BODY_A) }], cites: [], verifications: [],
      credibility: { passes: "0", needs: "0", self_bps: "1000", effective_bps: "1000", bottleneck: null, stale_cites: [] } },
    { n: "2", publisher: "3", published_height: "500", source: "model-verified:claude-opus-5-5",
      bodies: [{ lang: "zh", hash: sha(BODY_A) }, { lang: "en", hash: sha(BODY_B) }],
      cites: [{ unit: "9", version: "1", kind: "see-also" }],
      verifications: [{ verifier: "1", verdict: "pass", height: "85528", note_hash: ZERO },
        { verifier: "4", verdict: "needs-revision", height: "86000", note_hash: "ab".repeat(32) }],
      credibility: { passes: "1", needs: "1", self_bps: "4000", effective_bps: "4000", bottleneck: null, stale_cites: [] } },
  ],
};
const UNIT_2 = {
  id: "2", layer: "application", domain: "2", author: "3", created_height: "600", license: "CC-BY-4.0",
  versions: [
    { n: "1", publisher: "3", published_height: "600", source: "human",
      bodies: [{ lang: "zh", hash: sha(BODY_C) }],
      cites: [{ unit: "1", version: "2", kind: "basis" }, { unit: "7", version: "1", kind: "supersedes" }],
      verifications: [],
      credibility: { passes: "0", needs: "0", self_bps: "1000", effective_bps: "1000",
        bottleneck: { unit: "1", version: "2" }, stale_cites: [] } },
  ],
};

test("单元详情 → JSON-LD:标识、层级、领域、作者、许可、生命周期", () => {
  const d = O.unitToJsonLd({ ...UNIT_1, lifecycle: "active" });
  assert.equal(d["@context"], "https://blockchain.yeying.pub/ns/v1/context.jsonld");
  assert.equal(d.id, "https://blockchain.yeying.pub/id/unit/1");
  assert.equal(d.type, "KnowledgeUnit");
  assert.equal(d.layer, "kdg:Foundation");
  assert.equal(d.domain, "https://blockchain.yeying.pub/id/domain/2");
  assert.equal(d.author, "https://blockchain.yeying.pub/id/member/2");
  assert.equal(d.license, "https://creativecommons.org/licenses/by/4.0/");
  assert.equal(d.lifecycle, "kdg:Active");
  assert.equal(d.latestVersion, "https://blockchain.yeying.pub/id/unit/1/v/2");
  assert.equal(d.versions.length, 2);
  assert.equal(d.createdAtHeight, 441);
});

test("版本:来源与模型(PROV-O)、多语言正文、核验、可信度", () => {
  const [v1, v2] = O.unitToJsonLd(UNIT_1).versions;
  assert.equal(v1.sourceType, "model");
  assert.equal(v1.generatedBy.associatedWith.name, "claude-opus-5-5");
  assert.equal(v1.generatedBy.associatedWith.type, "Model");
  assert.equal(v2.sourceType, "model-verified");
  assert.equal(v2.publisher, "https://blockchain.yeying.pub/id/member/3");
  assert.deepEqual(v2.bodies.map((b) => b.inLanguage), ["zh", "en"]);
  assert.equal(v2.bodies[1].sha256, sha(BODY_B));
  assert.equal(v2.bodies[1].contentUrl, `https://blockchain.yeying.pub/bodies/${sha(BODY_B)}`);
  assert.equal(v2.verifications.length, 2);
  assert.equal(v2.verifications[0].verdict, "pass");
  assert.equal(v2.verifications[0].verified, "https://blockchain.yeying.pub/id/unit/1/v/2");
  assert.equal(v2.verifications[0].noteHash, undefined, "全零的说明哈希表示没有说明");
  assert.equal(v2.verifications[1].noteHash, "ab".repeat(32));
  assert.equal(v2.selfCredibility, 0.4);
  assert.deepEqual(v2.seeAlso, ["https://blockchain.yeying.pub/id/unit/9/v/1"]);
  const human = O.unitToJsonLd(UNIT_2).versions[0];
  assert.equal(human.sourceType, "human");
  assert.equal(human.generatedBy, undefined, "人工撰写没有生成活动");
});

test("引用:依据 → basedOn、参见 → seeAlso、取代 → replaces,都指向被引单元的具体版本;可信度瓶颈", () => {
  const v = O.unitToJsonLd(UNIT_2).versions[0];
  assert.deepEqual(v.basedOn, ["https://blockchain.yeying.pub/id/unit/1/v/2"]);
  assert.deepEqual(v.replaces, ["https://blockchain.yeying.pub/id/unit/7/v/1"]);
  assert.equal(v.bottleneck, "https://blockchain.yeying.pub/id/unit/1/v/2");
});

function usedKeys(obj, out = new Set()) {
  if (Array.isArray(obj)) obj.forEach((x) => usedKeys(x, out));
  else if (obj && typeof obj === "object") {
    for (const [k, v] of Object.entries(obj)) { if (!k.startsWith("@")) out.add(k); usedKeys(v, out); }
  }
  return out;
}

test("输出用到的每个词、每个取值都在上下文里定义(否则会被 JSON-LD 处理器静默丢弃)", () => {
  const docs = [O.unitToJsonLd({ ...UNIT_1, lifecycle: "contested" }), O.unitToJsonLd(UNIT_2), O.memberToJsonLd({ id: "2", pubkey: "ab".repeat(32) })];
  for (const d of docs) {
    for (const k of usedKeys(d)) assert.ok(k in CONTEXT || k === "id" || k === "type", `未定义的词:${k}`);
  }
  // @vocab 类型的取值也要是已定义的词
  for (const v of ["human", "model", "model-verified", "pass", "needs-revision"]) assert.ok(v in CONTEXT, v);
  // 前缀都指向绝对 IRI
  for (const p of ["kdg", "schema", "prov", "dcterms", "skos", "rdf", "rdfs", "owl", "xsd"]) {
    assert.match(CONTEXT[p], /^https?:\/\//, p);
  }
});

test("概念方案覆盖链上的全部取值;领域名称只在 domains.jsonld 定义", () => {
  const layers = readJson("layers.jsonld").concepts.map((c) => c.notation).sort();
  assert.deepEqual(layers, ["application", "explanation", "foundation", "scenario"]);
  const lifecycle = readJson("lifecycle.jsonld").concepts.map((c) => c.notation).sort();
  assert.deepEqual(lifecycle, ["active", "contested", "dormant", "superseded"]);
  const domains = readJson("domains.jsonld");
  const notations = domains.concepts.map((c) => c.notation);
  assert.deepEqual(notations, [...new Set(notations)], "领域编号不重复");
  for (const c of domains.concepts) {
    assert.equal(c.id, `https://blockchain.yeying.pub/id/domain/${c.notation}`);
    assert.ok(c.prefLabel.zh && c.prefLabel.en, `领域 ${c.notation} 应有中英文名`);
  }
  // 网页不再硬编码领域名称
  const app = readFileSync(join(here, "../public/app.js"), "utf8");
  assert.doesNotMatch(app, /大模型应用开发|运动健身/);
  assert.match(app, /\/ns\/v1\/domains\.jsonld/);
});

// ---- KnowledgeIndex:对着一个假节点 ------------------------------------------------------------
let fake, bodyDir, index;
const ACCOUNTS = { "1": "11".repeat(32), "2": "22".repeat(32), "3": "33".repeat(32) };

before(async () => {
  const list = [
    { id: "1", layer: "foundation", domain: "2", author: "2", created_height: "441", versions: "2",
      credibility: UNIT_1.versions[1].credibility, body_hashes: [sha(BODY_A), sha(BODY_B)], cites: UNIT_1.versions[1].cites, lifecycle: "active" },
    { id: "2", layer: "application", domain: "1", author: "3", created_height: "600", versions: "1",
      credibility: UNIT_2.versions[0].credibility, body_hashes: [sha(BODY_C)], cites: UNIT_2.versions[0].cites, lifecycle: "dormant" },
  ];
  const routes = {
    "/units": { total: "2", cursor: null, items: list },
    "/unit/1": UNIT_1,
    "/unit/2": UNIT_2,
    "/members": { total: "3", items: Object.keys(ACCOUNTS).map((id) => ({ id, established: id !== "3" })) },
  };
  fake = http.createServer((req, res) => {
    const p = new URL(req.url, "http://x").pathname;
    let body = routes[p];
    const acct = /^\/account\/(\d+)$/.exec(p);
    if (acct) body = { id: acct[1], pubkey: ACCOUNTS[acct[1]], nonce: "0" };
    if (!body) return res.writeHead(404).end("{}");
    res.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify(body));
  });
  await new Promise((ok) => fake.listen(0, "127.0.0.1", ok));
  bodyDir = mkdtempSync(join(tmpdir(), "kdg-bodies-"));
  for (const b of [BODY_A, BODY_B, BODY_C]) writeFileSync(join(bodyDir, sha(b)), b);
  index = new KnowledgeIndex(bodyDir, `http://127.0.0.1:${fake.address().port}`);
  await index.build();
});
after(() => { fake && fake.close(); bodyDir && rmSync(bodyDir, { recursive: true, force: true }); });

test("按正文哈希查单元与版本;同一份正文被多个版本复用时全部列出", () => {
  assert.deepEqual(index.lookupBody(sha(BODY_A)), { unit: "1", version: "1", versions: ["1", "2"], lang: "zh" });
  assert.deepEqual(index.lookupBody(sha(BODY_B)), { unit: "1", version: "2", versions: ["2"], lang: "en" });
  assert.equal(index.lookupBody(sha(BODY_C).toUpperCase()).unit, "2", "大小写不敏感");
  assert.equal(index.lookupBody("ff".repeat(32)), null);
});

test("按公钥查成员", () => {
  assert.deepEqual(index.lookupPubkey("22".repeat(32)), { id: "2", pubkey: "22".repeat(32), established: true });
  assert.equal(index.lookupPubkey("33".repeat(32)).established, false, "见习期中的成员");
  assert.equal(index.lookupPubkey("99".repeat(32)), null);
});

test("旧格式导出保持兼容;正文语言取自链上;新增每个版本的来源与核验", async () => {
  const out = await index.exportUnits();
  const u1 = out.find((u) => u.id === "1");
  for (const k of ["id", "layer", "domain", "author", "created_height", "versions", "credibility", "cites", "license", "bodies"]) {
    assert.ok(k in u1, `旧字段 ${k} 仍在`);
  }
  assert.deepEqual(u1.bodies.map((b) => b.lang), ["zh", "en"], "英文正文不再被标成 zh");
  assert.equal(u1.bodies[0].text, BODY_A);
  assert.equal(u1.version_history.length, 2);
  assert.equal(u1.version_history[1].source, "model-verified:claude-opus-5-5");
  assert.equal(u1.version_history[1].verifications.length, 2);
});

test("JSON-LD 导出:每个单元都有,带正文全文与生命周期", async () => {
  const docs = await index.exportJsonLd();
  assert.equal(docs.length, 2);
  const u2 = docs.find((d) => d.id.endsWith("/unit/2"));
  assert.equal(u2.lifecycle, "kdg:Dormant");
  assert.equal(u2.versions[0].bodies[0].text, BODY_C);
  assert.equal(u2.versions[0].bodies[0].title, "深蹲");
  assert.equal(index.unitJsonLd(1).versions[0].bodies[0].text, undefined, "单个单元默认不带正文全文");
  assert.equal(index.unitJsonLd(99), null);
});

test("/gaps 按领域筛选不再抛错(以前对 const 重新赋值)", () => {
  assert.doesNotThrow(() => index.gaps({ domain: "1" }));
  assert.ok(index.gaps({ domain: "1" }).every((g) => g.domain === "1"));
  assert.ok(index.gaps().length >= index.gaps({ domain: "1" }).length);
});
