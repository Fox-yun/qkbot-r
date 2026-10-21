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
    document, window, URL, URLSearchParams, TextEncoder, AbortSignal, crypto: webcrypto, Uint8Array, console,
    setTimeout(fn, delay) { timers.set(++timerId, { fn, delay }); return timerId; }, clearTimeout(id) { timers.delete(id); },
    async fetch(path, options) {
      calls.push({ path, options, body: options.body ? JSON.parse(options.body) : undefined });
      const result = await handler(path, options);
      if (result.error) throw result.error;
      return { status: result.status || 200, ok: !result.status || result.status < 400, async text() { if (result.bodyError) throw result.bodyError; return result.raw ?? JSON.stringify(result.body); } };
    },
  });
  const exports = "globalThis.frontend = { state, groups, api, authView, bootstrap, loadConfig, refreshQq, refreshStatus, getQr, performMemory, mutateMemory, checkOperation, clearOperation, selectConversation, loadMemories, loadEvidence, loadContext, openMemoryEditor, saveMemory, readSettings, readForm, adoptJson, syncJson, secretChanges, clearSession, runAction, updateControls, requestId, inputs, secretInputs, report, saveConfig, settingInputs, renderSettings, showView, loadContacts, toggleContact, renderContacts, invalidateContacts, renderStatus, personaChoices, fillPersonaSelect, refreshPersonaSelectors, renderPersonaLibrary, renderPersonaPreview, createPersona, deletePersona, applyGroupPersona, fetchModels, renderModels, invalidateModels, openScopePlugins, loadScopePlugins, toggleScopePlugin, refreshExternalPlugins };";
  vm.runInContext(script.replace("  void bootstrap();\n})();", "  " + exports + "\n})();"), context);
  const app = context.frontend;
  assert.ok(app, "test injection found application bootstrap");
  function config() {
    const result = { web: { enabled: true, listen: "127.0.0.1:3210", secure_cookie: false } };
    for (const group of app.groups) for (const def of group.fields) {
      if (def.secret) continue;
      const keys = def.path.split("."); let obj = result; for (const key of keys.slice(0, -1)) obj = obj[key] ||= {};
      obj[keys.at(-1)] = def.type === "check" ? true : def.type === "number" ? def.min ?? 1 : def.type === "nullableNumber" ? null : ["lines", "ids", "tools"].includes(def.type) ? [] : def.type === "select" ? def.choices[0][0] : def.type === "json" ? { "默认": "帮助群成员" } : "value";
    }
    result.plugins.ai.defaults = { ...result.plugins.ai.defaults, persona: "默认", paused: false, once: false, recent_messages: 24, input_budget: 6000, output_budget: 800 };
    result.plugins.ai.personas = { "默认": "自定义帮助群成员" };
    result.llm.api_key = ""; result.milky.access_token = null; result.qq_login.token = "";
    result.app.database = "data/test.db"; return result;
  }
  const sample = config();
  const snapshot = () => ({ config: structuredClone(sample), revision: "r1", tool_catalog: [{ name: "clock_now", description: "读取当前时间", plugin_id: "clock", effect: "read" }, { name: "reminder_create", description: "创建提醒", plugin_id: "reminder", effect: "local_write" }], persona_templates: { "默认": "内置默认正文", "模板甲": "内置模板正文" }, secrets: { llm_api_key: { effective_set: true }, milky_access_token: { effective_set: false }, llbot_token: { effective_set: true } } });
  const conversation = (id = "1:group:10:0") => ({ id, scope: "1:group:10", bot_id: 1, scene: "group", peer_id: 10, owner_id: null, mode: "group", scope_mode: "group", is_current: true, epoch: 0, memory_revision: 1, settings: sample.plugins.ai.defaults, summary: "", context_count: 2, memory_count: 1 });
  async function login() { app.authView({ authenticated: true, csrf_token: "csrf-test", setup_required: false }); handler = async (path) => ({ body: path === "/api/config" ? snapshot() : {} }); await app.loadConfig(); }
  return { app, document, window, ids, calls, timers, sample, snapshot, conversation, login, setHandler(fn) { handler = fn; } };
}

test("static UI removes admin-token entry and every literal DOM ID resolves", () => {
  const h = harness();
  for (const match of script.matchAll(/\$\("([^"]+)"\)/gu)) assert.ok(h.ids.has(match[1]), match[1]);
  assert.doesNotMatch(script + html, /localStorage|sessionStorage|Authorization|admin-token|access\.admins|\/bot status|\/bot plugins|\/ai(?:\s|<)/u);
  assert.doesNotMatch(script, /innerHTML|insertAdjacentHTML|setInterval/u);
  assert.doesNotMatch(script + html, /access\.groups|access\.friends|白名单/u);
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

const contacts = (botId = "123", count = 1) => ({
  bot_id: botId, fetched_at: 1800000000,
  groups: Array.from({ length: count }, (_, index) => ({ id: String(1000 + index), name: "群 " + index, remark: index === 0 ? "<img>测试备注" : "", enabled: true })),
  friends: [{ id: "789", name: "小明", remark: "朋友备注", enabled: true }],
});
async function contactPage(h, data = contacts()) {
  await h.login(); h.app.state.view = "contacts";
  h.setHandler(async () => ({ body: data })); await h.app.loadContacts();
}

test("contacts work while bot is stopped, paginate and search without additional polling", async () => {
  const h = harness(); await h.login();
  h.app.state.status = { online: false, phase: "stopped", bot_id: null, runtime_generation: 3 };
  h.app.state.view = "contacts";
  h.setHandler(async () => ({ body: contacts("123", 65) }));
  const timers = h.timers.size; await h.app.loadContacts();
  assert.equal(h.app.state.contacts.phase, "ready"); assert.equal(h.timers.size, timers);
  assert.equal(h.ids.get("contacts-account").textContent, "QQ 123");
  assert.equal(h.ids.get("contacts-list").children.length, 30);
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, false);
  h.app.renderStatus(); h.app.renderStatus();
  assert.equal(h.app.state.contacts.phase, "ready", "a stable stopped runtime must not invalidate contact controls");
  await h.ids.get("contacts-next").emit("click"); assert.equal(h.app.state.contacts.page, 2);
  await h.ids.get("contacts-next").emit("click"); assert.equal(h.ids.get("contacts-list").children.length, 5);
  const requests = h.calls.length;
  h.ids.get("contacts-search").value = "测试备注"; await h.ids.get("contacts-search").emit("input");
  assert.equal(h.app.state.contacts.page, 1); assert.equal(h.ids.get("contacts-list").children.length, 1);
  assert.match(h.ids.get("contacts-list").textContent, /<img>测试备注/u);
  h.ids.get("contacts-search").value = "1064"; await h.ids.get("contacts-search").emit("input");
  assert.match(h.ids.get("contacts-list").textContent, /1064/u);
  h.ids.get("contacts-search").value = ""; await h.ids.get("contacts-search").emit("input");
  await h.ids.get("contacts-friends").emit("click");
  assert.equal(h.ids.get("contacts-list").children.length, 1); assert.match(h.ids.get("contacts-list").textContent, /朋友备注/u);
  assert.equal(h.calls.length, requests, "search and pagination must use the loaded snapshot");
});

test("contact switch submits bound string IDs with CSRF and confirms only the matching response", async () => {
  const h = harness(); await contactPage(h);
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
  const changing = h.app.toggleContact("group", "1000", false); await Promise.resolve();
  const call = h.calls.at(-1);
  assert.equal(call.path, "/api/qq/contacts/toggle");
  assert.deepEqual(call.body, { bot_id: "123", scene: "group", peer_id: "1000", enabled: false });
  assert.equal(call.options.headers["X-CSRF-Token"], "csrf-test");
  assert.equal(h.app.state.contacts.groups[0].enabled, false);
  assert.match(h.ids.get("contacts-list").textContent, /保存中/u);
  assert.doesNotMatch(h.ids.get("contacts-message").textContent, /即时生效/u);
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, true);
  resolve({ body: call.body }); await changing;
  assert.match(h.ids.get("contacts-message").textContent, /已关闭，.*即时生效/u);
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, false);
});

