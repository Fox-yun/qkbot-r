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
  const exports = "globalThis.frontend = { state, groups, api, authView, bootstrap, loadConfig, refreshQq, refreshStatus, getQr, performMemory, mutateMemory, checkOperation, clearOperation, selectConversation, loadMemories, loadEvidence, loadContext, openMemoryEditor, saveMemory, readSettings, readForm, adoptJson, syncJson, secretChanges, clearSession, runAction, updateControls, requestId, inputs, secretInputs, report, saveConfig, settingInputs, renderSettings, showView, loadContacts, toggleContact, renderContacts, invalidateContacts, renderStatus, personaChoices, fillPersonaSelect, refreshPersonaSelectors, renderPersonaLibrary, renderPersonaPreview, createPersona, deletePersona, applyGroupPersona, fetchModels, renderModels, invalidateModels, testEmbedding, invalidateEmbeddingTest, testSearch, invalidateSearchTest, openScopePlugins, loadScopePlugins, toggleScopePlugin, refreshExternalPlugins, loadStickerGroups, loadStickerItems, selectStickerGroup, changeStickerGroupPage, refreshStickers, openStickerPreview, mutateSticker, retryStickerOperation, invalidateStickers, loadBudget, loadLearning, renderLearning, openLearningEditor, cancelLearningEdit, saveLearning, mutateLearning, invalidateLearning, loadContextOverview, renderContextOverview, invalidateContextOverview, loadPersonal, renderPersonal, mutatePersonal, invalidatePersonal, loadKnowledge, renderKnowledge, mutateKnowledge, invalidateKnowledge, filterConfig, openConfigSection };";
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
    result.plugins.ai.auto_reply = { prompt: "服务端补齐的接话策略", cooldown_seconds: 30, debounce_ms: 1500, max_wait_ms: 5000, max_messages: 8, max_age_seconds: 120, outcome_hints: true, adaptive_cooldown: false, max_cooldown_seconds: 300 };
    result.plugins.ai.learning = { enabled: false, semantic_recall: true, recall_limit: 8, prompt: "服务端学习规则", min_interval_seconds: 900, max_batch_messages: 40, max_changes_per_run: 6, max_note_chars: 2000, injection_budget: 12000 };
    Object.assign(result.limits, { learning_requests_per_hour: 4, learning_requests_per_day: 24, learning_max_input_budget: 8000, learning_max_output_tokens: 1200 });
    result.plugins.ai.context = { enabled: true, include_unanswered: true, cross_scene: "private_followup", recent_seconds: 7200, max_turns: 3, neighbors_before: 10, neighbors_after: 5, neighbor_seconds: 120, recall_budget: 3000, neighbor_budget: 1500 };
    result.plugins.ai.personas = { "默认": "自定义帮助群成员" };
    result.llm.api_key = ""; result.embedding.api_key = ""; result.milky.access_token = null; result.qq_login.token = "";
    result.app.database = "data/test.db"; return result;
  }
  const sample = config();
  const snapshot = () => ({ config: structuredClone(sample), revision: "r1", auto_reply_budget: { base_estimate: 1000 }, tool_catalog: [{ name: "clock_now", description: "读取当前时间", plugin_id: "clock", effect: "read" }, { name: "reminder_create", description: "创建提醒", plugin_id: "reminder", effect: "local_write" }], persona_templates: { "默认": "内置默认正文", "模板甲": "内置模板正文" }, secrets: { llm_api_key: { effective_set: true }, milky_access_token: { effective_set: false }, llbot_token: { effective_set: true } } });
  const conversation = (id = "1:group:10:0") => ({ id, scope: "1:group:10", bot_id: 1, scene: "group", peer_id: 10, owner_id: null, mode: "group", scope_mode: "group", is_current: true, epoch: 0, memory_revision: 1, settings: sample.plugins.ai.defaults, summary: "", context_count: 2, memory_count: 1 });
  async function login() { app.authView({ authenticated: true, csrf_token: "csrf-test", setup_required: false }); handler = async (path) => ({ body: path === "/api/config" ? snapshot() : {} }); await app.loadConfig(); }
  return { app, document, window, ids, calls, timers, sample, snapshot, conversation, login, setHandler(fn) { handler = fn; } };
}

test("automatic reply has a separate configuration group and uses server-normalized legacy defaults", async () => {
  const h = harness(); await h.login();
  const group = h.app.groups.find((entry) => entry.title === "自动接话");
  assert.ok(group); assert.equal(group.fields.length, 9);
  assert.match(group.description, /群与好友.*单独开启/u);
  assert.match(group.description, /默认关闭.*保存并应用/u);
  assert.match(group.description, /@.*关键词.*引用.*优先/u);
  const policy = h.sample.plugins.ai.auto_reply;
  for (const def of group.fields) {
    const control = h.app.inputs.get(def.path).control;
    if (def.type === "check") assert.equal(control.checked, policy[def.path.split(".").at(-1)]);
    else assert.equal(control.value, String(policy[def.path.split(".").at(-1)]));
    if (def.type === "number") {
      assert.equal(control.type, "number"); assert.equal(control.step, "1");
      assert.equal(control.min, String(def.min)); assert.equal(control.max, String(def.max));
    }
  }
  const prompt = h.app.inputs.get("plugins.ai.auto_reply.prompt").control;
  assert.equal(prompt.tagName, "TEXTAREA"); assert.equal(prompt.rows, 6); assert.equal(prompt.required, true);
  const age = h.app.inputs.get("plugins.ai.auto_reply.max_age_seconds").wrapper;
  assert.match(age.textContent, /不截断完整聊天上下文.*摘要.*长期记忆/u);
  assert.match(h.app.inputs.get("plugins.ai.auto_reply.cooldown_seconds").wrapper.textContent, /0.*不限制/u);

  // The API supplies serde-normalized defaults for old files. The browser must
  // retain whatever policy the backend returns, rather than copy a prompt into JS.
  const normalized = h.snapshot();
  normalized.config.plugins.ai.auto_reply = { ...policy, prompt: "另一版本的服务端策略\n保持 🐟 风格", cooldown_seconds: 45, max_messages: 12 };
  h.setHandler(async () => ({ body: normalized })); await h.app.loadConfig();
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.readForm().plugins.ai.auto_reply)), normalized.config.plugins.ai.auto_reply);
  assert.equal(h.app.state.formInvalid, false); assert.equal(h.app.state.dirty, false);
});

test("automatic reply settings save typed integers and apply without changing per-chat opt-ins", async () => {
  const h = harness(); await h.login();
  const values = { prompt: "  有新的问题时自然接话。\n无须回复时保持安静。  ", cooldown_seconds: "0", debounce_ms: "250", max_wait_ms: "1500", max_messages: "4", max_age_seconds: "5" };
  for (const [key, value] of Object.entries(values)) {
    const control = h.app.inputs.get("plugins.ai.auto_reply." + key).control;
    control.value = value; await control.emit("input");
  }
  let saved;
  h.setHandler(async (path, options) => {
    if (path === "/api/config" && options.method === "PUT") { saved = JSON.parse(options.body); return { body: {} }; }
    if (path === "/api/config") return { body: { ...h.snapshot(), config: saved.config, revision: "r2" } };
    return { body: { phase: "running", applied_revision: "r2", loaded_plugins: [] } };
  });
  await h.app.saveConfig(true);
  assert.deepEqual(saved.config.plugins.ai.auto_reply, { prompt: values.prompt.trim(), cooldown_seconds: 0, debounce_ms: 250, max_wait_ms: 1500, max_messages: 4, max_age_seconds: 5, outcome_hints: true, adaptive_cooldown: false, max_cooldown_seconds: 300 });
  assert.equal(saved.revision, "r1");
  assert.equal(h.calls.filter((call) => call.path === "/api/runtime/apply").length, 1);
  assert.equal(h.calls.some((call) => call.path.startsWith("/api/qq/contacts")), false);
  assert.equal(h.ids.get("config-state").textContent, "已应用");
});

test("automatic reply numeric fields reject fractions and out-of-range form values", async () => {
  const h = harness(); await h.login();
  const fields = h.app.groups.find((entry) => entry.title === "自动接话").fields.filter((def) => def.type === "number");
  const control = (key) => h.app.inputs.get("plugins.ai.auto_reply." + key).control;
  for (const def of fields) {
    control("debounce_ms").value = "100"; control("max_wait_ms").value = "30000"; control("max_age_seconds").value = "600";
    const input = h.app.inputs.get(def.path).control, original = input.value;
    for (const invalid of ["", "1.5", "NaN", "Infinity", "9007199254740992", String(def.min - 1), String(def.max + 1)]) {
      input.value = invalid; assert.throws(() => h.app.readForm(), /整数/u, def.path + "=" + invalid);
    }
    for (const accepted of [def.min, def.max]) {
      input.value = String(accepted);
      if (def.path.endsWith("max_age_seconds")) control("max_wait_ms").value = "5000";
      const value = h.app.readForm().plugins.ai.auto_reply[def.path.split(".").at(-1)];
      assert.equal(value, accepted); assert.equal(typeof value, "number");
    }
    input.value = original;
  }
});

test("automatic reply prompt validates trimmed Unicode characters and preserves multiline text", async () => {
  const h = harness(); await h.login(); const input = h.app.inputs.get("plugins.ai.auto_reply.prompt").control;
  for (const invalid of ["", " \n\t　 ", "鱼".repeat(2001), "🐟".repeat(2001)]) {
    input.value = invalid; assert.throws(() => h.app.readForm(), /接话策略.*2000/u);
  }
  for (const valid of ["单字", "🐟".repeat(2000), "第一段\n\n第二段"]) {
    input.value = " \n" + valid + "\t "; assert.equal(h.app.readForm().plugins.ai.auto_reply.prompt, valid);
  }
});

test("automatic reply waiting and age bounds apply to both forms and advanced JSON", async () => {
  const h = harness(); await h.login();
  const input = (key) => h.app.inputs.get("plugins.ai.auto_reply." + key).control;
  input("max_wait_ms").value = "1499";
  assert.throws(() => h.app.readForm(), /最长合并等待.*消息合并等待/u);
  input("max_wait_ms").value = "5001"; input("max_age_seconds").value = "5";
  assert.throws(() => h.app.readForm(), /消息有效期.*最长合并等待/u);
  input("max_wait_ms").value = "5000"; assert.equal(h.app.readForm().plugins.ai.auto_reply.max_age_seconds, 5);
  const original = h.app.state.draft;
  for (const patch of [
    { prompt: "\n " }, { prompt: "🐟".repeat(2001) },
    { cooldown_seconds: -1 }, { cooldown_seconds: 3601 }, { cooldown_seconds: "30" },
    { debounce_ms: 99 }, { debounce_ms: 10001 }, { debounce_ms: 2.5 },
    { max_wait_ms: 99 }, { max_wait_ms: 30001 }, { max_wait_ms: 1499 },
    { max_messages: 0 }, { max_messages: 33 },
    { max_age_seconds: 4 }, { max_age_seconds: 601 }, { max_wait_ms: 5001, max_age_seconds: 5 },
  ]) {
    const config = structuredClone(h.sample); Object.assign(config.plugins.ai.auto_reply, patch);
    h.ids.get("config-json").value = JSON.stringify(config);
    assert.throws(() => h.app.adoptJson(), /类型|接话策略|整数|不能短于/u);
    assert.equal(h.app.state.draft, original, "invalid JSON must preserve existing controls and draft");
  }
  const accepted = structuredClone(h.sample); accepted.plugins.ai.auto_reply.prompt = "  🐟\n可编辑策略  "; accepted.plugins.ai.auto_reply.cooldown_seconds = 0;
  h.ids.get("config-json").value = JSON.stringify(accepted); h.app.adoptJson();
  assert.equal(h.app.readForm().plugins.ai.auto_reply.prompt, "🐟\n可编辑策略");
  assert.equal(h.app.readForm().plugins.ai.auto_reply.cooldown_seconds, 0);
});

test("automatic reply budget counts UTF-8 and the final JSON escaping locally", async () => {
  const h = harness(); await h.login(); const snapshot = h.snapshot();
  snapshot.config.plugins.ai.auto_reply.prompt = " \n中文🐟\"\\\n行\t ";
  snapshot.config.plugins.ai.defaults.persona = "助手🐟\"";
  snapshot.config.plugins.ai.personas = { "助手🐟\"": "读\"图\"\n路径\\文🐟" };
  snapshot.config.plugins.ai.defaults.input_budget = 20000;
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  const hint = h.ids.get("auto-reply-budget"), requests = h.calls.length;
  assert.equal(hint.attrs.role, "status"); assert.equal(hint.attrs["aria-live"], "polite");
  // 19 UTF-8 bytes of policy, plus 48 bytes of persona after the system
  // message's outer JSON escaping. Padding whitespace is not submitted.
  assert.match(hint.textContent, /策略\/人设及固定规则约占 1067 \/ 20000 输入预算，剩余 18933/u);
  assert.match(hint.textContent, /当前消息.*历史.*摘要.*记忆.*工具/u);
  assert.match(hint.textContent, /保守估算.*不是模型精确 token.*会话.*预算或人设/u);
  assert.doesNotMatch(hint.className, /warning/u);
  h.window.confirm = () => { assert.fail("budget estimation must not ask for confirmation"); };
  const prompt = h.app.inputs.get("plugins.ai.auto_reply.prompt").control;
  prompt.value = prompt.value.trim() + "\n🐟\""; await prompt.emit("input");
  assert.match(hint.textContent, /约占 1075 \/ 20000/u, "newline, emoji and quote contribute 2 + 4 + 2 bytes");
  assert.equal(h.calls.length, requests, "typing must not probe APIs or invoke a model");
});

test("automatic reply budget warns at both thresholds and clears when the budget grows", async () => {
  const h = harness(); await h.login(); const snapshot = h.snapshot();
  snapshot.config.plugins.ai.auto_reply.prompt = "A"; snapshot.config.plugins.ai.defaults.persona = "N";
  snapshot.config.plugins.ai.personas = { N: "E" }; snapshot.auto_reply_budget.base_estimate = 4497;
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  let hint = h.ids.get("auto-reply-budget"), budget = h.app.inputs.get("plugins.ai.defaults.input_budget").control;
  const requests = h.calls.length;
  assert.match(hint.textContent, /接近输入预算.*4500 \/ 6000.*剩余 1500/u, "75% warns even with at least 1000 left");
  for (const [value, expected] of [[4500, /已达到或超出.*剩余 0/u], [4499, /已达到或超出.*超出 1/u], [6001, /约占 4500 \/ 6001/u]]) {
    budget.value = String(value); await budget.emit("input"); assert.match(hint.textContent, expected);
  }
  assert.doesNotMatch(hint.className, /warning/u); assert.doesNotMatch(hint.textContent, /接近|超出/u);
  budget.value = "20000"; await budget.emit("input"); assert.doesNotMatch(hint.className, /warning/u);
  assert.equal(h.calls.length, requests); assert.equal(budget.value, "20000");

  snapshot.auto_reply_budget.base_estimate = 997; snapshot.config.plugins.ai.defaults.input_budget = 2000;
  await h.app.loadConfig(); hint = h.ids.get("auto-reply-budget"); budget = h.app.inputs.get("plugins.ai.defaults.input_budget").control;
  assert.doesNotMatch(hint.className, /warning/u);
  budget.value = "1999"; await budget.emit("input");
  assert.match(hint.textContent, /接近输入预算.*1000 \/ 1999.*剩余 999/u, "less than 1000 remaining warns below 75%");
  budget.value = "2000"; await budget.emit("input"); assert.doesNotMatch(hint.className, /warning/u);
});

