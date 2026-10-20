(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    authenticated: false, csrf: "", setupRequired: false, session: 0, config: null, draft: null, revision: null,
    secrets: {}, status: null, qq: null, busy: false, dirty: false,
    jsonDirty: false, formInvalid: false, stale: false,
    runtimeTimer: null, qqTimer: null, runtimePolling: false, qqPolling: false,
    qqPollingPaused: false, qqQueryVersion: 0, qqFailures: 0, qqInterval: 5000, qqError: "", qrExpires: null, view: "overview",
    memory: { page: 1, totalPages: 1, list: [], selected: null, version: 0, detail: null, pageMemories: 1, totalMemoryPages: 1, rows: [], checked: new Set(), settingsDirty: false, editorDirty: false, editing: null, preview: null, conflict: false },
    pendingOperation: null,
    contacts: { version: 0, loading: false, saving: null, phase: "idle", message: "", botId: "", generation: null, groups: [], friends: [], fetchedAt: null, scene: "group", query: "", page: 1, totalPages: 1 },
  };
  const inputs = new Map();
  const secretInputs = new Map();
  const clone = (value) => JSON.parse(JSON.stringify(value));
  const get = (object, path) => path.split(".").reduce((value, key) => value?.[key], object);
  const put = (object, path, value) => {
    const keys = path.split(".");
    const last = keys.pop();
    let target = object;
    for (const key of keys) {
      if (!target[key] || typeof target[key] !== "object" || Array.isArray(target[key])) {
        throw new Error(`配置分组 ${path} 的结构不正确。`);
      }
      target = target[key];
    }
    target[last] = value;
  };
  const canonical = (value) => JSON.stringify(value, function (_key, entry) {
    if (entry && typeof entry === "object" && !Array.isArray(entry)) {
      return Object.fromEntries(Object.keys(entry).sort().map((key) => [key, entry[key]]));
    }
    return entry;
  });

  // Definitions build ordinary DOM elements; configuration and server data never become HTML.
  const field = (path, label, type = "text", hint = "", options = {}) => ({ path, label, type, hint, ...options });
  const groups = [
    { title: "模型接口", description: "兼容 Chat Completions 的远程 API；管理台不会自动发送试聊请求。", fields: [
      field("llm.chat_completions_url", "完整 API 地址", "text", "填写完整路径，例如 https://服务商/v1/chat/completions。", { full: true, placeholder: "https://…/v1/chat/completions" }),
      field("llm.model", "模型名称", "text", "使用服务商提供的准确模型 ID。"),
      { secret: "llm_api_key", label: "模型 API 密钥" },
      field("llm.max_concurrent_requests", "最大并发请求", "number", "前台聊天和后台整理共用请求额度。", { min: 1 }),
      field("llm.request_timeout_secs", "单次请求超时（秒）", "number", "", { min: 1 }),
      field("llm.tool_calling", "启用原生工具调用", "check", "模型需支持工具调用；工具仍由宿主检查权限和执行结果。"),
    ] },
    { title: "Milky 消息接口", description: "负责收发 QQ 消息。LLBot 常用端口 3010，请以实际配置为准。", fields: [
      field("milky.url", "Milky 地址", "text", "例如 http://127.0.0.1:3010，不填写 /api 后缀。", { placeholder: "http://127.0.0.1:3010" }),
      { secret: "milky_access_token", label: "Milky 访问令牌" },
      field("milky.bot_id", "预期机器人 QQ 号", "nullableNumber", "留空则使用登录接口返回的账号；填写后会校验是否一致。", { min: 1 }),
      field("milky.request_timeout_secs", "接口超时（秒）", "number", "", { min: 1 }),
    ] },
    { title: "QQ 登录管理", description: "由小鱼服务端连接 LLBot WebUI；它与 Milky 地址不同。", fields: [
      field("qq_login.enabled", "启用 QQ 登录适配", "check", "允许在管理台获取二维码与查询账号状态。", { full: true }),
      field("qq_login.url", "LLBot WebUI 地址", "text", "通常为 http://127.0.0.1:3080。", { placeholder: "http://127.0.0.1:3080" }),
      { secret: "llbot_token", label: "LLBot WebUI 密码 / 令牌" },
    ] },
    { title: "会话与触发", description: "真实 @机器人、私聊或关键词可触发聊天。每个会话的 Bot 开关在「群与好友」中管理。", fields: [
      field("plugins.ai.keywords", "关键词触发", "lines", "每行一个；子串匹配。清空后仍可通过真实 @或私聊触发。"),
      field("plugins.ai.defaults.mode", "新群默认会话模式", "select", "已有会话在「记忆与上下文」中单独管理。", { choices: [["group", "群共享上下文"], ["user", "群内按用户隔离"]] }),
      field("plugins.ai.defaults.persona", "默认人设名称", "text", "必须与下方人设表中的名称对应。"),
      field("plugins.ai.personas", "人设表", "json", "JSON 对象：名称对应人设文本。所有名称和文本均可编辑。", { full: true }),
    ] },
    { title: "上下文与记忆", description: "摘要、长期记忆读取和写入分别控制；输入预算采用保守字节估算。", fields: [
      field("plugins.ai.defaults.input_budget", "输入预算（估算）", "number", "", { min: 1024, max: 262144 }),
      field("plugins.ai.defaults.output_budget", "回复 token 上限", "number", "同时受模型及消息长度限制。", { min: 64, max: 32768 }),
      field("plugins.ai.defaults.recent_messages", "最近历史消息数", "number", "设为 0 时不载入最近原文。", { min: 0, max: 4096 }),
      field("plugins.ai.defaults.allowed_tools", "允许模型使用的工具", "lines", "每行一个已注册工具名，例如 clock_now。"),
      field("plugins.ai.defaults.summary", "生成并使用历史摘要", "check", "关闭后不生成或注入摘要，按历史条数读取原文。"),
      field("plugins.ai.defaults.memory_read", "检索长期记忆", "check", "将相关的、属于当前会话和用户的记忆加入回答。"),
      field("plugins.ai.defaults.memory_write", "自动提炼长期记忆", "check", "只接受带有效原文证据的事实变更。"),
      field("plugins.ai.read_group_background", "读取群内背景消息", "check", "未触发的群消息也可进入上下文，与长期记忆学习独立。"),
      field("plugins.ai.learn_all_group_messages", "学习群内背景消息", "check", "未触发的群消息也可用于长期记忆；仍受记忆写入开关约束。"),
      field("plugins.ai.defaults.paused", "默认暂停自动聊天", "check", "可在「记忆与上下文」恢复或只放行一次回复。"),
    ] },
    { title: "插件", description: "启用需要的功能。写入类工具默认要求用户确认。", fields: [
      field("plugins.ai.enabled", "AI 聊天", "check", "启用后需配置有效的模型地址和名称。"),
      field("plugins.clock.enabled", "时间查询", "check", "提供 /clock 指令和 clock_now 工具。"),
      field("plugins.reminder.enabled", "提醒", "check", "提供提醒创建、查询和取消功能。"),
      field("plugins.reminder.llm_writes_require_confirmation", "模型创建提醒需要确认", "check", "写入操作先展示预览，再由用户明确确认。"),
      field("plugins.reminder.max_active_per_user", "每人最多活跃提醒", "number", "", { min: 1 }),
    ] },
    { title: "容量与整理", description: "并发、队列和后台整理均有边界。可先保留默认值，再按使用情况调整。", fields: [
      field("limits.model_user_per_hour", "每位用户每小时模型请求上限", "number", "", { min: 1, max: 100000 }),
      field("limits.model_conversation_per_hour", "每个会话每小时模型请求上限", "number", "", { min: 1, max: 100000 }),
      field("limits.model_global_per_hour", "全局每小时模型请求上限", "number", "", { min: 1, max: 100000 }),
      field("limits.memory_requests_per_hour", "每小时后台记忆整理请求上限", "number", "", { min: 1, max: 100000 }),
      field("limits.queued_global", "全局等待队列", "number", "", { min: 1, max: 1024 }),
      field("limits.queued_per_conversation", "单会话等待队列", "number", "不能超过全局等待队列。", { min: 1 }),
      field("limits.turn_timeout_secs", "完整聊天轮次超时（秒）", "number", "", { min: 1 }),
      field("limits.max_model_calls", "每轮最多模型调用", "number", "", { min: 1 }),
      field("limits.max_tool_calls", "每轮最多工具调用", "number", "", { min: 0 }),
      field("limits.max_http_attempts", "每轮 HTTP 尝试上限", "number", "不会自动重试结果未知的请求。", { min: 1 }),
      field("limits.send_cooldown_secs", "AI 发言间隔（秒）", "number", "", { min: 0 }),
      field("limits.sends_per_hour", "每个会话每小时发送上限", "number", "", { min: 1 }),
      field("limits.retention_days", "原始消息保留天数", "number", "长期记忆独立保存；原文过期后无法重新生成对应摘要。", { min: 1 }),
      field("plugins.ai.compact_after_messages", "触发整理的消息数", "number", "后台只整理安全的完整前缀。", { min: 1 }),
      field("plugins.ai.compact_idle_secs", "闲置整理等待（秒）", "number", "", { min: 1 }),
      field("plugins.ai.memory_input_budget", "整理输入预算（估算）", "number", "", { min: 3072, max: 262144 }),
      field("plugins.ai.memory_output_budget", "整理输出 token 上限", "number", "", { min: 256, max: 8192 }),
      field("plugins.ai.max_memories_per_conversation", "每个会话最多长期记忆", "number", "", { min: 1 }),
      field("plugins.ai.memory_recall_limit", "每轮最多召回记忆", "number", "", { min: 1 }),
      field("plugins.ai.memory_recall_budget", "召回记忆预算（估算）", "number", "", { min: 64 }),
    ] },
    { title: "程序设置", description: "剩余低频参数可在完整 JSON 中编辑。监听地址、数据库路径和日志级别需要重启整个程序。", fields: [
      field("app.database", "SQLite 数据库路径", "text", "修改后需重启整个程序，管理服务与机器人会统一使用新数据库。"),
      field("app.timezone", "时区", "text", "例如 Asia/Shanghai。"),
      field("app.log", "日志过滤级别", "text", "例如 info 或 qkbot=debug。修改后需要重启整个小鱼程序；应用机器人配置不会重载日志过滤器。"),
    ] },
  ];

  class ApiError extends Error {
    constructor(message, status = 0, code = "", retryAfter = 0, path = "") { super(message); this.status = status; this.code = code; this.retryAfter = retryAfter; this.path = path; }
  }

  async function api(path, method = "GET", body) {
    const session = state.session;
    const headers = { Accept: "application/json" };
    if (method !== "GET" && state.csrf) headers["X-CSRF-Token"] = state.csrf;
    if (body !== undefined) headers["Content-Type"] = "application/json";
    let response;
    try {
      response = await fetch(path, {
        method, headers, body: body === undefined ? undefined : JSON.stringify(body),
        credentials: "same-origin", cache: "no-store", redirect: "error",
        signal: AbortSignal.timeout(20000),
      });
    } catch (error) {
      throw new ApiError(error.name === "TimeoutError" ? "请求超时，请检查管理服务是否仍在运行。" : "无法连接管理服务，请检查服务状态和网络。", 0, "network", 0, path);
    }
    if (session !== state.session) throw new ApiError("页面登录状态已变化。", -1);
    let text;
    try { text = await response.text(); } catch { throw new ApiError("响应传输中断，请先核对操作结果。", 0, "network", 0, path); }
    if (session !== state.session) throw new ApiError("页面登录状态已变化。", -1);
    let result = {};
    try { if (text) result = JSON.parse(text); } catch {
      throw new ApiError(`服务返回了无法读取的数据（HTTP ${response.status}）。`, response.status, "invalid_response", 0, path);
    }
    if (!response.ok) {
      const error = new ApiError(typeof result.error === "string" ? result.error : `请求失败（HTTP ${response.status}）。`, response.status, result.code || "", Number(result.retry_after_ms) || 0, path);
      if (result.code === "auth_required" || (response.status === 401 && !path.startsWith("/api/auth/"))) clearSession("登录会话已失效，请重新登录。");
      throw error;
    }
    return result;
  }

  function notice(message, kind = "success") {
    $("notice").className = `notice ${kind}`;
    $("notice-text").textContent = message;
    $("notice").hidden = false;
  }

  function report(error) {
    if (error.status === -1 || error.code === "auth_required") return;
    if (error.code === "reauth_required") { $("reauth-error").textContent = ""; $("reauth-dialog").showModal(); return; }
    if (error.code === "csrf_invalid") {
      notice("页面安全凭据已变化，正在重新读取。当前编辑保留，请稍后手动重试操作。", "warning");
      void api("/api/auth/state").then((view) => { if (view.authenticated) authView(view); else clearSession("登录会话已失效，请重新登录。"); }).catch((failure) => notice(failure.message, "error"));
      return;
    }
    if (error.status === 409 && error.path === "/api/config") {
      state.stale = true;
      $("config-warning").textContent = "配置已被其他页面或本机修改。本页编辑仍保留；请先复制需要保留的内容，再点击「重新载入」取得最新版本。";
      $("config-warning").hidden = false;
      updateControls();
    }
    notice(error.message || "操作未完成。", "error");
  }

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function secretSnapshot() {
    return Object.fromEntries([...secretInputs].map(([name, controls]) => [name, { action: controls.action.value, value: controls.value.value }]));
  }

  function describeSecret(name) {
    const meta = state.secrets[name];
    if (!meta) return "未读取到密钥状态；保持原值不会修改现有配置。";
    if (meta.env_set) return "当前由环境变量提供。文件中的修改不会覆盖环境变量。";
    if (meta.file_set) return "配置文件已设置；密钥不会回传到页面。";
    if (meta.effective_set) return `当前已配置（来源：${String(meta.source || "服务端")}）。`;
    return "当前未设置。";
  }

  function secretField(definition, pending) {
    const wrapper = element("div", "field");
    const id = `secret-${definition.secret}`;
    const label = element("label", "", definition.label);
    label.htmlFor = `${id}-action`;
    wrapper.append(label);
    const row = element("div", "secret-row");
    const action = document.createElement("select");
    action.id = `${id}-action`;
    for (const [value, title] of [["keep", "保持原值"], ["set", "设置新值"], ["clear", "清除文件值"]]) {
      const option = element("option", "", title); option.value = value; action.append(option);
    }
    const input = document.createElement("input");
    input.id = id; input.type = "password"; input.autocomplete = "off";
    input.spellcheck = false; input.setAttribute("aria-label", `${definition.label}新值`);
    input.placeholder = "选择设置新值后输入";
    action.value = pending?.action || "keep"; input.value = pending?.value || "";
    input.disabled = action.value !== "set";
    row.append(action, input);
    wrapper.append(row, element("p", "hint secret-state", describeSecret(definition.secret)));
    secretInputs.set(definition.secret, { action, value: input, label: definition.label });
    action.addEventListener("change", () => {
      input.disabled = action.value !== "set";
      if (action.value !== "set") input.value = "";
      markDirty();
    });
    input.addEventListener("input", markDirty);
    return wrapper;
  }

  function configField(definition) {
    const wrapper = element("div", `field${definition.full ? " full" : ""}${definition.type === "check" ? " check-field" : ""}`);
    const id = `field-${definition.path.replaceAll(".", "-")}`;
    const label = element("label", "", definition.label); label.htmlFor = id;
    let control;
    if (["lines", "ids", "json"].includes(definition.type)) {
      control = document.createElement("textarea");
      control.rows = definition.type === "json" ? 5 : 3;
      control.spellcheck = false;
    } else if (definition.type === "select") {
      control = document.createElement("select");
      for (const [value, title] of definition.choices) {
        const option = element("option", "", title); option.value = value; control.append(option);
      }
    } else {
      control = document.createElement("input");
      control.type = definition.type === "check" ? "checkbox" : ["number", "nullableNumber"].includes(definition.type) ? "number" : "text";
      if (control.type === "number") { control.step = "1"; control.inputMode = "numeric"; }
      if (definition.min !== undefined) control.min = String(definition.min);
      if (definition.max !== undefined) control.max = String(definition.max);
      if (definition.type === "number") control.required = true;
    }
    control.id = id; control.autocomplete = "off";
    if (definition.placeholder) control.placeholder = definition.placeholder;
    const value = get(state.draft, definition.path);
    if (definition.type === "check") control.checked = Boolean(value);
    else if (definition.type === "json") control.value = JSON.stringify(value ?? {}, null, 2);
    else if (["lines", "ids"].includes(definition.type)) control.value = Array.isArray(value) ? value.join("\n") : "";
    else control.value = value == null ? "" : String(value);
    const hint = element("p", "hint", definition.hint);
    hint.id = `${id}-hint`; control.setAttribute("aria-describedby", hint.id);
    if (definition.type === "check") { const copy = element("div"); copy.append(label, hint); wrapper.append(control, copy); }
    else { wrapper.append(label, control); if (definition.hint) wrapper.append(hint); }
    inputs.set(definition.path, { control, definition });
    control.addEventListener("input", markDirty);
    return wrapper;
  }

  function renderFields(pending = {}) {
    inputs.clear(); secretInputs.clear();
    $("config-groups").replaceChildren();
    groups.forEach((group, index) => {
      const card = element("section", "card");
      const heading = element("div", "config-section-head");
      heading.append(element("span", "section-number", String(index + 1).padStart(2, "0")));
      const title = element("div"); title.append(element("h2", "", group.title), element("p", "", group.description)); heading.append(title);
      const grid = element("div", "field-grid");
      for (const definition of group.fields) grid.append(definition.secret ? secretField(definition, pending[definition.secret]) : configField(definition));
      card.append(heading, grid); $("config-groups").append(card);
    });
    const web = state.config.web || {};
    $("web-readonly").textContent = `管理服务：${String(web.listen || "未配置")} · Secure Cookie：${web.secure_cookie ? "开启" : "关闭"} · web 分组只读；监听地址、数据库路径和日志过滤级别的变更需要重启整个程序。`;
    updateControls();
  }

  function readForm() {
    const config = clone(state.draft);
    for (const { control, definition: def } of inputs.values()) {
      let value = control.value.trim();
      if (def.type === "check") value = control.checked;
      else if (def.type === "number" || def.type === "nullableNumber") {
        if (def.type === "nullableNumber" && value === "") value = null;
        else {
          const numeric = Number(value);
          if (value === "" || !Number.isSafeInteger(numeric) || !control.validity.valid || (def.min !== undefined && numeric < def.min) || (def.max !== undefined && numeric > def.max)) throw new Error(`「${def.label}」需要填写有效范围内的整数。`);
          value = numeric;
        }
      } else if (def.type === "ids") {
        value = value ? value.split(/[\s,，;；]+/u).map((entry) => {
          const id = Number(entry);
          if (!/^\d+$/u.test(entry) || !Number.isSafeInteger(id) || id <= 0) throw new Error(`「${def.label}」只能填写正整数 QQ 号或群号。`);
          return id;
        }) : [];
        value = [...new Set(value)];
      } else if (def.type === "lines") value = [...new Set(value.split(/\r?\n/u).map((entry) => entry.trim()).filter(Boolean))];
      else if (def.type === "json") {
        try { value = JSON.parse(value); } catch { throw new Error(`「${def.label}」不是有效的 JSON。`); }
        if (!value || typeof value !== "object" || Array.isArray(value) || Object.values(value).some((entry) => typeof entry !== "string")) throw new Error(`「${def.label}」应为名称对应文本的 JSON 对象。`);
      }
      put(config, def.path, value);
    }
    validateImmutable(config);
    return config;
  }

  function validateImmutable(config) {
    if (!config || typeof config !== "object" || Array.isArray(config)) throw new Error("配置必须是 JSON 对象。");
    if (canonical(config.web) !== canonical(state.config.web)) throw new Error("web 分组不能在线修改，请恢复此分组，或通过本机配置文件修改后重启管理服务。");
    for (const path of ["llm.api_key", "milky.access_token", "qq_login.token"]) {
      const value = get(config, path);
      if (value != null && value !== "") throw new Error(`请勿在 JSON 的 ${path} 中填入密钥；使用对应的密钥控件设置。`);
    }
    for (const key of Object.keys(state.config)) {
      if (!Object.hasOwn(config, key)) throw new Error(`完整 JSON 缺少 ${key} 分组；请保留全部配置字段。`);
    }
  }

  function secretChanges() {
    const changes = {};
    for (const [name, controls] of secretInputs) {
      const action = controls.action.value;
      if (action === "set") {
        if (!controls.value.value.trim()) throw new Error(`请填写「${controls.label}」新值，或选择保持原值。`);
        changes[name] = { action, value: controls.value.value };
      } else changes[name] = { action };
    }
    return changes;
  }

  function markDirty() {
    state.dirty = true;
    if ($("advanced").open && !state.jsonDirty) syncJson();
    updateControls();
  }

  function syncJson() {
    try {
      $("config-json").value = JSON.stringify(readForm(), null, 2);
      state.formInvalid = false;
      $("json-state").textContent = "与分组表单同步。修改 JSON 会暂时锁定表单，保存也会采用当前 JSON。";
    } catch (error) {
      state.formInvalid = true;
      $("json-state").textContent = `尚未同步：${error.message} 请先修正分组表单，再编辑 JSON。`;
    }
    updateControls();
  }

  function adoptJson() {
    let config;
    try { config = JSON.parse($("config-json").value); } catch { throw new Error("完整 JSON 格式无效，请修正后再采用或保存。"); }
    validateImmutable(config);
    // Validate represented fields before replacing the UI, so malformed nested
    // objects cannot silently disappear into the editable form.
    for (const group of groups) for (const def of group.fields) {
      if (def.secret) continue;
      const value = get(config, def.path);
      const valid = def.type === "check" ? typeof value === "boolean"
        : def.type === "number" ? Number.isSafeInteger(value)
          : def.type === "nullableNumber" ? value === null || Number.isSafeInteger(value)
            : def.type === "ids" ? Array.isArray(value) && value.every((entry) => Number.isSafeInteger(entry) && entry > 0)
              : def.type === "lines" ? Array.isArray(value) && value.every((entry) => typeof entry === "string")
                : def.type === "json" ? value && typeof value === "object" && !Array.isArray(value) && Object.values(value).every((entry) => typeof entry === "string")
                  : typeof value === "string";
      if (!valid) throw new Error(`JSON 字段 ${def.path} 的类型不正确。`);
      if (def.type === "select" && !def.choices.some(([key]) => key === value)) throw new Error(`JSON 字段 ${def.path} 的选项无效。`);
    }
    const pending = secretSnapshot();
    state.draft = config; state.jsonDirty = false; state.formInvalid = false; state.dirty = true;
    renderFields(pending); syncJson();
  }

  function updateControls() {
    const unavailable = state.busy || !state.authenticated || !state.config;
    const pending = Boolean(state.pendingOperation);
    document.querySelectorAll("[data-action]").forEach((node) => { node.disabled = unavailable; });
    document.querySelectorAll("[data-memory-write]").forEach((node) => { node.disabled = unavailable || pending || state.memory.conflict || state.memory.settingsDirty; });
    $("config-fields").disabled = unavailable || state.jsonDirty || pending;
    $("config-json").disabled = unavailable || pending || (!state.jsonDirty && state.formInvalid);
    $("dirty-label").textContent = state.stale ? "请先核对最新配置" : state.dirty || state.jsonDirty ? "有尚未保存的修改" : "配置已同步";
    for (const id of ["save-config", "save-apply"]) $(id).disabled = unavailable || pending || state.stale;
    for (const id of ["connect-runtime", "apply-runtime", "stop-runtime"]) $(id).disabled = unavailable || pending || ["starting", "stopping"].includes(state.status?.phase);
    $("stop-runtime").disabled ||= state.status?.phase === "stopped";
    $("get-qr").disabled = unavailable || !canPollQq() || Boolean(state.qq?.online);
    $("conversation-settings-fields").disabled = unavailable || pending || !state.memory.detail || state.memory.conflict;
    const memoryBlocked = unavailable || pending || !state.memory.detail || state.memory.conflict;
    for (const id of ["pause-conversation", "once-conversation", "reset-conversation", "new-memory", "change-conversation-mode"]) $(id).disabled = memoryBlocked || state.memory.settingsDirty;
    $("save-conversation-settings").disabled = memoryBlocked || !state.memory.settingsDirty;
    $("conversation-mode").disabled = memoryBlocked || state.memory.settingsDirty || state.memory.detail?.scene !== "group";
    $("change-conversation-mode").disabled ||= state.memory.detail?.scene !== "group";
    $("forget-selected").disabled = memoryBlocked || state.memory.settingsDirty || !state.memory.checked.size;
    $("save-memory").disabled = memoryBlocked;
    $("confirm-forget").disabled = memoryBlocked || !state.memory.preview;
    $("conversation-dirty").textContent = state.memory.settingsDirty ? "有尚未保存的会话设置" : "与当前会话同步";
    $("memory-selection-count").textContent = state.memory.checked.size ? "已选择 " + state.memory.checked.size + " 条" : "未选择";
    for (const [prefix, page, total] of [["conversations", state.memory.page, state.memory.totalPages], ["memories", state.memory.pageMemories, state.memory.totalMemoryPages]]) {
      $(prefix + "-prev").disabled = unavailable || page <= 1;
      $(prefix + "-next").disabled = unavailable || page >= total;
    }
    $("check-operation").disabled = state.busy || !state.authenticated;
    $("logout").disabled = state.busy;
    $("change-password").disabled = state.busy;
    $("reauth-submit").disabled = state.busy;
    $("open-security").disabled = state.busy;
    document.querySelectorAll("#memory-editor-form input,#memory-editor-form select,#memory-editor-form textarea").forEach((node) => { node.disabled = state.busy || pending; });
    $("edit-memory-subject").disabled ||= Boolean(state.memory.editing?.id);
    $("edit-memory-locked").disabled ||= !state.memory.editing?.id;
    updateContactControls();
  }

  async function loadConfig() {
    const result = await api("/api/config");
    if (!result.config || typeof result.revision !== "string") throw new Error("配置响应缺少 config 或 revision。");
    state.config = clone(result.config); state.draft = clone(result.config);
    state.revision = result.revision; state.secrets = result.secrets || {};
    state.dirty = false; state.jsonDirty = false; state.formInvalid = false; state.stale = false;
    resumeQqPolling(); clearQr();
    $("config-warning").hidden = true;
    $("saved-revision").textContent = "已保存版本：" + state.revision;
    renderFields(); syncJson(); renderStatus(); renderQq(); scheduleQq(0);
  }

  function badge(id, text, kind) { $(id).textContent = text; $(id).className = "badge " + kind; }
  const phases = {
    stopped: ["已停止", "准备连接", "neutral"], starting: ["启动中", "正在启动机器人", "wait"],
    waiting_login: ["等待登录", "等待 QQ 登录", "wait"], running: ["运行中", "小鱼正在运行", "good"],
    stopping: ["停止中", "正在断开连接", "wait"], error: ["连接异常", "需要检查连接", "bad"],
  };
  const dateText = (stamp) => {
    if (!Number.isFinite(stamp)) return "—";
    const date = new Date(stamp > 1e12 ? stamp : stamp * 1000);
    return Number.isNaN(date.getTime()) ? "—" : date.toLocaleString("zh-CN", { hour12: false });
  };
  function renderStatus() {
    if (!state.status) { updateControls(); return; }
    const status = state.status;
    const contacts = state.contacts;
    if ((contacts.botId || contacts.loading) && ((contacts.generation != null && status.runtime_generation != null && contacts.generation !== status.runtime_generation) || (contacts.botId && status.online && status.bot_id != null && String(status.bot_id) !== contacts.botId))) {
      invalidateContacts("机器人连接或账号已变化，请刷新联系人列表。");
    }
    const [label, title, kind] = phases[status.phase] || ["未知状态", "等待状态更新", "neutral"];
    badge("runtime-badge", label, kind); badge("header-status", label, kind);
    $("runtime-title").textContent = title;
    $("runtime-message").textContent = String(status.message || "");
    $("bot-id").textContent = status.bot_id == null ? "—" : String(status.bot_id);
    $("started-at").textContent = dateText(status.started_at);
    $("revision-state").textContent = !status.applied_revision ? "尚未应用" : status.applied_revision === state.revision ? "已应用最新配置" : "有待应用配置";
    $("revision-detail").textContent = status.applied_revision ? "运行版本：" + status.applied_revision : "保存后点击连接或应用配置";
    $("poll-state").textContent = document.hidden ? "页面隐藏，已暂停轮询" : "自动同步";
    updateControls();
  }
  async function loadBudget() {
    const result = await api("/api/budget");
    const usage = Array.isArray(result.usage) ? result.usage : [];
    const total = (name) => usage.reduce((sum, row) => sum + (Number(row[name]) || 0), 0).toLocaleString("zh-CN");
    const box = $("budget-usage"); box.replaceChildren();
    for (const [title, value, hint] of [
      ["前台请求", String(result.used?.foreground ?? 0) + " / " + String(result.limits?.model_global_per_hour ?? "—"), "最近 1 小时 · 全局上限"],
      ["后台整理", String(result.used?.background ?? 0) + " / " + String(result.limits?.memory_requests_per_hour ?? "—"), "最近 1 小时 · 独立预算"],
      ["输入 token", total("input_tokens"), "最近 7 天 · 服务商实际用量"],
      ["输出 token", total("output_tokens"), "最近 7 天 · 服务商实际用量"],
    ]) { const item = element("div", "budget-item"); item.append(element("span", "section-label", title), element("strong", "", value), element("p", "hint", hint)); box.append(item); }
  }
  function canPollQq() { return Boolean(state.config?.qq_login?.enabled && state.secrets.llbot_token?.effective_set); }
  function clearQr() {
    $("qr-image").hidden = true; $("qr-image").removeAttribute("src"); $("qr-placeholder").hidden = false;
    state.qrExpires = null; $("qr-expiry").textContent = "二维码不会自动刷新，避免正在扫描时失效。"; $("get-qr").textContent = "获取登录二维码";
  }
  function renderQq() {
    let label = "待同步", kind = "neutral", text = "正在读取 QQ 状态。";
    if (!state.config?.qq_login?.enabled) { label = "未启用"; text = "请先在配置中启用 QQ 登录适配。"; }
    else if (!state.secrets.llbot_token?.effective_set) { label = "待配置"; text = "请先配置 LLBot WebUI 地址和密码。"; }
    else if (state.qqPollingPaused) { label = "等待检查"; kind = "bad"; text = state.qqError + " 自动查询已暂停，修正配置后手动刷新。"; }
    else if (state.qqError) { label = "暂时不可用"; kind = "wait"; text = state.qqError + " 将稍后重试。"; }
    else if (state.qq?.online) { label = "已登录"; kind = "good"; text = "QQ 已在线，每 30 秒或服务端建议间隔同步。"; clearQr(); }
    else if (state.qq) { label = "等待扫码"; kind = "wait"; text = "QQ 尚未登录，按服务端建议间隔同步状态。"; }
    badge("qq-badge", label, kind);
    $("qq-detail").textContent = text;
    $("account-name").textContent = state.qq?.online ? String(state.qq.nick || "已登录") : label;
    $("account-id").textContent = state.qq?.online ? "QQ " + String(state.qq.uin || "—") : "前往 QQ 登录查看详情";
    if (state.qrExpires && !$("qr-image").hidden) $("qr-expiry").textContent = Date.now() >= state.qrExpires * 1000 ? "二维码可能已过期，请手动刷新。" : "二维码有效期至 " + dateText(state.qrExpires);
    updateControls();
  }
  function stopTimers() { clearTimeout(state.runtimeTimer); clearTimeout(state.qqTimer); state.runtimeTimer = null; state.qqTimer = null; }
  function scheduleRuntime(delay = 5000) {
    clearTimeout(state.runtimeTimer);
    if (state.authenticated && !document.hidden) state.runtimeTimer = setTimeout(() => { void refreshStatus(); }, delay);
  }
  function scheduleQq(delay = state.qqInterval) {
    clearTimeout(state.qqTimer);
    if (state.authenticated && !document.hidden && canPollQq() && !state.qqPollingPaused) state.qqTimer = setTimeout(() => { void refreshQq(); }, delay);
  }
  function resumeQqPolling() {
    state.qqQueryVersion++; state.qqPolling = false; state.qqPollingPaused = false; state.qqFailures = 0;
    state.qqInterval = 5000; state.qqError = ""; state.qq = null; clearTimeout(state.qqTimer);
  }
  async function refreshStatus(manual = false) {
    if (!state.authenticated || state.runtimePolling || document.hidden) return;
    const session = state.session; state.runtimePolling = true;
    try { state.status = await api("/api/status"); renderStatus(); if (manual) notice("运行状态已更新。"); }
    catch (error) { if (session === state.session && error.status !== -1) { if (manual) report(error); else badge("header-status", "状态暂不可用", "wait"); } }
    finally { if (session === state.session) { state.runtimePolling = false; scheduleRuntime(); } }
  }
  function qqFailure(error) {
    state.qqError = error.message || "QQ 状态查询失败。";
    if (["qq_auth", "qq_config"].includes(error.code)) state.qqPollingPaused = true;
    else { state.qqFailures++; state.qqInterval = Math.min(60000, Math.max(Number(error.retryAfter) || 0, 3000 * 2 ** Math.min(state.qqFailures - 1, 5))); }
  }
  async function refreshQq(manual = false) {
    if (!state.authenticated || !canPollQq() || document.hidden) return;
    if (manual) resumeQqPolling();
    if (state.qqPolling || state.qqPollingPaused) return;
    const version = state.qqQueryVersion, session = state.session; state.qqPolling = true;
    try {
      const result = await api("/api/qq/status" + (manual ? "?force=true" : ""));
      if (version !== state.qqQueryVersion) return;
      const previous = state.qq;
      if (state.contacts.botId && previous?.online && String(previous.uin) === state.contacts.botId && (!result.online || String(result.uin) !== state.contacts.botId)) invalidateContacts("QQ 登录状态或账号已变化，请登录后刷新联系人列表。", "offline");
      state.qq = result; state.qqError = ""; state.qqFailures = 0;
      state.qqInterval = Math.max(result.online ? 30000 : 1000, Math.min(60000, Number(result.poll_interval_ms) || 5000));
      renderQq();
    } catch (error) { if (session === state.session && version === state.qqQueryVersion && error.status !== -1) { qqFailure(error); renderQq(); if (manual) report(error); } }
    finally { if (session === state.session && version === state.qqQueryVersion) { state.qqPolling = false; scheduleQq(); } }
  }
  async function getQr() {
    resumeQqPolling(); const version = state.qqQueryVersion;
    try {
      const result = await api("/api/qq/qrcode", "POST", {});
      if (version !== state.qqQueryVersion) return;
      if (typeof result.image !== "string" || result.image.length > 4 * 1024 * 1024 || !/^data:image\/png;base64,[A-Za-z0-9+/=\r\n]+$/u.test(result.image)) throw new Error("服务端没有返回有效的 PNG 二维码。");
      $("qr-image").src = result.image; $("qr-image").hidden = false; $("qr-placeholder").hidden = true;
      $("get-qr").textContent = "刷新登录二维码";
      state.qrExpires = Number(result.expires_at) || null; state.qqInterval = Math.max(1000, Math.min(60000, Number(result.poll_interval_ms) || 5000));
      renderQq(); scheduleQq();
    } catch (error) { if (version === state.qqQueryVersion && error.status !== -1) { qqFailure(error); renderQq(); scheduleQq(); } throw error; }
  }
  const CONTACT_PAGE_SIZE = 30;
  const contactId = (value) => typeof value === "string" && /^[1-9]\d{0,19}$/u.test(value);
  const contactsCurrent = (version, session) => state.authenticated && state.session === session && state.contacts.version === version && state.view === "contacts";
  function invalidateContacts(message = "进入此页后加载联系人列表。", phase = "idle") {
    const c = state.contacts;
    c.version++; c.loading = false; c.saving = null; c.phase = phase; c.message = message;
    c.botId = ""; c.generation = null; c.groups = []; c.friends = []; c.fetchedAt = null; c.page = 1;
    renderContacts();
  }
  function updateContactControls() {
    const c = state.contacts, blocked = !state.authenticated || state.busy;
    $("refresh-contacts").disabled = blocked || c.loading || Boolean(c.saving);
    $("contacts-search").disabled = !state.authenticated || c.loading;
    $("contacts-prev").disabled = blocked || c.loading || c.page <= 1;
    $("contacts-next").disabled = blocked || c.loading || c.page >= c.totalPages;
    document.querySelectorAll("[data-contact-toggle]").forEach((input) => { input.disabled = blocked || c.phase !== "ready" || c.loading || Boolean(c.saving); });
  }
  function renderContacts() {
    const c = state.contacts, name = c.scene === "group" ? "群聊" : "好友";
    $("contacts-account").textContent = c.botId ? "QQ " + c.botId : "尚未读取";
    $("contacts-updated").textContent = c.fetchedAt == null ? "按需读取 · 手动刷新" : "更新于 " + dateText(c.fetchedAt);
    for (const [id, scene, rows, title] of [["contacts-groups", "group", c.groups, "群聊"], ["contacts-friends", "friend", c.friends, "好友"]]) {
      const active = scene === c.scene;
      $(id).textContent = title + (c.botId ? " · " + rows.length : "");
      $(id).classList.toggle("active", active); $(id).setAttribute("aria-pressed", String(active));
    }
    const query = c.query.trim().toLocaleLowerCase();
    const rows = (c.scene === "group" ? c.groups : c.friends).filter((row) => !query || [row.name, row.remark, row.id].some((text) => text.toLocaleLowerCase().includes(query)));
    c.totalPages = Math.max(1, Math.ceil(rows.length / CONTACT_PAGE_SIZE)); c.page = Math.min(Math.max(c.page, 1), c.totalPages);
    const message = $("contacts-message");
    message.className = "contacts-message " + (["error", "uncertain"].includes(c.phase) ? "inline-error" : "muted");
    message.textContent = c.loading ? "正在读取当前账号的群与好友……" : c.message || (c.phase === "ready" ? rows.length ? `${name} ${rows.length} 个 · 修改后自动保存` : query ? "没有匹配的名称、备注或 ID。" : `当前账号暂无${name}。` : "请刷新列表，读取已登录 QQ 的群与好友。");
    const list = $("contacts-list"); list.replaceChildren(); list.setAttribute("aria-busy", String(c.loading));
    for (const row of rows.slice((c.page - 1) * CONTACT_PAGE_SIZE, c.page * CONTACT_PAGE_SIZE)) {
      const item = element("div", "contact-row"), identity = element("div", "contact-identity");
      const title = row.remark || row.name || (c.scene === "group" ? "群聊" : "好友");
      identity.append(element("strong", "contact-name", title));
      if (row.remark && row.name && row.remark !== row.name) identity.append(element("span", "hint contact-original", row.name));
      identity.append(element("span", "hint contact-id", (c.scene === "group" ? "群号 " : "QQ ") + row.id));
      const label = element("label", "contact-switch"), input = document.createElement("input");
      input.type = "checkbox"; input.checked = row.enabled; input.setAttribute("role", "switch"); input.dataset.contactToggle = row.id;
      input.setAttribute("aria-label", title + "（" + row.id + "）Bot 开关");
      const scene = c.scene, version = c.version;
      input.addEventListener("change", () => { if (version === state.contacts.version) void toggleContact(scene, row.id, input.checked); });
      label.append(input, element("span", "contact-switch-track"), element("span", "contact-switch-label", c.saving === scene + ":" + row.id ? "保存中…" : row.enabled ? "已开启" : "已关闭"));
      item.append(identity, label); list.append(item);
    }
    $("contacts-page").textContent = `第 ${c.page} / ${c.totalPages} 页 · 共 ${rows.length} 个`;
    updateContactControls();
  }
  function validateContacts(result) {
    if (!contactId(result.bot_id) || !Number.isFinite(result.fetched_at)) throw new Error("联系人响应缺少有效的账号或更新时间。");
    for (const key of ["groups", "friends"]) {
      if (!Array.isArray(result[key])) throw new Error("联系人列表格式不正确。");
      const seen = new Set();
      for (const row of result[key]) {
        if (!row || !contactId(row.id) || typeof row.name !== "string" || typeof row.remark !== "string" || typeof row.enabled !== "boolean" || seen.has(row.id)) throw new Error("联系人数据不完整或存在重复 ID，请刷新重试。");
        seen.add(row.id);
      }
    }
  }
  async function loadContacts(force = false) {
    const c = state.contacts;
    if (!state.authenticated || state.view !== "contacts" || c.loading || c.saving) return;
    const session = state.session, version = ++c.version;
    c.loading = true; c.phase = "loading"; c.message = ""; c.generation = state.status?.runtime_generation ?? null;
    c.groups = []; c.friends = []; c.botId = ""; c.fetchedAt = null; renderContacts();
    try {
      const result = await api("/api/qq/contacts" + (force ? "?force=true" : ""));
      if (!contactsCurrent(version, session)) return;
      validateContacts(result);
      c.botId = result.bot_id; c.groups = result.groups; c.friends = result.friends; c.fetchedAt = result.fetched_at; c.phase = "ready";
    } catch (error) {
      if (!contactsCurrent(version, session)) return;
      c.phase = [502, 503].includes(error.status) ? "offline" : "error";
      c.message = (c.phase === "offline" ? "暂时无法读取 QQ 联系人。请确认 QQ 已登录且 Milky 可连接，再手动刷新。 " : "联系人列表未能加载。 ") + error.message;
    } finally {
      if (contactsCurrent(version, session)) { c.loading = false; renderContacts(); }
    }
  }
  async function toggleContact(scene, peerId, enabled) {
    const c = state.contacts;
    if (state.view !== "contacts" || !state.authenticated || state.busy || c.phase !== "ready" || c.loading || c.saving || typeof enabled !== "boolean") { renderContacts(); return; }
    const row = (scene === "group" ? c.groups : scene === "friend" ? c.friends : []).find((item) => item.id === peerId);
    if (!row || row.enabled === enabled) return;
    const version = c.version, session = state.session, botId = c.botId, previous = row.enabled;
    c.saving = scene + ":" + peerId; c.message = ""; row.enabled = enabled; renderContacts();
    try {
      const result = await api("/api/qq/contacts/toggle", "POST", { bot_id: botId, scene, peer_id: peerId, enabled });
      if (!contactsCurrent(version, session)) return;
      if (result.bot_id !== botId || result.scene !== scene || result.peer_id !== peerId || result.enabled !== enabled) throw new Error("开关响应与当前账号或会话不一致。");
      c.message = `${row.remark || row.name || row.id} 的 Bot 已${enabled ? "开启" : "关闭"}，即时生效。`;
    } catch (error) {
      if (!contactsCurrent(version, session)) return;
      row.enabled = previous;
      if (error.status === 409) { invalidateContacts("QQ 账号或联系人已变化，旧列表已停用，请刷新后再操作。", "error"); return; }
      c.phase = "uncertain"; c.message = "开关未确认，已还原页面显示。请刷新列表核对实际状态。 " + error.message;
      if (error.code === "csrf_invalid") report(error);
    } finally {
      if (contactsCurrent(version, session)) { c.saving = null; renderContacts(); }
    }
  }
  async function runAction(action) {
    if (state.busy) return;
    const session = state.session; state.busy = true; updateControls();
    try { return await action(); } catch (error) { report(error); }
    finally { if (session === state.session) { state.busy = false; updateControls(); } }
  }
  const uncertain = (error) => error.status === 0 || error.status >= 500 || error.code === "invalid_response";
  function trackOperation(operation) {
    state.pendingOperation = operation;
    $("operation-text").textContent = operation.label + "的响应未确认。请查询结果；页面不会重复提交。" + (operation.requestId ? " 操作号：" + operation.requestId : "");
    $("operation-notice").hidden = false; updateControls();
  }
  function clearOperation() { state.pendingOperation = null; $("operation-notice").hidden = true; updateControls(); }
  async function applyRuntime(stop = false) {
    try { await api(stop ? "/api/runtime/stop" : "/api/runtime/apply", "POST", {}); }
    catch (error) {
      if (uncertain(error)) { trackOperation({ kind: "runtime", label: stop ? "断开连接" : "应用配置", goal: stop ? "stopped" : "running", revision: state.revision }); return; }
      throw error;
    }
    notice(stop ? "已提交断开请求。" : "已提交应用请求，连接进度会自动更新。"); await refreshStatus();
  }
  async function saveConfig(apply) {
    if (state.jsonDirty) adoptJson();
    const config = readForm(), secrets = secretChanges();
    const logChanged = config.app.log !== state.config.app.log, databaseChanged = config.app.database !== state.config.app.database;
    try { await api("/api/config", "PUT", { revision: state.revision, config, secrets }); }
    catch (error) {
      if (uncertain(error)) { state.stale = true; trackOperation({ kind: "config", label: "保存配置", previousRevision: state.revision, config }); return; }
      throw error;
    }
    await loadConfig();
    if (databaseChanged) notice("配置已保存。数据库路径已改变，请重启整个程序后再使用；不会在当前进程中应用。", "warning");
    else if (apply) { await applyRuntime(); if (logChanged) notice("配置已保存并提交应用。日志过滤级别仍需要重启整个程序。", "warning"); }
    else notice("配置已保存，应用后生效。" + (logChanged ? " 日志过滤级别需要重启整个程序。" : ""), logChanged ? "warning" : "success");
  }
  async function checkOperation() {
    const operation = state.pendingOperation; if (!operation) return;
    if (operation.kind === "memory") {
      const result = await api("/api/memory/operations/" + encodeURIComponent(operation.requestId));
      if (result.status === "completed") { clearOperation(); await operation.complete(result); }
      else notice("尚未查到完成记录。请稍后再次查询，不要重新提交。操作号：" + operation.requestId, "warning");
    } else if (operation.kind === "config") {
      const result = await api("/api/config");
      clearOperation(); state.stale = true;
      $("config-warning").hidden = false;
      $("config-warning").textContent = result.revision !== operation.previousRevision && canonical(result.config) === canonical(operation.config)
        ? "服务器已有匹配的配置。密钥不可回读，请核对密钥状态后重新载入；不会自动应用。"
        : "已取得服务器当前版本，但无法确认这次保存。当前编辑仍保留，请先复制再重新载入核对。";
      notice($("config-warning").textContent, "warning"); updateControls();
    } else {
      state.status = await api("/api/status"); renderStatus();
      const done = operation.goal === "stopped" ? state.status.phase === "stopped" : state.status.applied_revision === operation.revision && ["running", "waiting_login"].includes(state.status.phase);
      if (done || state.status.phase === "error") { clearOperation(); notice(done ? "运行状态已确认。" : String(state.status.message || "运行时应用失败。"), done ? "success" : "error"); }
      else notice("当前状态：" + String(state.status.phase) + "。请稍后再次查询。", "warning");
    }
  }
  function showView(view) {
    if (!["overview", "qq", "contacts", "config", "memory"].includes(view)) return;
    if (state.view === "contacts" && view !== "contacts") invalidateContacts();
    state.view = view;
    document.querySelectorAll(".view").forEach((node) => { node.hidden = node.id !== "view-" + view; });
    document.querySelectorAll(".nav-item").forEach((node) => { const current = node.dataset.view === view; node.classList.toggle("active", current); if (current) node.setAttribute("aria-current", "page"); else node.removeAttribute("aria-current"); });
    $("page-name").textContent = { overview: "运行概览", qq: "QQ 登录", contacts: "群与好友", config: "配置管理", memory: "记忆与上下文" }[view];
    if (view === "memory" && !state.memory.list.length) void runAction(loadConversations);
    if (view === "qq") scheduleQq(0);
    if (view === "contacts" && state.contacts.phase !== "ready") void loadContacts();
  }
  const hasEdits = () => state.dirty || state.jsonDirty || state.memory.settingsDirty || state.memory.editorDirty;
  function authView(result) {
    state.authenticated = Boolean(result.authenticated); state.csrf = result.csrf_token || ""; state.setupRequired = Boolean(result.setup_required);
    $("setup-code-field").hidden = !state.setupRequired; $("setup-confirm-field").hidden = !state.setupRequired;
    $("setup-code").required = state.setupRequired; $("setup-confirm").required = state.setupRequired;
    $("admin-password").minLength = state.setupRequired ? 12 : 1; $("admin-password").autocomplete = state.setupRequired ? "new-password" : "current-password";
    $("password-label").textContent = state.setupRequired ? "设置管理员密码" : "管理员密码";
    $("login-description").textContent = state.setupRequired ? "使用本机启动时显示的一次性初始化码，设置管理员密码。" : "输入管理员密码，进入本机管理台。";
    $("login-hint").textContent = state.setupRequired ? "密码至少 12 个字符。初始化码只用于首次设置。" : "登录状态由安全会话 Cookie 管理；退出后立即撤销。";
    $("login-button").textContent = state.setupRequired ? "初始化并登录" : "登录管理台"; $("login-button").disabled = false;
  }
  function clearSession(message = "") {
    stopTimers(); state.session++; state.authenticated = false; state.csrf = ""; state.busy = false;
    state.config = null; state.draft = null; state.status = null; state.qq = null; state.runtimePolling = false;
    state.secrets = {}; state.dirty = false; state.jsonDirty = false; state.pendingOperation = null; resumeQqPolling();
    invalidateContacts(); state.contacts.query = ""; $("contacts-search").value = "";
    inputs.clear(); secretInputs.clear(); $("config-groups").replaceChildren(); $("config-json").value = "";
    clearMemorySelection(); state.memory.list = []; $("conversation-list").replaceChildren(); $("budget-usage").replaceChildren();
    document.querySelectorAll("dialog").forEach((dialog) => { if (dialog.open) dialog.close(); });
    document.querySelectorAll('input[type="password"]').forEach((input) => { input.value = ""; });
    $("setup-code").value = ""; clearQr(); $("app").hidden = true; $("login-screen").hidden = false;
    $("notice").hidden = true; $("operation-notice").hidden = true; $("login-error").textContent = message;
    authView({ authenticated: false, setup_required: state.setupRequired }); updateControls();
  }
  async function enterSession(result) {
    authView(result); state.session++;
    await loadConfig();
    $("login-screen").hidden = true; $("app").hidden = false; $("login-error").textContent = "";
    document.querySelectorAll('input[type="password"]').forEach((input) => { input.value = ""; }); $("setup-code").value = "";
    showView("overview");
    await Promise.allSettled([refreshStatus(), refreshQq(), loadBudget()]);
    updateControls();
  }
  async function bootstrap() {
    try { const result = await api("/api/auth/state"); authView(result); if (result.authenticated) await enterSession(result); }
    catch (error) { $("login-error").textContent = error.message; $("login-button").disabled = false; }
  }


  const settingInputs = new Map();
  const settingDefinitions = [
    field("persona", "人设名称", "text", "对应已保存配置中的人设表。"),
    field("input_budget", "输入预算（保守估算）", "number", "", { min: 1024, max: 262144 }),
    field("output_budget", "输出 token 上限", "number", "", { min: 64, max: 32768 }),
    field("recent_messages", "最近历史条数", "number", "", { min: 0, max: 4096 }),
    field("allowed_tools", "允许的工具", "lines", "每行一个工具名，例如 clock_now。"),
    field("summary", "生成并使用摘要", "check", "关闭后不生成或注入摘要；保留原文按历史条数读取。"),
    field("memory_read", "读取长期记忆", "check", "将当前范围内相关记忆加入回复上下文。"),
    field("memory_write", "自动提炼记忆", "check", "与摘要开关独立，只处理有效的人类消息证据。"),
  ];
  const kindNames = { fact: "事实", preference: "偏好", task: "任务", event: "事件" };
  function positiveId(value, label, optional = false) {
    const text = String(value).trim(); if (!text && optional) return null;
    const number = Number(text);
    if (!/^\d+$/u.test(text) || !Number.isSafeInteger(number) || number <= 0) throw new Error(label + "需要填写有效的正整数 QQ 号。");
    return number;
  }
  function scopeTitle(conversation) {
    return (conversation.scene === "group" ? "群 " : "私聊 ") + String(conversation.peer_id ?? conversation.scope) + (conversation.owner_id ? " · 成员 " + conversation.owner_id : "");
  }
  function scopeDescription(conversation) {
    return "机器人 " + String(conversation.bot_id ?? "—") + " · " + (conversation.mode === "user" ? "成员隔离" : "共享上下文") + " · 版本 " + conversation.epoch + " / " + conversation.memory_revision + (conversation.is_current === false ? " · 历史会话" : "");
  }
  function memoryIsCurrent(version, id) { return state.authenticated && state.memory.version === version && state.memory.selected?.id === id; }
  function clearMemorySelection() {
    state.memory.version++; state.memory.selected = null; state.memory.detail = null; state.memory.rows = []; state.memory.checked.clear();
    state.memory.settingsDirty = false; state.memory.editorDirty = false; state.memory.editing = null; state.memory.preview = null; state.memory.conflict = false;
    settingInputs.clear(); $("conversation-settings-grid").replaceChildren(); $("memory-list").replaceChildren(); $("context-content").replaceChildren();
    $("selected-conversation").hidden = true; $("no-conversation").hidden = false; $("context-details").open = false;
    $("conversation-conflict").hidden = true; $("memory-editor-form").reset(); $("forget-preview").replaceChildren();
    for (const id of ["memory-editor", "forget-dialog"]) if ($(id).open) $(id).close();
  }
  function canDiscardMemory() {
    if (state.pendingOperation?.kind === "memory") { notice("请先查询并确认当前记忆操作的结果。", "warning"); return false; }
    return !(state.memory.settingsDirty || state.memory.editorDirty) || window.confirm("当前会话有未保存的编辑。确定放弃并重新载入或切换范围？");
  }
  async function loadConversations() {
    const query = new URLSearchParams({ page: String(state.memory.page) });
    for (const [id, key] of [["scope-bot", "bot_id"], ["scope-peer", "peer_id"], ["scope-owner", "owner_id"]]) {
      const number = positiveId($(id).value, $(id).previousElementSibling?.textContent || "筛选项", true);
      if (number !== null) query.set(key, String(number));
    }
    for (const [id, key] of [["scope-scene", "scene"], ["scope-mode", "mode"]]) if ($(id).value) query.set(key, $(id).value);
    const result = await api("/api/memory/conversations?" + query);
    state.memory.list = result.items || []; state.memory.totalPages = Math.max(1, result.total_pages || 1);
    $("conversations-page").textContent = "第 " + result.page + " / " + state.memory.totalPages + " 页 · " + result.total + " 个";
    renderConversationList(); updateControls();
  }
  function renderConversationList() {
    const box = $("conversation-list"); box.replaceChildren();
    if (!state.memory.list.length) box.append(element("p", "empty-state", "没有符合条件的会话。先在已开启 Bot 的会话中与机器人聊天，随后在这里刷新。"));
    for (const conversation of state.memory.list) {
      const button = element("button", "conversation-item" + (state.memory.selected?.id === conversation.id ? " selected" : ""));
      button.type = "button"; button.dataset.action = "";
      button.setAttribute("aria-pressed", String(state.memory.selected?.id === conversation.id));
      button.append(element("strong", "", scopeTitle(conversation)), element("span", "hint", "机器人 " + conversation.bot_id + " · " + (conversation.mode === "user" ? "成员隔离" : "共享") + " · " + conversation.memory_count + " 条记忆" + (conversation.is_current === false ? " · 历史" : "")));
      button.addEventListener("click", () => { if (!state.busy && canDiscardMemory()) void runAction(() => selectConversation(conversation)); });
      box.append(button);
    }
  }
  async function selectConversation(conversation) {
    clearMemorySelection(); state.memory.selected = clone(conversation); state.memory.pageMemories = 1;
    $("memory-keyword").value = ""; $("memory-subject").value = ""; $("memory-enabled").value = "";
    renderConversationList();
    await reloadSelected(); await loadMemories();
  }
  async function reloadSelected() {
    const id = state.memory.selected?.id, version = state.memory.version; if (!id) return;
    const detail = await api("/api/memory/conversations/" + encodeURIComponent(id));
    if (!memoryIsCurrent(version, id)) return;
    state.memory.detail = detail; state.memory.selected = detail; state.memory.conflict = false; state.memory.settingsDirty = false;
    $("no-conversation").hidden = true; $("selected-conversation").hidden = false; $("conversation-conflict").hidden = true;
    $("selected-scope-title").textContent = scopeTitle(detail); $("selected-scope-detail").textContent = scopeDescription(detail);
    badge("selected-scope-badge", detail.is_current === false ? "历史会话" : detail.settings.paused ? "已暂停" : "可聊天", detail.is_current === false ? "neutral" : detail.settings.paused ? "wait" : "good");
    $("pause-conversation").textContent = detail.settings.paused ? "恢复聊天" : "暂停聊天";
    $("once-conversation").textContent = detail.settings.once ? "已放行一次" : "只回复一次";
    $("conversation-mode").value = detail.scope_mode || detail.mode;
    renderSettings(); if ($("context-details").open) renderContext(detail); updateControls();
  }
  function renderSettings() {
    settingInputs.clear(); const box = $("conversation-settings-grid"); box.replaceChildren();
    for (const def of settingDefinitions) {
      const wrapper = element("div", "field" + (def.type === "check" ? " check-field" : ""));
      const label = element("label", "", def.label); label.htmlFor = "setting-" + def.path;
      const input = document.createElement(def.type === "lines" ? "textarea" : "input"); input.id = label.htmlFor;
      if (def.type !== "lines") input.type = def.type === "check" ? "checkbox" : def.type;
      input.autocomplete = "off"; const value = state.memory.detail.settings[def.path];
      if (def.type === "check") input.checked = Boolean(value);
      else input.value = def.type === "lines" ? (value || []).join("\n") : String(value ?? "");
      if (def.type === "number") { input.min = String(def.min); input.max = String(def.max); input.step = "1"; input.required = true; }
      if (def.path === "persona") input.required = true;
      const hint = element("p", "hint", def.hint);
      if (def.type === "check") { const copy = element("div"); copy.append(label, hint); wrapper.append(input, copy); }
      else wrapper.append(label, input, hint);
      input.addEventListener("input", () => { state.memory.settingsDirty = true; updateControls(); });
      settingInputs.set(def.path, { input, def }); box.append(wrapper);
    }
  }
  function readSettings() {
    const result = clone(state.memory.detail.settings);
    for (const [key, { input, def }] of settingInputs) {
      if (def.type === "check") result[key] = input.checked;
      else if (def.type === "number") {
        const number = Number(input.value);
        if (!input.value || !Number.isSafeInteger(number) || number < def.min || number > def.max) throw new Error("「" + def.label + "」超出允许范围。");
        result[key] = number;
      } else if (def.type === "lines") result[key] = [...new Set(input.value.split(/\r?\n/u).map((text) => text.trim()).filter(Boolean))];
      else result[key] = input.value.trim();
    }
    return result;
  }
  function renderContext(detail) {
    $("context-content").replaceChildren(element("p", "hint", "当前有效上下文 " + detail.context_count + " 条 · 摘要" + (detail.settings.summary ? "启用" : "未启用")));
    $("context-content").append(element("pre", "source-text", detail.summary || "尚无摘要。"));
  }
  async function loadContext() {
    const id = state.memory.selected?.id, version = state.memory.version; if (!id) return;
    const result = await api("/api/memory/conversations/" + encodeURIComponent(id));
    if (memoryIsCurrent(version, id)) renderContext(result);
  }
  async function loadMemories() {
    const id = state.memory.selected?.id, version = state.memory.version; if (!id) return;
    const query = new URLSearchParams({ conversation_id: id, page: String(state.memory.pageMemories) });
    if ($("memory-keyword").value.trim()) query.set("keyword", $("memory-keyword").value.trim());
    const subject = positiveId($("memory-subject").value, "人物", true); if (subject !== null) query.set("subject_id", String(subject));
    if ($("memory-enabled").value) query.set("active", $("memory-enabled").value);
    const result = await api("/api/memory/records?" + query);
    if (!memoryIsCurrent(version, id)) return;
    if ((result.items || []).some((item) => item.conversation_id !== id)) throw new Error("记忆响应的会话范围不一致，已停止显示。");
    state.memory.rows = result.items || []; state.memory.totalMemoryPages = Math.max(1, result.total_pages || 1);
    $("memories-page").textContent = "第 " + result.page + " / " + state.memory.totalMemoryPages + " 页 · " + result.total + " 条";
    renderMemories(); updateControls();
  }
  function memoryButton(text, handler, write = false) {
    const button = element("button", "text-button accent", text); button.type = "button";
    if (write) button.dataset.memoryWrite = ""; else button.dataset.action = "";
    button.addEventListener("click", () => { void runAction(handler); }); return button;
  }
  function renderMemories() {
    const box = $("memory-list"); box.replaceChildren();
    if (!state.memory.rows.length) box.append(element("p", "empty-state", "这个范围下没有符合筛选条件的记忆。"));
    for (const record of state.memory.rows) {
      const card = element("article", "memory-record");
      const top = element("div", "memory-record-top"), checkbox = document.createElement("input");
      checkbox.type = "checkbox"; checkbox.checked = state.memory.checked.has(record.id);
      checkbox.setAttribute("aria-label", "选择记忆 " + record.id); checkbox.dataset.memoryId = String(record.id);
      checkbox.addEventListener("change", () => { if (checkbox.checked) state.memory.checked.add(record.id); else state.memory.checked.delete(record.id); updateSelection(); updateControls(); });
      top.append(checkbox, element("span", "record-kind", kindNames[record.kind] || record.kind), element("span", "hint", "人物 " + record.subject_id), element("span", "record-state", (record.active ? "已启用" : "已停用") + " · " + (record.origin === "manual" ? "人工" : "自动") + (record.locked ? " · 锁定" : "")));
      card.append(top, element("p", "memory-text", record.text));
      const actions = element("div", "button-row");
      actions.append(memoryButton("编辑", () => openMemoryEditor(record), true), memoryButton(record.active ? "停用" : "启用", () => mutateMemory({ op: "set_active", id: record.id, version: record.version, active: !record.active }), true), memoryButton("遗忘", () => previewForget([record.id]), true));
      const details = element("details", "evidence-details"), summary = element("summary", "", "查看证据与来源"), evidence = element("div", "evidence-content"); details.append(summary, evidence);
      let loaded = false;
      details.addEventListener("toggle", () => {
        if (details.open && !loaded) { loaded = true; void loadEvidence(record, evidence, 1).catch((error) => { loaded = false; evidence.replaceChildren(element("p", "inline-error", error.message)); report(error); }); }
      });
      card.append(actions, details); box.append(card);
    }
    updateSelection();
  }
  function updateSelection() {
    const all = state.memory.rows.length > 0 && state.memory.rows.every((row) => state.memory.checked.has(row.id));
    $("select-page-memories").checked = all; $("select-page-memories").indeterminate = !all && state.memory.rows.some((row) => state.memory.checked.has(row.id));
  }
  async function loadEvidence(record, box, page) {
    const version = state.memory.version, id = state.memory.selected?.id;
    const result = await api("/api/memory/records/" + record.id + "?evidence_page=" + page);
    if (!memoryIsCurrent(version, id) || !box.isConnected) return;
    if (result.conversation_id !== id) throw new Error("证据不属于当前选中会话。");
    const evidence = result.evidence || {}; box.replaceChildren();
    box.append(element("p", "hint", "记录 #" + record.id + " · 版本 " + result.version + " · 更新于 " + dateText(result.updated_at) + (result.expires_at ? " · 到期 " + dateText(result.expires_at) : " · 长期有效")));
    if (!(evidence.items || []).length) box.append(element("p", "hint", "没有可显示的原文证据。人工记录不伪造聊天来源。"));
    for (const item of evidence.items || []) {
      const entry = element("div", "evidence-entry"), source = element("div", "source-container");
      entry.append(element("blockquote", "", item.quote), element("p", "hint", item.source_key));
      entry.append(memoryButton("按需查看关联原文", async () => {
        const value = await api("/api/memory/records/" + record.id + "/source?source_key=" + encodeURIComponent(item.source_key));
        if (!memoryIsCurrent(version, id) || !entry.isConnected) return;
        source.replaceChildren(element("pre", "source-text", value.available ? String(value.text || "") : "原文已过期或撤销，无法查看。"));
        if (value.available) source.prepend(element("p", "hint", "人物 " + value.subject_id + " · " + dateText(value.received_at)));
      }), source); box.append(entry);
    }
    const pagination = element("div", "pagination");
    if (page > 1) pagination.append(memoryButton("← 上一页证据", () => loadEvidence(record, box, page - 1)));
    pagination.append(element("span", "hint", "证据第 " + page + " / " + Math.max(1, evidence.total_pages || 1) + " 页"));
    if (page < evidence.total_pages) pagination.append(memoryButton("下一页证据 →", () => loadEvidence(record, box, page + 1)));
    box.append(pagination); updateControls();
  }
  function requestId() {
    if (crypto.randomUUID) return crypto.randomUUID();
    const bytes = crypto.getRandomValues(new Uint8Array(16)); bytes[6] = (bytes[6] & 15) | 64; bytes[8] = (bytes[8] & 63) | 128;
    return Array.from(bytes, (byte, index) => ([4, 6, 8, 10].includes(index) ? "-" : "") + byte.toString(16).padStart(2, "0")).join("");
  }
  function memoryConflict(error) {
    if (error.status === 409) { state.memory.conflict = true; $("conversation-conflict").hidden = false; updateControls(); }
  }
  async function finishMemory(result, completed) {
    if (typeof completed === "function") completed(result);
    state.memory.settingsDirty = false; state.memory.checked.clear(); state.memory.preview = null;
    await reloadSelected(); await loadMemories(); await loadConversations();
    notice("操作已完成；记忆与上下文已刷新。");
  }
  async function performMemory(path, payload, completed) {
    const version = state.memory.version, id = state.memory.selected?.id;
    const complete = async (result) => { if (memoryIsCurrent(version, id)) await finishMemory(result, completed); };
    let result;
    try { result = await api(path, "POST", payload); }
    catch (error) {
      if (uncertain(error)) { trackOperation({ kind: "memory", label: "记忆操作", requestId: payload.request_id, complete }); return; }
      memoryConflict(error); throw error;
    }
    await complete(result);
  }
  async function mutateMemory(action, completed) {
    const detail = state.memory.detail;
    if (!detail || state.pendingOperation || state.memory.conflict) throw new Error("请先确认未完成操作或重新载入当前会话。");
    await performMemory("/api/memory/mutate", { request_id: requestId(), conversation_id: detail.id, epoch: detail.epoch, memory_revision: detail.memory_revision, action }, completed);
  }
  function openMemoryEditor(record = null) {
    state.memory.editing = record ? clone(record) : {}; state.memory.editorDirty = false;
    $("memory-editor-title").textContent = record ? "编辑记忆" : "新增记忆";
    $("memory-editor-scope").textContent = scopeTitle(state.memory.detail) + " · 人工记录默认锁定，模型不会覆盖。";
    $("edit-memory-subject").value = String(record?.subject_id ?? state.memory.detail.owner_id ?? (state.memory.detail.scene === "friend" ? state.memory.detail.peer_id : ""));
    $("edit-memory-kind").value = record?.kind || "fact"; $("edit-memory-text").value = record?.text || "";
    $("edit-memory-locked").checked = true; $("edit-memory-enabled").checked = record?.active ?? true;
    $("edit-memory-ttl").value = record?.expires_at ? String(Math.max(1, Math.min(365, Math.ceil((record.expires_at - Date.now() / 1000) / 86400)))) : "";
    $("memory-editor-error").textContent = ""; $("memory-editor").showModal(); updateControls();
  }
  async function saveMemory() {
    const record = state.memory.editing, text = $("edit-memory-text").value.trim();
    if (!text || [...text].length > 180) throw new Error("记忆内容需要 1 至 180 个字符。");
    const ttl = $("edit-memory-ttl").value.trim(), days = ttl ? Number(ttl) : null;
    if (days !== null && (!Number.isInteger(days) || days < 1 || days > 365)) throw new Error("有效期应为 1 至 365 天，留空为长期。");
    const action = { op: record?.id ? "edit" : "create", kind: $("edit-memory-kind").value, text, ttl_days: days, active: $("edit-memory-enabled").checked };
    if (record?.id) Object.assign(action, { id: record.id, version: record.version, locked: $("edit-memory-locked").checked });
    else action.subject_id = positiveId($("edit-memory-subject").value, "人物");
    await mutateMemory(action, () => { state.memory.editorDirty = false; state.memory.editing = null; $("memory-editor").close(); });
  }
  async function previewForget(ids) {
    if (!ids.length) return;
    const detail = state.memory.detail, version = state.memory.version;
    try {
      const result = await api("/api/memory/forget/preview", "POST", { conversation_id: detail.id, epoch: detail.epoch, memory_revision: detail.memory_revision, ids });
      if (!memoryIsCurrent(version, detail.id)) return;
      state.memory.preview = result; $("forget-error").textContent = "";
      const box = $("forget-preview"); box.replaceChildren();
      box.append(element("p", "memory-text", "选择 " + result.requested_count + " 条 · 实际影响 " + result.affected_count + " 条记忆 · " + result.source_count + " 个来源"));
      box.append(element("p", "hint", "预览有效期至 " + dateText(result.expires_at)));
      box.append(element("p", "hint", "影响范围（机器人:类型:会话）："));
      const scopes = element("ul", "scope-preview"); for (const scope of result.scopes || []) scopes.append(element("li", "", scope)); box.append(scopes);
      box.append(element("p", "hint", "包含关联来源派生的其他记忆；确认会撤销这些来源并重置受影响上下文。"));
      $("forget-dialog").showModal(); updateControls();
    } catch (error) { memoryConflict(error); throw error; }
  }
  async function confirmForget() {
    const preview = state.memory.preview; if (!preview) return;
    if (preview.expires_at * 1000 <= Date.now()) throw new Error("预览已过期，请关闭并重新生成预览。");
    await performMemory("/api/memory/forget/confirm", { request_id: requestId(), preview_id: preview.preview_id }, () => { $("forget-dialog").close(); });
  }


  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault(); if (state.busy) return;
    $("login-error").textContent = ""; $("login-button").disabled = true; state.busy = true;
    try {
      const password = $("admin-password").value;
      if (state.setupRequired && ([...password].length < 12 || password !== $("setup-confirm").value)) throw new Error("密码至少 12 个字符，且两次输入需一致。");
      const result = await api(state.setupRequired ? "/api/auth/setup" : "/api/auth/login", "POST", state.setupRequired ? { code: $("setup-code").value.trim(), password } : { password });
      await enterSession(result);
    } catch (error) {
      if (error.code === "setup_complete") { authView({ setup_required: false, authenticated: false }); }
      $("login-error").textContent = error.message;
    } finally { state.busy = false; $("login-button").disabled = false; updateControls(); }
  });
  $("logout").addEventListener("click", () => {
    if ((hasEdits() || state.pendingOperation) && !window.confirm("有未保存编辑或待查询的操作。确定退出并清空本页内容？")) return;
    void runAction(async () => {
      try { await api("/api/auth/logout", "POST", {}); }
      catch (error) { if (error.code !== "auth_required") throw error; }
      clearSession("已退出，登录会话已撤销。");
    });
  });
  $("open-security").addEventListener("click", () => { $("security-error").textContent = ""; $("password-change-form").reset(); $("security-dialog").showModal(); });
  $("open-reauth").addEventListener("click", () => { $("security-dialog").close(); $("reauth-error").textContent = ""; $("reauth-dialog").showModal(); });
  $("password-change-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void runAction(async () => {
      try {
        const password = $("new-password").value;
        if ([...password].length < 12 || password !== $("confirm-password").value) throw new Error("新密码至少 12 个字符，且两次输入需一致。");
        authView(await api("/api/auth/reauth", "POST", { password: $("current-password").value }));
        authView(await api("/api/auth/password", "POST", { new_password: password }));
        $("password-change-form").reset(); $("security-dialog").close(); notice("密码已修改，其他旧会话已撤销。");
      } catch (error) { $("security-error").textContent = uncertain(error) ? "响应未确认。请使用新密码重新登录核对，不要重复提交修改。" : error.message; throw error; }
    });
  });
  $("reauth-form").addEventListener("submit", (event) => {
    event.preventDefault();
    void runAction(async () => {
      try { authView(await api("/api/auth/reauth", "POST", { password: $("reauth-password").value })); $("reauth-password").value = ""; $("reauth-dialog").close(); notice("验证成功，请返回继续刚才的操作。"); }
      catch (error) { $("reauth-error").textContent = error.message; throw error; }
    });
  });
  function closeDialog(id) {
    if (state.busy) return;
    if (id === "memory-editor" && (state.pendingOperation?.kind === "memory" || (state.memory.editorDirty && !window.confirm("确定放弃尚未保存的记忆编辑？")))) return;
    if (id === "memory-editor") { state.memory.editorDirty = false; state.memory.editing = null; }
    if (id === "security-dialog") $("password-change-form").reset();
    if (id === "reauth-dialog") $("reauth-form").reset();
    $(id).close(); updateControls();
  }
  document.querySelectorAll("[data-close]").forEach((button) => button.addEventListener("click", () => closeDialog(button.dataset.close)));
  document.querySelectorAll("dialog").forEach((dialog) => dialog.addEventListener("cancel", (event) => { event.preventDefault(); closeDialog(dialog.id); }));
  $("dismiss-notice").addEventListener("click", () => { $("notice").hidden = true; });
  $("check-operation").addEventListener("click", () => { void runAction(checkOperation); });
  document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => showView(button.dataset.view)));
  document.querySelector(".brand").addEventListener("click", (event) => { event.preventDefault(); showView("overview"); });
  $("config-form").addEventListener("submit", (event) => event.preventDefault());
  $("refresh-status").addEventListener("click", () => { void runAction(() => refreshStatus(true)); });
  $("refresh-qq").addEventListener("click", () => { void runAction(() => refreshQq(true)); });
  $("refresh-contacts").addEventListener("click", () => { void loadContacts(true); });
  for (const [id, scene] of [["contacts-groups", "group"], ["contacts-friends", "friend"]]) $(id).addEventListener("click", () => { state.contacts.scene = scene; state.contacts.page = 1; if (state.contacts.phase === "ready") state.contacts.message = ""; renderContacts(); });
  $("contacts-search").addEventListener("input", () => { state.contacts.query = $("contacts-search").value; state.contacts.page = 1; if (state.contacts.phase === "ready") state.contacts.message = ""; renderContacts(); });
  for (const [id, direction] of [["contacts-prev", -1], ["contacts-next", 1]]) $(id).addEventListener("click", () => { state.contacts.page += direction; renderContacts(); });
  $("refresh-budget").addEventListener("click", () => { void runAction(loadBudget); });
  for (const id of ["connect-runtime", "apply-runtime"]) $(id).addEventListener("click", () => {
    if ((state.dirty || state.jsonDirty) && !window.confirm("本页有未保存配置。继续使用服务器已保存的配置连接？")) return;
    void runAction(() => applyRuntime());
  });
  $("stop-runtime").addEventListener("click", () => { void runAction(() => applyRuntime(true)); });
  $("reload-config").addEventListener("click", () => {
    if (state.pendingOperation) { notice("请先查询待确认操作。", "warning"); return; }
    if ((state.dirty || state.jsonDirty) && !window.confirm("重新载入将丢弃本页未保存的配置。确定继续？")) return;
    void runAction(loadConfig);
  });
  $("save-config").addEventListener("click", () => { void runAction(() => saveConfig(false)); });
  $("save-apply").addEventListener("click", () => { void runAction(() => saveConfig(true)); });
  $("get-qr").addEventListener("click", () => { void runAction(getQr); });
  $("advanced").addEventListener("toggle", () => { if ($("advanced").open && !state.jsonDirty && state.config) syncJson(); });
  $("config-json").addEventListener("input", () => { state.jsonDirty = true; state.dirty = true; $("json-state").textContent = "JSON 有未采用修改，分组表单已暂时锁定。"; updateControls(); });
  $("adopt-json").addEventListener("click", () => { try { adoptJson(); notice("JSON 已采用到分组表单，尚未保存。"); } catch (error) { report(error); } });
  $("discard-json").addEventListener("click", () => {
    if (state.jsonDirty && !window.confirm("放弃完整 JSON 中尚未采用的编辑？")) return;
    state.jsonDirty = false; syncJson();
  });
  $("conversation-filter").addEventListener("submit", (event) => {
    event.preventDefault(); void runAction(async () => { if (!canDiscardMemory()) return; clearMemorySelection(); state.memory.page = 1; await loadConversations(); });
  });
  $("refresh-conversations").addEventListener("click", () => { void runAction(loadConversations); });
  for (const [id, direction] of [["conversations-prev", -1], ["conversations-next", 1]]) $(id).addEventListener("click", () => { void runAction(async () => { state.memory.page += direction; await loadConversations(); }); });
  $("reload-conversation").addEventListener("click", () => {
    if (!canDiscardMemory()) return;
    void runAction(async () => { const current = state.memory.selected; if (current) await selectConversation(current); });
  });
  $("conversation-settings-form").addEventListener("submit", (event) => { event.preventDefault(); void runAction(() => mutateMemory({ op: "settings", settings: readSettings() })); });
  $("pause-conversation").addEventListener("click", () => {
    void runAction(() => mutateMemory({ op: "settings", settings: { ...state.memory.detail.settings, paused: !state.memory.detail.settings.paused, once: false } }));
  });
  $("once-conversation").addEventListener("click", () => {
    void runAction(() => mutateMemory({ op: "settings", settings: { ...state.memory.detail.settings, paused: true, once: true } }));
  });
  $("reset-conversation").addEventListener("click", () => {
    if (window.confirm("确定重置「" + scopeTitle(state.memory.detail) + "」的短期上下文？未完成回答会失效，长期记忆保留。")) void runAction(() => mutateMemory({ op: "reset" }));
  });
  $("change-conversation-mode").addEventListener("click", () => {
    const mode = $("conversation-mode").value;
    if (mode === (state.memory.detail.scope_mode || state.memory.detail.mode)) { notice("当前范围已经使用该模式。"); return; }
    if (window.confirm("切换会话模式会开启新的隔离范围，旧上下文不会混入。新会话会在收到下一条聊天消息后出现。确定继续？")) void runAction(() => mutateMemory({ op: "mode", mode }));
  });
  $("memory-filter").addEventListener("submit", (event) => { event.preventDefault(); void runAction(async () => { state.memory.pageMemories = 1; state.memory.checked.clear(); await loadMemories(); }); });
  for (const [id, direction] of [["memories-prev", -1], ["memories-next", 1]]) $(id).addEventListener("click", () => { void runAction(async () => { state.memory.pageMemories += direction; await loadMemories(); }); });
  $("new-memory").addEventListener("click", () => { if (!state.busy) openMemoryEditor(); });
  $("select-page-memories").addEventListener("change", () => {
    for (const row of state.memory.rows) { if ($("select-page-memories").checked) state.memory.checked.add(row.id); else state.memory.checked.delete(row.id); }
    document.querySelectorAll("[data-memory-id]").forEach((node) => { node.checked = state.memory.checked.has(Number(node.dataset.memoryId)); }); updateSelection(); updateControls();
  });
  $("forget-selected").addEventListener("click", () => { void runAction(() => previewForget([...state.memory.checked])); });
  $("memory-editor-form").addEventListener("input", () => { state.memory.editorDirty = true; });
  $("memory-editor-form").addEventListener("submit", (event) => {
    event.preventDefault(); void runAction(async () => { try { await saveMemory(); } catch (error) { $("memory-editor-error").textContent = error.status === 409 ? "记录已变化。编辑仍保留，请复制后关闭并重新载入会话。" : error.message; throw error; } });
  });
  $("confirm-forget").addEventListener("click", () => { void runAction(async () => { try { await confirmForget(); } catch (error) { $("forget-error").textContent = error.message; throw error; } }); });
  $("context-details").addEventListener("toggle", () => { if ($("context-details").open) void runAction(loadContext); });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { stopTimers(); $("poll-state").textContent = "页面隐藏，已暂停轮询"; }
    else if (state.authenticated) { $("poll-state").textContent = "自动同步"; void refreshStatus(); void refreshQq(); }
  });
  window.addEventListener("beforeunload", (event) => { if (hasEdits() || state.pendingOperation) { event.preventDefault(); event.returnValue = ""; } });
  window.addEventListener("pagehide", stopTimers);
  window.addEventListener("pageshow", (event) => { if (event.persisted) { clearSession(); void bootstrap(); } });
  void bootstrap();
})();