test("lost or mismatched toggle acknowledgement rolls back the display and requires a refresh", async () => {
  for (const result of [{ error: new Error("lost") }, { body: { bot_id: "456", scene: "group", peer_id: "1000", enabled: false } }]) {
    const h = harness(); await contactPage(h); h.setHandler(async () => result);
    await h.app.toggleContact("group", "1000", false);
    assert.equal(h.app.state.contacts.groups[0].enabled, true); assert.equal(h.app.state.contacts.phase, "uncertain");
    assert.match(h.ids.get("contacts-message").textContent, /未确认.*刷新/u);
    assert.doesNotMatch(h.ids.get("contacts-message").textContent, /即时生效/u);
    assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, true);
    const requests = h.calls.length; await h.app.toggleContact("group", "1000", false); assert.equal(h.calls.length, requests);
    h.setHandler(async () => ({ body: { ...contacts(), groups: [{ ...contacts().groups[0], enabled: false }] } }));
    await h.app.loadContacts(true); assert.equal(h.calls.at(-1).path, "/api/qq/contacts?force=true");
    assert.equal(h.app.state.contacts.phase, "ready"); assert.equal(h.app.state.contacts.groups[0].enabled, false);
  }
});

test("contact responses from an old page, login or account cannot restore the old list", async () => {
  for (const invalidate of [
    (h) => h.app.showView("overview"),
    (h) => h.app.clearSession(),
    (h) => { h.app.state.status = { online: false, phase: "stopped", runtime_generation: 4 }; h.app.renderStatus(); },
  ]) {
    const h = harness(); await h.login(); h.app.state.view = "contacts";
    h.app.state.status = { online: false, phase: "stopped", runtime_generation: 3 };
    let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
    const old = h.app.loadContacts(); await Promise.resolve(); invalidate(h);
    resolve({ body: contacts() }); await old;
    assert.equal(h.app.state.contacts.botId, ""); assert.equal(h.ids.get("contacts-list").children.length, 0);
  }
  const h = harness(); await contactPage(h);
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
  const old = h.app.toggleContact("group", "1000", false); await Promise.resolve();
  h.app.state.status = { phase: "running", online: true, bot_id: "456", runtime_generation: 2 }; h.app.renderStatus();
  resolve({ body: { bot_id: "123", scene: "group", peer_id: "1000", enabled: false } }); await old;
  assert.equal(h.app.state.contacts.botId, ""); assert.doesNotMatch(h.ids.get("contacts-message").textContent, /即时生效/u);
});

test("stale account conflicts disable the old list; offline, empty and invalid data are clear", async () => {
  const h = harness(); await contactPage(h);
  h.setHandler(async () => ({ status: 409, body: { code: "contacts_stale", error: "账号变化" } }));
  await h.app.toggleContact("group", "1000", false);
  assert.equal(h.app.state.contacts.botId, ""); assert.equal(h.ids.get("contacts-list").children.length, 0);
  assert.match(h.ids.get("contacts-message").textContent, /旧列表已停用/u);
  h.setHandler(async () => ({ status: 502, body: { code: "contacts_offline", error: "QQ 未登录" } }));
  await h.app.loadContacts(true); assert.equal(h.app.state.contacts.phase, "offline");
  assert.match(h.ids.get("contacts-message").textContent, /QQ 已登录.*手动刷新/u);
  h.setHandler(async () => ({ body: { ...contacts(), groups: [], friends: [] } }));
  await h.app.loadContacts(true); assert.match(h.ids.get("contacts-message").textContent, /暂无群聊/u);
  h.setHandler(async () => ({ body: { ...contacts(), groups: [contacts().groups[0], contacts().groups[0]] } }));
  await h.app.loadContacts(true); assert.equal(h.app.state.contacts.phase, "error");
  assert.equal(h.ids.get("contacts-list").children.length, 0);
});

test("offline contact snapshots allow account-bound disabling but never enabling", async () => {
  const h = harness(); await contactPage(h);
  h.setHandler(async () => ({ status: 502, body: { code: "contacts_offline", error: "offline" } }));
  await h.app.loadContacts(true);
  assert.equal(h.app.state.contacts.phase, "cached");
  assert.equal(h.ids.get("contacts-account").textContent, "上次读取 QQ 123");
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, false);
  h.setHandler(async (_path, options) => ({ body: JSON.parse(options.body) }));
  await h.app.toggleContact("group", "1000", false);
  assert.deepEqual(h.calls.at(-1).body, { bot_id: "123", scene: "group", peer_id: "1000", enabled: false });
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, true);
  const requests = h.calls.length;
  await h.app.toggleContact("group", "1000", true);
  assert.equal(h.calls.length, requests);
  assert.match(h.ids.get("contacts-message").textContent, /刷新后才能开启/u);
  h.setHandler(async () => ({ body: { ...contacts(), groups: [{ ...contacts().groups[0], enabled: false }] } }));
  await h.app.loadContacts(true);
  assert.equal(h.app.state.contacts.phase, "ready");
  assert.equal(h.document.querySelectorAll("[data-contact-toggle]")[0].disabled, false);
});

test("offline fallback cannot restore a snapshot invalidated by an account change", async () => {
  const h = harness(); await contactPage(h);
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
  const pending = h.app.loadContacts(true); await Promise.resolve();
  h.app.invalidateContacts("账号已变化", "error");
  resolve({ status: 502, body: { code: "contacts_offline", error: "offline" } }); await pending;
  assert.equal(h.app.state.contacts.botId, "");
  assert.equal(h.ids.get("contacts-list").children.length, 0);
});

test("QQ offline status keeps disable-only contacts and a later account switch clears them", async () => {
  const h = harness(); await contactPage(h);
  h.app.state.view = "qq"; h.app.state.qq = { online: true, uin: "123" };
  h.setHandler(async () => ({ body: { online: false, uin: null, state: "offline" } }));
  await h.app.refreshQq(true);
  assert.equal(h.app.state.contacts.phase, "cached");
  assert.equal(h.app.state.contacts.botId, "123");
  h.setHandler(async () => ({ body: { online: true, uin: "456", state: "online" } }));
  await h.app.refreshQq(true);
  assert.equal(h.app.state.contacts.botId, "");
  assert.equal(h.ids.get("contacts-list").children.length, 0);
});

test("reply trigger switch roundtrips independently in global and conversation settings", async () => {
  const h = harness(); await h.login();
  const global = h.app.inputs.get("plugins.ai.defaults.reply_to_bot").control;
  assert.equal(global.checked, true);
  global.checked = false; await global.emit("change");
  assert.equal(h.app.readForm().plugins.ai.defaults.reply_to_bot, false);
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  const local = h.app.settingInputs.get("reply_to_bot").input;
  assert.equal(local.checked, true, "unsaved global changes do not overwrite conversation settings");
  local.checked = false; await local.emit("change");
  assert.equal(h.app.readSettings().reply_to_bot, false);
  assert.equal(h.app.readSettings().memory_write, true);
});

test("default and conversation personas select merged templates with custom precedence", async () => {
  const h = harness(); await h.login();
  const defaults = h.app.inputs.get("plugins.ai.defaults.persona").control;
  assert.equal(defaults.tagName, "SELECT"); assert.equal(defaults.children.length, 2);
  assert.equal(h.app.personaChoices(h.app.state.draft).get("默认"), "自定义帮助群成员");
  assert.equal(h.app.inputs.has("plugins.ai.personas"), false, "the human form must not expose a persona JSON field");
  defaults.value = "模板甲"; await defaults.emit("change");
  assert.equal(h.app.readForm().plugins.ai.defaults.persona, "模板甲");
  assert.deepEqual(Object.keys(h.app.readForm().plugins.ai.personas), ["默认"], "choosing a template must not copy it into custom configuration");
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  const settings = h.app.settingInputs.get("persona").input;
  assert.equal(settings.tagName, "SELECT"); settings.value = "模板甲"; await settings.emit("change");
  assert.equal(h.app.readSettings().persona, "模板甲");
  h.ids.get("persona-list").value = "模板甲"; await h.ids.get("persona-list").emit("change");
  assert.equal(h.ids.get("persona-text").value, "内置模板正文"); assert.equal(h.ids.get("persona-text").readOnly, true);
});

const groupPersonaResult = (payload) => ({ ...payload, status: "completed", affected_groups: 2, affected_conversations: 3, scopes: [payload.bot_id + ":group:1000", payload.bot_id + ":group:1001"] });
const emptyConversations = { items: [], page: 1, total_pages: 1, total: 0 };