test("automatic reply budget follows default persona edits, JSON adoption and loaded metadata", async () => {
  const h = harness(); await h.login();
  const usage = () => Number(h.ids.get("auto-reply-budget").textContent.match(/约占 (\d+) \/ /u)[1]);
  const initial = usage(), requests = h.calls.length;
  const body = h.ids.get("persona-text"); body.value += "🐟\"\n"; await body.emit("input");
  assert.equal(usage(), initial + 11, "custom persona body needs outer escaping: 4 + 4 + 3");
  const selected = h.app.inputs.get("plugins.ai.defaults.persona").control;
  selected.value = "模板甲"; await selected.emit("change");
  assert.equal(usage(), initial - 3, "built-in selection replaces both name and body costs");
  body.value += "这不是当前默认人设"; await body.emit("input");
  assert.equal(usage(), initial - 3, "editing an unselected persona does not inflate the default estimate");
  assert.equal(h.calls.length, requests);

  const adopted = structuredClone(h.sample);
  adopted.plugins.ai.defaults.persona = "N"; adopted.plugins.ai.personas = { N: "E" };
  adopted.plugins.ai.auto_reply.prompt = "  A  "; adopted.plugins.ai.defaults.input_budget = 2048;
  h.ids.get("config-json").value = JSON.stringify(adopted); h.app.adoptJson();
  assert.match(h.ids.get("auto-reply-budget").textContent, /约占 1003 \/ 2048.*剩余 1045/u);
  assert.equal(h.calls.length, requests, "adopting JSON only recomputes locally");
  const snapshot = h.snapshot(); snapshot.config = adopted; snapshot.auto_reply_budget.base_estimate = 3000;
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  assert.match(h.ids.get("auto-reply-budget").textContent, /已达到或超出.*约占 3003 \/ 2048.*超出 955/u);
});

test("automatic reply budget warnings never block saving or leak metadata into configuration", async () => {
  const h = harness(); await h.login(); const snapshot = h.snapshot();
  snapshot.auto_reply_budget.base_estimate = 7000;
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  assert.match(h.ids.get("auto-reply-budget").textContent, /已达到或超出/u);
  const prompt = h.app.inputs.get("plugins.ai.auto_reply.prompt").control;
  prompt.value = "保留原本的策略🐟"; await prompt.emit("input");
  const requests = h.calls.length; let saved;
  h.window.confirm = () => { assert.fail("budget warning must not request confirmation"); };
  h.setHandler(async (path, options) => {
    assert.equal(path, "/api/config", "saving must not probe a model or an estimate endpoint");
    if (options.method === "PUT") { saved = JSON.parse(options.body); return { body: {} }; }
    return { body: { ...snapshot, config: saved.config, revision: "r2", auto_reply_budget: { base_estimate: 8000 } } };
  });
  await h.app.saveConfig(false);
  assert.deepEqual(h.calls.slice(requests).map((call) => call.options.method), ["PUT", "GET"]);
  assert.equal(saved.config.plugins.ai.auto_reply.prompt, "保留原本的策略🐟");
  assert.equal(saved.config.plugins.ai.defaults.input_budget, 6000);
  assert.equal(Object.hasOwn(saved, "auto_reply_budget"), false); assert.equal(Object.hasOwn(saved.config, "auto_reply_budget"), false);
  assert.equal(JSON.stringify(saved).includes("base_estimate"), false);
  assert.match(h.ids.get("auto-reply-budget").textContent, /约占 8055 \/ 6000/u, "saved reload refreshes metadata and policy");
});

test("automatic reply budget degrades honestly when fixed overhead metadata is missing", async () => {
  const h = harness(); await h.login();
  for (const metadata of [undefined, { base_estimate: -1 }, { base_estimate: "1000" }]) {
    const snapshot = h.snapshot(); snapshot.auto_reply_budget = metadata;
    h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
    const hint = h.ids.get("auto-reply-budget");
    assert.match(hint.textContent, /仅策略\/人设约占/u);
    assert.match(hint.textContent, /未计入宿主固定规则.*无法判断预算是否足够/u);
    assert.match(hint.className, /warning/u);
    assert.equal(h.app.state.formInvalid, false); assert.doesNotThrow(() => h.app.readForm());
  }
  h.app.clearSession(); assert.equal(h.app.state.autoReplyBaseEstimate, null);
});

test("learning configuration uses backend defaults and validates limits in forms and JSON", async () => {
  const h = harness(); await h.login(); const group = h.app.groups.find((group) => group.title === "学习笔记");
  assert.ok(group); assert.equal(h.app.inputs.get("plugins.ai.learning.enabled").control.checked, false);
  assert.equal(h.app.inputs.get("plugins.ai.learning.semantic_recall").control.checked, true);
  assert.equal(h.app.inputs.get("plugins.ai.learning.recall_limit").control.value, "8");
  assert.equal(h.app.inputs.get("plugins.ai.learning.prompt").control.required, false);
  const text = h.app.inputs.get("plugins.ai.learning.prompt").control;
  text.value = " \n🐟" + "中".repeat(3999) + "  "; assert.equal([...h.app.readForm().plugins.ai.learning.prompt].length, 4000);
  text.value += "多"; assert.throws(() => h.app.readForm(), /4000/u);
  text.value = "  \n "; assert.equal(h.app.readForm().plugins.ai.learning.prompt, "");
  for (const def of group.fields.filter((def) => def.type === "number")) {
    const input = h.app.inputs.get(def.path).control, before = input.value;
    for (const value of ["", "1.5", String(def.min - 1), String(def.max + 1)]) { input.value = value; assert.throws(() => h.app.readForm(), /整数/u); }
    input.value = before;
  }
  const daily = h.app.inputs.get("limits.learning_requests_per_day").control; daily.value = "3"; assert.throws(() => h.app.readForm(), /不能低于每小时/u); daily.value = "24";
  const injection = h.app.inputs.get("plugins.ai.learning.injection_budget").control; assert.equal(injection.value, "12000");
  assert.match(h.app.inputs.get("plugins.ai.learning.injection_budget").wrapper.textContent, /普通聊天输入预算.*额外预算.*不占原有问题和历史/u);
  h.app.inputs.get("limits.learning_max_input_budget").control.value = "2048";
  for (const value of [0, 12000, 64000]) { injection.value = String(value); assert.equal(h.app.readForm().plugins.ai.learning.injection_budget, value, "chat note budget is independent from background learning input"); }
  const bad = structuredClone(h.sample); bad.plugins.ai.learning.min_interval_seconds = 1;
  h.ids.get("config-json").value = JSON.stringify(bad); assert.throws(() => h.app.adoptJson(), /学习间隔/u);
  const good = structuredClone(h.sample); good.plugins.ai.learning.enabled = true; good.plugins.ai.learning.prompt = "  管理员规则🐟  "; good.limits.learning_requests_per_day = 48;
  h.ids.get("config-json").value = JSON.stringify(good); h.app.adoptJson();
  assert.equal(h.app.readForm().plugins.ai.learning.prompt, "管理员规则🐟"); assert.equal(h.app.readForm().limits.learning_requests_per_day, 48);
  assert.ok(h.ids.get("auto-reply-budget").textContent.includes("输入预算"));
});

test("learning recall controls round-trip independently and reject invalid JSON values", async () => {
  const h = harness(); await h.login();
  const semantic = h.app.inputs.get("plugins.ai.learning.semantic_recall").control;
  const limit = h.app.inputs.get("plugins.ai.learning.recall_limit").control;
  semantic.checked = false;
  for (const value of [1, 8, 128]) {
    limit.value = String(value);
    const learning = h.app.readForm().plugins.ai.learning;
    assert.equal(learning.semantic_recall, false);
    assert.equal(learning.recall_limit, value);
    assert.equal(learning.injection_budget, 12000);
  }
  for (const value of [0, 129, 1.5, "8", null]) {
    const bad = structuredClone(h.sample); bad.plugins.ai.learning.recall_limit = value;
    h.ids.get("config-json").value = JSON.stringify(bad);
    assert.throws(() => h.app.adoptJson(), /召回笔记数.*整数|learning\.recall_limit.*类型/u);
  }
  for (const value of ["true", 1, null]) {
    const bad = structuredClone(h.sample); bad.plugins.ai.learning.semantic_recall = value;
    h.ids.get("config-json").value = JSON.stringify(bad);
    assert.throws(() => h.app.adoptJson(), /笔记向量召回.*true.*false|learning\.semantic_recall.*类型/u);
  }
  const good = structuredClone(h.sample);
  good.plugins.ai.learning.semantic_recall = false; good.plugins.ai.learning.recall_limit = 128;
  h.ids.get("config-json").value = JSON.stringify(good); h.app.adoptJson();
  assert.equal(h.app.readForm().plugins.ai.learning.semantic_recall, false);
  assert.equal(h.app.readForm().plugins.ai.learning.recall_limit, 128);
  assert.match(h.app.groups.find((group) => group.embedding).description, /学习笔记/u);
  assert.match(h.app.inputs.get("plugins.ai.learning.semantic_recall").wrapper.textContent, /每小时 120 次/u);
  assert.match(h.app.inputs.get("plugins.ai.learning.recall_limit").wrapper.textContent, /锁定只保护编辑/u);
});

function learningSnapshot() {
  return { bot_id: "123", revision: 2, max_note_chars: 2000, learning_enabled: false,
    notes: [{ id: 1, version: 2, kind: "experience", text: "<img>交流时先核对问题", scope: "shared", locked: true, deleted: false, invalidated: false, sources: [{ source_key: "123:group:10:1", quote: "群聊来源", scope: "123:group:10" }], updated_at: 1700000000 }],
    history: [{ id: 10, note_id: 1, version: 1, kind: "experience", text: "历史版本", scope: "shared", locked: false, deleted: false, rollback_allowed: true, created_at: 1700000000 }, { id: 11, note_id: 1, version: 2, kind: "experience", text: "失效历史", scope: "shared", locked: true, deleted: false, rollback_allowed: false, created_at: 1700000001 }] };
}
async function learningPage(h, snapshot = learningSnapshot()) {
  await h.login(); h.app.state.view = "learning";
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadLearning(); return snapshot;
}

test("learning page is lazy, account-bound and shows literal notes, provenance and unavailable rollback", async () => {
  const h = harness(); await h.login(); assert.equal(h.calls.some((call) => call.path.startsWith("/api/learning")), false);
  h.app.state.view = "learning"; h.setHandler(async () => ({ body: learningSnapshot() })); await h.app.loadLearning();
  assert.equal(h.ids.get("learning-account").textContent, "QQ 123");
  assert.match(h.ids.get("learning-list").textContent, /<img>交流时先核对问题.*群聊来源/su);
  assert.match(h.ids.get("learning-summary").textContent, /自动学习已关闭/u);
  const rollback = h.document.querySelectorAll("[data-learning-write]").filter((button) => button.textContent.includes("回滚"));
  assert.equal(rollback[0].disabled, false); assert.equal(rollback[1].disabled, true);
  await h.app.loadLearning(); assert.equal(h.calls.at(-1).path, "/api/learning?bot_id=123");
  const wrong = learningSnapshot(); wrong.bot_id = "456";
  h.setHandler(async () => ({ body: wrong })); await h.app.loadLearning();
  assert.equal(h.app.state.learning.snapshot, null); assert.equal(h.ids.get("learning-list").children.length, 0);
  assert.match(h.ids.get("learning-message").textContent, /范围或格式/u);
});

test("learning manual edits are locked by default and send exact account revisions with CSRF", async () => {
  const h = harness(), snapshot = await learningPage(h); h.app.openLearningEditor();
  assert.equal(h.ids.get("edit-learning-locked").checked, true);
  h.ids.get("edit-learning-text").value = "  新的交流经验🐟  "; await h.ids.get("learning-editor-form").emit("input");
  let payload;
  h.setHandler(async (path, options) => {
    if (path === "/api/learning/mutate") { payload = JSON.parse(options.body); assert.equal(options.headers["X-CSRF-Token"], "csrf-test"); snapshot.revision++; return { body: { bot_id: "123", request_id: payload.request_id, status: "completed", revision: snapshot.revision } }; }
    assert.equal(path, "/api/learning?bot_id=123"); return { body: snapshot };
  });
  await h.app.saveLearning();
  assert.equal(payload.bot_id, "123"); assert.equal(payload.revision, 2); assert.ok(payload.request_id);
  assert.deepEqual(payload.action, { op: "add", text: "新的交流经验🐟", kind: "experience", scope: "shared", locked: true });
  assert.equal(h.ids.get("learning-editor").hidden, true); assert.equal(h.app.state.learning.dirty, false);
  assert.match(h.ids.get("learning-message").textContent, /已保存/u);
  await h.app.mutateLearning({ op: "lock", id: 1, version: 2, locked: false });
  assert.equal(payload.revision, 3); assert.deepEqual(payload.action, { op: "lock", id: 1, version: 2, locked: false });
  assert.equal(h.calls.some((call) => /llm|embedding|runtime\/apply/u.test(call.path)), false);
});

test("learning conflicts preserve edits and uncertain mutations query the receipt without resubmitting", async () => {
  const h = harness(), snapshot = await learningPage(h); h.app.openLearningEditor(snapshot.notes[0]);
  h.ids.get("edit-learning-text").value = "保留编辑"; await h.ids.get("learning-editor-form").emit("input");
  h.setHandler(async () => ({ status: 409, body: { error: "版本已变化", code: "learning_conflict" } }));
  await assert.rejects(h.app.saveLearning(), /版本/u);
  assert.equal(h.ids.get("edit-learning-text").value, "保留编辑"); assert.equal(h.app.state.learning.conflict, true); assert.equal(h.ids.get("save-learning").disabled, true);
  h.app.cancelLearningEdit(); h.setHandler(async () => ({ body: snapshot })); await h.app.loadLearning(); h.app.openLearningEditor(); h.ids.get("edit-learning-text").value = "超时新笔记";
  let payload;
  h.setHandler(async (path, options) => { payload = JSON.parse(options.body); return { error: new Error("connection lost") }; });
  await h.app.saveLearning(); assert.equal(h.app.state.pendingOperation.kind, "learning");
  assert.equal(h.ids.get("save-learning").disabled, true);
  h.setHandler(async (path) => {
    if (path.startsWith("/api/learning/operations/")) { assert.ok(path.includes(payload.request_id)); assert.ok(path.endsWith("bot_id=123")); return { body: { bot_id: "123", request_id: payload.request_id, status: "completed", revision: 3 } }; }
    return { body: { ...snapshot, revision: 3 } };
  });
  await h.app.checkOperation(); assert.equal(h.app.state.pendingOperation, null); assert.equal(h.ids.get("learning-editor").hidden, true);
  assert.equal(h.calls.filter((call) => call.path === "/api/learning/mutate").length, 2, "one known conflict and one ambiguous mutation, no repeated write");
});

test("learning late responses cannot repaint a left page or signed-out session", async () => {
  const h = harness(); await learningPage(h); let resolve;
  h.setHandler(() => new Promise((done) => { resolve = done; }));
  const pending = h.app.loadLearning(); await Promise.resolve(); h.app.showView("overview"); resolve({ body: learningSnapshot() }); await pending;
  assert.equal(h.app.state.learning.snapshot, null); assert.equal(h.ids.get("learning-list").children.length, 0);
  h.app.state.view = "learning"; const next = h.app.loadLearning(); await Promise.resolve(); h.app.clearSession(); resolve({ body: learningSnapshot() }); await next;
  assert.equal(h.app.state.learning.snapshot, null); assert.equal(h.ids.get("learning-list").children.length, 0);
});

