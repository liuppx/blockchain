"use strict";
// 成员功能:在浏览器里持有密钥、核验知识单元、邀请新成员。依赖 signer.js(签名)与 app.js(j / el / esc / setView)。
// 私钥只在本机:种子用你的口令加密后存在这个浏览器里;解锁后的签名密钥只留在内存,刷新页面即清除。
(function () {
  const S = window.ZXSigner;
  let session = null; // { signKey, pubkey, account }
  let pending = null; // 生成流程中尚未保存的助记词

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
    const v = S.store.loadVault();
    const w = S.store.walletLink();
    const s = el("idStatus");
    if (!s) return;
    if (session) {
      const via = session.wallet ? "钱包 · " : "";
      s.textContent = session.account ? `${via}账户 ${session.account.id} · 已解锁` : `${via}已解锁 · 尚未成为成员`;
    } else if (!v && !w) s.textContent = "未设置身份";
    else s.textContent = "已锁定";
  }

  // ---- 夜莺钱包登录 -----------------------------------------------------------
  // 钱包不向网页开放 ed25519 任意签名,所以由钱包对一条固定消息 personal_sign,再派生成员密钥
  // (见 signer.js seedFromWalletSignature)。成员密钥不落盘:每次登录由钱包重新签一次即可。
  let walletProvider = null;
  window.addEventListener("eip6963:announceProvider", (e) => {
    const d = e.detail || {};
    if (d.info && d.info.rdns === "io.github.yeying" && d.provider) {
      const first = !walletProvider;
      walletProvider = d.provider;
      if (first) refreshWalletUi(); // 钱包插件可能比本脚本晚注入:检测到时补上顶栏按钮
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

  async function walletSign(provider, address) {
    // 传明文而不是十六进制:夜莺钱包的弹窗会把 0x 开头的消息解码成文字显示,但签名时
    // ethers 把字符串当 UTF-8 原文签——即签的是 "0x…" 这串字面量。显示与签名不一致,
    // 一旦钱包将来改成先解码再签,所有钱包用户的成员密钥都会变。明文两边一致,不受影响。
    const sig = await provider.request({
      method: "personal_sign",
      params: [S.walletDeriveMessage(address), address],
    });
    return S.seedFromWalletSignature(sig, address);
  }

  async function loginWithWallet(onProgress = () => {}) {
    const provider = findWallet();
    if (!provider) throw new Error("没有检测到夜莺钱包。请先安装钱包插件并刷新页面");
    onProgress("请在钱包里确认连接…");
    const accounts = await provider.request({ method: "eth_requestAccounts" });
    const address = String((accounts || [])[0] || "").toLowerCase();
    if (!/^0x[0-9a-f]{40}$/.test(address)) throw new Error("钱包没有返回以太坊地址;请在钱包里选择一个以太坊账户");

    const link = S.store.walletLink();
    const known = link && link.address === address;
    onProgress(known ? "请在钱包里签名,生成成员密钥…" : "请在钱包里签名,生成成员密钥(第 1 次,共 2 次)…");
    const seed = await walletSign(provider, address);
    const k = await S.keyFromSeed(seed);
    seed.fill(0);

    if (known) {
      if (k.pubkey !== link.pubkey) {
        throw new Error("这次派生的成员密钥与上次不同。门限(MPC)账户的签名每次不一样,不能用来登录;请换一个普通账户");
      }
    } else {
      // 第一次用这个账户:再签一次,确认签名是确定性的——否则下次登录会变成另一个人
      onProgress("请再签一次,确认每次都能得到同一把密钥(第 2 次,共 2 次)…");
      const seed2 = await walletSign(provider, address);
      const k2 = await S.keyFromSeed(seed2);
      seed2.fill(0);
      if (k2.pubkey !== k.pubkey) {
        throw new Error("两次签名派生出不同的密钥。门限(MPC)账户的签名每次不一样,不能用来登录;请换一个普通账户");
      }
      S.store.setWalletLink({ address, pubkey: k.pubkey });
    }
    onProgress("查找成员账户…");
    session = { signKey: k.signKey, pubkey: k.pubkey, account: await findAccount(k.pubkey), wallet: address };
    if (!provider._zxWatch && typeof provider.on === "function") {
      provider._zxWatch = true;
      // 钱包里切换或断开账户 ⇒ 退出,避免继续以旧账户的身份签名
      provider.on("accountsChanged", (accs) => {
        if (session && session.wallet && String((accs || [])[0] || "").toLowerCase() !== session.wallet) {
          lock();
          if (el("idState")) showIdentity();
        }
      });
    }
    status();
    return session;
  }

  function walletButton(id) {
    return findWallet()
      ? `<p><button id='${id}'>用夜莺钱包登录</button> <span class='ok'>✓ 已检测到夜莺钱包</span>
         <span class='mut'>密钥由钱包签名派生,本机不保存任何密钥</span></p>`
      : `<p class='mut'>✗ 没有检测到夜莺钱包。安装<a class='link' href='https://wallet.yeying.pub/' target='_blank' rel='noopener'>钱包插件</a>
         并确认它在本站已启用,然后刷新页面,这里会出现「用夜莺钱包登录」。</p>`;
  }

  // 顶栏的「钱包登录」:检测到钱包、且还没登录时显示
  function refreshWalletUi() {
    const b = el("idWalletTop");
    if (!b) return;
    b.style.display = findWallet() && !session ? "" : "none";
  }

  async function unlock(password) {
    const v = S.store.loadVault();
    if (!v) throw new Error("还没有设置身份");
    const seed = await S.openSeed(v, password);
    const k = await S.keyFromSeed(seed);
    seed.fill(0);
    if (k.pubkey !== v.pubkey) throw new Error("本机保存的身份已损坏");
    session = { signKey: k.signKey, pubkey: k.pubkey, account: await findAccount(k.pubkey) };
    status();
    return session;
  }

  function lock() { session = null; status(); }

  async function saveSeed(seed, password) {
    const k = await S.keyFromSeed(seed);
    S.store.saveVault(await S.sealSeed(seed, password, k.pubkey));
    seed.fill(0);
    session = { signKey: k.signKey, pubkey: k.pubkey, account: await findAccount(k.pubkey) };
    pending = null;
    status();
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
    [/invalid signature/, "签名无效:本机保存的身份与账户的公钥不一致"],
    [/nonce/i, "序号已过期(可能在别处刚提交过一条),请刷新后重试"],
  ];
  function reason(text) {
    let m = text;
    try { m = JSON.parse(text).error || text; } catch (_) { /* 非 JSON */ }
    for (const [re, zh] of REASONS) if (re.test(m)) return zh;
    return m.trim();
  }

  async function submit(action) {
    if (!session || !session.account) throw new Error("请先解锁身份;且该身份必须已是测试网成员");
    const token = S.store.token();
    if (!token) throw new Error("还没有填写写入令牌(在「我的身份」里设置,由组织者私下提供)");
    const nonce = (await j("/account/" + session.account.id)).nonce;
    const op = await S.signOp(session.signKey, Number(session.account.id), action, Number(nonce));
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

  async function showIdentity() {
    const v = S.store.loadVault();
    const link = S.store.walletLink();
    if (pending) return showGenerate();
    const useWallet = (id, msgId) => {
      if (!el(id)) return;
      el(id).addEventListener("click", async () => {
        try { await loginWithWallet((t) => msg(msgId, t)); showIdentity(); }
        catch (e) { msg(msgId, (e && e.code === 4001) ? "你在钱包里取消了" : e.message, "err"); }
      });
    };
    if (!v && !session) {
      setView(`<h2 style='border:0;padding:0 0 8px'>我的身份</h2>
        <p>你的身份是一把密钥。组织者和其他任何人都拿不到它。</p>
        ${link ? `<p>上次用钱包账户 <span class='mono'>${esc(link.address)}</span> 登录。</p>` : ""}
        ${walletButton("idWallet")}<p><span id='idWMsg'></span></p>
        <h3 class='md'>不用钱包</h3>
        <p class='mut'>密钥<b>只保存在这个浏览器里</b>,用你设置的口令加密。</p>
        <p><button id='idGen'>生成新身份</button> <span class='mut'>第一次加入测试网选这个</span></p>
        <p><button id='idImp'>用助记词导入</button> <span class='mut'>手上有 12 或 24 个词的助记词</span></p>
        <p><button id='idFile'>导入密钥文件</button> <span class='mut'>用 <code>node keygen --encrypt</code> 生成的 .json 文件,例如 <code>alice.json</code></span></p>
        ${link ? `<h3 class='md'>解除钱包关联</h3><p class='mut'>只清除本机记住的钱包地址;成员身份仍可随时用同一个钱包账户重新登录。</p><p><button id='idUnlink'>解除</button></p>` : ""}`);
      useWallet("idWallet", "idWMsg");
      el("idGen").addEventListener("click", async () => { pending = await S.generateMnemonic(); showGenerate(); });
      el("idImp").addEventListener("click", showImport);
      el("idFile").addEventListener("click", showImportFile);
      if (el("idUnlink")) el("idUnlink").addEventListener("click", () => { S.store.setWalletLink(null); showIdentity(); });
      return;
    }
    const acct = session && session.account;
    const pub = session ? session.pubkey : v.pubkey;
    // 本机已有口令加密的身份、尚未解锁:既可以输口令解锁它,也可以改用钱包登录(那是另一把密钥)
    const unlockUi = session
      ? `<p><button id='idLock'>${session.wallet ? "退出" : "锁定"}</button></p>`
      : `<p><input id='idPw' type='password' placeholder='口令' autocomplete='current-password'> <button id='idUnlock'>解锁</button> <span id='idMsg'></span></p>
         <h3 class='md'>或者用钱包登录</h3>
         <p class='mut'>钱包登录得到的是<b>另一把</b>成员密钥(由钱包账户派生),与上面这个本机身份不是同一个链上账户。</p>
         ${walletButton("idWallet")}<p><span id='idWMsg'></span></p>`;
    setView(`<h2 style='border:0;padding:0 0 8px'>我的身份</h2>
      <kv><b>公钥</b><span class='mono'>${esc(pub)}</span></kv>
      ${session && session.wallet ? `<kv><b>钱包</b><span class='mono'>${esc(session.wallet)}</span> <span class='mut'>(成员密钥由这个账户的签名派生,本机不保存)</span></kv>` : ""}
      <kv><b>状态</b><span id='idState'>${session ? (acct ? `账户 ${esc(acct.id)} · ${acct.established ? "已过见习期" : "见习期中(核验照常记录,但暂不计入可信度)"}` : "这个公钥还不是测试网成员:把它发给邀请你的成员") : "已锁定"}</span></kv>
      ${unlockUi}
      <h3 class='md'>写入令牌</h3>
      <p class='mut'>提交核验需要一个写入令牌,由组织者私下提供。它只用于防止刷写入,不代表你的身份(身份由你的密钥签名证明)。</p>
      <p><input id='idTok' type='password' size='40' placeholder='${S.store.token() ? "已保存(输入新值可替换)" : "粘贴写入令牌"}'> <button id='idTokSave'>保存</button> <span id='idTokMsg'></span></p>
      ${acct && acct.established ? `<h3 class='md'>邀请新成员</h3>
        <p class='mut'>新成员在自己的浏览器里「生成新身份」后,把公钥发给你。新成员有见习期;每位成员每天最多邀请一人。</p>
        <p><input id='invPk' size='70' placeholder='新成员的公钥(64 位十六进制)'> <button id='invGo'>邀请</button> <span id='invMsg'></span></p>` : ""}
      ${v ? `<h3 class='md'>移除本机身份</h3>
      <p class='mut'>从这个浏览器删除加密的密钥。之后只能用助记词恢复——没有助记词,这个身份就永久丢失。</p>
      <p><button id='idForget'>从本机移除</button></p>` : ""}`);
    useWallet("idWallet", "idWMsg");
    if (session) el("idLock").addEventListener("click", () => { lock(); showIdentity(); });
    else el("idUnlock").addEventListener("click", async () => {
      msg("idMsg", "解锁中…(需要几秒)");
      try { await unlock(el("idPw").value); showIdentity(); } catch (e) { msg("idMsg", e.message, "err"); }
    });
    el("idTokSave").addEventListener("click", () => { S.store.setToken(el("idTok").value); el("idTok").value = ""; msg("idTokMsg", "已保存", "ok"); });
    if (el("idForget")) el("idForget").addEventListener("click", () => {
      if (!confirm("确定从本机移除身份?没有助记词将无法恢复。")) return;
      S.store.forget(); lock(); showIdentity();
    });
    if (el("invGo")) el("invGo").addEventListener("click", async () => {
      const pk = el("invPk").value.trim().toLowerCase();
      if (!/^[0-9a-f]{64}$/.test(pk)) return msg("invMsg", "公钥应为 64 位十六进制", "err");
      if (!confirm(`邀请这个公钥加入测试网?\n${pk}\n你将成为他的邀请人。`)) return;
      try {
        msg("invMsg", "签名并提交中…");
        const n = await submit({ type: "invite", pubkey: pk });
        msg("invMsg", (await waitNonce(n)) ? "已入块。新成员现在可以在浏览器里导入身份了" : "已提交,等待入块(稍后在成员列表查看)", "ok");
      } catch (e) { msg("invMsg", e.message, "err"); }
    });
  }

  function showGenerate() {
    const ws = pending.split(" ");
    setView(`<h2 style='border:0;padding:0 0 8px'>生成新身份</h2>
      <p class='err'>下面 24 个词就是你的身份。<b>按顺序抄在纸上,妥善保管;不要拍照、不要截图、不要发给任何人</b>,包括组织者。
      换电脑、换浏览器、清空浏览器数据之后,只能用它们恢复。</p>
      <ol class='mnemonic'>${ws.map((w) => `<li>${esc(w)}</li>`).join("")}</ol>
      <p><label><input type='checkbox' id='genOk'> 我已按顺序抄好这 24 个词</label></p>
      <p>设置一个口令,用来在这个浏览器里加密密钥(至少 8 个字符;每次核验前解锁时要用):</p>
      <p><input id='genPw' type='password' placeholder='口令' autocomplete='new-password'>
         <input id='genPw2' type='password' placeholder='再输一次' autocomplete='new-password'>
         <button id='genSave'>保存身份</button> <button id='genCancel'>放弃</button> <span id='genMsg'></span></p>`);
    el("genCancel").addEventListener("click", () => { pending = null; showIdentity(); });
    el("genSave").addEventListener("click", async () => {
      if (!el("genOk").checked) return msg("genMsg", "请先确认已抄好助记词", "err");
      if (el("genPw").value !== el("genPw2").value) return msg("genMsg", "两次口令不一致", "err");
      try {
        msg("genMsg", "加密保存中…");
        await saveSeed(await S.mnemonicToSeed(pending), el("genPw").value);
        showIdentity();
      } catch (e) { msg("genMsg", e.message, "err"); }
    });
  }

  function showImport() {
    setView(`<h2 style='border:0;padding:0 0 8px'>用助记词导入</h2>
      <p class='mut'>输入 12 或 24 个词,用空格隔开。助记词只在这个页面里使用,不会发送到任何地方。</p>
      <p><textarea id='impWords' rows='3' cols='80' autocomplete='off' spellcheck='false'></textarea></p>
      <p><input id='impPass' type='password' placeholder='BIP-39 附加口令(没有就留空)' size='30'></p>
      <p>设置本机加密口令(至少 8 个字符):<input id='impPw' type='password' autocomplete='new-password'>
         <button id='impSave'>导入</button> <button id='impCancel'>取消</button> <span id='impMsg'></span></p>`);
    el("impCancel").addEventListener("click", showIdentity);
    el("impSave").addEventListener("click", async () => {
      try {
        msg("impMsg", "导入中…");
        await saveSeed(await S.mnemonicToSeed(el("impWords").value, el("impPass").value), el("impPw").value);
        el("impWords").value = "";
        showIdentity();
      } catch (e) { msg("impMsg", e.message, "err"); }
    });
  }

  function showImportFile() {
    setView(`<h2 style='border:0;padding:0 0 8px'>导入密钥文件</h2>
      <p class='mut'>选择 <code>node keygen --encrypt</code> 生成的 .json 密钥文件。文件只在这个页面里读取和解密,不会上传到任何地方。</p>
      <p><input id='ksFile' type='file' accept='.json,application/json'></p>
      <p>密钥文件的口令:<input id='ksPw' type='password' autocomplete='off'></p>
      <p>本机加密口令(至少 8 个字符,每次核验前解锁时用;可以与上面相同):<input id='ksLocal' type='password' autocomplete='new-password'></p>
      <p><button id='ksGo'>导入</button> <button id='ksCancel'>取消</button> <span id='ksMsg'></span></p>`);
    el("ksCancel").addEventListener("click", showIdentity);
    el("ksGo").addEventListener("click", async () => {
      const f = el("ksFile").files[0];
      if (!f) return msg("ksMsg", "请先选择密钥文件", "err");
      if (f.size > 64 * 1024) return msg("ksMsg", "这个文件太大,不像是密钥文件", "err");
      if ((el("ksLocal").value || "").length < 8) return msg("ksMsg", "本机加密口令至少 8 个字符", "err");
      msg("ksMsg", "解密中…密钥文件用了很强的加密,需要几秒到十几秒,页面可能短暂没有响应");
      await new Promise((ok) => setTimeout(ok, 50)); // 让提示先显示出来
      try {
        const seed = await S.decryptKeystore(await f.text(), el("ksPw").value);
        await saveSeed(seed, el("ksLocal").value);
        el("ksPw").value = ""; el("ksLocal").value = "";
        showIdentity();
      } catch (e) { msg("ksMsg", e.message, "err"); }
    });
  }

  // ---- 单元页的核验面板 --------------------------------------------------------
  function checklistFrom(text) {
    const m = /^##\s*核验要点[^\n]*\n([\s\S]*?)(?=^##\s|$(?![\s\S]))/m.exec(text || "");
    if (!m) return [];
    return m[1].split("\n").map((l) => /^\s*(?:\d+\.|[-*])\s+(.*)$/.exec(l)).filter(Boolean).map((x) => x[1]);
  }

  async function attachReview(u) {
    const slot = el("reviewSlot");
    if (!slot) return;
    const v = u.versions[u.versions.length - 1];
    const vn = Number(v.n);
    const head = `<h2 style='border:0;padding:12px 0 6px'>核验 v${esc(vn)}</h2>`;
    if (!S.store.loadVault() && !session) {
      const link = S.store.walletLink();
      slot.innerHTML = head + (link && findWallet()
        ? `<p><button id='rvWallet'>用夜莺钱包登录后核验</button> <span id='rvMsg'></span></p>`
        : "<p class='mut'>要核验这个单元,先在<a class='link' id='goId'>「我的身份」</a>里设置你的密钥或用钱包登录。</p>");
      if (el("goId")) el("goId").addEventListener("click", showIdentity);
      if (el("rvWallet")) el("rvWallet").addEventListener("click", async () => {
        try { await loginWithWallet((t) => msg("rvMsg", t)); attachReview(u); }
        catch (e) { msg("rvMsg", (e && e.code === 4001) ? "你在钱包里取消了" : e.message, "err"); }
      });
      return;
    }
    if (!session) {
      slot.innerHTML = head + `<p><input id='rvPw' type='password' placeholder='口令' autocomplete='current-password'> <button id='rvUnlock'>解锁后核验</button> <span id='rvMsg'></span></p>`;
      el("rvUnlock").addEventListener("click", async () => {
        msg("rvMsg", "解锁中…(需要几秒)");
        try { await unlock(el("rvPw").value); attachReview(u); } catch (e) { msg("rvMsg", e.message, "err"); }
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
        msg("rvMsg", "签名并提交中…");
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
  }

  window.ZXMember = {
    attachReview, showIdentity, submit, waitNonce,
    account: () => (session && session.account ? session.account.id : null),
    unlocked: () => !!session,
    hasVault: () => !!S.store.loadVault() || !!session,
    unlock, loginWithWallet,
  };
  init();
})();
