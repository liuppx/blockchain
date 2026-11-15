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
