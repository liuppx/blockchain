"use strict";
// 成员功能:用夜莺钱包核验知识单元、邀请新成员、写作与修订。
//
// 密钥在钱包里,网页不持有私钥:网页只把"意图"(谁、第几条、做什么)交给钱包,
// 钱包自己编码、在弹窗里显示可读摘要、确认后签名。接口见
// yeying-community/wallet 的 docs/protocols/知行图协议.md。
//
// 浏览器本地存密钥的那种做法已经下线。此前在浏览器里生成过身份的成员,
// 页面会提示把密钥移进钱包(钱包「账户管理 → 导入钱包」里网络选「知行图」,用助记词或
// 密钥文件导入,导入后就是同一个链上账户)。
(function () {
  const U = window.ZXUtil;   // 十六进制、写入令牌、旧密钥清理;网页里没有私钥代码
  let session = null; // { pubkey, account, wallet }
  let lastUnit = null;   // 当前打开的单元:恢复登录后用它重画核验面板
  let restoring = null;  // 正在进行的恢复登录(避免初始化与钱包宣告两处同时请求)

  // ---- 身份 -----------------------------------------------------------------
  async function findAccount(pubkey) {
    const ms = (await j("/members?limit=500")).items || [];
    for (const m of ms) {
      try {
        const a = await j("/account/" + m.id);
        if (a.pubkey === pubkey) return { id: m.id, established: m.established, nonce: a.nonce };
      } catch (_) { /* 跳过 */ }
    }
    return null;
  }

  function status() {
    refreshWalletUi();
    const s = el("idStatus");
    if (!s) return;
    if (session) s.textContent = session.account ? `账户 ${session.account.id} · 已连接钱包` : "已连接钱包 · 尚未成为成员";
    else s.textContent = "未连接钱包";
  }

  // ---- 夜莺钱包 -----------------------------------------------------------------
  let walletProvider = null;
  window.addEventListener("eip6963:announceProvider", (e) => {
    const d = e.detail || {};
    if (d.info && d.info.rdns === "io.github.yeying" && d.provider) {
      const first = !walletProvider;
      walletProvider = d.provider;
      if (first) {
        // 钱包插件通常比本脚本晚注入(实测晚约 20ms):检测到时补上顶栏按钮,并尝试恢复登录
        refreshWalletUi();
        if (!session) restoreSession().catch(() => {});
      }
    }
  });
  window.dispatchEvent(new Event("eip6963:requestProvider"));

  function findWallet() {
    // 钱包插件可能比页面脚本晚注入:每次需要时再请求一次(EIP-6963 的回应是同步派发的)
    if (!walletProvider) window.dispatchEvent(new Event("eip6963:requestProvider"));
    if (walletProvider) return walletProvider;
    const eth = window.ethereum;
    if (eth && eth.isYeYing) return eth;
    if (eth && Array.isArray(eth.providers)) return eth.providers.find((p) => p.isYeYing) || null;
    return null;
  }

  // 钱包报错 → 给成员看的说明。旧版钱包不认识 zhixing_* 方法,会把它当成 EVM 请求转发出去而失败。
  function walletError(e) {
    const code = e && e.code;
    const m = String((e && e.message) || e || "");
    if (code === 4001 || /rejected/i.test(m)) return new Error("你在钱包里取消了");
    if (code === 4900 || /知行图(密钥|账户)/.test(m)) {
      return new Error("钱包里还没有知行图账户:打开夜莺钱包「账户管理 → 导入钱包」,网络选「知行图」,用助记词或密钥文件导入,再回来登录");
    }
    if (code === 4100) return new Error("这个网站还没有连接你的知行图密钥,请重新用钱包登录");
    if (/ZHIXING_/.test(m)) return new Error("钱包拒绝了这条操作:" + m.replace(/^.*ZHIXING_[A-Z_]+:?\\s*/, ""));
    return new Error("你的夜莺钱包还不支持知行图(或出错了:" + m.slice(0, 80) + ")。请把钱包升级到最新版本");
  }

  async function loginWithWallet(onProgress = () => {}) {
    const provider = findWallet();
    if (!provider) throw new Error("没有检测到夜莺钱包。请先安装钱包插件并刷新页面");
    onProgress("请在钱包里确认连接知行图…");
    let accounts;
    try {
      accounts = await provider.request({ method: "zhixing_requestAccounts" });
    } catch (e) {
      throw walletError(e);
    }
    const pubkey = String((accounts || [])[0] || "").toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(pubkey)) throw walletError(new Error("钱包返回的不是知行图公钥"));
    onProgress("查找成员账户…");
    session = { wallet: provider, pubkey, account: await findAccount(pubkey) };
    status();
    return session;
  }

  /**
   * 恢复登录:页面重新加载后,钱包里这个站点还授权着(而且知行图账户还在),就不该要求重新登录。
   * 用 zhixing_accounts:它不弹窗、不需要解锁,已授权就返回公钥,没授权就返回空数组。
   */
  function restoreSession() {
    if (session) return Promise.resolve(true);
    if (!restoring) restoring = doRestoreSession().finally(() => { restoring = null; });
    return restoring;
  }
  async function doRestoreSession() {
    const provider = findWallet();
    if (!provider) return false;
    let accounts;
    try {
      accounts = await provider.request({ method: "zhixing_accounts" });
    } catch (_) {
      return false; // 旧版钱包不认识这个方法:让人点登录按钮即可
    }
    const pubkey = String((accounts || [])[0] || "").toLowerCase();
    if (!/^[0-9a-f]{64}$/.test(pubkey)) return false;
    session = { wallet: provider, pubkey, account: await findAccount(pubkey) };
    status();
    // 正开着单元页时,把核验面板重画成"已登录"的样子(不抢走用户正在看的页面)
    if (lastUnit) { try { await attachReview(lastUnit); } catch (_) { /* 面板重画失败不影响登录 */ } }
    return true;
  }

  /** 用当前身份签一条操作,返回可提交给节点的字节。 */
  async function signAction(actor, action, nonce) {    let out;
    try {
      out = await session.wallet.request({
        method: "zhixing_signKnowledgeOp",
        params: [{ actor: String(actor), nonce: String(nonce), action }],
      });
    } catch (e) {
      throw walletError(e);
    }
    if (!out || out.address !== session.pubkey) throw new Error("钱包里的知行图密钥已更换,请重新用钱包登录");
    return U.hexToBytes(out.signed);
  }

  function walletButton(id) {
    return findWallet()
      ? `<p><button id='${id}'>用夜莺钱包登录</button> <span class='ok'>✓ 已检测到夜莺钱包</span>
         <span class='mut'>知行图密钥由钱包保管,每次签名都在钱包里确认</span></p>`
      : `<p class='mut'>✗ 没有检测到夜莺钱包。安装<a class='link' href='https://wallet.yeying.pub/' target='_blank' rel='noopener'>钱包插件</a>
         并确认它在本站已启用,然后刷新页面,这里会出现「用夜莺钱包登录」。</p>`;
  }

  // 顶栏的「钱包登录」:检测到钱包、且还没登录时显示
  function refreshWalletUi() {
    const b = el("idWalletTop");
    if (!b) return;
    b.style.display = findWallet() && !session ? "" : "none";
  }

  // ---- 提交 -----------------------------------------------------------------
  const REJECT = {
    401: "写入令牌不对或没有填写(在「我的身份」里设置)",
    409: "这条操作已经提交过,或序号已过期;请刷新后重试",
    429: "提交太频繁,请稍后再试",
  };

  // 节点拒绝原因(lib.rs ChainError 的英文消息)→ 给成员看的说明
  const REASONS = [
    [/invited too recently/, "你最近已经邀请过一人;每位成员每天最多邀请一人,请明天再试"],
    [/still in probation/, "你还在见习期,暂时不能邀请新成员"],
    [/already used by an account/, "这个公钥已经是测试网的账户了,不需要再邀请"],
    [/cannot verify it/, "这一版由你发布,不能由你自己核验"],
    [/is not a member/, "你的账户还不是测试网成员"],
    [/does not exist|has no version/, "这个单元或版本不存在(可能刚被修订,请刷新页面)"],
    [/invalid signature/, "签名无效:钱包里的公钥与账户不一致"],
    [/nonce/i, "序号已过期(可能在别处刚提交过一条),请刷新后重试"],
  ];
  function reason(text) {
    let m = text;
    try { m = JSON.parse(text).error || text; } catch (_) { /* 非 JSON */ }
    for (const [re, zh] of REASONS) if (re.test(m)) return zh;
    return m.trim();
  }

  async function submit(action) {
    if (!session || !session.account) throw new Error("请先用夜莺钱包登录;且该身份必须已是测试网成员");
    const token = U.token();
    if (!token) throw new Error("还没有填写写入令牌(在「我的身份」里设置,由组织者私下提供)");
    const nonce = (await j("/account/" + session.account.id)).nonce;
    const op = await signAction(Number(session.account.id), action, Number(nonce));
    const r = await fetch(RPC + "/knowledge", {
      method: "POST",
      headers: { "Authorization": "Bearer " + token, "Content-Type": "application/octet-stream", "Accept": "application/json" },
      body: op,
    });
    const text = await r.text();
    if (!r.ok) throw new Error(REJECT[r.status] || `节点拒绝:${reason(text)}`);
    return Number(nonce);
  }

  async function waitNonce(after, timeoutMs = 20000) {
    const t0 = Date.now();
    while (Date.now() - t0 < timeoutMs) {
      const a = await j("/account/" + session.account.id);
      if (Number(a.nonce) > after) return true;
      await new Promise((ok) => setTimeout(ok, 1000));
    }
    return false;
  }

  // ---- 「我的身份」页 ----------------------------------------------------------
  function msg(id, text, cls = "mut") { const m = el(id); if (m) { m.className = cls; m.textContent = text; } }

  /** 旧版本在浏览器本地存过密钥 ⇒ 提示把它移进钱包。 */
  function legacyNotice() {
    if (!U.hasLegacyKey()) return "";
    return `<div class="banner warn" style="padding:10px 14px;margin:12px 0;border-radius:6px">
      <p><b>这个浏览器里还留着旧版网页自己保存的密钥。</b>那条路已经下线:现在密钥由夜莺钱包保管,
      网页不持有私钥,每次签名都在钱包里确认。</p>
      <p class='mut'>你的链上账户没有变——在钱包「账户管理 → 导入钱包」里网络选「知行图」,用<b>同一组助记词</b>或密钥文件导入,
      导入后就是同一个账户。导入完成后可以清掉本机这份旧密钥(下面的「清除本机旧密钥」)。</p>
      <p><button id='idForgetLegacy'>清除本机旧密钥</button> <span id='idForgetMsg'></span></p>
    </div>`;
  }

  async function showIdentity() {
    const useWallet = (id, msgId) => {
      if (!el(id)) return;
      el(id).addEventListener("click", async () => {
        try { await loginWithWallet((t) => msg(msgId, t)); showIdentity(); }
        catch (e) { msg(msgId, (e && e.code === 4001) ? "你在钱包里取消了" : e.message, "err"); }
      });
    };

    if (!session) {
      setView(`<h2 style='border:0;padding:0 0 8px'>我的身份</h2>
        <p>你的身份是一把密钥,由<b>夜莺钱包</b>保管。组织者和其他任何人都拿不到它。</p>
        ${walletButton("idWallet")}<p><span id='idWMsg'></span></p>
        ${legacyNotice()}
        <h3 class='md'>写入令牌</h3>
        <p class='mut'>提交核验需要一个写入令牌,由组织者私下提供。它只用于防止刷写入,不代表你的身份(身份由钱包的签名证明)。</p>
        <p><input id='idTok' type='password' size='40' placeholder='${U.token() ? "已保存(输入新值可替换)" : "粘贴写入令牌"}'> <button id='idTokSave'>保存</button> <span id='idTokMsg'></span></p>`);
      useWallet("idWallet", "idWMsg");
      el("idTokSave").addEventListener("click", () => { U.setToken(el("idTok").value); el("idTok").value = ""; msg("idTokMsg", "已保存", "ok"); });
      if (el("idForgetLegacy")) el("idForgetLegacy").addEventListener("click", () => {
        if (!confirm("清除这个浏览器里旧版保存的密钥?\n确认已经把同一组助记词导入钱包后再做这一步。")) return;
        U.removeLegacyKey();
        msg("idForgetMsg", "已清除", "ok");
        showIdentity();
      });
      return;
    }

    const acct = session.account;
    setView(`<h2 style='border:0;padding:0 0 8px'>我的身份</h2>
      <kv><b>公钥</b><span class='mono'>${esc(session.pubkey)}</span></kv>
      <kv><b>密钥</b><span>由夜莺钱包保管 <span class='mut'>(每次签名都在钱包里确认;本机不保存任何密钥)</span></span></kv>
      <kv><b>状态</b><span id='idState'>${acct ? `账户 ${esc(acct.id)} · ${acct.established ? "已过见习期" : "见习期中(核验照常记录,但暂不计入可信度)"}` : "这个公钥还不是测试网成员:把它发给邀请你的成员"}</span></kv>
      ${acct ? "" : `<p><button id='idCopyPk'>复制公钥</button> <span id='idCopyMsg' class='mut'>发给邀请你的成员;对方邀请后,刷新本页就能看到你的账户号</span></p>`}
      <p><button id='idLock'>退出</button></p>
      <h3 class='md'>写入令牌</h3>
      <p class='mut'>提交核验需要一个写入令牌,由组织者私下提供。它只用于防止刷写入,不代表你的身份。</p>
      <p><input id='idTok' type='password' size='40' placeholder='${U.token() ? "已保存(输入新值可替换)" : "粘贴写入令牌"}'> <button id='idTokSave'>保存</button> <span id='idTokMsg'></span></p>
      ${acct && acct.established ? `<h3 class='md'>邀请新成员</h3>
        <p class='mut'>新成员在夜莺钱包「账户管理 → 创建钱包 → 知行图钱包」里创建账户,用钱包登录本站后点「复制公钥」发给你。新成员有见习期;每位成员每天最多邀请一人。</p>
        <p><input id='invPk' size='70' placeholder='新成员的公钥(64 位十六进制)'> <button id='invGo'>邀请</button> <span id='invMsg'></span></p>` : ""}`);
    el("idLock").addEventListener("click", () => { session = null; status(); showIdentity(); });
    if (el("idCopyPk")) el("idCopyPk").addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(session.pubkey); msg("idCopyMsg", "已复制。发给邀请你的成员;对方邀请后,刷新本页", "ok"); }
      catch (_) { msg("idCopyMsg", "复制失败,请手动选中上面的公钥复制", "err"); }
    });
    el("idTokSave").addEventListener("click", () => { U.setToken(el("idTok").value); el("idTok").value = ""; msg("idTokMsg", "已保存", "ok"); });
    if (el("invGo")) el("invGo").addEventListener("click", async () => {
      const pk = el("invPk").value.trim().toLowerCase();
      if (!/^[0-9a-f]{64}$/.test(pk)) return msg("invMsg", "公钥应为 64 位十六进制", "err");
      if (!confirm(`邀请这个公钥加入测试网?\n${pk}\n你将成为他的邀请人。`)) return;
      try {
        msg("invMsg", "签名并提交中…");
        const n = await submit({ type: "invite", pubkey: pk });
        msg("invMsg", (await waitNonce(n)) ? "已入块。请告诉新成员刷新页面,就能看到自己的账户号" : "已提交,等待入块(稍后在成员列表查看)", "ok");
      } catch (e) { msg("invMsg", e.message, "err"); }
    });
  }

  function checklistFrom(text) {
    const m = /^##\s*核验要点[^\n]*\n([\s\S]*?)(?=^##\s|$(?![\s\S]))/m.exec(text || "");
    if (!m) return [];
    return m[1].split("\n").map((l) => /^\s*(?:\d+\.|[-*])\s+(.*)$/.exec(l)).filter(Boolean).map((x) => x[1]);
  }

  async function attachReview(u) {
    const slot = el("reviewSlot");
    if (!slot) return;
    lastUnit = u;
    const v = u.versions[u.versions.length - 1];
    const vn = Number(v.n);
    const head = `<h2 style='border:0;padding:12px 0 6px'>核验 v${esc(vn)}</h2>`;
    if (!session) {
      slot.innerHTML = head + (findWallet()
        ? `<p><button id='rvWallet'>用夜莺钱包登录后核验</button> <span id='rvMsg'></span></p>`
        : "<p class='mut'>要核验这个单元,先<a class='link' id='goId'>「我的身份」</a>里用夜莺钱包登录。</p>");
      if (el("goId")) el("goId").addEventListener("click", showIdentity);
      if (el("rvWallet")) el("rvWallet").addEventListener("click", async () => {
        try { await loginWithWallet((t) => msg("rvMsg", t)); attachReview(u); }
        catch (e) { msg("rvMsg", (e && e.code === 4001) ? "你在钱包里取消了" : e.message, "err"); }
      });
      return;
    }
    const me = session.account;
    if (!me) { slot.innerHTML = head + "<p class='mut'>你的公钥还不是测试网成员,暂时不能核验。</p>"; return; }
    if (String(v.publisher) === String(me.id)) {
      slot.innerHTML = head + "<p class='mut'>这一版由你发布,不能由你自己核验。</p>";
      return;
    }
    const mine = v.verifications.find((r) => String(r.verifier) === String(me.id));
    // 核验人指派(服务侧确定性计算;现阶段只是提示,链仍接受所有成员的核验)
    let assignNote = "";
    try {
      const a = await (await fetch("/assign?unit=" + encodeURIComponent(u.id) + "&version=" + vn)).json();
      if (a && a.assigned && a.assigned.length) {
        const isMine = a.assigned.includes(String(me.id));
        assignNote = `<p class='mut'>本版指派给 ${a.assigned.map((x) => "#" + x).join("、")}` +
          `${isMine ? "<b>(包括你)</b>" : ""}。指派由区块哈希确定性算出,任何人都能复核;` +
          `现阶段只是提示,链仍接受所有成员的核验。</p>`;
        const others = a.verifications.filter((x) => x.assigned);
        if (others.length) {
          assignNote += `<p class='mut'>已有指派者的结论:${others.map((x) =>
            `#${x.verifier} ${x.verdict === "pass" ? "通过" : "需修改"}`).join("、")}</p>`;
        }
      }
    } catch (_) { /* 指派不可用不影响核验 */ }
    let items = [];
    const b = v.bodies[0] ? await fetchBody(v.bodies[0].hash) : null;
    if (b && b.ok) items = checklistFrom(b.text);
    const checks = items.length
      ? `<p>请逐条确认单元末尾的"核验要点"(勾选全部后才能给"通过"):</p><ol class='checklist'>${items.map((t, i) =>
          `<li><label><input type='checkbox' class='rvChk' id='rvc${i}'> ${mdInline(t)}</label></li>`).join("")}</ol>`
      : "<p class='mut'>这个单元没有列出核验要点;请按领域的审查清单逐项检查。</p>";
    slot.innerHTML = head + assignNote + (mine ? `<p>你之前的结论:<b>${mine.verdict === "pass" ? "通过" : "需修改"}</b>(高度 ${esc(mine.height)})。再次提交会以最新的结论为准。</p>` : "")
      + (me.established ? "" : "<p class='err'>你还在见习期:核验照常记录,但暂不计入可信度。</p>")
      + checks
      + `<p class='mut'>你只对<b>核验当时能知道的事</b>负责;后来出现的新证据推翻它,是"诚实的错",不会受罚。
         有真正的学术争议时,不要强行判对错,可以写一个观点不同的新单元。</p>
         <p><button id='rvPass'>通过</button> <button id='rvNeeds'>需修改</button> <span id='rvMsg'></span></p>`;
    const passBtn = el("rvPass");
    const sync = () => { passBtn.disabled = items.length > 0 && ![...document.querySelectorAll(".rvChk")].every((c) => c.checked); };
    document.querySelectorAll(".rvChk").forEach((c) => c.addEventListener("change", sync));
    sync();
    const go = async (verdict) => {
      const label = verdict === "pass" ? "通过" : "需修改";
      if (!confirm(`以账户 ${me.id} 的身份,对单元 #${u.id} 的 v${vn} 给出"${label}"?\n这条记录会永久保存在链上。`)) return;
      try {
        msg("rvMsg", "在钱包里签名…");
        const n = await submit({ type: "verify", unit: Number(u.id), version: vn, verdict });
        msg("rvMsg", "已提交,等待入块…");
        if (await waitNonce(n)) showUnit(u.id); else msg("rvMsg", "已提交,稍后刷新查看", "ok");
      } catch (e) { msg("rvMsg", e.message, "err"); }
    };
    passBtn.addEventListener("click", () => go("pass"));
    el("rvNeeds").addEventListener("click", () => go("needs-revision"));
  }

  // ---- 挂到页面上 -------------------------------------------------------------
  function init() {
    const btn = document.createElement("button");
    btn.id = "idBtn";
    btn.textContent = "我的身份";
    const st = document.createElement("span");
    st.id = "idStatus";
    st.className = "mut";
    const h = document.querySelector("header");
    h.appendChild(btn);
    // 顶栏直接放「钱包登录」:不用先找到「我的身份」页
    const wb = document.createElement("button");
    wb.id = "idWalletTop";
    wb.textContent = "用夜莺钱包登录";
    wb.style.display = "none";
    wb.addEventListener("click", async () => {
      const s = el("idStatus");
      try {
        await loginWithWallet((t) => { if (s) s.textContent = t; });
        showIdentity();
      } catch (e) {
        if (s) s.textContent = (e && e.code === 4001) ? "你在钱包里取消了" : e.message;
      }
    });
    h.appendChild(wb);
    h.appendChild(st);
    btn.addEventListener("click", showIdentity);
    if (!crypto.subtle) st.textContent = "此浏览器不支持签名(需要 HTTPS 与较新的浏览器)";
    else status();
    refreshWalletUi();
    // 页面重新加载后自动恢复登录(站点已授权且钱包里还有知行图账户时)
    restoreSession().then((ok) => { if (!ok) refreshWalletUi(); }).catch(() => {});
  }

  window.ZXMember = {
    attachReview, showIdentity, submit, waitNonce,
    account: () => (session && session.account ? session.account.id : null),
    unlocked: () => !!session,
    connected: () => !!session,
    hasVault: () => !!session,          // 旧的本机身份已下线:只有钱包连接才算有身份
    loginWithWallet,
  };
  init();
})();
