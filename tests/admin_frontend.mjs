import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { webcrypto } from "node:crypto";

// Minimal DOM doubles exercise request/state behavior without dependencies or a provider.
const html = readFileSync(new URL("../src/admin/static/index.html", import.meta.url), "utf8");
const script = readFileSync(new URL("../src/admin/static/app.js", import.meta.url), "utf8");
function harness() {
  const nodes = new Set(), ids = new Map(), calls = [], timers = new Map();
  let timerId = 0, handler = async (path) => ({ body: path === "/api/auth/state" ? { setup_required: false, authenticated: false } : {} });
  class Element {
    constructor(tag) {
      this.tagName = tag.toUpperCase(); this.children = []; this.listeners = {}; this.dataset = {}; this.attrs = {};
      this.value = ""; this.checked = false; this.disabled = false; this.hidden = false; this.open = false; this.className = ""; this.validity = { valid: true };
      nodes.add(this);
    }
    set id(value) { this._id = value; ids.set(value, this); }
    get id() { return this._id || ""; }
    set textContent(value) { this._text = String(value); for (const child of this.children) child.parentElement = null; this.children = []; }
    get textContent() { return (this._text || "") + this.children.map((child) => child.textContent).join(""); }
    get isConnected() { return Boolean(this.root || this.parentElement?.isConnected); }
    get classList() { return { toggle: (name, on) => { const set = new Set(this.className.split(/\s+/u)); if (on) set.add(name); else set.delete(name); this.className = [...set].join(" "); } }; }
    append(...items) { for (const item of items) { this.children.push(item); item.parentElement = this; if (this.tagName === "SELECT" && !this.value) this.value = item.value; } }
    prepend(...items) { for (const item of [...items].reverse()) { this.children.unshift(item); item.parentElement = this; } }
    replaceChildren(...items) { this.textContent = ""; this.append(...items); }
    setAttribute(key, value) {
      this.attrs[key] = String(value);
      if (key === "id") this.id = value;
      else if (key === "class") this.className = value;
      else if (key.startsWith("data-")) this.dataset[key.slice(5).replace(/-([a-z])/gu, (_, x) => x.toUpperCase())] = value;
      else if (["hidden", "disabled", "required", "checked", "open"].includes(key)) this[key] = true;
      else this[key] = value;
    }
    removeAttribute(key) { delete this.attrs[key]; if (key === "src") this.src = ""; }
    addEventListener(name, callback) { (this.listeners[name] ||= []).push(callback); }
    async emit(name, event = {}) { for (const callback of this.listeners[name] || []) await callback({ preventDefault() {}, ...event }); }
    showModal() { this.open = true; }
    close() { this.open = false; }
    reset() { for (const node of nodes) if (descendant(node, this) && ["INPUT", "TEXTAREA"].includes(node.tagName)) { node.value = ""; node.checked = false; } }
  }
  function descendant(node, parent) { while (node.parentElement) { if (node.parentElement === parent) return true; node = node.parentElement; } return false; }
  function matches(node, selector) {
    if (selector.startsWith("#")) return node.id === selector.slice(1);
    if (selector.startsWith(".")) return node.className.split(/\s+/u).includes(selector.slice(1));
    const match = selector.match(/^([a-z]+)?(?:\[([^=\]]+)(?:="([^"]*)")?\])?$/iu);
    if (!match) return false;
    if (match[1] && node.tagName !== match[1].toUpperCase()) return false;
    if (match[2]) {
      const key = match[2], value = key.startsWith("data-") ? node.dataset[key.slice(5).replace(/-([a-z])/gu, (_, x) => x.toUpperCase())] : node[key] ?? node.attrs[key];
      if (value === undefined) return false; if (match[3] !== undefined && String(value) !== match[3]) return false;
    }
    return true;
  }
  function queryAll(selector) {
    return [...nodes].filter((node) => node.isConnected && selector.split(",").some((part) => {
      const pieces = part.trim().split(/\s+/u);
      if (!matches(node, pieces.at(-1))) return false;
      if (pieces.length === 1) return true;
      let parent = node.parentElement; while (parent) { if (matches(parent, pieces[0])) return true; parent = parent.parentElement; } return false;
    }));
  }
  const root = new Element("document"); root.root = true;
  const stack = [root];
  for (const token of html.matchAll(/<\/?([a-z][a-z0-9-]*)\b([^>]*)>/giu)) {
    if (token[0].startsWith("</")) { const index = stack.findLastIndex((node) => node.tagName === token[1].toUpperCase()); if (index > 0) stack.length = index; continue; }
    const node = new Element(token[1]);
    for (const attr of token[2].matchAll(/([a-z][\w-]*)(?:="([^"]*)")?/giu)) node.setAttribute(attr[1], attr[2] ?? "");
    stack.at(-1).append(node);
    if (!["INPUT", "IMG", "META", "LINK", "BR", "HR"].includes(node.tagName)) stack.push(node);
  }
  const document = { hidden: false, getElementById: (id) => ids.get(id) || null, createElement: (tag) => new Element(tag), querySelectorAll: queryAll, querySelector: (selector) => queryAll(selector)[0] || null, listeners: {}, addEventListener(name, fn) { this.listeners[name] = fn; } };
  const window = { confirm: () => true, listeners: {}, addEventListener(name, fn) { this.listeners[name] = fn; } };
  const context = vm.createContext({
    document, window, URLSearchParams, AbortSignal, crypto: webcrypto, Uint8Array, console,
    setTimeout(fn, delay) { timers.set(++timerId, { fn, delay }); return timerId; }, clearTimeout(id) { timers.delete(id); },
    async fetch(path, options) {
      calls.push({ path, options, body: options.body ? JSON.parse(options.body) : undefined });
      const result = await handler(path, options);
      if (result.error) throw result.error;
      return { status: result.status || 200, ok: !result.status || result.status < 400, async text() { if (result.bodyError) throw result.bodyError; return result.raw ?? JSON.stringify(result.body); } };
    },
  });
  const exports = "globalThis.frontend = { state, groups, api, authView, bootstrap, loadConfig, refreshQq, refreshStatus, getQr, performMemory, mutateMemory, checkOperation, clearOperation, selectConversation, loadMemories, loadEvidence, loadContext, openMemoryEditor, saveMemory, readSettings, readForm, adoptJson, syncJson, secretChanges, clearSession, runAction, updateControls, requestId, inputs, secretInputs, report, saveConfig, settingInputs, renderSettings };";
  vm.runInContext(script.replace("  void bootstrap();\n})();", "  " + exports + "\n})();"), context);
  const app = context.frontend;
  assert.ok(app, "test injection found application bootstrap");
  function config() {
    const result = { web: { enabled: true, listen: "127.0.0.1:3210", secure_cookie: false } };
    for (const group of app.groups) for (const def of group.fields) {
      if (def.secret) continue;
      const keys = def.path.split("."); let obj = result; for (const key of keys.slice(0, -1)) obj = obj[key] ||= {};
      obj[keys.at(-1)] = def.type === "check" ? true : def.type === "number" ? def.min ?? 1 : def.type === "nullableNumber" ? null : ["lines", "ids"].includes(def.type) ? [] : def.type === "select" ? def.choices[0][0] : def.type === "json" ? { "默认": "帮助群成员" } : "value";
    }
    result.plugins.ai.defaults = { ...result.plugins.ai.defaults, persona: "默认", paused: false, once: false, recent_messages: 24, input_budget: 6000, output_budget: 800 };
    result.llm.api_key = ""; result.milky.access_token = null; result.qq_login.token = "";
    result.app.database = "data/test.db"; return result;
  }
  const sample = config();
  const snapshot = () => ({ config: structuredClone(sample), revision: "r1", secrets: { llm_api_key: { effective_set: true }, milky_access_token: { effective_set: false }, llbot_token: { effective_set: true } } });
  const conversation = (id = "1:group:10:0") => ({ id, scope: "1:group:10", bot_id: 1, scene: "group", peer_id: 10, owner_id: null, mode: "group", scope_mode: "group", is_current: true, epoch: 0, memory_revision: 1, settings: sample.plugins.ai.defaults, summary: "", context_count: 2, memory_count: 1 });
  async function login() { app.authView({ authenticated: true, csrf_token: "csrf-test", setup_required: false }); handler = async (path) => ({ body: path === "/api/config" ? snapshot() : {} }); await app.loadConfig(); }
  return { app, document, window, ids, calls, timers, sample, snapshot, conversation, login, setHandler(fn) { handler = fn; } };
}

test("static UI removes admin-token entry and every literal DOM ID resolves", () => {
  const h = harness();
  for (const match of script.matchAll(/\$\("([^"]+)"\)/gu)) assert.ok(h.ids.has(match[1]), match[1]);
  assert.doesNotMatch(script + html, /localStorage|sessionStorage|Authorization|admin-token|access\.admins|\/bot status|\/bot plugins|\/ai(?:\s|<)/u);
  assert.doesNotMatch(script, /innerHTML|insertAdjacentHTML|setInterval/u);
});
test("setup and login state use password and CSRF session; writes send no bearer", async () => {
  const h = harness();
  h.app.authView({ setup_required: true, authenticated: false });
  assert.equal(h.ids.get("setup-code-field").hidden, false);
  await h.login();
  h.setHandler(async () => ({ body: {} })); await h.app.api("/api/runtime/apply", "POST", {});
  const request = h.calls.at(-1); assert.equal(request.options.credentials, "same-origin");
  assert.equal(request.options.headers["X-CSRF-Token"], "csrf-test");
  assert.equal(request.options.headers.Authorization, undefined);
});
test("advanced JSON preserves edits and rejects read-only Web modifications", async () => {
  const h = harness(); await h.login();
  const changed = structuredClone(h.sample); changed.web.listen = "0.0.0.0:9999";
  h.ids.get("config-json").value = JSON.stringify(changed);
  assert.throws(() => h.app.adoptJson(), /web/u);
  changed.web.listen = h.sample.web.listen; changed.plugins.ai.defaults.input_budget = 12000;
  h.ids.get("config-json").value = JSON.stringify(changed); h.app.adoptJson();
  assert.equal(h.app.readForm().plugins.ai.defaults.input_budget, 12000);
});
test("QQ authentication failures pause; manual retry resumes; online polls at least 30 seconds", async () => {
  const h = harness(); await h.login();
  h.setHandler(async () => ({ status: 502, body: { code: "qq_auth", error: "凭据不正确" } }));
  await h.app.refreshQq(); assert.equal(h.app.state.qqPollingPaused, true);
  const before = h.calls.length; await h.app.refreshQq(); assert.equal(h.calls.length, before);
  h.setHandler(async () => ({ body: { online: true, uin: "123", nick: "<img>", poll_interval_ms: 3000 } }));
  await h.app.refreshQq(true); assert.equal(h.app.state.qqPollingPaused, false);
  assert.equal(h.calls.at(-1).path, "/api/qq/status?force=true"); assert.equal(h.app.state.qqInterval, 30000);
  assert.equal(h.ids.get("account-name").textContent, "<img>");
});
test("QQ transient failures back off and hidden pages make no polling request", async () => {
  const h = harness(); await h.login();
  h.setHandler(async () => ({ status: 502, body: { code: "qq_transient", error: "稍后", retry_after_ms: 4000 } }));
  await h.app.refreshQq(); assert.equal(h.app.state.qqInterval, 4000);
  await h.app.refreshQq(); assert.equal(h.app.state.qqInterval, 6000);
  h.document.hidden = true; const before = h.calls.length; await h.app.refreshQq(); await h.app.refreshStatus();
  assert.equal(h.calls.length, before);
});
test("an old QQ response cannot overwrite a changed configuration", async () => {
  const h = harness(); await h.login(); let resolve;
  h.setHandler((path) => path === "/api/config" ? Promise.resolve({ body: h.snapshot() }) : new Promise((done) => { resolve = done; }));
  const old = h.app.refreshQq(); await Promise.resolve(); await h.app.loadConfig();
  resolve({ body: { online: true, nick: "旧账号", uin: "111" } }); await old;
  assert.equal(h.app.state.qq, null);
});
test("memory is loaded only for a selected scope; stale responses cannot cross scopes", async () => {
  const h = harness(); await h.login(); assert.equal(h.calls.some((call) => call.path.includes("/memory/")), false);
  const a = h.conversation(), b = { ...h.conversation("1:friend:20:0"), scope: "1:friend:20", scene: "friend", peer_id: 20, owner_id: 20 };
  let resolveA;
  h.setHandler((path) => {
    if (path.includes("/conversations/")) return Promise.resolve({ body: path.includes(encodeURIComponent(a.id)) ? a : b });
    if (path.includes("conversation_id=" + encodeURIComponent(a.id))) return new Promise((done) => { resolveA = done; });
    return Promise.resolve({ body: { items: [{ id: 2, conversation_id: b.id, subject_id: 20, text: "B", kind: "fact", active: true, origin: "manual" }], page: 1, total: 1, total_pages: 1 } });
  });
  const loadingA = h.app.selectConversation(a); for (let i = 0; i < 8 && !resolveA; i++) await Promise.resolve();
  assert.ok(resolveA); await h.app.selectConversation(b);
  resolveA({ body: { items: [{ id: 1, conversation_id: a.id, text: "A" }], page: 1, total: 1, total_pages: 1 } }); await loadingA;
  assert.equal(h.app.state.memory.selected.id, b.id); assert.equal(h.app.state.memory.rows[0].text, "B");
});
test("memory write timeout queries request ledger without resubmitting", async () => {
  const h = harness(); await h.login(); const cv = h.conversation(); h.app.state.memory.selected = cv; h.app.state.memory.detail = cv;
  let completed = false;
  h.setHandler(async (path) => path.endsWith("/mutate") ? { error: new Error("lost") } : { body: { request_id: "request-test", status: "completed" } });
  await h.app.performMemory("/api/memory/mutate", { request_id: "request-test" }, () => {});
  assert.equal(h.app.state.pendingOperation.requestId, "request-test");
  h.app.state.pendingOperation.complete = async () => { completed = true; };
  await h.app.checkOperation();
  assert.equal(completed, true); assert.equal(h.app.state.pendingOperation, null);
  assert.equal(h.calls.filter((call) => call.path.endsWith("/mutate")).length, 1);
});
test("body interruption and invalid success JSON are uncertain writes", async () => {
  for (const result of [{ bodyError: new Error("body lost") }, { raw: "invalid json" }]) {
    const h = harness(); await h.login(); h.setHandler(async () => result);
    await h.app.performMemory("/api/memory/mutate", { request_id: "test-ambiguous" }, () => {});
    assert.equal(h.app.state.pendingOperation.requestId, "test-ambiguous");
  }
});
test("optimistic conflict retains memory editor text and disables unsafe writes", async () => {
  const h = harness(); await h.login(); const cv = h.conversation(); h.app.state.memory.selected = cv; h.app.state.memory.detail = cv;
  h.ids.get("edit-memory-text").value = "我正在编辑的资料"; h.app.state.memory.editorDirty = true;
  h.setHandler(async () => ({ status: 409, body: { error: "version conflict" } }));
  await assert.rejects(h.app.mutateMemory({ op: "reset" }), /version conflict/u);
  assert.equal(h.ids.get("edit-memory-text").value, "我正在编辑的资料"); assert.equal(h.app.state.memory.conflict, true);
  assert.equal(h.ids.get("save-memory").disabled, true);
});
test("malformed memory scope response is rejected before display", async () => {
  const h = harness(); await h.login(); const cv = h.conversation(); h.app.state.memory.selected = cv; h.app.state.memory.detail = cv;
  h.setHandler(async () => ({ body: { items: [{ id: 1, conversation_id: "other", text: "secret" }], page: 1, total: 1, total_pages: 1 } }));
  await assert.rejects(h.app.loadMemories(), /范围/u); assert.equal(h.app.state.memory.rows.length, 0);
});
test("manual create is locked by the server and edit defaults to locked", async () => {
  const h = harness(); await h.login(); const cv = h.conversation(); h.app.state.memory.detail = cv; h.app.state.memory.selected = cv;
  h.app.openMemoryEditor(); assert.equal(h.ids.get("edit-memory-locked").checked, true); assert.equal(h.ids.get("edit-memory-locked").disabled, true);
  h.app.openMemoryEditor({ id: 1, subject_id: 7, text: "资料", kind: "fact", locked: false, active: false });
  assert.equal(h.ids.get("edit-memory-locked").checked, true); assert.equal(h.ids.get("edit-memory-subject").disabled, true);
});
test("true logout calls the server before removing local session", async () => {
  const h = harness(); await h.login(); h.setHandler(async () => ({ body: {} }));
  await h.ids.get("logout").emit("click"); for (let i = 0; i < 10; i++) await Promise.resolve();
  assert.ok(h.calls.some((call) => call.path === "/api/auth/logout" && call.options.method === "POST")); assert.equal(h.app.state.authenticated, false);
});
