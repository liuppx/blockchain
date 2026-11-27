"""在 macOS 的 Microsoft Edge 中代填知行图新单元。"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

# 与 kgraph 共用模型标识的规则(同目录的解析器)
sys.path.insert(0, str(Path(__file__).resolve().parent))


SITE = "https://blockchain.yeying.pub"
# 只有与 author.js 初始模板一致的现有表单才可直接代填。
TEMPLATE_HASH = "3528d0a9a8e14b671a8010e4153449e7e56fe32e8177a90f68053b3080a04846"
LAYERS = ("foundation", "explanation", "scenario", "application")
KINDS = ("basis", "see-also", "supersedes")

EDGE = """
on run argv
  set javascriptCode to item 1 of argv
  set requestedTab to item 2 of argv
  set expectedAccount to item 3 of argv
  if application "Microsoft Edge" is not running then error "请打开 Microsoft Edge"
  tell application "Microsoft Edge"
    repeat with browserWindow in windows
      set tabNumber to 0
      repeat with browserTab in tabs of browserWindow
        set tabNumber to tabNumber + 1
        if URL of browserTab starts with "https://blockchain.yeying.pub/" then
          set tabId to (id of browserTab) as text
          if requestedTab is "" or requestedTab is tabId then
            set responds to true
            if requestedTab is "" then
              try
                with timeout of 3 seconds
                  execute browserTab javascript "document.readyState"
                end timeout
              on error errorText number errorNumber
                if errorNumber is not -1712 then error errorText number errorNumber
                set responds to false
              end try
            end if
            if responds then
              with timeout of 3 seconds
                set reply to execute browserTab javascript javascriptCode
              end timeout
              if requestedTab is "" and reply is "started" then
                set active tab index of browserWindow to tabNumber
                set index of browserWindow to 1
                activate
                return tabId
              else if requestedTab is not "" then
                return reply
              end if
            end if
          end if
        end if
      end repeat
    end repeat
  end tell
  if requestedTab is not "" then error "知行图草稿标签页已关闭或切换到其他站点"
  error "请在 Edge 打开知行图并登录账户 " & expectedAccount & "。如果已有代填操作，请等它结束"
end run
"""

FILL = """
const byId = (id) => document.getElementById(id);
const loggedIn = () => byId("idStatus")?.textContent === "账户 " + input.account + " · 已连接钱包";
const state = document.documentElement.dataset;
if (location.origin !== input.site ||
    !loggedIn() || JSON.parse(state.zhixingFillResult || "{}").status === "running") return "unavailable";