test("all-group persona choices use saved entries, start unselected and retain removed selections explicitly", async () => {
  const h = harness(); await h.login(); const picker = h.ids.get("group-persona-select");
  assert.equal(picker.value, ""); assert.equal(h.ids.get("apply-group-persona").disabled, true);
  assert.deepEqual(picker.children.map((option) => option.value), ["", "默认", "模板甲"]);
  h.app.state.draft.plugins.ai.personas["未保存"] = "草稿正文"; h.app.refreshPersonaSelectors();
  assert.equal(picker.children.some((option) => option.value === "未保存"), false);
  picker.value = "模板甲"; await picker.emit("change");
  h.app.refreshPersonaSelectors(); assert.equal(picker.value, "模板甲");
  const updated = h.snapshot(); updated.persona_templates = { "模板乙": "新模板" };
  h.setHandler(async () => ({ body: updated })); await h.app.loadConfig();
  assert.equal(picker.value, "模板甲"); assert.equal(picker.attrs["aria-invalid"], "true");
  assert.equal(h.ids.get("apply-group-persona").disabled, true);
  await assert.rejects(h.app.applyGroupPersona(), /已失效/u);
  assert.equal(h.calls.some((call) => call.options.method === "POST"), false);
});

test("all-group application binds fresh account and sends one CSRF mutation without changing defaults or friend drafts", async () => {
  const h = harness(); await h.login(); const picker = h.ids.get("group-persona-select");
  picker.value = "模板甲";
  h.app.state.draft.plugins.ai.defaults.persona = "草稿默认"; h.app.state.dirty = true;
  h.app.state.status = { bot_id: 999 }; h.app.state.contacts.botId = "999";
  const friend = { ...h.conversation("123:friend:20:0"), bot_id: 123, scene: "friend", scope: "123:friend:20" };
  h.app.state.memory.selected = friend; h.app.state.memory.detail = friend; h.app.state.memory.settingsDirty = true;
  h.app.renderSettings(); const input = h.app.settingInputs.get("input_budget").input; input.value = "12000";
  h.ids.get("scope-scene").value = "friend"; h.ids.get("scope-peer").value = "20";
  let confirmation = "", payload;
  h.window.confirm = (text) => { confirmation = text; return true; };
  h.setHandler(async (path, options) => {
    if (path === "/api/qq/contacts?force=true") return { body: contacts("123", 2) };
    if (path === "/api/ai/groups/persona") { payload = JSON.parse(options.body); return { body: groupPersonaResult(payload) }; }
    return { body: emptyConversations };
  });
  await h.app.runAction(h.app.applyGroupPersona);
  assert.match(confirmation, /QQ 123.*全部 2 个群/u); assert.match(confirmation, /好友、全局默认、历史和记忆保持不变/u);
  assert.equal(payload.bot_id, "123"); assert.equal(payload.persona, "模板甲"); assert.match(payload.request_id, /^[0-9a-f-]{36}$/u);
  const mutation = h.calls.find((call) => call.path === "/api/ai/groups/persona");
  assert.equal(mutation.options.headers["X-CSRF-Token"], "csrf-test"); assert.deepEqual(Object.keys(payload).sort(), ["bot_id", "persona", "request_id"]);
  assert.equal(h.app.state.config.plugins.ai.defaults.persona, "默认"); assert.equal(h.app.state.draft.plugins.ai.defaults.persona, "草稿默认");
  assert.equal(h.app.state.dirty, true); assert.equal(h.app.state.memory.detail, friend);
  assert.equal(h.app.settingInputs.get("input_budget").input, input); assert.equal(input.value, "12000");
  assert.equal(h.app.state.memory.settingsDirty, true); assert.equal(h.app.state.memory.conflict, false);
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
  assert.match(h.ids.get("group-persona-message").textContent, /2 个群.*3 个已有会话/u);
});

test("all-group success reloads affected group versions but preserves unsaved group edits", async () => {
  for (const dirty of [false, true]) {
    const h = harness(); await h.login(); h.ids.get("group-persona-select").value = "模板甲";
    const group = { ...h.conversation("123:group:1000:0"), bot_id: 123, peer_id: 1000, scope: "123:group:1000" };
    h.app.state.memory.selected = group; h.app.state.memory.detail = group; h.app.renderSettings();
    const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000";
    h.app.state.memory.settingsDirty = dirty; h.app.state.memory.editorDirty = dirty;
    h.ids.get("edit-memory-text").value = "未保存记忆正文";
    const refreshed = { ...group, epoch: group.epoch + 1, memory_revision: group.memory_revision + 1, settings: { ...group.settings, persona: "模板甲" }, summary: "原摘要", context_count: 9 };
    h.setHandler(async (path, options) => {
      if (path.startsWith("/api/qq/contacts")) return { body: contacts("123", 2) };
      if (path === "/api/ai/groups/persona") return { body: groupPersonaResult(JSON.parse(options.body)) };
      return { body: path.includes("/conversations/") ? refreshed : emptyConversations };
    });
    await h.app.runAction(h.app.applyGroupPersona);
    if (dirty) {
      assert.equal(h.app.state.memory.detail, group); assert.equal(h.app.state.memory.conflict, true);
      assert.equal(h.app.settingInputs.get("input_budget").input, budget); assert.equal(budget.value, "12000");
      assert.equal(h.ids.get("edit-memory-text").value, "未保存记忆正文");
      assert.equal(h.ids.get("save-conversation-settings").disabled, true); assert.equal(h.ids.get("save-memory").disabled, true);
      assert.match(h.ids.get("group-persona-message").textContent, /未保存编辑已保留/u);
      assert.equal(h.calls.some((call) => call.path.includes("/conversations/")), false);
    } else {
      assert.equal(h.app.state.memory.detail.epoch, refreshed.epoch); assert.equal(h.app.state.memory.detail.summary, "原摘要");
      assert.equal(h.app.state.memory.detail.context_count, 9); assert.equal(h.app.state.memory.conflict, false);
      assert.equal(h.app.settingInputs.get("persona").input.value, "模板甲");
    }
  }
});

test("all-group application does not write on empty groups, unavailable contacts or dismissed confirmation", async () => {
  for (const scenario of ["empty", "offline", "invalid", "cancel"]) {
    const h = harness(); await h.login(); h.ids.get("group-persona-select").value = "模板甲";
    h.window.confirm = () => false;
    h.setHandler(async () => scenario === "offline" ? { status: 502, body: { error: "QQ 未连接", code: "contacts_offline" } }
      : { body: scenario === "invalid" ? { ...contacts(), bot_id: "bad" } : contacts("123", scenario === "empty" ? 0 : 2) });
    await h.app.runAction(h.app.applyGroupPersona);
    assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 0, scenario);
    assert.equal(h.app.state.groupPersona.saving, false); assert.equal(h.app.state.pendingOperation, null);
    assert.doesNotMatch(h.ids.get("group-persona-message").textContent, /个群已使用/u);
  }
});

test("all-group account or unapplied-persona rejection preserves selection and offers no false success", async () => {
  for (const response of [
    { status: 409, body: { error: "账号已变化，请刷新后重试", code: "contacts_stale" } },
    { status: 400, body: { error: "当前运行配置中没有该人设，请先应用全局配置" } },
  ]) {
    const h = harness(); await h.login(); h.ids.get("group-persona-select").value = "模板甲";
    h.setHandler(async (path) => path.startsWith("/api/qq/contacts") ? { body: contacts("123", 2) } : response);
    await h.app.runAction(h.app.applyGroupPersona);
    assert.equal(h.ids.get("group-persona-select").value, "模板甲"); assert.equal(h.app.state.pendingOperation, null);
    assert.equal(h.app.state.groupPersona.kind, "error"); assert.equal(h.ids.get("group-persona-message").textContent, response.body.error);
    assert.equal(h.app.state.config.plugins.ai.defaults.persona, "默认");
  }
});

test("uncertain all-group writes query their exact receipt without resubmitting or accepting another operation", async () => {
  for (const failure of ["network", "invalid", "mismatch"]) {
    const h = harness(); await h.login(); h.ids.get("group-persona-select").value = "模板甲";
    let payload;
    h.setHandler(async (path, options) => {
      if (path.startsWith("/api/qq/contacts")) return { body: contacts("123", 2) };
      payload = JSON.parse(options.body);
      return failure === "network" ? { error: new Error("lost response") } : failure === "invalid" ? { raw: "bad-json" }
        : { body: { ...groupPersonaResult(payload), bot_id: "999" } };
    });
    await h.app.runAction(h.app.applyGroupPersona);
    assert.equal(h.app.state.pendingOperation.requestId, payload.request_id); assert.equal(h.ids.get("apply-group-persona").disabled, true);
    await h.app.applyGroupPersona(); assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
    h.setHandler(async () => ({ body: { ...groupPersonaResult(payload), request_id: "other-operation" } }));
    await assert.rejects(h.app.checkOperation(), /未能确认/u); assert.equal(h.app.state.pendingOperation.requestId, payload.request_id);
    h.setHandler(async (path) => ({ body: path.startsWith("/api/memory/operations/") ? groupPersonaResult(payload) : emptyConversations }));
    await h.app.checkOperation();
    assert.equal(h.app.state.pendingOperation, null); assert.match(h.ids.get("group-persona-message").textContent, /2 个群已使用/u);
    assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
  }
});

