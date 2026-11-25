"use strict";
// 在浏览器里写知识:创建单元、为已有单元发布新版本。
// 流程:编辑正文(Markdown)→ 预览 → 计算 SHA-256 → 上传到正文仓库(/bodies/<哈希>,仓库只接受哈希一致的内容)
// → 在夜莺钱包里签名 create / publish 操作 → 提交 → 等入块。依赖 util.js(格式检查、写入令牌)与 member.js;网页里没有私钥代码。
(function () {
  const U = window.ZXUtil;
  const M = () => window.ZXMember;
  const MAX_BODY = 64 * 1024;
  const LAYER_FROM_ZH = { 基础: "foundation", 解释: "explanation", 场景: "scenario", 应用: "application" };

  const TEMPLATE = `# 标题:一句话说清楚这条知识

| 字段 | 内容 |
|---|---|
| 层级 | 基础 |
| 来源 | 人工撰写 |
| 状态 | **待核验** |
| 版本 | v1 |
| 语言 | zh |

## 核心陈述

一两句话说清楚这条知识是什么。

## 正文

先讲"为什么",再讲"怎么做"。只讲一件事;不看其他单元也能读懂。

## 适用范围与局限

- 适用:
- 不适用 / 会失效的情况:

## 引用

(在下方"引用"栏里选择所依据或参见的单元;这里可以写为什么引用。)

## 证据

| 论断 | 出处 | 证据类型 |
|---|---|---|

## 核验要点(给人类审查者)

1. 你最没把握、最需要懂行的人确认的地方。

## 核验记录

| 核验人 | 版本 | 结论 | 日期 | 说明 |
|---|---|---|---|---|
`;

  function msg(text, cls = "mut") { const m = el("auMsg"); if (m) { m.className = cls; m.textContent = text; } }

  async function sha256Hex(bytes) {
    const d = await crypto.subtle.digest("SHA-256", bytes);
    return Array.from(new Uint8Array(d), (b) => b.toString(16).padStart(2, "0")).join("");
  }

  async function uploadBody(bytes, hash) {
    const token = U.token();
    if (!token) throw new Error("还没有填写写入令牌(在「我的身份」里设置)");
    const r = await fetch("/bodies/" + hash, {
      method: "PUT", body: bytes,
      headers: { "Authorization": "Bearer " + token, "Content-Type": "text/markdown; charset=utf-8" },
    });
    if (r.status === 401) throw new Error("写入令牌不对");
    if (r.status === 413) throw new Error("正文太长(上限 64 KiB)");
    if (!r.ok) throw new Error(`上传正文失败(HTTP ${r.status}):${(await r.text()).trim()}`);
  }

  // ---- 引用编辑 ---------------------------------------------------------------
  // 每条引用指向某个单元的**当前版本**("依据"锁定版本;被引用单元以后修订,浏览器会提示"引用已过时")
  async function citeRow(c) {
    let title = "", cur = c.version;
    try {
      const u = await j("/unit/" + c.unit);
      cur = Number(u.versions.length);
      const v = u.versions[u.versions.length - 1];
      const b = v.bodies[0] ? await fetchBody(v.bodies[0].hash) : null;
      title = b && b.ok ? bodyTitle(b.text) : "";
    } catch (_) { title = "(不存在的单元)"; }
    return { unit: Number(c.unit), version: cur, kind: c.kind, title, stale: c.version && c.version < cur ? c.version : 0 };
  }

  function renderCites(state) {
    const box = el("auCites");
    box.innerHTML = (state.cites.length ? `<table><tr><th>单元</th><th>类型</th><th>版本</th><th></th></tr>` + state.cites.map((c, i) =>
      `<tr><td>#${esc(c.unit)} ${esc(c.title)}</td>
         <td><select data-i='${i}' class='auKind'><option value='basis'${c.kind === "basis" ? " selected" : ""}>依据</option>
           <option value='see-also'${c.kind === "see-also" ? " selected" : ""}>参见</option>
           <option value='supersedes'${c.kind === "supersedes" ? " selected" : ""}>取代</option></select></td>
         <td>v${esc(c.version)}${c.stale ? ` <span class='mut'>(原为 v${esc(c.stale)},已更新到当前版本)</span>` : ""}</td>
         <td><button data-i='${i}' class='auDel'>移除</button></td></tr>`).join("") + "</table>" : "<p class='mut'>还没有引用。</p>")
      + `<p><input id='auCiteId' size='6' placeholder='单元编号'> <select id='auCiteKind'><option value='basis'>依据</option><option value='see-also'>参见</option><option value='supersedes'>取代</option></select>
         <button id='auCiteAdd'>添加引用</button></p>
         <p class='mut'><b>依据</b>:本单元建立在它之上,它若被证伪,本单元也站不住;依据会传递可信度,不能成环。
         <b>参见</b>:相关、可互相补充,任何一方出错不影响另一方;不传递可信度。
         <b>取代</b>:本单元取代它(它是旧版本)。这是"挑战"的表达方式:新旧单元**并存**,链上不强行裁决,
         靠证据、引用与使用随时间分高下;被取代的单元不再传递可信度,历史与署名全部保留。</p>`;
    box.querySelectorAll(".auKind").forEach((s) => s.addEventListener("change", () => { state.cites[s.dataset.i].kind = s.value; }));
    box.querySelectorAll(".auDel").forEach((b) => b.addEventListener("click", () => { state.cites.splice(b.dataset.i, 1); renderCites(state); }));
    el("auCiteAdd").addEventListener("click", async () => {
      const id = Number(el("auCiteId").value);
      if (!Number.isInteger(id) || id < 1) return msg("单元编号应为正整数", "err");
      if (state.unit && id === Number(state.unit)) return msg("不能引用自己所在的单元", "err");
      if (state.cites.some((c) => c.unit === id)) return msg("已经引用过这个单元", "err");
      if (state.cites.length >= 32) return msg("引用最多 32 条", "err");
      const row = await citeRow({ unit: id, kind: el("auCiteKind").value });
      if (row.title === "(不存在的单元)") return msg(`单元 #${id} 不存在`, "err");
      state.cites.push(row);
      msg("");
      renderCites(state);
    });
  }

  // ---- 表单 -------------------------------------------------------------------
  /**
   * 链上**下一块**起接受的领域(新单元最早就在下一块入块)。
   * 领域词表里有名字,不等于链已经接受:新增领域按高度激活,激活前创建会被链拒绝。
   * 读不到(旧版节点没有 /domains)时退回词表里的全部领域。
   */
  async function acceptedDomains() {
    try {
      const r = await fetch("/rpc/domains", { headers: { Accept: "application/json" } });
      if (r.ok) {
        const d = await r.json();
        if (d && Array.isArray(d.next)) return new Set(d.next.map(String));
      }
    } catch (_) { /* 旧节点:退回词表 */ }
    return null;
  }

  async function openEditor(state) {
    if (!M() || !M().hasVault()) return M().showIdentity();
    const isNew = !state.unit;
    const accepted = isNew ? await acceptedDomains() : null;
    const usable = Object.entries(DOMAIN_ZH).filter(([k]) => !accepted || accepted.has(String(k)));
    // 默认领域不可用(例如还停在一个链不接受的领域)时,改选第一个可用的
    if (usable.length && !usable.some(([k]) => Number(k) === Number(state.domain))) state.domain = Number(usable[0][0]);
    const domains = usable.map(([k, v]) =>
      `<option value='${k}'${Number(k) === Number(state.domain) ? " selected" : ""}>${esc(v)} (${k})</option>`).join("");
    const layers = Object.entries(LAYER_ZH).map(([k, v]) =>
      `<option value='${k}'${k === state.layer ? " selected" : ""}>${esc(v)}</option>`).join("");
    const src = state.source || { type: "human" };
    setView(`<p><a class='link' id='auBack'>← ${isNew ? "knowledge" : "返回单元 #" + esc(state.unit)}</a></p>
      <h2 style='border:0;padding:0 0 8px'>${isNew ? "写新单元" : `修订单元 #${esc(state.unit)}(发布 v${esc(state.nextVersion)})`}</h2>
      ${isNew ? "" : "<p class='mut'>任何成员都可以修订别人的单元,你会被记录为这个版本的发布者。修订后,之前的核验只对旧版本有效,新版本需要重新核验。</p>"}
      ${isNew ? `<p>领域 <select id='auDomain'>${domains}</select> 层级 <select id='auLayer'>${layers}</select>
        <span class='mut'>(层级会按正文表格里的"层级"自动选择)</span></p>` : ""}
      <p>来源 <select id='auSrc'>
          <option value='human'${src.type === "human" ? " selected" : ""}>人工撰写</option>
          <option value='model'${src.type === "model" ? " selected" : ""}>模型生成</option>
          <option value='model-verified'${src.type === "model-verified" ? " selected" : ""}>模型生成、人工修改过</option></select>
        <input id='auModel' size='24' placeholder='模型名称与版本' value='${esc(src.id || "")}'>
        <span class='mut'>如实标注;模型生成的内容在有人核验通过前,可信度从 0.1 起步</span></p>
      <div class='auSplit'>
        <div><h3 class='md'>正文(Markdown)</h3><textarea id='auText' spellcheck='false'></textarea>
          <p class='mut' id='auSize'></p></div>
        <div><h3 class='md'>预览</h3><div class='mdbody auPreview' id='auPreview'></div></div>
      </div>
      <h3 class='md'>引用</h3><div id='auCites'></div>
      <p class='mut'>提交即同意以 <b>CC BY 4.0</b> 许可开放这段内容(任何人可使用,包括商用,只需署名)。</p>
      <p><button id='auGo'>${isNew ? "创建单元" : "发布新版本"}</button> <span id='auMsg'></span></p>`);
    const ta = el("auText");
    ta.value = state.text;
    const refresh = () => {
      el("auPreview").innerHTML = renderMd(ta.value);
      const n = new TextEncoder().encode(ta.value).length;
      el("auSize").textContent = `${n} 字节 / 上限 ${MAX_BODY}`;
      el("auSize").className = n > MAX_BODY ? "err" : "mut";
      if (isNew) {
        const m = /\|\s*层级\s*\|\s*([^|\s]+)/.exec(ta.value);
        if (m && LAYER_FROM_ZH[m[1]]) el("auLayer").value = LAYER_FROM_ZH[m[1]];
      }
    };
    let t;
    ta.addEventListener("input", () => { clearTimeout(t); t = setTimeout(refresh, 200); });
    refresh();
    const srcSync = () => { el("auModel").style.display = el("auSrc").value === "human" ? "none" : ""; };
    el("auSrc").addEventListener("change", srcSync);
    srcSync();
    renderCites(state);
    el("auBack").addEventListener("click", () => (isNew ? showTab("knowledge") : showUnit(state.unit)));
    el("auGo").addEventListener("click", () => submitEditor(state, isNew));
  }

  async function submitEditor(state, isNew) {
    const btn = el("auGo");
    try {
      if (!M().unlocked()) throw new Error("请先在「我的身份」里用夜莺钱包登录");
      const me = M().account();
      if (!me) throw new Error("你的公钥还不是测试网成员");
      const text = el("auText").value;
      const bytes = new TextEncoder().encode(text);
      if (!text.trim()) throw new Error("正文是空的");
      if (bytes.length > MAX_BODY) throw new Error("正文太长(上限 64 KiB)");
      if (!/^#\s+\S/m.test(text)) throw new Error("正文第一行应是标题(以 # 开头)");
      const st = el("auSrc").value;
      const source = st === "human" ? { type: "human" } : { type: st, id: el("auModel").value.trim() };
      if (st !== "human" && !source.id) throw new Error("请填写模型名称与版本");
      const hash = await sha256Hex(bytes);
      const cites = state.cites.map((c) => ({ unit: c.unit, version: c.version, kind: c.kind }));
      const common = { bodies: [{ lang: "zh", hash }], source, cites };
      const action = isNew
        ? { type: "create", layer: el("auLayer").value, domain: Number(el("auDomain").value), ...common }
        : { type: "publish", unit: Number(state.unit), ...common };
      U.checkAction(action); // 先在本地检查格式(长度、数量等),有问题就不上传
      const label = isNew ? `在「${DOMAIN_ZH[action.domain]}」创建一个${LAYER_ZH[action.layer]}单元` : `为单元 #${state.unit} 发布 v${state.nextVersion}`;
      if (!confirm(`以账户 ${me} 的身份${label}?\n内容将以 CC BY 4.0 许可公开,并永久记录在链上。`)) return;
      btn.disabled = true;
      msg("上传正文…");
      await uploadBody(bytes, hash);
      msg("签名并提交…");
      const before = isNew ? Number((await j("/units?limit=1")).total) : 0;
      const n = await M().submit(action);
      msg("已提交,等待入块…");
      if (!(await M().waitNonce(n))) { msg("已提交,稍后刷新查看", "ok"); return; }
      bodyCacheForget(hash);
      if (!isNew) return showUnit(state.unit);
      // 新单元的编号:从提交前的总数往后找,作者是我、正文哈希一致的那个
      for (let id = before + 1; id <= before + 20; id++) {
        try {
          const u = await j("/unit/" + id);
          if (String(u.author) === String(me) && u.versions[0].bodies.some((b) => b.hash === hash)) return showUnit(id);
        } catch (_) { break; }
      }
      showTab("knowledge");
    } catch (e) {
      msg(e.message, "err");
    } finally {
      if (btn) btn.disabled = false;
    }
  }

  function bodyCacheForget(hash) { if (typeof bodyCache !== "undefined") bodyCache.delete(hash); }

  // ---- 入口 -------------------------------------------------------------------
  function newUnit() {
    openEditor({ unit: null, domain: 1, layer: "foundation", text: TEMPLATE, source: { type: "human" }, cites: [] });
  }

  async function revise(u) {
    const v = u.versions[u.versions.length - 1];
    let text = "";
    const b = v.bodies[0] ? await fetchBody(v.bodies[0].hash) : null;
    if (b && b.ok) text = b.text;
    const s = v.source || "human";
    const [type, ...rest] = String(s).split(":");
    const source = type === "human" ? { type: "human" } : { type, id: rest.join(":") };
    const cites = await Promise.all(v.cites.map((c) => citeRow({ unit: c.unit, version: Number(c.version), kind: c.kind })));
    openEditor({ unit: u.id, nextVersion: u.versions.length + 1, text, source, cites });
  }

  function attachKnowledge() {
    const box = el("authorSlot");
    if (!box) return;
    box.innerHTML = "<button id='auNew'>写新单元</button>";
    el("auNew").addEventListener("click", newUnit);
  }

  function attachUnit(u) {
    const box = el("reviseSlot");
    if (!box) return;
    box.innerHTML = "<button id='auRevise'>修订(发布新版本)</button>";
    el("auRevise").addEventListener("click", () => revise(u));
  }

  window.ZXAuthor = { attachKnowledge, attachUnit, newUnit, revise };
})();