const report = (status, message = "") => { state.zhixingFillResult = JSON.stringify({ status, message }); };
report("running");
const get = async (path) => {
  const response = await fetch(path, {
    headers: { Accept: "application/json" }, signal: AbortSignal.timeout(5000), cache: "no-store"
  });
  if (!response.ok) throw new Error(path + " 返回 HTTP " + response.status);
  return response.json();
};
const waitFor = async (read, message) => {
  for (let attempt = 0; attempt < 60; attempt++) {
    const result = read();
    if (result) return result;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(message);
};
const field = (id, value) => {
  const control = byId(id);
  control.value = String(value);
  if (control.value !== String(value)) throw new Error("无法设置字段 " + id);
  return control;
};
const fieldsMatch = () => byId("auDomain")?.value === String(input.domain) &&
  byId("auLayer")?.value === input.layer && byId("auSrc")?.value === input.source &&
  byId("auModel")?.value === input.model;

(async () => {
  const { found } = await get("/lookup?body=" + input.hash);
  if (found !== false) throw new Error("该正文可能已发布，请先核对链上记录");
  if (!loggedIn()) throw new Error("知行图账户已变更");
  if (!byId("auText")) {
    if (!byId("auNew")) byId("tab-knowledge")?.click();
    const create = await waitFor(() => byId("auNew"), "打开知识页超时");
    if (!byId("auText")) create.click();
  }
  const editor = await waitFor(() => byId("auText"), "打开新单元编辑器超时");
  if (!byId("auDomain")) throw new Error("当前编辑器用于修订已有单元");
  const previousText = editor.value;
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(previousText));
  const previousHash = Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, "0")).join("");
  const citationRows = () => byId("auCites").querySelectorAll("tr");
  const sameDraft = state.zhixingDraftHash === input.hash &&
    previousText === input.markdown && fieldsMatch();
  if ((!sameDraft && (previousHash !== input.templateHash || citationRows().length)) ||
      editor !== byId("auText") || editor.value !== previousText || !loggedIn()) {
    throw new Error("检测到其他待编辑草稿，请保留原草稿并在新标签页打开知行图");
  }
  field("auText", input.markdown).dispatchEvent(new Event("input", { bubbles: true }));
  field("auDomain", input.domain);
  field("auSrc", input.source).dispatchEvent(new Event("change", { bubbles: true }));
  field("auModel", input.model);
  await waitFor(() => byId("auSize")?.textContent.startsWith(input.bytes + " 字节"), "正文预览校验失败");
  field("auLayer", input.layer);
  state.zhixingDraftHash = input.hash;
  if (citationRows().length > input.cites.length + 1) throw new Error("表单中已有其他引用");
  for (const [index, cite] of input.cites.entries()) {
    const unit = await get("/rpc/unit/" + cite.unit);
    if (unit.versions?.length !== cite.version) {
      throw new Error("单元 #" + cite.unit + " 的当前版本与草稿引用不符");
    }
    if (!citationRows()[index + 1]) {
      field("auCiteId", cite.unit);
      field("auCiteKind", cite.kind);
      byId("auCiteAdd").click();
    }
    const row = await waitFor(
      () => byId("auCites")?.querySelectorAll("tr")[index + 1],
      "引用未添加 #" + cite.unit
    );
    if (!row.cells[0].textContent.startsWith("#" + cite.unit + " ") ||
        row.cells[1].querySelector("select").value !== cite.kind ||
        row.cells[2].textContent.trim() !== "v" + cite.version) {
      throw new Error("表单中的引用与草稿不符");
    }
  }
  if (editor !== byId("auText") || editor.value !== input.markdown ||
      !fieldsMatch() || !loggedIn()) {
    throw new Error("表单内容与草稿不符");
  }
  report("ready");
})().catch((error) => {
  report("error", error.message);
});
return "started";
"""


def citation(value):
    try:
        unit_text, version_text, kind = value.split(":")
        unit, version = int(unit_text), int(version_text)
        if not 1 <= unit < 2**53 or not 1 <= version < 2**32 or kind not in KINDS:
            raise ValueError
    except ValueError:
        raise argparse.ArgumentTypeError("引用格式为 单元:版本:类型，例如 12:2:see-also") from None
    return {"unit": unit, "version": version, "kind": kind}


def source_arg(value):
    """把 --source / --model 归一成 (种类, 模型标识)。

    模型标识按 kgraph.model_id 的规则转成小写连字符("GPT-6" → "gpt-6"),
    与 zx、kimport 一致——链上原样存这个字符串,同一个模型必须总是同一个标识,
    否则按来源统计时一个模型会被拆成两个。
    """
    import kgraph
    kind, _, model = value.partition(":")
    model = kgraph.model_id(model)
    if value == "human":
        return kind, ""
    if kind in ("model", "model-verified") and 1 <= len(model.encode("utf-8")) <= 64:
        return kind, model
    raise argparse.ArgumentTypeError(
        "来源应为 human、model:模型标识 或 model-verified:模型标识，模型标识上限 64 字节"
    )


def edge(javascript, account, tab=""):
    result = subprocess.run(
        ["osascript", "-", javascript, tab, account], input=EDGE, text=True,
        capture_output=True, timeout=15,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return result.stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("markdown", type=Path, help="知识单元的 Markdown 文件")
    parser.add_argument("--account", type=int, required=True, help="已登录知行图的账户编号")
    parser.add_argument("--domain", type=int, required=True, help="领域编号")
    parser.add_argument("--layer", choices=LAYERS, default="foundation", help="层级，默认为 foundation")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--source", type=source_arg, help="human、model:标识或 model-verified:标识")
    source.add_argument("--model", dest="source", type=lambda value: source_arg("model:" + value),
                        help="模型生成来源的简写，等同于 --source model:标识")
    parser.add_argument("--cite", action="append", type=citation, default=[],
                        help="单元:版本:类型，可多次指定")
    parser.add_argument("--preview", action="store_true", help="在终端打印字段和正文哈希")
    args = parser.parse_args()
    try:
        body = args.markdown.read_bytes()
        markdown = body.decode("utf-8")
        if not markdown.startswith("# ") or "\r" in markdown or len(body) > 64 * 1024:
            raise ValueError("正文须为带一级标题的 UTF-8 Markdown，使用 LF 换行，至多 64 KiB")
        if len(args.cite) > 32 or len({cite["unit"] for cite in args.cite}) != len(args.cite):
            raise ValueError("最多 32 条引用，每个单元只能引用一次")
        if not 1 <= args.account < 2**64 or not 0 <= args.domain < 2**32:
            raise ValueError("账户编号或领域编号无效")
        account = str(args.account)
        source_kind, model = args.source
        draft = {
            "site": SITE, "account": account, "domain": args.domain, "layer": args.layer,
            "source": source_kind, "model": model, "cites": args.cite, "bytes": len(body),
            "hash": hashlib.sha256(body).hexdigest(),
        }
        if args.preview:
            print(json.dumps(draft, ensure_ascii=False, indent=2))
            return
        payload = json.dumps({**draft, "markdown": markdown, "templateHash": TEMPLATE_HASH}, ensure_ascii=False)
        tab = edge("((input) => {" + FILL + "})(" + payload + ")", account)
        status_js = (
            "location.origin === " + json.dumps(SITE) +
            " ? (document.documentElement.dataset.zhixingFillResult || '{\"status\":\"missing\"}') : null"
        )
        deadline = time.monotonic() + 20 + 12 * len(args.cite)
        while time.monotonic() < deadline:
            result = json.loads(edge(status_js, account, tab))
            if result and result["status"] == "ready":
                print("已代填并核对表单。正文 SHA-256 " + draft["hash"])
                return
            if not result or result["status"] != "running":
                raise RuntimeError(result.get("message", "知行图标签页已变更") if result else "知行图标签页已变更")
            time.sleep(0.25)
        raise RuntimeError("代填超时，请查看 Edge 中的草稿")
    except subprocess.TimeoutExpired:
        parser.exit(1, "等待 Edge 响应超时，请检查知行图标签页\n")
    except (OSError, ValueError, RuntimeError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