test("all-group in-flight work is single-flight and late results cannot enter a new login session", async () => {
  for (const stage of ["contacts", "mutation"]) {
    const h = harness(); await h.login(); h.ids.get("group-persona-select").value = "模板甲";
    let resolve, payload;
    h.setHandler((path, options) => {
      if (path.startsWith("/api/qq/contacts") && stage === "mutation") return Promise.resolve({ body: contacts("123", 2) });
      if (options.body) payload = JSON.parse(options.body);
      return new Promise((done) => { resolve = done; });
    });
    const old = h.app.applyGroupPersona(); for (let i = 0; i < 10 && !resolve; i++) await Promise.resolve();
    assert.ok(resolve); const before = h.calls.length; await h.app.applyGroupPersona(); assert.equal(h.calls.length, before);
    h.app.clearSession(); await h.login();
    resolve({ body: stage === "contacts" ? contacts("123", 2) : groupPersonaResult(payload) }); await old;
    assert.equal(h.app.state.pendingOperation, null); assert.equal(h.app.state.groupPersona.message, "");
    assert.equal(h.ids.get("group-persona-select").value, ""); assert.equal(h.app.state.groupPersona.saving, false);
  }
});

test("persona edits and new copies remain draft-only and survive switching and option refresh", async () => {
  const h = harness(); await h.login();
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  const defaultSelect = h.app.inputs.get("plugins.ai.defaults.persona").control;
  defaultSelect.value = "模板甲"; await defaultSelect.emit("change");
  const budget = h.app.inputs.get("plugins.ai.defaults.input_budget").control; budget.value = "9000";
  h.ids.get("persona-text").value = "我尚未保存的正文"; await h.ids.get("persona-text").emit("input");
  h.ids.get("persona-list").value = "模板甲"; await h.ids.get("persona-list").emit("change");
  h.ids.get("persona-new-name").value = "我的副本"; h.app.createPersona(true);
  assert.equal(h.app.state.draft.plugins.ai.personas["我的副本"], "内置模板正文");
  assert.equal(h.app.state.config.plugins.ai.personas["我的副本"], undefined);
  assert.equal(h.app.settingInputs.get("persona").input.children.some((option) => option.value === "我的副本"), false);
  assert.equal(defaultSelect.value, "模板甲"); assert.equal(budget.value, "9000");
  h.ids.get("persona-list").value = "默认"; await h.ids.get("persona-list").emit("change");
  assert.equal(h.ids.get("persona-text").value, "我尚未保存的正文");
  assert.equal(h.app.state.config.plugins.ai.personas["默认"], "自定义帮助群成员");
  assert.equal(h.app.readForm().plugins.ai.defaults.input_budget, 9000);
});

test("new blank personas require text and deleting a selected custom leaves an explicit invalid choice", async () => {
  const h = harness(); await h.login();
  h.ids.get("persona-new-name").value = "临时人设"; h.app.createPersona();
  assert.equal(h.ids.get("persona-text").readOnly, false); assert.throws(() => h.app.readForm(), /非空正文/u);
  h.ids.get("persona-text").value = "完整正文"; await h.ids.get("persona-text").emit("input");
  const defaults = h.app.inputs.get("plugins.ai.defaults.persona").control;
  defaults.value = "临时人设"; await defaults.emit("change");
  h.app.deletePersona();
  assert.equal(defaults.value, "临时人设"); assert.equal(defaults.attrs["aria-invalid"], "true");
  assert.match(defaults.children[0].textContent, /已失效/u); assert.throws(() => h.app.readForm(), /已失效/u);
  defaults.value = "模板甲"; await defaults.emit("change"); assert.equal(h.app.readForm().plugins.ai.defaults.persona, "模板甲");
});

test("saved persona refresh preserves unsaved conversation controls and marks removed names invalid", async () => {
  const h = harness(); await h.login();
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  const persona = h.app.settingInputs.get("persona").input, budget = h.app.settingInputs.get("input_budget").input;
  persona.value = "模板甲"; await persona.emit("change"); budget.value = "12000"; await budget.emit("input");
  const updated = h.snapshot(); updated.config.plugins.ai.personas = { "新保存的人设": "新正文" }; updated.config.plugins.ai.defaults.persona = "新保存的人设";
  updated.persona_templates = { "模板乙": "模板乙正文" };
  h.setHandler(async () => ({ body: updated })); await h.app.loadConfig();
  assert.equal(h.app.settingInputs.get("persona").input, persona); assert.equal(budget.value, "12000");
  assert.equal(persona.value, "模板甲"); assert.equal(persona.attrs["aria-invalid"], "true");
  assert.equal(h.app.state.memory.settingsDirty, true); assert.throws(() => h.app.readSettings(), /已失效/u);
  persona.value = "新保存的人设"; await persona.emit("change");
  assert.equal(h.app.readSettings().input_budget, 12000); assert.equal(h.app.readSettings().persona, "新保存的人设");
});

test("legacy missing persona is never silently replaced by an available template", async () => {
  const h = harness(); await h.login(); const updated = h.snapshot();
  updated.config.plugins.ai.defaults.persona = "已移除旧人设";
  h.setHandler(async () => ({ body: updated })); await h.app.loadConfig();
  const defaults = h.app.inputs.get("plugins.ai.defaults.persona").control;
  assert.equal(defaults.value, "已移除旧人设"); assert.throws(() => h.app.readForm(), /已失效/u);
  h.app.state.memory.detail = { ...h.conversation(), settings: { ...h.sample.plugins.ai.defaults, persona: "已移除旧人设" } }; h.app.renderSettings();
  assert.equal(h.app.settingInputs.get("persona").input.value, "已移除旧人设"); assert.throws(() => h.app.readSettings(), /已失效/u);
});

test("persona names and bodies stay literal, including prototype keys and advanced JSON", async () => {
  const h = harness(); await h.login();
  for (const name of ["__proto__", "constructor", "<img onerror=alert(1)>"]) {
    h.ids.get("persona-new-name").value = name; h.app.createPersona();
    h.ids.get("persona-text").value = "<script>正文</script>"; await h.ids.get("persona-text").emit("input");
    assert.equal(Object.hasOwn(h.app.state.draft.plugins.ai.personas, name), true);
    assert.equal(h.ids.get("persona-title").textContent, name);
  }
  const before = JSON.parse(JSON.stringify(h.app.readForm()));
  assert.equal(before.plugins.ai.personas.__proto__, "<script>正文</script>");
  before.plugins.ai.defaults.persona = "__proto__";
  h.ids.get("config-json").value = JSON.stringify(before); h.app.adoptJson();
  assert.equal(h.app.readForm().plugins.ai.defaults.persona, "__proto__");
  assert.equal(h.app.personaChoices(h.app.state.draft).has("toString"), false);
  assert.equal({}.polluted, undefined);
  h.app.clearSession(); await h.login();
  assert.ok(h.ids.get("persona-manager").isConnected, "logout/login must preserve the editor's real DOM container");
});

test("secret inputs infer keep/set, allow reversible explicit clear, and preserve drafts through JSON", async () => {
  const h = harness(); await h.login();
  for (const controls of h.app.secretInputs.values()) {
    assert.equal(controls.value.type, "password"); assert.equal(controls.value.disabled, false); assert.equal(controls.action, undefined);
  }
  assert.equal(h.app.secretChanges().llm_api_key.action, "keep");
  let key = h.app.secretInputs.get("llm_api_key"); key.value.value = "temporary-key"; await key.value.emit("input");
  assert.equal(h.app.secretChanges().llm_api_key.value, "temporary-key");
  await key.clearButton.emit("click"); assert.equal(key.value.value, ""); assert.equal(h.app.secretChanges().llm_api_key.action, "clear");
  assert.match(key.status.textContent, /保存后生效/u); assert.equal(key.clearButton.textContent, "撤销清除");
  h.app.syncJson(); h.app.adoptJson(); key = h.app.secretInputs.get("llm_api_key");
  assert.equal(h.app.secretChanges().llm_api_key.action, "clear");
  await key.clearButton.emit("click"); assert.equal(key.value.value, "temporary-key");
  key.value.value = ""; await key.value.emit("input"); assert.equal(h.app.secretChanges().llm_api_key.action, "keep");
  await key.clearButton.emit("click"); key.value.value = "new-key"; await key.value.emit("input");
  assert.equal(h.app.secretChanges().llm_api_key.action, "set");
  key.value.value = "   "; assert.throws(() => h.app.secretChanges(), /空白/u);
  const snapshot = h.snapshot(); snapshot.secrets.llm_api_key = { env_set: true, file_set: true, effective_set: true };
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig(); key = h.app.secretInputs.get("llm_api_key");
  await key.clearButton.emit("click"); assert.match(key.status.textContent, /不会覆盖环境变量/u);
});

