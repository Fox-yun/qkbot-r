(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    token: "", session: 0, config: null, draft: null, revision: null,
    secrets: {}, status: null, qq: null, busy: false, dirty: false,
    jsonDirty: false, formInvalid: false, stale: false, poll: null,
    polling: false, qqPollingPaused: false, qqQueryVersion: 0, qqError: "", qrExpires: null, view: "overview",
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
    { title: "会话与触发", description: "仅处理白名单中的会话。真实 @机器人、私聊或关键词可触发聊天。", fields: [
      field("access.groups", "允许的群号", "ids", "每行一个，也可使用逗号或空格分隔。空列表不允许群聊。"),
      field("access.friends", "允许的私聊 QQ 号", "ids", "每行一个。空列表不允许私聊。"),
      field("access.admins", "管理员 QQ 号", "ids", "管理员可以修改群共享会话的设置。"),
      field("plugins.ai.keywords", "关键词触发", "lines", "每行一个；子串匹配。清空后仍可通过真实 @或私聊触发。"),
      field("plugins.ai.defaults.mode", "新群默认会话模式", "select", "已有群请使用 /ai mode 切换，避免复用旧会话。", { choices: [["group", "群共享上下文"], ["user", "群内按用户隔离"]] }),
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
      field("plugins.ai.defaults.paused", "默认暂停自动聊天", "check", "可使用 /ai resume 恢复，或 /ai once 放行一次。"),
    ] },
    { title: "插件", description: "启用需要的功能。写入类工具默认要求用户确认。", fields: [
      field("plugins.ai.enabled", "AI 聊天", "check", "启用后需配置有效的模型地址和名称。"),
      field("plugins.clock.enabled", "时间查询", "check", "提供 /clock 指令和 clock_now 工具。"),
      field("plugins.reminder.enabled", "提醒", "check", "提供提醒创建、查询和取消功能。"),
      field("plugins.reminder.llm_writes_require_confirmation", "模型创建提醒需要确认", "check", "写入操作先展示预览，再由用户明确确认。"),
      field("plugins.reminder.max_active_per_user", "每人最多活跃提醒", "number", "", { min: 1 }),
    ] },
    { title: "容量与整理", description: "并发、队列和后台整理均有边界。可先保留默认值，再按使用情况调整。", fields: [
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
    { title: "程序设置", description: "剩余低频参数可在完整 JSON 中编辑。管理端监听地址和令牌只能在本机配置文件修改。", fields: [
      field("app.database", "SQLite 数据库路径", "text", "修改路径会使用该路径对应的数据库。"),
      field("app.timezone", "时区", "text", "例如 Asia/Shanghai。"),
      field("app.log", "日志过滤级别", "text", "例如 info 或 qkbot=debug。修改后需要重启整个小鱼程序；应用机器人配置不会重载日志过滤器。"),
    ] },
  ];

  class ApiError extends Error {
    constructor(message, status = 0) { super(message); this.status = status; }
  }

  async function api(path, method = "GET", body) {
    const session = state.session;
    const headers = { Accept: "application/json", Authorization: `Bearer ${state.token}` };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    let response;
    try {
      response = await fetch(path, {
        method, headers, body: body === undefined ? undefined : JSON.stringify(body),
        credentials: "omit", cache: "no-store", redirect: "error",
        signal: AbortSignal.timeout(20000),
      });
    } catch (error) {
      throw new ApiError(error.name === "TimeoutError" ? "请求超时，请检查管理服务是否仍在运行。" : "无法连接管理服务，请检查服务状态和网络。");
    }
    if (session !== state.session) throw new ApiError("页面登录状态已变化。", -1);
    if (response.status === 401) {
      logout("管理令牌无效或已失效，请重新登录。");
      throw new ApiError("管理令牌无效或已失效，请重新登录。", 401);
    }
    const text = await response.text();
    if (session !== state.session) throw new ApiError("页面登录状态已变化。", -1);
    let result = {};
    try { if (text) result = JSON.parse(text); } catch {
      throw new ApiError(`服务返回了无法读取的数据（HTTP ${response.status}）。`, response.status);
    }
    if (!response.ok) throw new ApiError(typeof result.error === "string" ? result.error : `请求失败（HTTP ${response.status}）。`, response.status);
    return result;
  }

  function notice(message, kind = "success") {
    $("notice").className = `notice ${kind}`;
    $("notice-text").textContent = message;
    $("notice").hidden = false;
  }

  function report(error) {
    if (error.status === 401 || error.status === -1) return;
    if (error.status === 409) {
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
    $("web-readonly").textContent = `管理服务：${String(web.listen || "未配置")} · web 分组与管理令牌只读，请在本机配置文件中修改并重启管理服务。${state.secrets.admin_token?.env_set ? " 管理令牌当前由环境变量提供。" : ""}`;
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
    for (const path of ["llm.api_key", "milky.access_token", "qq_login.token", "web.token"]) {
      const value = get(config, path);
      if (value != null && value !== "") throw new Error(`请勿在 JSON 的 ${path} 中填入密钥；使用对应的密钥控件设置。管理令牌不能在线修改。`);
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
    const unavailable = state.busy || !state.config;
    document.querySelectorAll("[data-action]").forEach((button) => { button.disabled = unavailable; });
    $("config-fields").disabled = unavailable || state.jsonDirty;
    $("config-json").disabled = unavailable || state.formInvalid;
    $("save-config").disabled = unavailable || state.stale;
    $("save-apply").disabled = unavailable || state.stale;
    $("adopt-json").disabled = unavailable || !state.jsonDirty;
    $("discard-json").disabled = unavailable || !state.jsonDirty;
    $("dirty-label").textContent = state.jsonDirty ? "JSON 有未采用的修改" : state.dirty ? "有未保存的修改" : "配置已同步";
    const phase = state.status?.phase;
    const transitional = phase === "starting" || phase === "stopping";
    $("connect-runtime").hidden = !["stopped", "error"].includes(phase);
    $("apply-runtime").hidden = ["stopped", "error"].includes(phase);
    $("connect-runtime").disabled = unavailable || transitional || !phase;
    $("apply-runtime").disabled = unavailable || transitional || !phase;
    $("stop-runtime").disabled = unavailable || phase === "stopped" || phase === "stopping" || !phase;
    $("get-qr").disabled = unavailable || !state.config?.qq_login?.enabled || !state.secrets.llbot_token?.effective_set || state.qq?.online === true;
  }

  async function loadConfig() {
    const result = await api("/api/config");
    if (!result.config || typeof result.revision !== "string") throw new Error("配置响应缺少 config 或 revision。");
    state.config = clone(result.config); state.draft = clone(result.config);
    state.revision = result.revision; state.secrets = result.secrets || {};
    state.dirty = false; state.jsonDirty = false; state.formInvalid = false; state.stale = false;
    resumeQqPolling();
    $("config-warning").hidden = true;
    $("saved-revision").textContent = `已保存版本：${state.revision}`;
    renderFields(); syncJson(); renderStatus();
  }

  function badge(id, text, kind) { $(id).textContent = text; $(id).className = `badge ${kind}`; }
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
    const [label, title, kind] = phases[status.phase] || ["未知状态", "等待状态更新", "neutral"];
    badge("runtime-badge", label, kind); badge("header-status", label, kind);
    $("runtime-title").textContent = title;
    $("runtime-message").textContent = typeof status.message === "string" ? status.message : "";
    $("bot-id").textContent = status.bot_id == null ? "—" : String(status.bot_id);
    $("started-at").textContent = dateText(status.started_at);
    const matches = state.revision && status.applied_revision === state.revision;
    $("revision-state").textContent = !status.applied_revision ? "尚未应用" : matches ? "已应用最新配置" : "有待应用配置";
    $("revision-detail").textContent = status.applied_revision ? `运行版本：${status.applied_revision}` : "保存后点击连接或应用配置";
    updateControls();
  }

  function renderQq() {
    const qq = state.qq;
    if (!state.config?.qq_login?.enabled) {
      badge("qq-badge", "适配已关闭", "neutral");
      $("qq-detail").textContent = "请先在配置管理中启用 QQ 登录适配并保存。";
      $("account-name").textContent = state.status?.online ? "机器人已在线" : "登录适配未启用";
      $("account-id").textContent = state.status?.bot_id ? `QQ ${state.status.bot_id}` : "可使用外部客户端完成登录";
    } else if (!state.secrets.llbot_token?.effective_set) {
      badge("qq-badge", "未配置凭据", "wait");
      $("qq-detail").textContent = "请在配置管理中设置 LLBot WebUI 密码 / 令牌并保存，再获取二维码。";
      $("account-name").textContent = state.status?.online ? "机器人已在线" : "等待配置登录凭据";
      $("account-id").textContent = state.status?.bot_id ? `QQ ${state.status.bot_id}` : "自动登录状态查询尚未启用";
    } else if (state.qqPollingPaused) {
      badge("qq-badge", "登录查询已暂停", "bad");
      $("qq-detail").textContent = `${state.qqError || "无法连接 QQ 管理端。"} 自动查询已暂停，请检查地址与 WebUI 密码，手动刷新状态、获取二维码或保存配置后重试。`;
      $("account-name").textContent = "QQ 状态暂不可用";
      $("account-id").textContent = "已暂停重复请求，避免触发上游登录保护";
    } else if (qq?.online) {
      badge("qq-badge", "QQ 已登录", "good");
      $("account-name").textContent = qq.nick || "QQ 已登录";
      $("account-id").textContent = qq.uin ? `QQ ${qq.uin}` : "账号在线";
      $("qq-detail").textContent = `${qq.nick || "QQ"}${qq.uin ? `（${qq.uin}）` : ""} 已登录。等待中的机器人会自动连接。`;
      clearQr();
      $("qr-placeholder").replaceChildren(element("span", "", "✓"), element("strong", "", "QQ 已登录"), element("p", "", "返回运行概览即可检查机器人连接。"));
      $("qr-expiry").textContent = "账号已在线，无需重新获取二维码。";
    } else {
      badge("qq-badge", "QQ 未登录", "wait");
      $("account-name").textContent = "尚未登录";
      $("account-id").textContent = "前往 QQ 登录页扫码";
      $("qq-detail").textContent = "登录状态每 5 秒自动同步。扫码成功后无需反复重启机器人。";
      if ($("qr-image").hidden) $("qr-placeholder").replaceChildren(element("span", "", "▦"), element("strong", "", "等待获取二维码"), element("p", "", "点击下方按钮，用手机 QQ 扫码登录。"));
      updateQrExpiry();
    }
    updateControls();
  }

  function clearQr() {
    $("qr-image").removeAttribute("src"); $("qr-image").hidden = true;
    $("qr-placeholder").hidden = false; state.qrExpires = null;
    $("get-qr").textContent = "获取登录二维码";
  }

  function updateQrExpiry() {
    if (state.qrExpires == null) {
      $("qr-expiry").textContent = "二维码不会自动刷新；失效后请手动获取新的二维码。";
      return;
    }
    const expires = state.qrExpires > 1e12 ? state.qrExpires : state.qrExpires * 1000;
    $("qr-expiry").textContent = expires <= Date.now() ? "二维码已过期，请点击「刷新二维码」。" : `二维码有效期至 ${dateText(state.qrExpires)}；不会自动刷新。`;
  }

  function resumeQqPolling() {
    state.qqQueryVersion += 1;
    state.qqPollingPaused = false;
    state.qqError = "";
  }

  async function refreshStatus(manual = false) {
    if (manual) resumeQqPolling();
    if (!state.token || state.polling) return;
    state.polling = true;
    const session = state.session;
    try {
      const requests = [api("/api/status")];
      const queryQq = Boolean(state.config?.qq_login?.enabled && state.secrets.llbot_token?.effective_set && !state.qqPollingPaused);
      const qqVersion = state.qqQueryVersion;
      if (queryQq) requests.push(api("/api/qq/status"));
      const results = await Promise.allSettled(requests);
      if (session !== state.session) return;
      if (results[0].status === "fulfilled") { state.status = results[0].value; renderStatus(); }
      const currentQq = queryQq && qqVersion === state.qqQueryVersion;
      if (currentQq && results[1].status === "fulfilled") state.qq = results[1].value;
      const failed = results[0].status === "rejected" ? results[0] : currentQq && results[1].status === "rejected" ? results[1] : null;
      $("poll-state").textContent = failed ? "状态同步失败" : `已同步 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`;
      if (currentQq && results[1].status === "rejected") {
        state.qqPollingPaused = true; state.qq = null;
        state.qqError = results[1].reason.message || "无法查询 QQ 登录状态。";
      }
      renderQq();
      if (failed && manual) report(failed.reason);
      else if (manual) notice("状态已刷新。");
      if (!state.qq?.online) updateQrExpiry();
    } finally { if (session === state.session) state.polling = false; }
  }

  async function runAction(action) {
    if (state.busy) return;
    state.busy = true; updateControls();
    const session = state.session;
    try { await action(); } catch (error) { report(error); }
    finally { if (session === state.session) { state.busy = false; updateControls(); } }
  }

  async function applyRuntime() {
    await api("/api/runtime/apply", "POST", {});
    notice("连接任务已提交。QQ 尚未登录时，请前往 QQ 登录页扫码，机器人会自动等待连接。");
    await refreshStatus();
  }

  async function saveConfig(apply) {
    if (state.stale) throw new Error("请先重新载入最新配置版本，再保存。");
    if (state.jsonDirty) adoptJson();
    const config = readForm();
    const logChanged = config.app.log !== state.config.app.log;
    const logHint = logChanged ? " 日志级别已修改，需要重启整个小鱼程序后生效。" : "";
    const secrets = secretChanges();
    try { await api("/api/config", "PUT", { revision: state.revision, config, secrets }); } catch (error) {
      if (error.status === 0 || error.status >= 500) {
        state.stale = true;
        $("config-warning").textContent = "保存结果尚未确认，本页编辑仍保留。请先复制需要保留的内容，再重新载入核实服务端配置；不会自动重复保存。";
        $("config-warning").hidden = false;
      }
      throw error;
    }
    // Fetch the normalized saved version; never reuse an optimistic or stale revision.
    try { await loadConfig(); } catch (error) {
      if (error.status === 401 || error.status === -1) throw error;
      state.stale = true;
      $("config-warning").textContent = "配置已保存，但读取新版本失败。请重新载入后再继续编辑或应用。";
      $("config-warning").hidden = false;
      throw new Error(`配置已保存；尚未应用。${error.message}${logHint}`);
    }
    if (apply) {
      try {
        await applyRuntime();
        if (logChanged) notice(`配置已保存，连接任务已提交。${logHint.trim()}`);
      } catch (error) {
        if (error.status === 401 || error.status === -1) throw error;
        throw new Error(`配置已保存，但应用未完成：${error.message}${logHint}`);
      }
    } else notice(`配置已保存。运行中的机器人保持当前配置，点击「应用已保存配置」后生效。${logHint}`);
  }

  function showView(view) {
    if (!["overview", "qq", "config"].includes(view)) return;
    state.view = view;
    for (const name of ["overview", "qq", "config"]) $(`view-${name}`).hidden = name !== view;
    document.querySelectorAll(".nav-item").forEach((button) => {
      const active = button.dataset.view === view;
      button.classList.toggle("active", active);
      if (active) button.setAttribute("aria-current", "page"); else button.removeAttribute("aria-current");
    });
    window.scrollTo({ top: 0, behavior: "instant" });
  }

  function logout(message = "") {
    clearInterval(state.poll); state.poll = null; state.token = ""; state.session += 1;
    state.config = null; state.draft = null; state.revision = null; state.secrets = {};
    state.status = null; state.qq = null; state.busy = false; state.polling = false;
    state.qqPollingPaused = false; state.qqError = "";
    state.dirty = false; state.jsonDirty = false; state.formInvalid = false; state.stale = false;
    inputs.clear(); secretInputs.clear(); $("config-groups").replaceChildren();
    $("config-json").value = ""; $("admin-token").value = ""; clearQr();
    $("app").hidden = true; $("login-screen").hidden = false;
    $("login-error").textContent = message; $("login-button").disabled = false;
    $("notice").hidden = true; $("admin-token").focus();
  }

  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if ($("login-button").disabled) return;
    const token = $("admin-token").value.trim();
    if (token.length < 32) { $("login-error").textContent = "管理令牌至少需要 32 个字符。"; return; }
    $("login-button").disabled = true; $("login-error").textContent = "";
    state.token = token; state.session += 1;
    const session = state.session;
    try {
      await loadConfig();
      $("admin-token").value = ""; $("login-screen").hidden = true; $("app").hidden = false;
      showView("overview");
      await refreshStatus();
      if (session !== state.session) return;
      state.poll = setInterval(() => { if (!state.busy) void refreshStatus(); }, 5000);
    } catch (error) { logout(error.message || "登录失败。"); }
    finally { $("login-button").disabled = false; }
  });
  $("logout").addEventListener("click", () => {
    if ((state.dirty || state.jsonDirty) && !window.confirm("有未保存的配置修改。退出会丢弃这些修改，确定退出吗？")) return;
    logout();
  });
  $("dismiss-notice").addEventListener("click", () => { $("notice").hidden = true; });
  document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => showView(button.dataset.view)));
  document.querySelector(".brand").addEventListener("click", (event) => { event.preventDefault(); showView("overview"); });
  $("config-form").addEventListener("submit", (event) => event.preventDefault());
  for (const id of ["refresh-status", "refresh-qq"]) $(id).addEventListener("click", () => { void runAction(() => refreshStatus(true)); });
  for (const id of ["connect-runtime", "apply-runtime"]) $(id).addEventListener("click", () => {
    void runAction(async () => {
      if (state.dirty || state.jsonDirty || state.stale) throw new Error("本页有未保存或过期的配置，请先「保存并应用」，或重新载入已保存版本后再连接。");
      await applyRuntime();
    });
  });
  $("stop-runtime").addEventListener("click", () => {
    void runAction(async () => {
      await api("/api/runtime/stop", "POST", {});
      notice("断开任务已提交。管理台保持可用，QQ 登录状态不受影响。");
      await refreshStatus();
    });
  });
  $("reload-config").addEventListener("click", () => {
    if ((state.dirty || state.jsonDirty) && !window.confirm("重新载入会丢弃本页尚未保存的编辑。确定继续吗？")) return;
    void runAction(async () => { await loadConfig(); notice("已载入最新保存的配置。"); await refreshStatus(); });
  });
  $("save-config").addEventListener("click", () => { void runAction(() => saveConfig(false)); });
  $("save-apply").addEventListener("click", () => { void runAction(() => saveConfig(true)); });
  $("get-qr").addEventListener("click", () => {
    void runAction(async () => {
      resumeQqPolling();
      let result;
      try { result = await api("/api/qq/qrcode", "POST", {}); } catch (error) {
        if (error.status !== 401 && error.status !== -1) {
          state.qqPollingPaused = true; state.qqError = error.message; state.qq = null; renderQq();
        }
        throw error;
      }
      if (typeof result.image !== "string" || result.image.length > 4 * 1024 * 1024 || !/^data:image\/png;base64,[A-Za-z0-9+/]+={0,2}$/u.test(result.image)) throw new Error("管理服务未返回有效的 PNG 二维码。");
      $("qr-image").src = result.image; $("qr-image").hidden = false; $("qr-placeholder").hidden = true;
      state.qrExpires = Number.isFinite(result.expires_at) ? result.expires_at : null;
      $("get-qr").textContent = "刷新二维码"; updateQrExpiry();
      notice("二维码已获取，请用手机 QQ 扫码。页面会自动查询登录状态，不会自动刷新二维码。");
    });
  });
  $("advanced").addEventListener("toggle", () => {
    if ($("advanced").open && state.config && !state.jsonDirty) syncJson();
  });
  $("config-json").addEventListener("input", () => {
    state.jsonDirty = true; state.dirty = true;
    $("json-state").textContent = "JSON 已修改，分组表单暂时锁定。点击「采用 JSON 到表单」或保存会采用此处内容；关闭折叠区不会丢弃编辑。";
    updateControls();
  });
  $("adopt-json").addEventListener("click", () => { try { adoptJson(); notice("JSON 已采用到分组表单，尚未保存。"); } catch (error) { report(error); } });
  $("discard-json").addEventListener("click", () => {
    if (!window.confirm("确定放弃尚未采用的 JSON 编辑，并恢复分组表单中的内容吗？")) return;
    state.jsonDirty = false; syncJson();
  });
  window.addEventListener("beforeunload", (event) => {
    if (state.dirty || state.jsonDirty) { event.preventDefault(); event.returnValue = ""; }
  });
  // Clear volatile credentials before the browser can keep this page in its back/forward cache.
  window.addEventListener("pagehide", () => logout());
})();
