// 核验人指派(服务侧)的检验:确定性、可验证性、与发布者无关。
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const { assign } = require(join(here, "../review-assign.js"));

const MEMBERS = ["1", "2", "3", "4", "5", "6", "7", "8"];
const BASE = { blockHash: "ab".repeat(32), unit: 3, version: 2, publisher: "4", members: MEMBERS, k: 4 };

test("指派是确定性的:同样的输入得到同样的结果", () => {
  const a = assign(BASE), b = assign(BASE);
  assert.deepEqual(a.assigned, b.assigned);
  assert.equal(a.seed, b.seed);
});

test("指派者不含发布者", () => {
  const a = assign(BASE);
  assert.ok(!a.assigned.includes("4"), "发布者不应被指派");
  assert.equal(a.candidates.length, MEMBERS.length - 1);
});

test("候选不足时全部指派", () => {
  const a = assign({ ...BASE, members: ["1", "4"], k: 4 });
  assert.deepEqual(a.assigned, ["1"]);
});

test("不同版本 / 不同区块得到不同指派", () => {
  const a = assign(BASE);
  const b = assign({ ...BASE, version: 3 });
  const c = assign({ ...BASE, blockHash: "cd".repeat(32) });
  assert.notEqual(a.seed, b.seed);
  assert.notEqual(a.seed, c.seed);
});

test("成员集合相同则结果相同(可复核:任何人都能算出同样的指派)", () => {
  const a = assign(BASE);
  const b = assign({ ...BASE, members: [...MEMBERS].reverse() });
  assert.deepEqual(a.assigned, b.assigned, "成员顺序不影响指派");
});

// ---- 知识经济激活后:以链上指派为准 -------------------------------------------------
const { ReviewAssigner } = require(join(here, "../review-assign.js"));

/** 假的 RPC:按路径返回固定数据,不起任何网络。 */
function fakeAssigner(routes) {
  const a = new ReviewAssigner("http://unused");
  a.rpcJson = async (p) => {
    const key = Object.keys(routes).find((k) => p.startsWith(k));
    if (!key) throw new Error("no route " + p);
    const v = routes[key];
    return typeof v === "function" ? v(p) : v;
  };
  return a;
}

const UNIT = {
  id: "2",
  versions: [{
    n: "1", publisher: "1", published_height: "20",
    verifications: [{ verifier: "3", verdict: "pass", height: "21" }, { verifier: "9", verdict: "pass", height: "22" }],
  }],
};

test("激活后:指派以链上为准,标记为强制", async () => {
  const a = fakeAssigner({
    "/economy": { active: true, activation_height: "8",
      versions: [{ unit: "2", version: "1", assigned: ["3", "4", "5", "6"], established_height: null, approvers: [] }] },
    "/unit/2": UNIT,
  });
  const r = await a.forVersion("2", "1");
  assert.equal(r.source, "chain");
  assert.equal(r.enforced, true);
  assert.deepEqual(r.assigned, ["3", "4", "5", "6"]);
  // 核验人 9 不在指派名单:记录照常显示,但标为未指派
  assert.deepEqual(r.verifications.map((x) => [x.verifier, x.assigned]), [["3", true], ["9", false]]);
});

test("激活前发布的版本链上没有指派:退回确定性提示", async () => {
  const a = fakeAssigner({
    "/economy": { active: true, activation_height: "100", versions: [] },
    "/unit/2": UNIT,
    "/block/20": { hash: "ab".repeat(32) },
    "/members": { items: [{ id: "1", inviter: "0" }, { id: "3", inviter: "0" }, { id: "4", inviter: "0" }] },
  });
  const r = await a.forVersion("2", "1");
  assert.equal(r.source, "hint");
  assert.equal(r.enforced, false);
  assert.ok(!r.assigned.includes("1"), "提示里也不含发布者");
});

test("节点没有 /economy(旧版本):按未激活处理,不报错", async () => {
  const a = fakeAssigner({
    "/economy": () => { throw new Error("404"); },
    "/unit/2": UNIT,
    "/block/20": { hash: "cd".repeat(32) },
    "/members": { items: [{ id: "1", inviter: "0" }, { id: "3", inviter: "0" }] },
  });
  const r = await a.forVersion("2", "1");
  assert.equal(r.source, "hint");
});

test("已确立的版本不再出现在「待我核验」里", async () => {
  const econ = (established) => ({ active: true, activation_height: "8",
    versions: [{ unit: "2", version: "1", assigned: ["4", "5"], established_height: established, approvers: [] }] });
  const routes = (established) => ({
    "/economy": econ(established),
    "/units": { items: [{ id: "2", versions: "1", lifecycle: "active" }], cursor: null },
    "/unit/2": UNIT,
  });
  assert.equal((await fakeAssigner(routes(null)).pendingFor("4")).length, 1, "未确立:待核验");
  assert.equal((await fakeAssigner(routes("30")).pendingFor("4")).length, 0, "已确立:不再需要");
});

test("激活后,「待我核验」只列链上指派的版本(激活前发布的不算任务)", async () => {
  const a = fakeAssigner({
    "/economy": { active: true, activation_height: "8", versions: [] },
    "/units": { items: [{ id: "2", versions: "1", lifecycle: "active" }], cursor: null },
    "/unit/2": UNIT,
    "/block/20": { hash: "ab".repeat(32) },
    "/members": { items: ["1", "3", "4", "5", "6"].map((id) => ({ id, inviter: "0" })) },
  });
  // 单元 #2 没有链上指派(激活前发布),即使提示里有这个成员,也不列为任务
  for (const m of ["3", "4", "5", "6"]) {
    assert.equal((await a.pendingFor(m)).length, 0, `成员 ${m}`);
  }
});