const modelsResponse = (models = ["model-b", "model-a"]) => ({ models, chat_completions_url: "https://models.example/v1/chat/completions", key_source: "temporary", environment_overrides_saved_key: false });
async function prepareModels(h) {
  await h.login(); h.app.inputs.get("llm.chat_completions_url").control.value = "https://models.example/v1";
  h.app.inputs.get("llm.model").control.value = "previous-model";
  h.app.secretInputs.get("llm_api_key").value.value = "temporary-test-key";
}

test("model discovery uses the current key and CSRF without saving, and keeps the current model and unrelated drafts", async () => {
  const h = harness(); await prepareModels(h);
  h.app.inputs.get("app.timezone").control.value = "Europe/London";
  h.setHandler(async () => ({ body: { ...modelsResponse(["model-a", "<img onerror=alert(1)>", "模型甲"]), environment_overrides_saved_key: true } }));
  const count = h.calls.length; await h.app.fetchModels();
  assert.equal(h.calls.length, count + 1); const request = h.calls.at(-1);
  assert.equal(request.path, "/api/llm/models"); assert.equal(request.options.method, "POST"); assert.equal(request.options.headers["X-CSRF-Token"], "csrf-test");
  assert.deepEqual(request.body, { revision: "r1", url: "https://models.example/v1", api_key: { action: "set", value: "temporary-test-key" } });
  assert.equal(h.app.readForm().llm.chat_completions_url, modelsResponse().chat_completions_url);
  assert.equal(h.app.readForm().llm.model, "previous-model"); assert.equal(h.app.readForm().app.timezone, "Europe/London");
  const list = h.ids.get("model-list"); assert.equal(list.value, "previous-model"); assert.equal(list.children[2].textContent, "<img onerror=alert(1)>");
  assert.match(h.ids.get("models-message").textContent, /环境变量仍优先/u); assert.equal(h.app.inputs.get("llm.model").wrapper.hidden, true);
  list.value = "模型甲"; await list.emit("change"); assert.equal(h.app.readForm().llm.model, "模型甲");
  await h.ids.get("manual-model").emit("click"); assert.equal(h.app.inputs.get("llm.model").wrapper.hidden, false);
  const model = h.app.inputs.get("llm.model").control; model.value = "manual-again"; await model.emit("input"); assert.equal(list.value, "manual-again");
  assert.equal(h.app.secretInputs.get("llm_api_key").value.value, "temporary-test-key");
});

test("empty and invalid discovery results retain the model and allow manual input; failures do not rewrite the endpoint", async () => {
  const h = harness(); await prepareModels(h);
  h.setHandler(async () => ({ body: modelsResponse([]) })); await h.app.fetchModels();
  assert.equal(h.app.readForm().llm.model, "previous-model"); assert.equal(h.ids.get("models-select-row").hidden, true);
  assert.match(h.ids.get("models-message").textContent, /未返回可用模型/u); assert.equal(h.app.inputs.get("llm.model").wrapper.hidden, false);
  const url = h.app.inputs.get("llm.chat_completions_url").control;
  for (const response of [
    { status: 400, body: { code: "models_key_required", error: "新地址需要手动填写该服务密钥。" } },
    { status: 502, body: { code: "models_unavailable", error: "模型服务不可用，请核对地址与密钥。" } },
    { body: modelsResponse([" x "]) }, { body: modelsResponse(["汉".repeat(86)]) },
    { body: { ...modelsResponse(), chat_completions_url: "https://untrusted.example/chat/completions" } },
  ]) {
    url.value = "https://models.example/v1"; await url.emit("input");
    h.setHandler(async () => response); await h.app.fetchModels();
    assert.equal(h.app.readForm().llm.model, "previous-model"); assert.equal(url.value, "https://models.example/v1");
    assert.equal(h.app.state.models.kind, "error"); assert.equal(h.app.state.models.loading, false);
  }
});

test("discovery has only one request in flight and no automatic first selection or polling", async () => {
  const h = harness(); await prepareModels(h); h.app.inputs.get("llm.model").control.value = "";
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; })); const count = h.calls.length, timers = h.timers.size;
  const first = h.app.fetchModels(); await h.app.fetchModels(); await h.ids.get("fetch-models").emit("click");
  assert.equal(h.calls.length, count + 1); assert.equal(h.ids.get("fetch-models").disabled, true);
  resolve({ body: modelsResponse() }); await first;
  assert.equal(h.app.readForm().llm.model, ""); assert.equal(h.ids.get("model-list").value, ""); assert.equal(h.timers.size, timers);
  assert.equal(h.ids.get("fetch-models").disabled, false);
});

test("URL/key changes including ABA, config reload, JSON edits and logout invalidate in-flight discovery", async () => {
  for (const change of [
    async (h) => { const url = h.app.inputs.get("llm.chat_completions_url").control; url.value = "https://other.example/v1"; await url.emit("input"); },
    async (h) => { const url = h.app.inputs.get("llm.chat_completions_url").control; const old = url.value; url.value = "https://other.example/v1"; await url.emit("input"); url.value = old; await url.emit("input"); },
    async (h) => { const key = h.app.secretInputs.get("llm_api_key").value; key.value = "replacement-key"; await key.emit("input"); },
    async (h) => { const key = h.app.secretInputs.get("llm_api_key"); await key.clearButton.emit("click"); },
    async (h) => { await h.app.loadConfig(); },
    async (h) => { await h.ids.get("config-json").emit("input"); },
    async (h) => { h.app.clearSession(); await h.login(); },
  ]) {
    const h = harness(); await prepareModels(h); let resolve;
    h.setHandler((path) => path === "/api/config" ? Promise.resolve({ body: h.snapshot() }) : new Promise((done) => { resolve = done; }));
    const first = h.app.fetchModels(); await change(h);
    const before = h.app.inputs.get("llm.chat_completions_url").control.value, currentModel = h.app.inputs.get("llm.model").control.value;
    resolve({ body: modelsResponse() }); await first;
    assert.equal(h.app.state.models.items.length, 0); assert.equal(h.app.state.models.loading, false);
    assert.equal(h.app.inputs.get("llm.chat_completions_url").control.value, before);
    assert.equal(h.app.inputs.get("llm.model").control.value, currentModel);
    assert.ok(h.ids.get("model-picker").isConnected);
  }
});

test("editing model during discovery is preserved and URL or key edits clear an old list", async () => {
  const h = harness(); await prepareModels(h); let resolve;
  h.setHandler(() => new Promise((done) => { resolve = done; })); const first = h.app.fetchModels();
  const model = h.app.inputs.get("llm.model").control; model.value = "edited-while-loading"; await model.emit("input");
  resolve({ body: modelsResponse() }); await first; assert.equal(model.value, "edited-while-loading");
  h.app.secretInputs.get("llm_api_key").value.value = "changed"; await h.app.secretInputs.get("llm_api_key").value.emit("input");
  assert.equal(h.app.state.models.items.length, 0); assert.equal(model.value, "edited-while-loading");
});

test("both save-current buttons only save configuration and send inferred secret actions", async () => {
  for (const id of ["save-config", "save-config-top"]) {
    const h = harness(); await prepareModels(h); h.app.inputs.get("llm.chat_completions_url").control.value = modelsResponse().chat_completions_url;
    const key = h.app.secretInputs.get("milky_access_token"); await key.clearButton.emit("click");
    h.setHandler(async (path, options) => ({ body: path === "/api/config" && options.method === "GET" ? h.snapshot() : { revision: "r2" } }));
    await h.ids.get(id).emit("click");
    await new Promise((resolve) => setImmediate(resolve));
    const writes = h.calls.filter((call) => call.options.method !== "GET");
    assert.equal(writes.length, 1); assert.equal(writes[0].path, "/api/config"); assert.equal(writes[0].options.method, "PUT");
    assert.deepEqual(writes[0].body.secrets.llm_api_key, { action: "set", value: "temporary-test-key" });
    assert.deepEqual(writes[0].body.secrets.milky_access_token, { action: "clear" }); assert.deepEqual(writes[0].body.secrets.llbot_token, { action: "keep" });
    assert.equal(h.calls.some((call) => call.path.startsWith("/api/runtime/")), false); assert.equal(h.app.state.busy, false);
  }
});