test("learning editors enforce the shared total character cap without counting replaced or invalidated text", async () => {
  const h = harness(), snapshot = learningSnapshot(); snapshot.max_note_chars = 100;
  snapshot.notes[0].text = "鱼".repeat(80);
  snapshot.notes.push({ ...snapshot.notes[0], id: 2, text: "旧".repeat(100), invalidated: true });
  await learningPage(h, snapshot); h.app.openLearningEditor();
  h.ids.get("edit-learning-text").value = "🐟".repeat(21);
  await assert.rejects(h.app.saveLearning(), /全部有效笔记.*100.*80/u);
  const requests = h.calls.length; assert.equal(h.calls.some((call) => call.path === "/api/learning/mutate"), false);
  h.app.openLearningEditor(snapshot.notes[0]); h.ids.get("edit-learning-text").value = "🐟".repeat(100);
  let action;
  h.setHandler(async (path, options) => {
    if (path === "/api/learning/mutate") { const payload = JSON.parse(options.body); action = payload.action; return { body: { bot_id: "123", request_id: payload.request_id, status: "completed", revision: 3 } }; }
    return { body: { ...snapshot, revision: 3 } };
  });
  await h.app.saveLearning(); assert.equal([...action.text].length, 100); assert.equal(action.id, 1); assert.equal(action.version, 2);
  assert.equal(h.calls.length, requests + 2);
});

test("learning uses the third budget pool with hourly and rolling-day limits", async () => {
  const h = harness(); await h.login();
  h.setHandler(async () => ({ body: { used: { foreground: 1, background: 2, learning: 3, learning_day: 12 }, limits: { model_global_per_hour: 100, memory_requests_per_hour: 30, learning_requests_per_hour: 4, learning_requests_per_day: 24 }, usage: [] } }));
  await h.app.loadBudget();
  assert.match(h.ids.get("budget-usage").textContent, /学习笔记3 \/ 4.*第三个独立额度池.*24 小时 12 \/ 24/u);
});

test("search configuration and network-read catalog retain explicit grants and independent secrets", async () => {
  const h = harness(); await h.login();
  const snapshot = h.snapshot();
  snapshot.tool_catalog.push({ name: "web_search", description: "联网查询", plugin_id: "web_search", effect: "network_read" });
  snapshot.config.plugins.web_search.enabled = false;
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadConfig();
  const selected = h.app.inputs.get("plugins.ai.defaults.allowed_tools").control.toolPicker;
  assert.equal(selected.selected.has("web_search"), false);
  assert.equal(selected.rows.get("web_search").checkbox.disabled, true);
  const key = h.app.secretInputs.get("search_api_key"); key.value.value = "temporary-search-fixture"; await key.value.emit("input");
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.secretChanges().search_api_key)), { action: "set", value: "temporary-search-fixture" });
  assert.equal(h.app.secretChanges().llm_api_key.action, "keep");
  const config = h.app.readForm(); assert.equal(config.plugins.web_search.enabled, false);
  assert.ok(!JSON.stringify(config).includes("temporary-search-fixture"));
});

test("search preview sends one synthetic test with CSRF and never saves or applies", async () => {
  const h = harness(); await h.login();
  h.app.secretInputs.get("search_api_key").value.value = "preview-search-fixture";
  h.setHandler(async () => ({ body: { provider: "tavily", result_count: 1, key_source: "temporary", environment_overrides_saved_key: true } }));
  const before = h.calls.length; await h.app.testSearch();
  assert.equal(h.calls.length, before + 1);
  const call = h.calls.at(-1); assert.equal(call.path, "/api/search/test");
  assert.equal(call.options.headers["X-CSRF-Token"], "csrf-test");
  assert.deepEqual(JSON.parse(call.options.body), { revision: "r1", api_key: { action: "set", value: "preview-search-fixture" } });
  assert.match(h.app.state.search.message, /连接成功/u); assert.match(h.app.state.search.message, /环境变量/u);
});

test("search preview is single flight and discards stale responses after editing or logout", async () => {
  for (const edit of [true, false]) {
    const h = harness(); await h.login(); let finish;
    h.setHandler(() => new Promise((resolve) => { finish = resolve; }));
    const before = h.calls.length, pending = h.app.testSearch(); await h.app.testSearch();
    assert.equal(h.calls.length, before + 1);
    if (edit) {
      const key = h.app.secretInputs.get("search_api_key"); key.value.value = "changed"; await key.value.emit("input");
    } else h.app.clearSession();
    finish({ body: { provider: "tavily", result_count: 1, key_source: "file" } }); await pending;
    assert.equal(h.app.state.search.message, ""); assert.equal(h.app.state.search.loading, false);
  }
});

test("search limit rejection is visible beside the save button and names the allowed range", async () => {
  const h = harness(); await h.login(); h.app.openConfigSection("tools");
  const input = h.app.inputs.get("plugins.web_search.max_calls_per_turn").control;
  input.value = "3"; await input.emit("input"); const before = h.calls.length;
  await h.ids.get("save-config").emit("click"); await new Promise(setImmediate);
  assert.equal(h.calls.length, before, "out-of-range search limit never reaches the save endpoint");
  const feedback = h.ids.get("config-save-message");
  assert.equal(feedback.hidden, false); assert.match(feedback.textContent, /每轮搜索上限.*1–2.*整数/u);
  assert.ok(feedback.parentElement.children.includes(h.ids.get("dirty-label")), "feedback stays in the same sticky save bar");
  assert.match(h.app.inputs.get("plugins.web_search.max_calls_per_turn").wrapper.textContent, /允许范围：1–2/u);
  assert.equal(input.value, "3"); assert.equal(h.app.state.dirty, true);
  assert.equal(h.app.state.busy, false); assert.equal(h.ids.get("save-config").disabled, false);
  assert.equal(h.ids.get("save-config").attrs["aria-busy"], "false");
});

test("all config save buttons show progress and preserve search credentials and hidden persona edits", async () => {
  for (const id of ["save-config", "save-config-top", "save-apply"]) {
    const h = harness(); await h.login(); h.app.openConfigSection("tools");
    const input = h.app.inputs.get("plugins.web_search.max_calls_per_turn").control;
    input.value = "2"; await input.emit("input");
    h.app.secretInputs.get("search_api_key").value.value = "synthetic-search-key";
    let finish, saved, applies = 0;
    h.setHandler((path, options) => {
      if (path === "/api/config" && options.method === "PUT") { saved = JSON.parse(options.body); return new Promise((resolve) => { finish = resolve; }); }
      if (path === "/api/config") return { body: { ...h.snapshot(), config: saved.config, revision: "r2" } };
      if (path === "/api/runtime/apply") applies++;
      return { body: { phase: "running", applied_revision: "r2", loaded_plugins: [] } };
    });
    await h.ids.get(id).emit("click"); await new Promise(setImmediate);
    assert.equal(h.ids.get(id).textContent, "正在保存…"); assert.equal(h.ids.get(id).disabled, true);
    assert.equal(h.ids.get(id).attrs["aria-busy"], "true");
    assert.match(h.ids.get("config-save-message").textContent, /正在校验并保存/u);
    assert.equal(saved.config.plugins.web_search.max_calls_per_turn, 2);
    assert.deepEqual(saved.config.plugins.ai.personas, h.sample.plugins.ai.personas);
    assert.deepEqual(saved.secrets.search_api_key, { action: "set", value: "synthetic-search-key" });
    await h.ids.get(id).emit("click"); assert.equal(h.calls.filter((call) => call.options.method === "PUT").length, 1);
    finish({ body: {} }); await new Promise(setImmediate);
    assert.equal(h.app.state.busy, false); assert.equal(h.ids.get(id).disabled, false);
    assert.equal(h.ids.get(id).attrs["aria-busy"], "false"); assert.equal(h.app.state.revision, "r2");
    assert.match(h.ids.get("config-save-message").textContent, /已保存|已提交/u);
    assert.equal(applies, id === "save-apply" ? 1 : 0);
  }
});

test("server rejection and uncertain config save show local recovery without losing the draft", async () => {
  for (const uncertain of [false, true]) {
    const h = harness(); await h.login(); h.app.openConfigSection("tools");
    const input = h.app.inputs.get("plugins.web_search.max_calls_per_turn").control;
    input.value = "2"; await input.emit("input");
    h.setHandler(async () => uncertain ? { error: Object.assign(new Error("timeout"), { name: "TimeoutError" }) }
      : { status: 400, body: { error: "搜索超时必须短于工具总超时" } });
    await h.ids.get("save-config").emit("click"); await new Promise(setImmediate);
    assert.equal(h.app.state.busy, false); assert.equal(input.value, "2"); assert.equal(h.app.state.dirty, true);
    assert.equal(h.ids.get("config-save-message").hidden, false);
    assert.match(h.ids.get("config-save-message").textContent, uncertain ? /响应未确认.*查询结果/u : /搜索超时必须短于工具总超时/u);
    assert.equal(h.ids.get("config-check-operation").hidden, !uncertain);
    assert.equal(h.ids.get("save-config").disabled, uncertain);
    if (uncertain) {
      const before = h.calls.length; await h.ids.get("save-config").emit("click"); await new Promise(setImmediate);
      assert.equal(h.calls.length, before, "the save handler also refuses duplicate uncertain writes");
      h.setHandler(async () => ({ body: h.snapshot() }));
      await h.ids.get("config-check-operation").emit("click"); await new Promise(setImmediate);
      assert.equal(h.calls.at(-1).options.method, "GET");
      assert.match(h.ids.get("config-save-message").textContent, /无法确认这次保存/u);
      assert.equal(h.calls.length, before + 1);
    }
  }
});

test("successful config write followed by reload failure is not reported as an unsaved retry", async () => {
  const h = harness(); await h.login();
  h.setHandler(async (_path, options) => options.method === "PUT" ? { body: {} } : { error: new Error("connection lost") });
  await h.ids.get("save-config").emit("click"); await new Promise(setImmediate);
  assert.equal(h.app.state.busy, false); assert.equal(h.app.state.stale, true);
  assert.equal(h.ids.get("save-config").disabled, true);
  assert.match(h.ids.get("config-save-message").textContent, /配置已保存.*读取保存结果失败.*重新载入/u);
  assert.equal(h.calls.filter((call) => call.options.method === "PUT").length, 1);
});

async function editConfigFields(h, fields) {
  for (const [path, value] of Object.entries(fields)) {
    const input = h.app.inputs.get(path).control; input.value = String(value); await input.emit("input");
  }
}
async function clickConfig(h, id) { await h.ids.get(id).emit("click"); await new Promise(setImmediate); }
const plainForm = (h) => JSON.parse(JSON.stringify(h.app.readForm()));

test("multiple search limits save together after all visible field errors are corrected", async () => {
  for (const advanced of [false, true]) {
    const h = harness(); h.sample.limits.tool_timeout_secs = 15; await h.login(); h.app.openConfigSection("tools");
    h.ids.get("advanced").open = advanced;
    const prefix = "plugins.web_search.";
    await editConfigFields(h, { [prefix + "max_results"]: 6, [prefix + "max_calls_per_turn"]: 3, [prefix + "request_timeout_secs"]: 13 });
    const before = h.calls.length; await clickConfig(h, "save-config");
    assert.equal(h.calls.length, before, "all invalid limits are rejected before a PUT");
    for (const [key, range] of [["max_results", /1–5/u], ["max_calls_per_turn", /1–2/u], ["request_timeout_secs", /1–12/u]]) {
      const input = h.app.inputs.get(prefix + key).control;
      assert.equal(input.attrs["aria-invalid"], "true", key);
      const error = h.ids.get(input.id + "-error"); assert.ok(error, key + " has an inline error");
      assert.equal(error.hidden, false); assert.match(error.textContent, range);
    }
    const values = { max_results: 5, max_calls_per_turn: 2, request_timeout_secs: 12, max_response_bytes: 1048576, max_output_bytes: 8192,
      max_concurrent_requests: 16, user_per_hour: 120, conversation_per_hour: 600, global_per_hour: 3000 };
    await editConfigFields(h, Object.fromEntries(Object.entries(values).map(([key, value]) => [prefix + key, value])));
    let saved;
    h.setHandler(async (_path, options) => {
      if (options.method === "PUT") { saved = JSON.parse(options.body); return { body: {} }; }
      return { body: { ...h.snapshot(), config: saved.config, revision: "r2" } };
    });
    await clickConfig(h, "save-config");
    assert.equal(h.calls.filter((call) => call.options.method === "PUT").length, 1);
    for (const [key, value] of Object.entries(values)) assert.equal(saved.config.plugins.web_search[key], value, key);
    assert.equal(h.app.state.formInvalid, false); assert.equal(h.app.state.busy, false); assert.equal(h.app.state.stale, false);
    assert.equal(h.ids.get("save-config").disabled, false);
    assert.match(h.ids.get("config-save-message").textContent, /配置已保存/u);
    assert.equal(h.ids.get("config-save-message-top").textContent, h.ids.get("config-save-message").textContent);
  }
});

test("search timeout cross-check names the search field without inventing hourly quota ordering", async () => {
  const h = harness(); h.sample.limits.tool_timeout_secs = 8; await h.login();
  await editConfigFields(h, { "plugins.web_search.request_timeout_secs": 8, "plugins.web_search.user_per_hour": 1000,
    "plugins.web_search.conversation_per_hour": 2, "plugins.web_search.global_per_hour": 1 });
  const before = h.calls.length; await clickConfig(h, "save-config"); assert.equal(h.calls.length, before);
  const input = h.app.inputs.get("plugins.web_search.request_timeout_secs").control;
  assert.equal(input.attrs["aria-invalid"], "true"); assert.match(h.ids.get(input.id + "-error").textContent, /工具.*超时/u);
  await editConfigFields(h, { "plugins.web_search.request_timeout_secs": 7 });
  assert.equal(plainForm(h).plugins.web_search.user_per_hour, 1000);
  assert.equal(plainForm(h).plugins.web_search.global_per_hour, 1);
});