const globalTools = (h) => h.app.inputs.get("plugins.ai.defaults.allowed_tools").control;
const conversationTools = (h) => h.app.settingInputs.get("allowed_tools").input;
const toolCheckbox = (control, name) => control.toolPicker.rows.get(name).checkbox;

test("global tools are checkboxes from the server catalog, with explicit empty selection and dynamic new tools", async () => {
  const h = harness(); await h.login(); const updated = h.snapshot();
  updated.tool_catalog.push({ name: "brand_new_tool", description: "来自新增插件注册项", plugin_id: "clock", effect: "local_write" });
  h.setHandler(async () => ({ body: updated })); await h.app.loadConfig(); const control = globalTools(h);
  assert.equal(control.tagName, "DIV"); assert.equal(control.toolPicker.rows.size, 3);
  assert.equal(h.app.readForm().plugins.ai.defaults.allowed_tools.length, 0); assert.match(control.textContent, /禁用全部工具/u);
  const checkbox = toolCheckbox(control, "brand_new_tool"); assert.equal(checkbox.type, "checkbox"); assert.equal(checkbox.disabled, false);
  checkbox.checked = true; await checkbox.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["brand_new_tool"]); assert.equal(h.app.state.dirty, true);
  assert.match(control.textContent, /来自新增插件注册项/u); assert.match(control.textContent, /本地写入/u);
  h.app.syncJson(); h.app.adoptJson(); assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["brand_new_tool"]);
  const native = h.app.inputs.get("llm.tool_calling").control; native.checked = false; await native.emit("input");
  assert.match(globalTools(h).textContent, /原生工具调用已关闭/u); assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["brand_new_tool"]);
  const fresh = toolCheckbox(globalTools(h), "brand_new_tool"); fresh.checked = false; await fresh.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], []);
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  assert.deepEqual([...h.app.readSettings().allowed_tools], []); assert.match(conversationTools(h).textContent, /禁用全部工具/u);
});

test("conversation choices use saved global permissions, never unsaved global tool or plugin drafts", async () => {
  const h = harness(); await h.login(); const saved = h.snapshot(); saved.config.plugins.ai.defaults.allowed_tools = ["clock_now"];
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig(); h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  let conversation = conversationTools(h); assert.equal(toolCheckbox(conversation, "clock_now").disabled, false); assert.equal(toolCheckbox(conversation, "reminder_create").disabled, true);
  const clock = toolCheckbox(globalTools(h), "clock_now"), reminder = toolCheckbox(globalTools(h), "reminder_create");
  clock.checked = false; await clock.emit("change"); reminder.checked = true; await reminder.emit("change");
  const plugin = h.app.inputs.get("plugins.clock.enabled").control; plugin.checked = false; await plugin.emit("input");
  conversation = conversationTools(h); assert.equal(toolCheckbox(conversation, "clock_now").disabled, false); assert.equal(toolCheckbox(conversation, "reminder_create").disabled, true);
  const forbidden = toolCheckbox(conversation, "reminder_create"); forbidden.checked = true; await forbidden.emit("change");
  assert.equal(forbidden.checked, false); assert.deepEqual([...h.app.readSettings().allowed_tools], []);
  const permitted = toolCheckbox(conversation, "clock_now"); permitted.checked = true; await permitted.emit("change");
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now"]); assert.equal(h.app.state.memory.settingsDirty, true);
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["reminder_create"]);
});

test("unknown and disabled selected tools remain visible and removable without silently discarding them", async () => {
  const h = harness(); await h.login(); const saved = h.snapshot();
  saved.config.plugins.ai.defaults.allowed_tools = ["retired_tool", "clock_now"]; saved.config.plugins.clock.enabled = false;
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig(); const control = globalTools(h);
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["retired_tool", "clock_now"]);
  for (const name of ["retired_tool", "clock_now"]) { assert.equal(toolCheckbox(control, name).checked, true); assert.equal(toolCheckbox(control, name).disabled, false); }
  assert.match(control.textContent, /未注册的旧工具/u); assert.match(control.textContent, /所属插件已关闭/u);
  h.app.syncJson(); h.app.adoptJson(); assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["retired_tool", "clock_now"]);
  const freshControl = globalTools(h), retired = toolCheckbox(freshControl, "retired_tool"); retired.checked = false; await retired.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["clock_now"]); assert.equal(retired.disabled, true);
  retired.checked = true; await retired.emit("change"); assert.equal(retired.checked, false);
  const plugin = h.app.inputs.get("plugins.clock.enabled").control; plugin.checked = true; await plugin.emit("input");
  assert.equal(toolCheckbox(freshControl, "clock_now").checked, true); assert.equal(toolCheckbox(freshControl, "clock_now").disabled, false);
  assert.equal(freshControl.toolPicker.rows.has("retired_tool"), false);
  const changed = h.app.readForm(); changed.plugins.ai.defaults.allowed_tools = [123]; h.ids.get("config-json").value = JSON.stringify(changed);
  assert.throws(() => h.app.adoptJson(), /allowed_tools/u);
});

test("catalog and saved permission refresh preserve conversation selections and other unsaved settings", async () => {
  const h = harness(); await h.login(); const initial = h.snapshot(); initial.config.plugins.ai.defaults.allowed_tools = ["clock_now", "reminder_create"];
  h.setHandler(async () => ({ body: initial })); await h.app.loadConfig(); h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
  const selected = toolCheckbox(conversationTools(h), "clock_now"); selected.checked = true; await selected.emit("change");
  const next = h.snapshot(); next.config.plugins.ai.defaults.allowed_tools = ["reminder_create"]; next.tool_catalog = next.tool_catalog.filter((tool) => tool.name !== "clock_now");
  h.setHandler(async () => ({ body: next })); await h.app.loadConfig();
  assert.equal(h.app.settingInputs.get("input_budget").input, budget); assert.equal(budget.value, "12000"); assert.equal(h.app.state.memory.settingsDirty, true);
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now"]); assert.equal(h.app.readSettings().input_budget, 12000);
  const stale = toolCheckbox(conversationTools(h), "clock_now"); assert.equal(stale.checked, true); assert.equal(stale.disabled, false);
  assert.match(conversationTools(h).textContent, /未注册的旧工具/u); stale.checked = false; await stale.emit("change");
  assert.deepEqual([...h.app.readSettings().allowed_tools], []); assert.equal(stale.disabled, true);
  const current = toolCheckbox(conversationTools(h), "reminder_create"); assert.equal(current.disabled, false);
  current.checked = true; await current.emit("change"); assert.deepEqual([...h.app.readSettings().allowed_tools], ["reminder_create"]);
});

test("tool metadata stays literal, prototype names are safe, and malformed catalogs never replace the draft", async () => {
  const h = harness(); await h.login(); const snapshot = h.snapshot();
  snapshot.tool_catalog = [{ name: "__proto__", description: "<img onerror=alert(1)>", plugin_id: "clock", effect: "read" }, { name: "constructor", description: "不可继承对象属性", plugin_id: "constructor", effect: "read" }];
  snapshot.config.plugins.ai.defaults.allowed_tools = ["<missing-tool>"];
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig(); const control = globalTools(h);
  assert.match(control.textContent, /<img onerror=alert\(1\)>/u); assert.match(control.textContent, /<missing-tool>/u);
  const checkbox = toolCheckbox(control, "__proto__"); assert.equal(checkbox.disabled, false); checkbox.checked = true; await checkbox.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["<missing-tool>", "__proto__"]); assert.equal(toolCheckbox(control, "constructor").disabled, true);
  const malformed = h.snapshot(); malformed.tool_catalog.push({ ...malformed.tool_catalog[0] });
  h.setHandler(async () => ({ body: malformed })); await assert.rejects(h.app.loadConfig(), /工具目录/u);
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["<missing-tool>", "__proto__"]);
  h.app.state.busy = true; h.app.updateControls(); assert.equal(checkbox.disabled, true);
  checkbox.checked = false; await checkbox.emit("change"); assert.equal(checkbox.checked, true); assert.equal({}.polluted, undefined);
});