test("stale search drafts refresh by GET and merge non-overlapping edits before an explicit new save", async () => {
  const h = harness(); await h.login(); h.app.openConfigSection("tools");
  await editConfigFields(h, { "plugins.web_search.max_calls_per_turn": 2 });
  h.setHandler(async () => ({ status: 409, body: { error: "配置已被其他操作修改，请刷新后重试" } }));
  await clickConfig(h, "save-config"); assert.equal(h.app.state.stale, true);
  await editConfigFields(h, { "plugins.web_search.global_per_hour": 120 });
  h.app.state.draft.plugins.ai.personas["本地新增"] = "只保存在当前草稿的人设";
  assert.equal(h.ids.get("save-config").disabled, true);
  assert.equal(h.ids.get("config-refresh-draft").hidden, false); assert.equal(h.ids.get("config-refresh-draft-top").hidden, false);
  const latest = h.snapshot(); latest.revision = "r2"; latest.config.plugins.web_search.max_results = 5;
  latest.config.llm.model = "synthetic-server-model"; latest.config.plugins.ai.personas["服务器新增"] = "服务器独立更新的人设";
  const before = h.calls.length; h.setHandler(async () => ({ body: latest }));
  await clickConfig(h, "config-refresh-draft");
  assert.equal(h.calls.length, before + 1); assert.equal(h.calls.at(-1).options.method, "GET");
  assert.equal(h.app.state.revision, "r2"); assert.equal(h.app.state.stale, false); assert.equal(h.app.state.dirty, true);
  assert.equal(h.ids.get("save-config").disabled, false); assert.equal(h.ids.get("config-refresh-draft").hidden, true);
  const merged = plainForm(h);
  assert.equal(merged.plugins.web_search.max_calls_per_turn, 2); assert.equal(merged.plugins.web_search.global_per_hour, 120);
  assert.equal(merged.plugins.web_search.max_results, 5); assert.equal(merged.llm.model, "synthetic-server-model");
  assert.equal(merged.plugins.ai.personas["本地新增"], "只保存在当前草稿的人设");
  assert.equal(merged.plugins.ai.personas["服务器新增"], "服务器独立更新的人设");
  assert.equal(h.app.state.config.plugins.web_search.max_calls_per_turn, latest.config.plugins.web_search.max_calls_per_turn, "new baseline remains the server config");
  let saved;
  h.setHandler(async (_path, options) => {
    if (options.method === "PUT") { saved = JSON.parse(options.body); return { body: {} }; }
    return { body: { ...latest, config: saved.config, revision: "r3" } };
  });
  await clickConfig(h, "save-config"); assert.equal(saved.revision, "r2"); assert.deepEqual(saved.config, merged);
  assert.equal(h.calls.some((call) => call.path === "/api/runtime/apply"), false);
});

test("conflicting stale fields reject the entire refresh and preserve every local draft value", async () => {
  const h = harness(); await h.login();
  await editConfigFields(h, { "plugins.web_search.global_per_hour": 120, "plugins.web_search.max_calls_per_turn": 2 });
  h.app.state.draft.plugins.ai.personas["本地新增"] = "不能丢失";
  h.app.state.stale = true; h.app.updateControls();
  const before = plainForm(h), baseline = JSON.stringify(h.app.state.config), draft = JSON.stringify(h.app.state.draft);
  const latest = h.snapshot(); latest.revision = "r2"; latest.config.plugins.web_search.global_per_hour = 240;
  latest.config.llm.model = "must-not-partially-apply";
  h.setHandler(async () => ({ body: latest })); await clickConfig(h, "config-refresh-draft-top");
  assert.equal(h.app.state.revision, "r1"); assert.equal(h.app.state.stale, true); assert.equal(h.app.state.dirty, true);
  assert.deepEqual(plainForm(h), before); assert.equal(JSON.stringify(h.app.state.config), baseline); assert.equal(JSON.stringify(h.app.state.draft), draft);
  assert.match(h.ids.get("config-save-message").textContent, /两边都已修改.*未覆盖任何内容/u);
  assert.equal(h.ids.get("config-save-message-top").textContent, h.ids.get("config-save-message").textContent);
  assert.equal(h.calls.some((call) => call.options.method === "PUT"), false);
});

test("stale refresh treats tool grants as an atomic array and accepts identical concurrent changes", async () => {
  for (const same of [false, true]) {
    const h = harness(); h.sample.plugins.ai.defaults.allowed_tools = ["clock_now"]; await h.login();
    const picker = h.app.inputs.get("plugins.ai.defaults.allowed_tools").control.toolPicker;
    picker.rows.get("reminder_create").checkbox.checked = true; await picker.rows.get("reminder_create").checkbox.emit("change");
    h.app.state.stale = true; h.app.updateControls();
    const local = plainForm(h).plugins.ai.defaults.allowed_tools;
    const latest = h.snapshot(); latest.revision = "r2"; latest.config.plugins.ai.defaults.allowed_tools = same ? local : [];
    h.setHandler(async () => ({ body: latest })); await clickConfig(h, "config-refresh-draft");
    assert.equal(h.app.state.stale, !same); assert.equal(h.app.state.revision, same ? "r2" : "r1");
    assert.deepEqual(plainForm(h).plugins.ai.defaults.allowed_tools, local);
    assert.equal(h.calls.some((call) => call.options.method === "PUT"), false);
    if (!same) assert.match(h.ids.get("config-save-message").textContent, /两边都已修改.*未覆盖任何内容/u);
  }
});

test("stale refresh never rebases pending secret changes or loses them on failure", async () => {
  for (const action of ["set", "clear"]) {
    const h = harness(); await h.login(); await editConfigFields(h, { "plugins.web_search.max_calls_per_turn": 2 });
    const secret = h.app.secretInputs.get("search_api_key"); secret.value.value = action === "set" ? "synthetic-pending-secret" : ""; secret.clear = action === "clear";
    h.app.state.stale = true; h.app.updateControls(); const before = plainForm(h), actions = JSON.stringify(h.app.secretChanges());
    h.setHandler(async () => ({ body: { ...h.snapshot(), revision: "r2" } })); await clickConfig(h, "config-refresh-draft");
    assert.equal(h.app.state.revision, "r1"); assert.equal(h.app.state.stale, true); assert.deepEqual(plainForm(h), before);
    assert.equal(JSON.stringify(h.app.secretChanges()), actions); assert.match(h.ids.get("config-save-message").textContent, /密钥/u);
    assert.equal(h.calls.some((call) => call.options.method === "PUT"), false);
    assert.doesNotMatch(h.ids.get("config-save-message").textContent, /synthetic-pending-secret/u);
  }
});

test("stale refresh read failure preserves the draft and other pending writes remain explained", async () => {
  const h = harness(); await h.login(); await editConfigFields(h, { "plugins.web_search.global_per_hour": 120 });
  h.app.state.stale = true; h.app.updateControls(); const before = plainForm(h);
  h.setHandler(async () => ({ error: new Error("synthetic connection lost") })); await clickConfig(h, "config-refresh-draft");
  assert.equal(h.app.state.revision, "r1"); assert.equal(h.app.state.stale, true); assert.deepEqual(plainForm(h), before);
  assert.equal(h.app.state.busy, false); assert.equal(h.ids.get("config-refresh-draft").disabled, false);
  assert.match(h.ids.get("config-save-message").textContent, /连接|失败/u);
  for (const kind of ["memory", "learning"]) {
    h.app.state.pendingOperation = { kind, label: "其他页面待确认操作", requestId: "synthetic-pending" }; h.app.updateControls();
    assert.equal(h.ids.get("save-config").disabled, true); assert.equal(h.ids.get("config-check-operation").hidden, false);
    const refresh = h.ids.get("config-refresh-draft"); assert.ok(refresh.hidden || refresh.disabled, "pending writes do not offer a usable refresh action");
    const count = h.calls.length; await clickConfig(h, "config-refresh-draft"); assert.equal(h.calls.length, count, "the handler also refuses a refresh while any write is unresolved");
    assert.match(h.ids.get("config-save-message").textContent, /其他页面待确认操作/u);
    assert.equal(h.ids.get("config-save-message-top").textContent, h.ids.get("config-save-message").textContent);
  }
});

test("late stale-refresh responses cannot restore a signed-out page or overwrite a new session save", async () => {
  for (const relogin of [false, true]) {
    const h = harness(); await h.login(); await editConfigFields(h, { "plugins.web_search.global_per_hour": 120 });
    h.app.state.stale = true; h.app.updateControls(); let finishRefresh;
    h.setHandler(() => new Promise((resolve) => { finishRefresh = resolve; }));
    await clickConfig(h, "config-refresh-draft"); assert.equal(h.app.state.busy, true);
    h.app.clearSession();
    let finishSave, newSave;
    if (relogin) {
      await h.login(); await editConfigFields(h, { "plugins.web_search.max_calls_per_turn": 2 });
      h.setHandler(() => new Promise((resolve) => { finishSave = resolve; }));
      newSave = h.app.runAction(() => h.app.saveConfig(false)); await new Promise(setImmediate);
      assert.equal(h.app.state.configSave.phase, "正在保存…"); assert.equal(h.app.state.busy, true);
    }
    const stale = h.snapshot(); stale.revision = "old-session-r99"; stale.config.plugins.web_search.global_per_hour = 999;
    finishRefresh({ body: stale }); await new Promise(setImmediate);
    if (!relogin) {
      assert.equal(h.app.state.config, null); assert.equal(h.app.state.authenticated, false);
      assert.equal(h.ids.get("app").hidden, true); assert.equal(h.app.state.configSave.message, "");
    } else {
      assert.equal(h.app.state.revision, "r1"); assert.equal(h.app.state.configSave.phase, "正在保存…");
      assert.equal(h.app.state.busy, true); assert.equal(h.ids.get("save-config").attrs["aria-busy"], "true");
      assert.equal(plainForm(h).plugins.web_search.max_calls_per_turn, 2);
      assert.notEqual(plainForm(h).plugins.web_search.global_per_hour, 999);
      const saved = { ...h.snapshot(), revision: "new-session-r2", config: plainForm(h) };
      h.setHandler(async () => ({ body: saved })); finishSave({ body: {} }); await newSave;
      assert.equal(h.app.state.revision, "new-session-r2"); assert.equal(h.app.state.busy, false);
      assert.match(h.ids.get("config-save-message").textContent, /配置已保存/u);
    }
  }
  // An independent sign-out can run after api's final session check, before
  // refreshConfigDraft resumes. Schedule that exact Promise reaction boundary.
  const h = harness(); await h.login(); await editConfigFields(h, { "plugins.web_search.global_per_hour": 120 });
  h.app.state.stale = true; h.app.updateControls();
  const latest = { ...h.snapshot(), revision: "microtask-r2", config: plainForm(h) };
  h.setHandler(async () => ({ body: { toJSON() {
    let next = Promise.resolve(); for (let step = 0; step < 3; step++) next = next.then(() => {});
    void next.then(() => h.app.clearSession()); return latest;
  } } }));
  await clickConfig(h, "config-refresh-draft");
  assert.equal(h.app.state.authenticated, false); assert.equal(h.app.state.config, null); assert.equal(h.app.state.draft, null);
  assert.equal(h.app.inputs.size, 0); assert.equal(h.ids.get("app").hidden, true);
});

test("stale refresh rejects a merged persona reference deleted remotely without replacing the draft", async () => {
  const h = harness(); h.sample.plugins.ai.personas.Custom = "本地可选的人设"; await h.login();
  await editConfigFields(h, { "plugins.ai.defaults.persona": "Custom", "plugins.web_search.max_calls_per_turn": 2 });
  h.app.state.stale = true; h.app.updateControls();
  const before = plainForm(h), baseline = JSON.stringify(h.app.state.config), draft = JSON.stringify(h.app.state.draft);
  const latest = h.snapshot(); latest.revision = "r2"; delete latest.config.plugins.ai.personas.Custom;
  assert.equal(latest.config.plugins.ai.defaults.persona, "默认");
  h.setHandler(async () => ({ body: latest })); await clickConfig(h, "config-refresh-draft");
  assert.equal(h.app.state.revision, "r1"); assert.equal(h.app.state.stale, true); assert.equal(h.app.state.dirty, true);
  assert.deepEqual(plainForm(h), before); assert.equal(JSON.stringify(h.app.state.config), baseline); assert.equal(JSON.stringify(h.app.state.draft), draft);
  assert.equal(h.app.inputs.get("plugins.ai.defaults.persona").control.value, "Custom");
  assert.match(h.ids.get("config-save-message").textContent, /人设/u);
  assert.equal(h.calls.some((call) => call.options.method === "PUT"), false);
});

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
test("global memory sharing switch survives normal form editing", async () => {
  const h = harness(); await h.login();
  const toggle = h.app.inputs.get("plugins.ai.global_memory_shared").control;
  assert.equal(toggle.checked, true);
  toggle.checked = false; await toggle.emit("input");
  assert.equal(h.app.readForm().plugins.ai.global_memory_shared, false);
  const restored = h.snapshot(); restored.config.plugins.ai.global_memory_shared = false;
  h.setHandler(async () => ({ body: restored })); await h.app.loadConfig();
  assert.equal(h.app.inputs.get("plugins.ai.global_memory_shared").control.checked, false);
});
test("shared memory displays another source conversation and loads its evidence in the current scope", async () => {
  const h = harness(); await h.login();
  const cv = { ...h.conversation(), memory_scope: "bot:1", global_memory_shared: true };
  const record = { id: 1, conversation_id: "1:friend:7:v0:user:7", source_scope: "1:friend:7", source_owner_id: 7, memory_scope: "bot:1", access_conversation_id: cv.id, subject_id: 7, kind: "preference", text: "喜欢红茶", active: true, origin: "auto", version: 1, updated_at: 1800000000 };
  h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? cv : path.includes("/records/1?") ? { ...record, evidence: { items: [{ source_key: "1:friend:7:8", quote: "喜欢红茶" }], total_pages: 1 } } : { memory_scope: "bot:1", access_conversation_id: cv.id, global_memory_shared: true, items: [record], page: 1, total_pages: 1, total: 1 } }));
  await h.app.selectConversation(cv);
  assert.match(h.ids.get("memory-range-description").textContent, /全域共享.*所有群聊和私聊/u);
  assert.match(h.ids.get("memory-list").textContent, /来源：机器人 1 · 私聊 7/u);
  const box = h.document.createElement("div"); h.ids.get("memory-list").append(box);
  await h.app.loadEvidence(record, box, 1);
  assert.match(box.textContent, /喜欢红茶/u);
  h.app.openMemoryEditor(record);
  assert.match(h.ids.get("memory-editor-scope").textContent, /私聊 7.*所有群聊和私聊共享/u);
});
test("shared memory rejects another bot, member, access conversation or mismatched detail", async () => {
  const h = harness(); await h.login();
  const cv = { ...h.conversation("1:friend:7:v0:user:7"), scene: "friend", peer_id: 7, owner_id: 7, memory_scope: "bot:1", global_memory_shared: true };
  h.app.state.memory.selected = cv; h.app.state.memory.detail = cv;
  const record = { id: 1, conversation_id: "1:group:20:v0:group", source_scope: "1:group:20", memory_scope: "bot:1", access_conversation_id: cv.id, subject_id: 7, text: "共享资料", kind: "fact" };
  const page = { memory_scope: "bot:1", access_conversation_id: cv.id, global_memory_shared: true, items: [record], page: 1, total: 1, total_pages: 1 };
  for (const patch of [{ memory_scope: "bot:2" }, { subject_id: 8 }, { access_conversation_id: "another-entry" }]) {
    h.setHandler(async () => ({ body: { ...page, items: [{ ...record, ...patch }] } }));
    await assert.rejects(h.app.loadMemories(), /范围/u);
    assert.equal(h.app.state.memory.rows.length, 0);
  }
  h.setHandler(async () => ({ body: { ...page, global_memory_shared: false } }));
  await assert.rejects(h.app.loadMemories(), /范围/u);
  const box = h.document.createElement("div"); h.ids.get("memory-list").append(box);
  for (const patch of [{ memory_scope: "bot:2" }, { subject_id: 8 }, { conversation_id: "wrong-source" }]) {
    h.setHandler(async () => ({ body: { ...record, ...patch, evidence: { items: [] } } }));
    await assert.rejects(h.app.loadEvidence(record, box, 1), /范围/u);
    assert.equal(box.textContent, "");
  }
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
  groups: Array.from({ length: count }, (_, index) => ({ id: String(1000 + index), name: "群 " + index, remark: index === 0 ? "<img>测试备注" : "", enabled: true, auto_reply_enabled: false })),
  friends: [{ id: "789", name: "小明", remark: "朋友备注", enabled: true, auto_reply_enabled: false }],
});
async function contactPage(h, data = contacts()) {
  await h.login(); h.app.state.view = "contacts";
  h.setHandler(async () => ({ body: data })); await h.app.loadContacts();
}

test("auto reply is off by default and saves only the selected group or friend", async () => {
  const h = harness(); await contactPage(h);
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]')[0].checked, false);
  h.setHandler(async (_path, options) => ({ body: JSON.parse(options.body) }));
  await h.app.toggleContact("group", "1000", true, true);
  assert.equal(h.calls.at(-1).path, "/api/qq/contacts/auto-reply");
  assert.equal(h.calls.at(-1).options.headers["X-CSRF-Token"], "csrf-test");
  assert.deepEqual(h.calls.at(-1).body, { bot_id: "123", scene: "group", peer_id: "1000", enabled: true });
  assert.equal(h.app.state.contacts.groups[0].auto_reply_enabled, true);
  assert.equal(h.app.state.contacts.friends[0].auto_reply_enabled, false);
  assert.equal(h.app.state.contacts.groups[0].enabled, true);
  await h.ids.get("contacts-friends").emit("click");
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]')[0].checked, false);
  await h.app.toggleContact("friend", "789", true, true);
  assert.equal(h.calls.at(-1).body.scene, "friend");
  assert.equal(h.app.state.contacts.friends[0].auto_reply_enabled, true);
  await h.app.toggleContact("friend", "789", false, true);
  assert.match(h.ids.get("contacts-message").textContent, /私聊仍可正常直接对话/u);
});

test("auto reply failed and stale responses cannot change a different contact or session", async () => {
  const h = harness(); await contactPage(h);
  h.setHandler(async () => ({ body: { bot_id: "123", scene: "friend", peer_id: "1000", enabled: true } }));
  await h.app.toggleContact("group", "1000", true, true);
  assert.equal(h.app.state.contacts.groups[0].auto_reply_enabled, false);
  assert.equal(h.app.state.contacts.phase, "uncertain");
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]')[0].disabled, true);
  h.setHandler(async () => ({ body: contacts() })); await h.app.loadContacts(true);
  let finish; h.setHandler(() => new Promise((resolve) => { finish = resolve; }));
  const pending = h.app.toggleContact("group", "1000", true, true); await Promise.resolve();
  h.app.clearSession(); finish({ body: { bot_id: "123", scene: "group", peer_id: "1000", enabled: true } }); await pending;
  assert.equal(h.app.state.contacts.groups.length, 0);
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]').length, 0);
});

test("cached contacts permit disabling auto reply but require live refresh to enable it", async () => {
  const h = harness(), data = contacts(); data.groups[0].auto_reply_enabled = true; await contactPage(h, data);
  h.setHandler(async () => ({ status: 502, body: { code: "contacts_offline", error: "offline" } })); await h.app.loadContacts(true);
  assert.equal(h.app.state.contacts.phase, "cached");
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]')[0].disabled, false);
  h.setHandler(async (_path, options) => ({ body: JSON.parse(options.body) }));
  await h.app.toggleContact("group", "1000", false, true);
  const count = h.calls.length; await h.app.toggleContact("group", "1000", true, true);
  assert.equal(h.calls.length, count); assert.equal(h.app.state.contacts.groups[0].auto_reply_enabled, false);
  assert.equal(h.document.querySelectorAll('[data-contact-auto-reply]')[0].disabled, true);
});

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
const embeddingResponse = () => ({ dimensions: 1024, text_ok: true, image_ok: true, key_source: "temporary", environment_overrides_saved_key: false });
async function prepareEmbedding(h) {
  await h.login();
  h.app.inputs.get("embedding.embeddings_url").control.value = "https://vectors.example/v1/embeddings";
  h.app.inputs.get("embedding.model").control.value = "Qwen/Qwen3-VL-Embedding-8B";
  h.app.inputs.get("embedding.dimensions").control.value = "1024";
  h.app.inputs.get("embedding.request_timeout_secs").control.value = "15";
  h.app.secretInputs.get("embedding_api_key").value.value = "temporary-vector-key";
}

test("embedding has independent controls and a synthetic-only preview that does not save", async () => {
  const h = harness(); await prepareEmbedding(h);
  h.app.secretInputs.get("llm_api_key").value.value = "unrelated-chat-key";
  const before = h.calls.length;
  h.setHandler(async () => ({ body: embeddingResponse() }));
  await h.app.testEmbedding();
  assert.equal(h.calls.length, before + 1);
  const request = h.calls.at(-1);
  assert.equal(request.path, "/api/embedding/test");
  assert.equal(request.options.headers["X-CSRF-Token"], "csrf-test");
  assert.deepEqual(request.body, {
    revision: "r1", url: "https://vectors.example/v1/embeddings", model: "Qwen/Qwen3-VL-Embedding-8B",
    dimensions: 1024, request_timeout_secs: 15, api_key: { action: "set", value: "temporary-vector-key" },
  });
  assert.doesNotMatch(JSON.stringify(request.body), /unrelated-chat-key|messages|conversation/u);
  assert.match(h.ids.get("embedding-test-message").textContent, /1024.*配置尚未/u);
  assert.equal(h.app.readForm().embedding.api_key, "");
  h.app.inputs.get("stickers.enabled").control.checked = true;
  h.app.inputs.get("stickers.auto_collect").control.checked = false;
  h.app.inputs.get("stickers.send_cooldown_secs").control.value = "90";
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.readForm().stickers)), { enabled: true, auto_collect: false, send_cooldown_secs: 90 });
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.secretChanges().embedding_api_key)), { action: "set", value: "temporary-vector-key" });
  const draft = structuredClone(h.sample); draft.embedding.api_key = "must-not-go-in-json";
  h.ids.get("config-json").value = JSON.stringify(draft);
  assert.throws(() => h.app.adoptJson(), /embedding.api_key/u);
});

test("embedding preview suppresses duplicate requests and results from changed drafts", async () => {
  for (const change of [
    async (h) => { const field = h.app.inputs.get("embedding.dimensions").control; field.value = "512"; await field.emit("input"); },
    async (h) => { const field = h.app.secretInputs.get("embedding_api_key").value; field.value = "replacement"; await field.emit("input"); },
    async (h) => { h.app.clearSession(); },
  ]) {
    const h = harness(); await prepareEmbedding(h);
    let resolve;
    h.setHandler(() => new Promise((done) => { resolve = done; }));
    const before = h.calls.length, pending = h.app.testEmbedding();
    await h.app.testEmbedding();
    assert.equal(h.calls.length, before + 1);
    assert.equal(h.ids.get("embedding-test").disabled, true);
    await change(h);
    resolve({ body: embeddingResponse() }); await pending;
    assert.equal(h.app.state.embedding.message, "");
    assert.equal(h.app.state.embedding.loading, false);
  }
});

test("embedding key actions and invalid provider capabilities remain visible without saving", async () => {
  const h = harness(); await prepareEmbedding(h);
  const key = h.app.secretInputs.get("embedding_api_key");
  key.value.value = "";
  h.setHandler(async () => ({ status: 400, body: { code: "embedding_test_key_required", error: "新地址需要新密钥。" } }));
  await h.app.testEmbedding();
  assert.deepEqual(h.calls.at(-1).body.api_key, { action: "keep" });
  assert.match(h.ids.get("embedding-test-message").textContent, /新密钥/u);
  await key.clearButton.emit("click");
  await h.app.testEmbedding();
  assert.deepEqual(h.calls.at(-1).body.api_key, { action: "clear" });
  key.value.value = "new-key"; await key.value.emit("input");
  h.setHandler(async () => ({ body: { ...embeddingResponse(), dimensions: 512 } }));
  await h.app.testEmbedding();
  assert.equal(h.app.state.embedding.kind, "error");
  assert.match(h.ids.get("embedding-test-message").textContent, /维度/u);
  assert.equal(h.app.inputs.get("embedding.dimensions").control.value, "1024");
  const calls = h.calls.length;
  h.app.inputs.get("embedding.dimensions").control.value = "63";
  await h.app.testEmbedding();
  assert.equal(h.calls.length, calls);
});
async function prepareModels(h) {
  await h.login(); h.app.inputs.get("llm.chat_completions_url").control.value = "https://models.example/v1";
  h.app.inputs.get("llm.model").control.value = "previous-model";
  h.app.secretInputs.get("llm_api_key").value.value = "temporary-test-key";
}

test("Responses selection keeps both endpoints and discovers models for the selected protocol", async () => {
  const h = harness(); await prepareModels(h);
  const protocol = h.app.inputs.get("llm.protocol").control;
  const chat = h.app.inputs.get("llm.chat_completions_url"), responses = h.app.inputs.get("llm.responses_url");
  assert.equal(protocol.value, "chat_completions");
  assert.equal(chat.wrapper.hidden, false); assert.equal(responses.wrapper.hidden, true);
  responses.control.value = "https://models.example/v1";
  protocol.value = "responses"; await protocol.emit("input");
  assert.equal(chat.wrapper.hidden, true); assert.equal(responses.wrapper.hidden, false);
  h.setHandler(async () => ({ body: { ...modelsResponse(), responses_url: "https://models.example/v1/responses" } }));
  await h.app.fetchModels();
  assert.equal(h.calls.at(-1).body.url, "https://models.example/v1");
  assert.equal(responses.control.value, "https://models.example/v1/responses");
  assert.equal(chat.control.value, "https://models.example/v1");
  assert.equal(h.app.readForm().llm.protocol, "responses");
  assert.equal(h.app.readForm().llm.model, "previous-model");
  protocol.value = "chat_completions"; await protocol.emit("input");
  assert.equal(h.app.state.models.items.length, 0);
  assert.equal(chat.wrapper.hidden, false); assert.equal(responses.wrapper.hidden, true);
  assert.equal(responses.control.value, "https://models.example/v1/responses");
  assert.equal(h.app.secretInputs.get("llm_api_key").value.value, "temporary-test-key");
});

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
    async (h) => { const control = h.app.inputs.get("llm.protocol").control; control.value = "responses"; await control.emit("input"); },
    async (h) => { const control = h.app.inputs.get("llm.protocol").control; control.value = "responses"; await control.emit("input"); control.value = "chat_completions"; await control.emit("input"); },
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

test("vision switch roundtrips independently of native tools and model selection", async () => {
  const h = harness(); await h.login();
  const vision = h.app.inputs.get("llm.vision").control;
  assert.equal(vision.type, "checkbox"); assert.equal(vision.checked, true);
  const initial = h.app.readForm().llm;
  vision.checked = false; await vision.emit("input");
  assert.equal(h.app.readForm().llm.vision, false); assert.equal(h.app.state.dirty, true);
  assert.equal(h.app.readForm().llm.tool_calling, initial.tool_calling);
  assert.equal(h.app.readForm().llm.model, initial.model);
  h.app.syncJson(); h.app.adoptJson();
  assert.equal(h.app.inputs.get("llm.vision").control.checked, false);
  const saved = h.snapshot(); saved.config.llm.vision = false;
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig();
  assert.equal(h.app.inputs.get("llm.vision").control.checked, false);
  const restored = h.app.inputs.get("llm.vision").control; restored.checked = true; await restored.emit("input");
  assert.equal(h.app.readForm().llm.vision, true);
});

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

async function appliedConversation(h) {
  h.sample.plugins.ai.defaults.allowed_tools = ["clock_now"];
  await h.login();
  h.setHandler(async () => ({ body: { phase: "running", applied_revision: "r1", loaded_plugins: ["ai", "clock"] } }));
  await h.app.refreshStatus();
  const detail = structuredClone(h.conversation());
  h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? detail : emptyConversations }));
  await h.app.selectConversation(detail);
  return detail;
}

async function saveSearchDefaults(h) {
  const saved = h.snapshot(); saved.revision = "r2";
  saved.config.plugins.ai.defaults.allowed_tools = ["clock_now", "web_search"];
  saved.tool_catalog.push({ name: "web_search", description: "联网搜索", plugin_id: "web_search", effect: "network_read" });
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig();
  return saved;
}

async function observeApplied(h, revision = "r2") {
  h.setHandler(async () => ({ body: { phase: "running", applied_revision: revision, loaded_plugins: ["ai", "clock", "web_search"] } }));
  await h.app.refreshStatus();
}

test("applied defaults invalidate clean conversation settings so a budget edit cannot remove a newly allowed search tool", async () => {
  const h = harness(), old = await appliedConversation(h);
  await saveSearchDefaults(h);
  assert.equal(h.app.state.status.applied_revision, "r1");
  assert.equal(h.app.state.memory.conflict, false, "saving without applying must preserve the running conversation settings");
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now"]);
  await observeApplied(h);
  assert.equal(h.app.state.memory.conflict, true);
  assert.equal(h.ids.get("conversation-conflict").hidden, false);
  assert.match(h.ids.get("conversation-conflict").textContent, /运行配置已切换.*草稿仍保留.*重新载入/u);
  assert.match(h.ids.get("conversation-dirty").textContent, /保存已暂停/u);
  assert.equal(h.ids.get("conversation-settings-fields").disabled, true);
  assert.equal(h.ids.get("save-conversation-settings").disabled, true);
  assert.equal(h.ids.get("reload-conversation").disabled, false);
  const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
  await h.ids.get("conversation-settings-form").emit("submit"); await new Promise(setImmediate);
  assert.equal(h.calls.some((call) => call.options.method === "POST"), false);
  assert.equal(budget.value, "12000");
  const fresh = { ...old, settings: { ...old.settings, allowed_tools: ["clock_now", "web_search"] } };
  let sent;
  h.setHandler(async (path, options) => {
    if (path === "/api/status") return { body: { phase: "running", applied_revision: "r2" } };
    if (path === "/api/memory/mutate") { sent = JSON.parse(options.body); return { body: { request_id: sent.request_id, status: "completed" } }; }
    return { body: path.includes("/conversations/") ? fresh : emptyConversations };
  });
  await h.ids.get("reload-conversation").emit("click"); await new Promise(setImmediate);
  assert.equal(h.app.state.memory.conflict, false); assert.equal(h.app.state.memory.settingsDirty, false);
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now", "web_search"]);
  const freshBudget = h.app.settingInputs.get("input_budget").input; freshBudget.value = "12000"; await freshBudget.emit("input");
  await h.ids.get("conversation-settings-form").emit("submit"); await new Promise(setImmediate);
  assert.deepEqual(sent.action.settings.allowed_tools, ["clock_now", "web_search"]);
  assert.equal(sent.action.settings.input_budget, 12000);
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 1);
});