function pluginSnapshot(h) {
  const snapshot = h.snapshot();
  snapshot.config.plugins.native = { enabled: ["wordle", "retired"], superusers: [], mc_servers: [], today_waifu: { allow_change: true }, poke: { send_text: true } };
  snapshot.config.nonebot = { enabled: false, token: "", listen: "127.0.0.1:3215", plugins: [{ id: "nb_test", module: "nonebot_plugin_test", name: "兼容插件", description: "", commands: ["/test"], url_hosts: [], enabled: true }], queue_capacity: 32 };
  snapshot.plugin_catalog = [
    { id: "wordle", name: "Wordle", description: "共享猜词", runtime: "rust" },
    { id: "checkin", name: "🦌签到", description: "<img> 只是文字", runtime: "rust" },
    { id: "nb_test", name: "兼容插件", description: "", runtime: "nonebot" },
  ];
  snapshot.secrets.nonebot_token = { file_set: true, effective_set: true }; return snapshot;
}
async function preparePlugins(h) {
  await h.login(); const snapshot = pluginSnapshot(h);
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig(); return snapshot;
}
function globalPlugin(h, id) { return h.document.querySelectorAll(`[data-plugin-id="${id}"]`)[0]; }
function scopePluginResponse(query, enabled = true, revision = 0) {
  return { ...query, plugins: [{ id: "wordle", name: "Wordle", description: "共享猜词", runtime: "rust", commands: ["/wordle"], group_only: false, globally_enabled: true, enabled, revision }] };
}
test("native and NoneBot plugin configuration roundtrips without losing drafts or exposing tokens", async () => {
  const h = harness(); await preparePlugins(h);
  assert.equal(globalPlugin(h, "wordle").checked, true); assert.equal(globalPlugin(h, "checkin").checked, false);
  assert.equal(globalPlugin(h, "retired").checked, true);
  globalPlugin(h, "wordle").checked = false; await globalPlugin(h, "wordle").emit("change");
  globalPlugin(h, "checkin").checked = true; await globalPlugin(h, "checkin").emit("change");
  h.ids.get("nonebot-enabled").checked = true; await h.ids.get("nonebot-enabled").emit("change");
  const options = JSON.parse(h.ids.get("native-options-json").value); options.superusers = [123456];
  h.ids.get("native-options-json").value = JSON.stringify(options); await h.ids.get("native-options-json").emit("input");
  const secret = h.app.secretInputs.get("nonebot_token"); secret.value.value = "temporary-local-fixture-key"; await secret.value.emit("input");
  let config = h.app.readForm();
  assert.deepEqual([...config.plugins.native.enabled], ["checkin", "retired"]); assert.deepEqual([...config.plugins.native.superusers], [123456]);
  assert.equal(config.nonebot.enabled, true); assert.equal(config.nonebot.plugins[0].enabled, true); assert.equal(config.nonebot.token, "");
  h.app.syncJson(); h.app.adoptJson(); config = h.app.readForm();
  assert.deepEqual([...config.plugins.native.enabled], ["checkin", "retired"]);
  assert.equal(h.app.secretInputs.get("nonebot_token").value.value, "temporary-local-fixture-key");
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.secretChanges().nonebot_token)), { action: "set", value: "temporary-local-fixture-key" });
  const bad = structuredClone(config); bad.nonebot.token = "must-not-be-in-json"; h.ids.get("config-json").value = JSON.stringify(bad);
  assert.throws(() => h.app.adoptJson(), /独立|密钥/u);
  assert.equal(h.calls.some((call) => call.options.method !== "GET"), false);
});
test("new NoneBot definitions stay disabled until selected and invalid advanced edits preserve checkboxes", async () => {
  const h = harness(); await preparePlugins(h);
  const defs = JSON.parse(h.ids.get("nonebot-definitions-json").value);
  defs.push({ id: "nb_new", module: "nonebot_plugin_new", name: "<new>", description: "", commands: ["/new"], url_hosts: [] });
  h.ids.get("nonebot-definitions-json").value = JSON.stringify(defs);
  h.app.refreshExternalPlugins();
  assert.equal(globalPlugin(h, "nb_test").checked, true); assert.equal(globalPlugin(h, "nb_new").checked, false);
  globalPlugin(h, "nb_new").checked = true; await globalPlugin(h, "nb_new").emit("change");
  assert.equal(h.app.readForm().nonebot.plugins[1].enabled, true);
  h.ids.get("native-options-json").value = "{broken";
  assert.throws(() => h.app.readForm(), /JSON/u); assert.equal(globalPlugin(h, "nb_new").checked, true);
  h.app.clearSession(); assert.equal(h.app.secretInputs.size, 0); assert.equal(h.document.querySelectorAll("[data-plugin-id]").length, 0);
});
test("malformed plugin JSON never replaces the existing draft or pending secret", async () => {
  const h = harness(); await preparePlugins(h);
  const key = h.app.secretInputs.get("nonebot_token"); key.value.value = "pending-local-test-token";
  const original = h.app.state.draft;
  for (const mutate of [
    (config) => { config.plugins.native.enabled = {}; },
    (config) => { config.nonebot.plugins = {}; },
    (config) => { config.nonebot.plugins = [null]; },
    (config) => { config.nonebot.plugins.push({ ...config.nonebot.plugins[0] }); },
  ]) {
    const config = JSON.parse(JSON.stringify(h.app.readForm())); mutate(config);
    h.ids.get("config-json").value = JSON.stringify(config);
    assert.throws(() => h.app.adoptJson(), /JSON 字段/u);
    assert.equal(h.app.state.draft, original);
    assert.equal(globalPlugin(h, "wordle").checked, true);
    assert.equal(h.app.secretInputs.get("nonebot_token").value.value, "pending-local-test-token");
  }
});
test("scope plugin toggles bind account chat and revision, unknown writes require readback", async () => {
  const h = harness(); await h.login(); h.app.state.contacts.botId = "123";
  const query = { bot_id: "123", scene: "group", peer_id: "456" };
  h.setHandler(async (path, options) => ({ body: scopePluginResponse(query, options.method === "GET", options.method === "GET" ? 0 : 1) }));
  await h.app.openScopePlugins("group", "456", "测试群");
  await h.app.toggleScopePlugin("wordle", false);
  const request = h.calls.at(-1); assert.equal(request.path, "/api/plugins/scope");
  assert.deepEqual(request.body, { ...query, plugin: "wordle", enabled: false, revision: 0 });
  assert.equal(request.options.headers["X-CSRF-Token"], "csrf-test");
  assert.equal(h.app.state.scopePlugins.rows[0].enabled, false);
  h.setHandler(async () => ({ error: new TypeError("connection lost") }));
  await h.app.toggleScopePlugin("wordle", true);
  assert.equal(h.app.state.scopePlugins.uncertain, true); const count = h.calls.length;
  await h.app.toggleScopePlugin("wordle", true); assert.equal(h.calls.length, count);
  assert.match(h.ids.get("scope-plugins-message").textContent, /不会重复提交/u);
  h.setHandler(async () => ({ body: scopePluginResponse(query, true, 2) })); await h.app.loadScopePlugins();
  assert.equal(h.app.state.scopePlugins.uncertain, false); assert.equal(h.app.state.scopePlugins.rows[0].revision, 2);
});
test("late scope plugin responses cannot cross contacts or restore a signed-out panel", async () => {
  const h = harness(); await h.login(); h.app.state.contacts.botId = "123";
  let resolve; const slow = new Promise((done) => { resolve = done; });
  h.setHandler(async (path) => path.includes("peer_id=456") ? slow : { body: scopePluginResponse({ bot_id: "123", scene: "group", peer_id: "789" }, false, 4) });
  const first = h.app.openScopePlugins("group", "456", "A");
  await h.app.openScopePlugins("group", "789", "B");
  resolve({ body: scopePluginResponse({ bot_id: "123", scene: "group", peer_id: "456" }) }); await first;
  assert.equal(h.app.state.scopePlugins.query.peer_id, "789"); assert.equal(h.app.state.scopePlugins.rows[0].revision, 4);
  h.app.clearSession(); assert.equal(h.ids.get("scope-plugins-dialog").open, false); assert.equal(h.app.state.scopePlugins.query, null);
});