test("applied revision changes preserve dirty conversation drafts and explicit overrides until the user reloads", async () => {
  const h = harness(), old = await appliedConversation(h);
  const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
  const selected = toolCheckbox(conversationTools(h), "clock_now"); selected.checked = false; await selected.emit("change");
  h.app.openMemoryEditor({ id: 5, conversation_id: old.id, subject_id: 7, text: "原记录", kind: "fact", active: true, version: 1 });
  h.ids.get("edit-memory-text").value = "未保存记忆正文"; h.app.state.memory.editorDirty = true;
  h.app.state.memory.preview = { preview_id: "old-preview" }; h.ids.get("forget-dialog").showModal();
  await saveSearchDefaults(h); await observeApplied(h);
  assert.equal(h.app.settingInputs.get("input_budget").input, budget); assert.equal(budget.value, "12000");
  assert.deepEqual([...h.app.readSettings().allowed_tools], []);
  assert.equal(h.app.state.memory.settingsDirty, true); assert.equal(h.app.state.memory.editorDirty, true);
  assert.equal(h.ids.get("edit-memory-text").value, "未保存记忆正文"); assert.equal(h.ids.get("memory-editor").open, true);
  assert.equal(h.ids.get("save-memory").disabled, true); assert.equal(h.app.state.memory.preview, null); assert.equal(h.ids.get("forget-dialog").open, false);
  h.window.confirm = () => false;
  const count = h.calls.length; await h.ids.get("reload-conversation").emit("click"); await new Promise(setImmediate);
  assert.equal(h.calls.length, count); assert.equal(h.app.state.memory.conflict, true); assert.equal(budget.value, "12000");
  h.window.confirm = () => true;
  h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? old : emptyConversations }));
  await h.ids.get("reload-conversation").emit("click"); await new Promise(setImmediate);
  assert.equal(h.app.state.memory.conflict, false);
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now"], "an authoritative explicit override must not be expanded to global tools");
  assert.equal(h.app.state.memory.editorDirty, false); assert.equal(h.ids.get("memory-editor").open, false);
});

test("conversation reload retires older status responses and their errors without changing the fresh form", async () => {
  for (const rejected of [false, true]) {
    const h = harness(), old = await appliedConversation(h); await saveSearchDefaults(h); await observeApplied(h);
    let finish;
    h.setHandler(() => new Promise((resolve) => { finish = resolve; })); const pending = h.app.refreshStatus();
    const fresh = { ...old, settings: { ...old.settings, allowed_tools: ["clock_now", "web_search"] } };
    h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? fresh : emptyConversations }));
    await h.app.selectConversation(fresh);
    const version = h.app.state.memory.version, field = h.app.settingInputs.get("input_budget").input;
    finish(rejected ? { error: new Error("stale network failure") } : { body: { phase: "running", applied_revision: "r1", loaded_plugins: ["ai", "clock"] } });
    await pending;
    assert.equal(h.app.state.status.applied_revision, "r2"); assert.equal(h.app.state.memory.conflict, false);
    assert.equal(h.app.state.memory.version, version); assert.equal(h.app.settingInputs.get("input_budget").input, field);
    assert.deepEqual([...h.app.readSettings().allowed_tools], ["clock_now", "web_search"]);
  }
});

test("a later applied revision rejects an in-flight conversation reload and keeps a visible recovery entry", async () => {
  const h = harness(), old = await appliedConversation(h); let finish;
  h.setHandler(() => new Promise((resolve) => { finish = resolve; }));
  const pending = h.app.selectConversation(old);
  await observeApplied(h);
  assert.equal(h.app.state.memory.conflict, true); assert.equal(h.ids.get("selected-conversation").hidden, false);
  assert.equal(h.ids.get("reload-conversation").disabled, false);
  finish({ body: old }); await pending;
  assert.equal(h.app.state.memory.detail, null); assert.equal(h.app.state.memory.conflict, true);
  assert.equal(h.ids.get("conversation-settings-fields").disabled, true);
  const fresh = { ...old, settings: { ...old.settings, input_budget: 9000 } };
  h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? fresh : emptyConversations }));
  await h.app.selectConversation(fresh);
  assert.equal(h.app.state.memory.conflict, false); assert.equal(h.app.readSettings().input_budget, 9000);
});

test("late conversation and runtime reads cannot cross a selection change or a signed-out session", async () => {
  for (const logout of [false, true]) {
    const h = harness(), old = await appliedConversation(h); let finishDetail, finishStatus;
    h.setHandler((path) => new Promise((resolve) => { if (path === "/api/status") finishStatus = resolve; else finishDetail = resolve; }));
    const pendingDetail = h.app.selectConversation(old), pendingStatus = h.app.refreshStatus();
    if (logout) { h.app.clearSession(); await h.login(); }
    const next = { ...h.conversation("1:friend:30:0"), scene: "friend", peer_id: 30, scope: "1:friend:30", settings: { ...old.settings, allowed_tools: [] } };
    h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? next : emptyConversations }));
    await h.app.selectConversation(next);
    const status = h.app.state.status, version = h.app.state.memory.version;
    finishDetail({ body: old }); finishStatus({ body: { phase: "running", applied_revision: "old-r0", loaded_plugins: [] } });
    await Promise.all([pendingDetail, pendingStatus]);
    assert.equal(h.app.state.memory.selected.id, next.id); assert.equal(h.app.state.memory.version, version);
    assert.equal(h.app.state.memory.conflict, false); assert.equal(h.app.state.status, status);
    assert.deepEqual([...h.app.readSettings().allowed_tools], []);
  }
});

test("runtime-operation readback supersedes polling before marking a conversation stale", async () => {
  const h = harness(); await appliedConversation(h); let finish;
  h.setHandler(() => new Promise((resolve) => { finish = resolve; })); const pending = h.app.refreshStatus();
  h.app.state.pendingOperation = { kind: "runtime", goal: "running", revision: "r2" };
  h.setHandler(async () => ({ body: { phase: "running", applied_revision: "r2", loaded_plugins: ["ai", "clock", "web_search"] } }));
  await h.app.checkOperation();
  assert.equal(h.app.state.pendingOperation, null); assert.equal(h.app.state.memory.conflict, true);
  const version = h.app.state.memory.version;
  finish({ body: { phase: "running", applied_revision: "r1", loaded_plugins: ["ai", "clock"] } }); await pending;
  assert.equal(h.app.state.status.applied_revision, "r2"); assert.equal(h.app.state.memory.version, version);
});

test("settings save checks applied configuration even while an older runtime poll is pending", async () => {
  const h = harness(); await appliedConversation(h); await saveSearchDefaults(h);
  let finishOld;
  h.setHandler(() => new Promise((resolve) => { finishOld = resolve; })); const pending = h.app.refreshStatus();
  const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
  h.setHandler(async (path) => { assert.equal(path, "/api/status"); return { body: { phase: "running", applied_revision: "r2" } }; });
  await h.ids.get("conversation-settings-form").emit("submit"); await new Promise(setImmediate);
  assert.equal(h.app.state.memory.conflict, true); assert.equal(budget.value, "12000"); assert.equal(h.app.state.memory.settingsDirty, true);
  assert.equal(h.calls.filter((call) => call.options.method === "POST").length, 0);
  finishOld({ body: { phase: "running", applied_revision: "r1" } }); await pending;
  assert.equal(h.app.state.status.applied_revision, "r2"); assert.equal(h.app.state.memory.conflict, true);
});

test("settings preflight permits saved-but-unapplied defaults and rejects unavailable or transitioning runtime state", async () => {
  for (const scenario of ["saved-only", "starting", "stopping", "error", "malformed", "superseded", "logout", "selection"]) {
    const h = harness(), old = await appliedConversation(h); await saveSearchDefaults(h);
    const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
    let finish, sent;
    h.setHandler(async (path, options) => {
      if (path === "/api/status") {
        if (["superseded", "logout", "selection"].includes(scenario)) return new Promise((resolve) => { finish = resolve; });
        if (scenario === "error") return { error: new Error("synthetic status failure") };
        return { body: scenario === "malformed" ? {} : { phase: scenario === "saved-only" ? "running" : scenario, applied_revision: "r1" } };
      }
      if (path === "/api/memory/mutate") { sent = JSON.parse(options.body); return { body: { request_id: sent.request_id, status: "completed" } }; }
      return { body: path.includes("/conversations/") ? old : emptyConversations };
    });
    const saving = h.app.runAction(() => h.app.mutateMemory({ op: "settings", settings: h.app.readSettings() }));
    if (["superseded", "logout", "selection"].includes(scenario)) {
      assert.ok(finish);
      if (scenario === "logout") h.app.clearSession();
      else {
        const next = scenario === "selection" ? { ...old, id: "1:friend:20:0", scene: "friend", scope: "1:friend:20", peer_id: 20 } : old;
        h.setHandler(async (path) => ({ body: path.includes("/conversations/") ? next : emptyConversations }));
        await h.app.selectConversation(next);
      }
      finish({ body: { phase: "running", applied_revision: "r1" } });
    }
    await saving;
    assert.equal(h.calls.filter((call) => call.options.method === "POST").length, scenario === "saved-only" ? 1 : 0, scenario);
    if (scenario === "saved-only") { assert.deepEqual(sent.action.settings.allowed_tools, ["clock_now"]); assert.equal(sent.action.settings.input_budget, 12000); }
    else if (["starting", "stopping", "error", "malformed"].includes(scenario)) { assert.equal(budget.value, "12000"); assert.equal(h.app.state.memory.settingsDirty, true); }
  }
});