test("configuration distinguishes drafts, saved revisions, applied runtime and persistent apply blockers", async () => {
  const h = harness(); const snapshot = await preparePlugins(h);
  assert.equal(h.ids.get("config-state").textContent, "已保存待应用");
  h.app.state.status = { phase: "running", applied_revision: "r1", loaded_plugins: ["wordle", "nb_test"] }; h.app.renderStatus();
  assert.equal(h.ids.get("dirty-label").textContent, "已应用");
  const loaded = () => h.document.querySelectorAll('[data-plugin-loaded="wordle"]')[0].textContent;
  assert.equal(loaded(), "当前运行已启用");
  assert.match(h.document.querySelectorAll('[data-plugin-loaded="nb_test"]')[0].textContent, /桥接.*Python.*另行确认/u);
  globalPlugin(h, "wordle").checked = false; await globalPlugin(h, "wordle").emit("change");
  assert.equal(h.ids.get("config-state").textContent, "未保存"); assert.equal(loaded(), "当前运行已启用");
  snapshot.revision = "r2"; snapshot.config.nonebot.enabled = true;
  snapshot.apply_error = "NoneBot 缺少连接令牌。只使用 Rust 原生插件时请关闭 NoneBot。";
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  assert.equal(h.ids.get("config-state").textContent, "不可应用");
  assert.equal(h.ids.get("config-apply-error").hidden, false);
  assert.match(h.ids.get("config-apply-error").textContent, /缺少连接令牌.*关闭 NoneBot/u);
  await h.ids.get("dismiss-notice").emit("click"); h.app.renderStatus();
  assert.equal(h.ids.get("config-apply-error").hidden, false, "blocking detail must survive dismissal and status polling");
  assert.equal(h.calls.some((call) => call.path === "/api/runtime/apply"), false);
});

test("save-and-apply rejection preserves saved state and the original failure while the old host remains active", async () => {
  const h = harness(); const saved = await preparePlugins(h);
  h.app.state.status = { phase: "running", applied_revision: "r1", loaded_plugins: ["wordle"] };
  const reason = "NoneBot token 无效；请补充连接令牌或关闭兼容服务。";
  h.setHandler(async (path, options) => {
    if (path === "/api/config" && options.method === "PUT") {
      saved.config = JSON.parse(options.body).config; saved.revision = "r2"; return { body: saved };
    }
    if (path === "/api/config") return { body: saved };
    if (path === "/api/runtime/apply") return { status: 400, body: { error: reason } };
    return { body: { phase: "running", applied_revision: "r1", loaded_plugins: ["wordle"] } };
  });
  globalPlugin(h, "checkin").checked = true; await globalPlugin(h, "checkin").emit("change");
  await h.app.runAction(() => h.app.saveConfig(true));
  assert.equal(h.app.state.revision, "r2"); assert.equal(h.app.state.dirty, false);
  assert.match(h.ids.get("notice-text").textContent, /配置已保存但未生效/u);
  assert.ok(h.ids.get("notice-text").textContent.includes(reason));
  assert.equal(h.app.state.applyFailure.message, reason);
  assert.equal(h.ids.get("config-state").textContent, "应用失败");
  await h.app.refreshStatus();
  assert.equal(h.ids.get("config-state").textContent, "应用失败");
  assert.match(h.document.querySelectorAll('[data-plugin-loaded="checkin"]')[0].textContent, /当前运行未启用/u);
  assert.equal(globalPlugin(h, "checkin").checked, true);
  assert.equal(h.calls.filter((call) => call.path === "/api/runtime/apply").length, 1);
});

test("known invalid saved config is not applied; fixing it explicitly restores the applied state", async () => {
  const h = harness(); let saved = await preparePlugins(h), blocked = true;
  h.setHandler(async (path, options) => {
    if (path === "/api/config" && options.method === "PUT") {
      saved = { ...saved, config: JSON.parse(options.body).config, revision: blocked ? "r2" : "r3",
        apply_error: blocked ? "启用 NoneBot 时必须配置连接令牌。" : null };
      return { body: saved };
    }
    if (path === "/api/config") return { body: saved };
    return { body: { phase: "running", applied_revision: "r3", loaded_plugins: ["wordle", "checkin"] } };
  });
  h.ids.get("nonebot-enabled").checked = true; await h.ids.get("nonebot-enabled").emit("change");
  await h.app.runAction(() => h.app.saveConfig(true));
  assert.equal(h.ids.get("config-state").textContent, "不可应用");
  assert.match(h.ids.get("notice-text").textContent, /配置已保存但未生效.*必须配置连接令牌/u);
  assert.equal(h.calls.some((call) => call.path === "/api/runtime/apply"), false);
  h.ids.get("nonebot-enabled").checked = false; await h.ids.get("nonebot-enabled").emit("change");
  assert.equal(h.ids.get("config-state").textContent, "未保存"); assert.equal(h.ids.get("config-apply-error").hidden, false);
  blocked = false; await h.app.runAction(() => h.app.saveConfig(true));
  assert.equal(h.ids.get("config-state").textContent, "已应用");
  assert.equal(h.ids.get("config-apply-error").hidden, true);
  assert.equal(h.app.state.applyFailure, null);
  assert.equal(h.calls.filter((call) => call.path === "/api/runtime/apply").length, 1);
});

test("scope permission success never claims an unloaded plugin is running", async () => {
  const h = harness(); await h.login(); h.app.state.contacts.botId = "123";
  h.app.state.applyError = "请补充 NoneBot 令牌或关闭兼容服务。";
  const query = { bot_id: "123", scene: "group", peer_id: "456" };
  h.setHandler(async (_path, options) => {
    const result = scopePluginResponse(query, options.method !== "GET", options.method === "GET" ? 0 : 1);
    result.plugins[0].globally_enabled = false; return { body: result };
  });
  await h.app.openScopePlugins("group", "456", "测试群"); await h.app.toggleScopePlugin("wordle", true);
  assert.equal(h.app.state.scopePlugins.rows[0].enabled, true);
  assert.match(h.ids.get("scope-plugins-message").textContent, /已允许.*当前运行未启用.*令牌/u);
  assert.match(h.ids.get("scope-plugins-list").textContent, /会话已允许.*当前运行未启用/u);
  assert.doesNotMatch(h.ids.get("scope-plugins-message").textContent, /已开启|即时生效/u);
  assert.equal(h.calls.some((call) => call.path === "/api/runtime/apply"), false);
});

test("uncertain apply retains its error and does not treat a previous matching revision as success", async () => {
  const h = harness(); const saved = await preparePlugins(h);
  h.app.state.status = { phase: "running", applied_revision: "r1", loaded_plugins: ["wordle"] };
  h.setHandler(async (path) => path === "/api/config" ? { body: saved } : { error: new Error("response interrupted") });
  await h.app.runAction(() => h.app.saveConfig(true));
  assert.equal(h.app.state.pendingOperation.kind, "runtime");
  assert.equal(h.ids.get("config-state").textContent, "已保存待应用");
  assert.match(h.ids.get("notice-text").textContent, /应用结果尚未确认.*无法连接管理服务/u);
  assert.equal(h.calls.filter((call) => call.path === "/api/runtime/apply").length, 1);
});

test("late runtime status cannot restore plugin state after logout", async () => {
  const h = harness(); await preparePlugins(h); let resolve;
  h.setHandler(() => new Promise((done) => { resolve = done; }));
  const pending = h.app.refreshStatus(); h.app.clearSession();
  resolve({ body: { phase: "running", applied_revision: "r1", loaded_plugins: ["wordle"] } }); await pending;
  assert.equal(h.app.state.status, null); assert.equal(h.app.state.applyFailure, null);
});

test("authoritative polling clears a recovered apply failure but retains the saved configuration blocker", async () => {
  const h = harness(); await preparePlugins(h);
  h.app.state.applyFailure = { revision: "r1", message: "上次应用失败。" };
  h.app.state.status = { phase: "running", applied_revision: "r0", loaded_plugins: [] }; h.app.renderStatus();
  assert.equal(h.ids.get("config-state").textContent, "应用失败");
  h.setHandler(async () => ({ body: { phase: "waiting_login", applied_revision: "r1", loaded_plugins: ["wordle"] } }));
  await h.app.refreshStatus();
  assert.equal(h.app.state.applyFailure, null); assert.equal(h.ids.get("config-state").textContent, "已应用");
  h.app.state.applyFailure = { revision: "r1", message: "旧失败。" };
  h.app.state.applyError = "当前环境中的 NoneBot 令牌无效。";
  await h.app.refreshStatus();
  assert.equal(h.app.state.applyFailure, null);
  assert.equal(h.ids.get("config-state").textContent, "不可应用");
  assert.match(h.ids.get("config-apply-error").textContent, /当前环境中的 NoneBot 令牌无效/u);
});