test("an accepted queued apply blocks old conversation settings until the target runs or a terminal transition is confirmed", async () => {
  for (const outcome of ["running", "error", "stopped", "logout", "initially-stopped"]) {
    const h = harness(), old = await appliedConversation(h); await saveSearchDefaults(h);
    let status = { phase: "running", applied_revision: "r1", runtime_generation: 1 };
    if (outcome === "initially-stopped") {
      status = { phase: "stopped", applied_revision: null, runtime_generation: 1 };
      h.setHandler(async (path) => ({ body: path === "/api/status" ? status : path.includes("/conversations/") ? old : emptyConversations }));
      await h.app.refreshStatus(); await h.app.selectConversation(old);
    }
    const budget = h.app.settingInputs.get("input_budget").input; budget.value = "12000"; await budget.emit("input");
    h.setHandler(async (path) => {
      if (path === "/api/runtime/apply" || path === "/api/runtime/stop") return { status: 202, body: { accepted: true } };
      assert.equal(path, "/api/status"); return { body: status };
    });
    await h.ids.get("apply-runtime").emit("click"); await new Promise(setImmediate);
    assert.equal(h.app.state.runtimeApplyTarget.revision, "r2");
    assert.equal(h.app.state.memory.conflict, false);
    await h.ids.get("conversation-settings-form").emit("submit"); await new Promise(setImmediate);
    assert.equal(h.calls.some((call) => call.path === "/api/memory/mutate"), false, outcome);
    assert.match(h.ids.get("notice-text").textContent, /应用请求已排队.*尚未保存/u);
    assert.equal(budget.value, "12000"); assert.equal(h.app.state.memory.settingsDirty, true);
    if (outcome === "logout") {
      h.app.clearSession(); await h.login(); assert.equal(h.app.state.runtimeApplyTarget, null); continue;
    }
    if (outcome === "stopped") {
      await h.ids.get("stop-runtime").emit("click"); await new Promise(setImmediate);
      assert.ok(h.app.state.runtimeApplyTarget, "accepting stop while the old host still runs must not unlock settings");
    }
    status = outcome === "running" ? { phase: "running", applied_revision: "r2", runtime_generation: 2 }
      : { phase: outcome === "initially-stopped" ? "error" : outcome, applied_revision: null, runtime_generation: 2 };
    await h.app.refreshStatus();
    assert.equal(h.app.state.runtimeApplyTarget, null, outcome);
    assert.equal(h.app.state.memory.conflict, true, "failed startup can change defaults before publishing an applied revision");
    assert.equal(budget.value, "12000");
  }
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

test("final reply tools display their own effect and retain global and conversation permissions", async () => {
  const h = harness(); await h.login(); const saved = h.snapshot();
  saved.tool_catalog.push({ name: "reply_message", description: "提交本轮唯一回复，可引用及 @", plugin_id: "ai", effect: "reply" });
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig();
  assert.match(globalTools(h).textContent, /本轮回复 · ai/u);
  const reply = toolCheckbox(globalTools(h), "reply_message"); assert.equal(reply.disabled, false); assert.equal(reply.checked, false);
  reply.checked = true; await reply.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["reply_message"]);
  h.app.state.memory.detail = h.conversation(); h.app.renderSettings();
  assert.equal(toolCheckbox(conversationTools(h), "reply_message").disabled, true);
  saved.config.plugins.ai.defaults.allowed_tools = ["reply_message"];
  await h.app.loadConfig();
  assert.equal(toolCheckbox(conversationTools(h), "reply_message").disabled, false);
  const conversationReply = toolCheckbox(conversationTools(h), "reply_message"); conversationReply.checked = true; await conversationReply.emit("change");
  assert.deepEqual([...h.app.readSettings().allowed_tools], ["reply_message"]);
  const enabled = h.app.inputs.get("plugins.ai.enabled").control; enabled.checked = false; await enabled.emit("input");
  assert.match(globalTools(h).textContent, /所属插件已关闭/u);
  assert.equal(toolCheckbox(globalTools(h), "reply_message").checked, true);
});

test("asynchronous sticker search remains read-only and keeps explicit tool selection", async () => {
  const h = harness(); await h.login(); const saved = h.snapshot();
  saved.tool_catalog.push({ name: "search_stickers", description: "搜索当前群的表情", plugin_id: "ai", effect: "async_read" });
  h.setHandler(async () => ({ body: saved })); await h.app.loadConfig();
  assert.match(globalTools(h).textContent, /异步只读 · ai/u);
  const search = toolCheckbox(globalTools(h), "search_stickers");
  assert.equal(search.checked, false); assert.equal(search.disabled, false);
  search.checked = true; await search.emit("change");
  assert.deepEqual([...h.app.readForm().plugins.ai.defaults.allowed_tools], ["search_stickers"]);
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

const stickerGroup = (group = 10) => ({ scope: `1:group:${group}`, bot_id: 1, group_id: group, count: 2, bytes: 2048, pending: 1, failed: 0 });
const stickerGroups = (groups = [stickerGroup()], page = 1, totalPages = 1) => ({ groups, page, total_pages: totalPages, total: groups.length });
const stickerItem = (id = 1, overrides = {}) => ({ id, revision: 2, mime: 'image/gif', bytes: 1024, thumbnail_bytes: 100, created_at: 1800000000, updated_at: 1800000001, index_status: 'pending', ...overrides });
const stickerPage = (scope = '1:group:10', items = [stickerItem()], page = 1, totalPages = 1) => ({ scope, items, total: items.length, page, total_pages: totalPages, page_size: 20, index: { enabled: true, model: 'synthetic-model', dimensions: 1024 }, running: true });
async function gallery(h, groups = [stickerGroup()]) {
  await h.login(); h.app.state.view = 'stickers';
  h.setHandler(async (path) => ({ body: path.startsWith('/api/stickers/groups') ? stickerGroups(groups) : stickerPage(new URL(path, 'https://local.test').searchParams.get('scope')) }));
  await h.app.loadStickerGroups(); await h.app.selectStickerGroup(groups[0].scope);
}

test('sticker gallery is lazy, paginated, and uses only authenticated same-origin image routes', async () => {
  const h = harness(); await h.login(); assert.equal(h.calls.some((call) => call.path.startsWith('/api/stickers')), false);
  h.app.state.view = 'stickers';
  h.setHandler(async (path) => ({ body: path.startsWith('/api/stickers/groups') ? stickerGroups([stickerGroup()], 1, 2) : stickerPage('1:group:10', [stickerItem(), stickerItem(2, { index_status: 'failed' })], 1, 2) }));
  await h.app.loadStickerGroups(); await h.app.selectStickerGroup('1:group:10');
  assert.equal(h.document.querySelectorAll('.sticker-card').length, 2);
  assert.equal(h.ids.get('stickers-next').disabled, false); assert.equal(h.ids.get('sticker-groups-next').disabled, false);
  const image = h.document.querySelectorAll('.sticker-image')[0];
  assert.equal(image.src, '/api/stickers/items/1/image?scope=1%3Agroup%3A10&variant=thumbnail&revision=2');
  assert.match(h.ids.get('sticker-grid').textContent, /等待索引/u); assert.match(h.ids.get('sticker-grid').textContent, /索引失败/u);
  h.app.openStickerPreview(h.app.state.stickers.rows[0]);
  const original = h.ids.get('sticker-preview-image'); assert.match(original.src, /variant=original/u);
  original.onload(); assert.match(h.ids.get('sticker-preview-message').textContent, /GIF/u);
  await h.app.selectStickerGroup(''); assert.equal(original.src, ''); assert.equal(image.src, ''); assert.equal(h.ids.get('sticker-preview-dialog').open, false);
  h.setHandler(async () => ({ body: stickerGroups([stickerGroup(20)], 2, 2) })); await h.app.changeStickerGroupPage(2);
  assert.equal(h.app.state.stickers.groupPage, 2); assert.match(h.ids.get('sticker-group').textContent, /20/u); assert.equal(h.app.state.stickers.scope, '');
});

test('sticker item responses are bound to session, selected group, and latest request', async () => {
  const h = harness(); await gallery(h, [stickerGroup(), stickerGroup(20)]); let old;
  h.setHandler((path) => new URL(path, 'https://local.test').searchParams.get('scope') === '1:group:10' ? new Promise((resolve) => { old = resolve; }) : Promise.resolve({ body: stickerPage('1:group:20', [stickerItem(20)]) }));
  const first = h.app.loadStickerItems(); await Promise.resolve(); await h.app.selectStickerGroup('1:group:20');
  old({ body: stickerPage('1:group:10', [stickerItem(10)]) }); await first;
  assert.equal(h.app.state.stickers.rows[0].id, 20); assert.doesNotMatch(h.ids.get('sticker-grid').textContent, /#10\b/u);
  let earlier; h.setHandler(() => new Promise((resolve) => { earlier = resolve; })); const stale = h.app.loadStickerItems(); await Promise.resolve();
  h.setHandler(async () => ({ body: stickerPage('1:group:20', [stickerItem(21)]) })); await h.app.loadStickerItems();
  earlier({ body: stickerPage('1:group:20', [stickerItem(22)]) }); await stale; assert.equal(h.app.state.stickers.rows[0].id, 21);
  let afterLogout; h.setHandler(() => new Promise((resolve) => { afterLogout = resolve; })); const pending = h.app.loadStickerItems(); await Promise.resolve();
  h.app.clearSession(); afterLogout({ body: stickerPage('1:group:20', [stickerItem(23)]) }); await pending;
  assert.equal(h.app.state.stickers.rows.length, 0); assert.equal(h.document.querySelectorAll('.sticker-image').length, 0); assert.equal(h.ids.get('sticker-preview-image').src, '');
});

test('sticker gallery rejects wrong scopes and malformed image identities before constructing URLs', async () => {
  for (const invalid of [stickerPage('1:group:99'), stickerPage('1:group:10', [stickerItem('../escape')]), stickerPage('1:group:10', [stickerItem(1, { mime: 'image/svg+xml' })]), stickerPage('1:group:10', [stickerItem(1, { scope: '1:group:99' })])]) {
    const h = harness(); await gallery(h); h.setHandler(async () => ({ body: invalid })); await h.app.loadStickerItems();
    assert.equal(h.app.state.stickers.rows.length, 0); assert.equal(h.document.querySelectorAll('.sticker-image').length, 0); assert.match(h.ids.get('sticker-message').textContent, /范围或格式/u);
  }
});

test('sticker deletion confirms exact group and item; cancellation never submits', async () => {
  const h = harness(); await gallery(h); const before = h.calls.length; let confirmation;
  h.window.confirm = (text) => { confirmation = text; return false; };
  await h.app.mutateSticker('delete', h.app.state.stickers.rows[0]);
  assert.match(confirmation, /群 10.*机器人 1.*#1/u); assert.equal(h.calls.length, before);
  h.window.confirm = () => true;
  h.setHandler(async (path, options) => {
    if (path === '/api/stickers/delete') { const payload = JSON.parse(options.body); return { body: { request_id: payload.request_id, scope: payload.scope, action: 'delete', affected: 1, receipt: 'deleted' } }; }
    return { body: path.startsWith('/api/stickers/groups') ? stickerGroups() : stickerPage('1:group:10', []) };
  });
  await h.app.mutateSticker('delete', h.app.state.stickers.rows[0]);
  const write = h.calls.find((call) => call.path === '/api/stickers/delete');
  assert.equal(write.body.id, 1); assert.equal(write.body.revision, 2); assert.equal(write.body.scope, '1:group:10'); assert.equal(write.options.headers['X-CSRF-Token'], 'csrf-test');
  assert.equal(h.app.state.stickers.operation, null); assert.match(h.ids.get('sticker-message').textContent, /已删除 1/u);
});

test('ambiguous sticker writes retain the exact request id, block duplicate actions, and retry idempotently', async () => {
  for (const failure of [{ error: new Error('timeout') }, { raw: 'not JSON' }, { body: { request_id: 'wrong', scope: '1:group:10', action: 'reindex', affected: 1, receipt: 'queued' } }]) {
    const h = harness(); await gallery(h); const item = h.app.state.stickers.rows[0]; h.setHandler(async () => failure);
    await h.app.mutateSticker('reindex', item); const operation = h.app.state.stickers.operation; assert.ok(operation); assert.equal(operation.sending, false);
    assert.doesNotMatch(h.ids.get('sticker-message').textContent, /已提交|已完成/u);
    const before = h.calls.length; await h.app.mutateSticker('delete', item); assert.equal(h.calls.length, before);
    h.setHandler(async (path, options) => {
      if (path === '/api/stickers/reindex') { const payload = JSON.parse(options.body); return { body: { request_id: payload.request_id, scope: payload.scope, action: 'reindex', affected: 1, receipt: 'queued' } }; }
      return { body: path.startsWith('/api/stickers/groups') ? stickerGroups() : stickerPage() };
    });
    await h.app.retryStickerOperation(); const writes = h.calls.filter((call) => call.path === '/api/stickers/reindex');
    assert.equal(writes.length, 2); assert.deepEqual(writes[0].body, writes[1].body); assert.equal(h.app.state.stickers.operation, null);
    assert.match(h.ids.get('sticker-message').textContent, /已提交.*尚未完成/u);
  }
});

test('sticker conflicts never report success and group reindex confirms API usage without item IDs', async () => {
  const h = harness(); await gallery(h); let text; h.window.confirm = (message) => { text = message; return true; };
  h.setHandler(async () => ({ status: 409, body: { error: 'changed' } })); await h.app.mutateSticker('reindex');
  assert.match(text, /全部.*向量 API.*请求用量/u); const write = h.calls.at(-1);
  assert.equal(write.body.id, undefined); assert.equal(write.body.revision, undefined); assert.equal(write.body.scope, '1:group:10');
  assert.equal(h.app.state.stickers.operation, null); assert.match(h.ids.get('sticker-message').textContent, /已变化.*请刷新/u); assert.doesNotMatch(h.ids.get('sticker-message').textContent, /已提交|已完成/u);
});

test('sticker mutation completion cannot repaint a newly selected group or restore images after logout', async () => {
  const h = harness(); await gallery(h, [stickerGroup(), stickerGroup(20)]); let complete;
  h.setHandler((path) => path === '/api/stickers/reindex' ? new Promise((resolve) => { complete = resolve; }) : Promise.resolve({ body: stickerPage('1:group:20', [stickerItem(20)]) }));
  const pending = h.app.mutateSticker('reindex', h.app.state.stickers.rows[0]); await Promise.resolve();
  const payload = h.app.state.stickers.operation.payload; await h.app.selectStickerGroup('1:group:20');
  complete({ body: { request_id: payload.request_id, scope: payload.scope, action: 'reindex', affected: 1, receipt: 'queued' } }); await pending;
  assert.equal(h.app.state.stickers.scope, '1:group:20'); assert.equal(h.app.state.stickers.rows[0].id, 20); assert.doesNotMatch(h.ids.get('sticker-message').textContent, /已提交/u);
  const image = h.document.querySelectorAll('.sticker-image')[0]; h.app.openStickerPreview(h.app.state.stickers.rows[0]); const lateLoad = h.ids.get('sticker-preview-image').onload;
  h.app.clearSession(); lateLoad(); assert.equal(image.src, ''); assert.equal(h.ids.get('sticker-preview-image').src, ''); assert.equal(h.ids.get('sticker-preview-message').textContent, '');
});

test('sticker pagination returns to the last valid page after deletions', async () => {
  const h = harness(); await gallery(h);
  h.app.state.stickers.page = 2; h.app.state.stickers.totalPages = 2;
  h.setHandler(async (path) => {
    const page = Number(new URL(path, 'https://local.test').searchParams.get('page'));
    return { body: stickerPage('1:group:10', page === 2 ? [] : [stickerItem(4)], page, 1) };
  });
  await h.app.loadStickerItems(2); assert.equal(h.app.state.stickers.page, 1); assert.equal(h.app.state.stickers.rows[0].id, 4);
  assert.equal(h.ids.get('stickers-prev').disabled, true); assert.equal(h.app.state.stickers.loading, false);
});

test('sticker group requests and preview failure cannot repopulate a left or signed-out page', async () => {
  const h = harness(); await gallery(h); let resolve;
  h.app.openStickerPreview(h.app.state.stickers.rows[0]); h.ids.get('sticker-preview-image').onerror();
  assert.equal(h.ids.get('sticker-preview-image').src, ''); assert.match(h.ids.get('sticker-preview-message').textContent, /无法读取/u);
  h.setHandler(() => new Promise((done) => { resolve = done; })); const pending = h.app.loadStickerGroups(); await Promise.resolve();
  h.app.showView('overview'); resolve({ body: stickerGroups([stickerGroup(99)]) }); await pending;
  assert.equal(h.app.state.stickers.groups[0].group_id, 10); assert.equal(h.document.querySelectorAll('.sticker-image').length, 0);
  h.app.state.view = 'stickers'; const loggedOut = h.app.loadStickerGroups(); await Promise.resolve(); h.app.clearSession();
  resolve({ body: stickerGroups([stickerGroup(99)]) }); await loggedOut;
  assert.equal(h.app.state.stickers.groups.length, 0); assert.equal(h.ids.get('sticker-group').value, '');
});

test('selected sticker group survives group-list pagination and refresh, including an emptied library', async () => {
  const h = harness(); await gallery(h); h.app.state.stickers.totalGroupPages = 2;
  const originalImage = h.document.querySelectorAll('.sticker-image')[0];
  h.setHandler(async () => ({ body: stickerGroups([stickerGroup(20)], 2, 2) })); await h.app.changeStickerGroupPage(2);
  assert.equal(h.app.state.stickers.scope, '1:group:10'); assert.equal(h.ids.get('sticker-group').value, '1:group:10'); assert.equal(h.app.state.stickers.rows[0].id, 1);
  assert.equal(h.document.querySelectorAll('.sticker-image')[0], originalImage);
  h.setHandler(async (path) => ({ body: path.startsWith('/api/stickers/groups') ? stickerGroups([stickerGroup(20)], 2, 2) : stickerPage('1:group:10', []) }));
  await h.app.refreshStickers(); assert.equal(h.app.state.stickers.scope, '1:group:10'); assert.equal(h.ids.get('sticker-group').value, '1:group:10');
  assert.match(h.ids.get('sticker-message').textContent, /本群暂无图片.*\/save/u); assert.equal(h.ids.get('reindex-sticker-group').disabled, true);
});

test('confirmed sticker mutation distinguishes a later listing refresh failure', async () => {
  const h = harness(); await gallery(h);
  h.setHandler(async (path, options) => {
    if (path === '/api/stickers/reindex') { const payload = JSON.parse(options.body); return { body: { request_id: payload.request_id, scope: payload.scope, action: 'reindex', affected: 1, receipt: 'queued' } }; }
    return { status: 503, body: { error: '列表暂不可用' } };
  });
  await h.app.mutateSticker('reindex', h.app.state.stickers.rows[0]);
  assert.equal(h.app.state.stickers.operation, null); assert.match(h.ids.get('sticker-message').textContent, /已提交.*尚未完成.*列表刷新未完成/u);
});

function contextSnapshot(h, overrides = {}) {
  return {
    bot_id: "1", enabled: true, config: structuredClone(h.sample.plugins.ai.context),
    counts: { turns: 17, memories: 8, notes: 4 },
    recent: [{ id: 1, user_id: "42", conversation_id: "1:group:10:v0:group", scene: "group", peer_id: "10", completed_at: 1792800000, request_key: "msg:100", reply_key: "msg:101", user_text: "<img src=x onerror=alert(1)>第二个方案呢？", assistant_text: "第二个方案是本地部署。" }],
    ...overrides,
  };
}

test("topic summaries show unresolved items and literal sources, and compression target stays below trigger", async () => {
  const h = harness(); await h.login();
  h.app.state.view = "memory"; h.app.state.memory.selected = h.conversation();
  const detail = { ...h.conversation(), summary: JSON.stringify({ topics: [{
    title: "<img src=x onerror=alert(1)>", participants: [42], summary: "讨论搬家",
    decisions: ["周五确认"], open_questions: ["费用待确认"], key_details: ["预算8192"], sources: ["1:group:10:100"],
  }] }) };
  h.setHandler(async () => ({ body: detail })); await h.app.loadContext();
  assert.match(h.ids.get("context-content").textContent, /参与者：42.*未完成事项：\n费用待确认.*关键原文：\n预算8192.*来源：\n1:group:10:100/su);
  assert.ok(h.ids.get("context-content").textContent.includes("<img src=x onerror=alert(1)>"));
  assert.equal(h.document.querySelectorAll("#context-content img").length, 0);
  const target = h.app.inputs.get("plugins.ai.compaction.target_percent").control;
  target.value = h.app.inputs.get("plugins.ai.compaction.trigger_percent").control.value;
  assert.throws(() => h.app.readForm(), /目标占用必须低于触发/u);
  target.value = "20";
  assert.equal(h.app.readForm().plugins.ai.compaction.target_percent, 20);
});

test("continuity window defaults to ten before and five after and saves typed bounded settings", async () => {
  const h = harness(); await h.login();
  const group = h.app.groups.find((entry) => entry.title === "跨会话接续"); assert.equal(group.fields.length, 10);
  assert.equal(h.app.inputs.get("plugins.ai.context.neighbors_before").control.value, "10");
  assert.equal(h.app.inputs.get("plugins.ai.context.neighbors_after").control.value, "5");
  h.app.openConfigSection("context");
  for (const [name, value] of Object.entries({ neighbors_before: "15", neighbors_after: "7", neighbor_seconds: "180", recent_seconds: "604800", max_turns: "32", neighbor_budget: "0" })) {
    const control = h.app.inputs.get("plugins.ai.context." + name).control; control.value = value; await control.emit("input");
  }
  assert.equal(h.calls.some((call) => call.path === "/api/config" && call.options.method === "PUT"), false);
  let saved;
  h.setHandler(async (path, options) => {
    if (path === "/api/config" && options.method === "PUT") { saved = JSON.parse(options.body); return { body: {} }; }
    if (path === "/api/config") return { body: { ...h.snapshot(), config: saved.config, revision: "r2" } };
    return { body: { phase: "running", applied_revision: "r2", loaded_plugins: [] } };
  });
  await h.app.saveConfig(true);
  assert.deepEqual(saved.config.plugins.ai.context, { enabled: true, include_unanswered: true, cross_scene: "private_followup", recent_seconds: 604800, max_turns: 32, neighbors_before: 15, neighbors_after: 7, neighbor_seconds: 180, recall_budget: 3000, neighbor_budget: 0 });
  assert.equal(h.calls.filter((call) => call.path === "/api/runtime/apply").length, 1);
});

test("continuity validates integer ranges and background budget containment in form and JSON", async () => {
  const h = harness(); await h.login();
  for (const [name, value] of [["neighbors_before", "51"], ["neighbors_after", "-1"], ["recent_seconds", "604801"], ["max_turns", "3.5"], ["neighbor_budget", "3001"]]) {
    const control = h.app.inputs.get("plugins.ai.context." + name).control, previous = control.value; control.value = value;
    assert.throws(() => h.app.readForm(), /范围|不能超过/u); control.value = previous;
  }
  const config = h.app.readForm(); config.plugins.ai.context.neighbor_budget = 3001;
  h.ids.get("config-json").value = JSON.stringify(config); h.app.state.jsonDirty = true;
  assert.throws(() => h.app.adoptJson(), /不能超过/u);
  assert.equal(h.app.state.jsonDirty, true);
});

test("knowledge navigation unifies memory and notes while config filters preserve hidden drafts", async () => {
  const h = harness(); await h.login();
  h.app.state.memory.list = [h.conversation()]; h.app.showView("memory");
  assert.equal(h.ids.get("knowledge-header").hidden, false);
  const nav = h.document.querySelectorAll(".nav-item").find((node) => node.dataset.view === "context");
  assert.equal(nav.attrs["aria-current"], "page");
  const memoryTab = h.document.querySelectorAll(".workspace-tab").find((node) => node.dataset.view === "memory");
  assert.equal(memoryTab.attrs["aria-current"], "page");
  const model = h.app.inputs.get("llm.model").control; model.value = "draft-model"; await model.emit("input");
  h.app.openConfigSection("context");
  assert.equal(h.app.state.view, "config"); assert.equal(h.ids.get("knowledge-header").hidden, true);
  for (const card of h.document.querySelectorAll("[data-config-group]")) assert.equal(card.hidden, card.dataset.configGroup !== "context");
  assert.equal(h.app.readForm().llm.model, "draft-model");
  h.app.filterConfig("all"); assert.equal(h.document.querySelectorAll("[data-config-group]").some((card) => card.hidden), false);
  assert.equal(model.value, "draft-model");
});

test("continuity overview is lazy and previews literal account-bound sources with applied settings", async () => {
  const h = harness(); await h.login();
  assert.equal(h.calls.some((call) => call.path === "/api/context/overview"), false);
  const snapshot = contextSnapshot(h); snapshot.config.neighbors_before = 12;
  h.setHandler(async () => ({ body: snapshot })); h.app.state.view = "context"; await h.app.loadContextOverview();
  assert.match(h.ids.get("context-neighbors").textContent, /前 12 \/ 后 5/u);
  assert.equal(h.app.inputs.get("plugins.ai.context.neighbors_before").control.value, "10", "applied snapshot never overwrites the saved config draft");
  assert.match(h.ids.get("context-counts").textContent, /17.*预览问答.*8.*个人记忆.*4.*经验笔记/su);
  assert.match(h.ids.get("context-recent-list").textContent, /群 10.*用户 42.*机器人 1.*msg:100.*msg:101/su);
  assert.match(h.ids.get("context-recent-list").textContent, /<img src=x onerror=alert\(1\)>/u);
  assert.equal(h.document.querySelectorAll(".context-quote img").length, 0);
  assert.equal(h.ids.get("context-input-budget").textContent, "6,000");
  h.app.state.config.plugins.ai.learning.enabled = true; h.app.renderContextOverview();
  assert.equal(h.ids.get("context-input-budget").textContent, "18,000");
  assert.match(h.ids.get("context-budget-note").textContent, /普通输入加上.*笔记额度.*接续占普通输入/u);
});

test("continuity empty, malformed and failed reads clear sources without a fake success state", async () => {
  const h = harness(); await h.login(); h.app.state.view = "context";
  h.setHandler(async () => ({ body: contextSnapshot(h, { bot_id: null, recent: [], counts: { turns: 0, memories: 0, notes: 0 }, message: "等待机器人连接。" }) }));
  await h.app.loadContextOverview(); assert.match(h.ids.get("context-overview-message").textContent, /等待机器人/u);
  assert.match(h.ids.get("context-recent-list").textContent, /连接机器人/u);
  const bad = contextSnapshot(h); bad.recent[0].user_id = 42;
  h.setHandler(async () => ({ body: bad })); await h.app.loadContextOverview();
  assert.equal(h.app.state.context.snapshot, null); assert.equal(h.ids.get("context-recent-list").textContent, "");
  assert.match(h.ids.get("context-overview-message").textContent, /来源格式无效/u);
  h.setHandler(async () => ({ status: 503, body: { error: { code: "unavailable", message: "正在重连" } } }));
  await h.app.loadContextOverview(); assert.equal(h.app.state.context.snapshot, null);
  assert.equal(h.ids.get("refresh-context-overview").disabled, false);
});

test("unanswered episodes and retry receipts render with separate IDs and literal source text", async () => {
  const h = harness(); await h.login(); h.app.state.view = "context";
  const snapshot = contextSnapshot(h);
  snapshot.counts.episodes = 1;
  snapshot.recent.push({ ...snapshot.recent[0], kind: "episode", reply_key: null, assistant_text: null, user_text: "<script>原始话题</script>" });
  snapshot.learning_batches = [{ conversation_id: "1:group:10:v0:group", epoch: 1, start_position: 0, through_position: 10, attempts: 3, status: "needs_review", summary_reason: "", rejected_count: 1, updated_at: 1792800000, reasons: ["evidence_not_exact_or_ineligible"] }];
  h.setHandler(async () => ({ body: snapshot })); await h.app.loadContextOverview();
  assert.notEqual(h.app.state.context.snapshot, null);
  assert.match(h.ids.get("context-recent-list").textContent, /话题片段 #1.*<script>原始话题<\/script>/su);
  assert.match(h.ids.get("context-learning-state").textContent, /待核对.*已尝试 3 次.*evidence_not_exact_or_ineligible/su);
  assert.equal(h.document.querySelectorAll(".context-quote script").length, 0);
  snapshot.learning_batches[0].conversation_id = "99:group:10:v0:group";
  await h.app.loadContextOverview();
  assert.equal(h.app.state.context.snapshot, null);
  assert.equal(h.ids.get("context-learning-state").textContent, "");
});

test("continuity rejects late reads after navigation, logout and account changes", async () => {
  const h = harness(); await h.login(); h.app.state.view = "context";
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
  const pending = h.app.loadContextOverview(); await Promise.resolve();
  h.app.showView("overview"); resolve({ body: contextSnapshot(h) }); await pending;
  assert.equal(h.app.state.context.snapshot, null); assert.equal(h.ids.get("context-recent-list").textContent, "");
  h.app.state.view = "context"; const logoutPending = h.app.loadContextOverview(); await Promise.resolve();
  h.app.clearSession(); resolve({ body: contextSnapshot(h) }); await logoutPending;
  assert.equal(h.app.state.context.snapshot, null);
  await h.login(); h.app.state.view = "context"; h.setHandler(async () => ({ body: contextSnapshot(h) })); await h.app.loadContextOverview();
  h.app.state.status = { phase: "running", online: true, bot_id: 99 }; h.app.renderStatus();
  assert.equal(h.app.state.context.snapshot, null); assert.match(h.ids.get("context-overview-message").textContent, /账号已变化/u);
});

test("continuity cannot display another bot's source conversation or an account replaced during loading", async () => {
  const h = harness(); await h.login(); h.app.state.view = "context";
  const wrong = contextSnapshot(h); wrong.recent[0].conversation_id = "99:group:10:v0:group";
  h.setHandler(async () => ({ body: wrong })); await h.app.loadContextOverview();
  assert.equal(h.app.state.context.snapshot, null); assert.match(h.ids.get("context-overview-message").textContent, /来源会话.*不一致/u);
  let resolve; h.setHandler(() => new Promise((done) => { resolve = done; }));
  const pending = h.app.loadContextOverview(); await Promise.resolve();
  h.app.state.status = { phase: "running", online: true, bot_id: 99 }; h.app.renderStatus();
  resolve({ body: contextSnapshot(h) }); await pending;
  assert.equal(h.app.state.context.snapshot, null); assert.equal(h.ids.get("context-recent-list").textContent, "");
  assert.match(h.ids.get("context-overview-message").textContent, /账号已变化/u);
});

function personalSnapshot(user = "7") {
  return { bot_id: "1", character_id: "default", enabled: true, page: 1, total_pages: 1, people: ["7", "8"], profile: user ? { user_id: user, revision: 3, events: [{ id: 1, version: 2, kind: "address", value: "<img src=x onerror=alert(1)>", scope: "1:friend:7", source_key: "1:friend:7:1", quote: "请叫我阿云", occurred_at: 10, expires_at: null, corrected: true }], memories: [{ id: 2, kind: "preference", text: "喜欢白茶", scope: "1:group:2", active: true }] } : null };
}
test("personal profiles render literal account-bound preferences and send independent revisions", async () => {
  const h = harness(); await h.login(); h.app.state.view = "personal";
  h.setHandler(async (path) => ({ body: path.startsWith("/api/personal?") ? personalSnapshot() : { status: "applied", bot_id: "1", user_id: "7", character_id: "default", revision: 4 } }));
  await h.app.loadPersonal("7");
  assert.match(h.ids.get("personal-preferences").textContent, /<img src=x onerror=alert\(1\)>/u);
  assert.equal(h.document.querySelectorAll("#personal-preferences img").length, 0);
  assert.match(h.ids.get("personal-memories").textContent, /喜欢白茶/u);
  await h.app.mutatePersonal({ op: "correct", id: 1, version: 2, value: "云同学" });
  const request = h.calls.find((c) => c.path === "/api/personal/mutate");
  assert.equal(request.body.bot_id, "1"); assert.equal(request.body.user_id, "7"); assert.equal(request.body.character_id, "default"); assert.equal(request.body.revision, 3);
  assert.equal(request.body.action.value, "云同学"); assert.ok(request.body.request_id);
});
test("personal profiles reject foreign sources, subjects and replaced accounts", async () => {
  for (const change of [(v) => { v.profile.events[0].scope = "2:friend:7"; }, (v) => { v.profile.user_id = "8"; }, (v) => { v.bot_id = "2"; }]) {
    const h = harness(); await h.login(); h.app.state.view = "personal"; h.app.state.status = { online: true, bot_id: 1 };
    const value = personalSnapshot(); change(value); h.setHandler(async () => ({ body: value })); await h.app.loadPersonal("7");
    assert.equal(h.app.state.personal.snapshot, null); assert.equal(h.ids.get("personal-profile").hidden, true); assert.ok(h.app.state.personal.message);
  }
});
test("late personal reads cannot restore a closed or signed-out profile", async () => {
  const h = harness(); await h.login(); h.app.state.view = "personal";
  let release; h.setHandler(() => new Promise((resolve) => { release = resolve; }));
  const loading = h.app.loadPersonal("7"); await Promise.resolve(); h.app.clearSession(); release({ body: personalSnapshot() }); await loading;
  assert.equal(h.app.state.personal.snapshot, null); assert.equal(h.ids.get("personal-profile").hidden, true);
});
test("personal reset conflicts keep the server revision and show a readback instruction", async () => {
  const h = harness(); await h.login(); h.app.state.view = "personal";
  h.setHandler(async (path) => path.includes("mutate") ? { status: 409, body: { error: "版本变化", code: "personal_conflict" } } : { body: personalSnapshot() });
  await h.app.loadPersonal("7"); await h.app.mutatePersonal({ op: "reset" });
  assert.equal(h.app.state.personal.snapshot.profile.revision, 3); assert.match(h.app.state.personal.message, /刷新核对/u); assert.equal(h.app.state.personal.saving, false);
});

function knowledgeSnapshot() {
  return { bot_id: "1", enabled: true, page: 1, total_pages: 1, counts: [{ kind: "joke", status: "pending", count: 1 }],
    outcomes: { periods: [{ period: "24h", rows: [{ automatic: true, replies: 4, engaged: 1, corrected: 2, positive: 0, quoted: 1, avg_first_response_secs: 12 }] }] },
    items: [{ item: { id: 5, bot_id: 1, kind: "joke", subject: "scope", subject_id: 0, scope: "1:group:50", character_id: "", domain: "", text: "<img src=x onerror=alert(1)>泡泡梗", origin: "auto", confidence: 0.8, importance: 3, status: "pending", locked: false, version: 2, created_at: 1, updated_at: 1, expires_at: null },
      sources: [{ source_key: "1:group:50:3", role: "evidence", quote: "泡泡梗笑死", source_hash: "", memory_version: null }], stats: null, usable: false }] };
}
test("knowledge review renders literal items with outcomes and approves the exact version", async () => {
  const h = harness(); await h.login(); h.app.state.view = "knowledge";
  h.setHandler(async (path) => ({ body: path.startsWith("/api/knowledge?") ? knowledgeSnapshot() : { status: "applied", id: 5, version: 3 } }));
  await h.app.loadKnowledge();
  assert.match(h.ids.get("knowledge-items").textContent, /<img src=x onerror=alert\(1\)>泡泡梗/u);
  assert.equal(h.document.querySelectorAll("#knowledge-items img").length, 0);
  assert.match(h.ids.get("knowledge-items").textContent, /本群的梗 · 待审核/u);
  assert.match(h.ids.get("knowledge-outcomes").textContent, /主动接话 4 次，有人回应 25%，被否定 2 次/u);
  await h.app.mutateKnowledge(h.app.state.knowledge.snapshot.items[0].item, { op: "approve" });
  const request = h.calls.find((c) => c.path === "/api/knowledge/mutate");
  assert.equal(request.body.bot_id, "1"); assert.equal(request.body.id, 5); assert.equal(request.body.version, 2);
  assert.deepEqual(request.body.action, { op: "approve" }); assert.ok(request.body.request_id);
});
test("knowledge review rejects malformed items and replaced accounts", async () => {
  for (const change of [(v) => { v.items[0].item.kind = "secret"; }, (v) => { v.bot_id = "2"; }, (v) => { v.items[0].item.status = "deleted"; }]) {
    const h = harness(); await h.login(); h.app.state.view = "knowledge"; h.app.state.status = { online: true, bot_id: 1 };
    const value = knowledgeSnapshot(); change(value); h.setHandler(async () => ({ body: value })); await h.app.loadKnowledge();
    assert.equal(h.app.state.knowledge.snapshot, null); assert.ok(h.app.state.knowledge.message);
  }
});
test("knowledge settings validate switches and bounded numbers", async () => {
  const h = harness(); await h.login();
  const group = h.app.groups.find((entry) => entry.title === "相处学习与共同经历");
  assert.ok(group); assert.equal(group.fields.length, 12);
  const budget = h.app.inputs.get("plugins.ai.knowledge.injection_budget").control;
  budget.value = "8001"; assert.throws(() => h.app.readForm(), /整数/u);
  budget.value = "2000"; assert.equal(h.app.readForm().plugins.ai.knowledge.injection_budget, 2000);
});
