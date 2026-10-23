(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    authenticated: false, csrf: "", setupRequired: false, session: 0, config: null, draft: null, revision: null,
    secrets: {}, status: null, qq: null, busy: false, dirty: false, applyError: null, applyFailure: null,
    jsonDirty: false, formInvalid: false, stale: false,
    configSave: { phase: "", message: "", kind: "" },
    runtimeTimer: null, qqTimer: null, runtimePolling: false, qqPolling: false,
    qqPollingPaused: false, qqQueryVersion: 0, qqFailures: 0, qqInterval: 5000, qqError: "", qrExpires: null, view: "overview",
    memory: { page: 1, totalPages: 1, list: [], selected: null, version: 0, detail: null, pageMemories: 1, totalMemoryPages: 1, rows: [], checked: new Set(), settingsDirty: false, editorDirty: false, editing: null, preview: null, conflict: false },
    pendingOperation: null,
    personaTemplates: {}, personaSelected: null, toolCatalog: [], pluginCatalog: [], autoReplyBaseEstimate: null,
    groupPersona: { saving: false, message: "", kind: "" },
    scopePlugins: { version: 0, query: null, rows: [], loading: false, saving: false, uncertain: false },
    models: { version: 0, loading: false, items: [], manual: true, message: "", kind: "" },
    embedding: { version: 0, loading: false, message: "", kind: "" },
    search: { version: 0, loading: false, message: "", kind: "" },
    stickers: { version: 0, groupRequest: 0, itemRequest: 0, groupsLoading: false, loading: false, groups: [], groupPage: 1, totalGroupPages: 1, scope: "", rows: [], page: 1, totalPages: 1, total: 0, index: null, running: false, message: "", kind: "", operation: null, preview: null },
    contacts: { version: 0, loading: false, saving: null, phase: "idle", message: "", botId: "", generation: null, groups: [], friends: [], fetchedAt: null, scene: "group", query: "", page: 1, totalPages: 1 },
    learning: { version: 0, loading: false, saving: false, snapshot: null, editing: null, dirty: false, conflict: false, message: "" },
    context: { version: 0, loading: false, snapshot: null, message: "" },
    configCategory: "all",
  };
  const inputs = new Map();
  const secretInputs = new Map();
  let pluginControls = null;
  let embeddingControls = null;
  let searchControls = null;
  let autoReplyBudgetControl = null;
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
  const autoReplyFields = [
    field("plugins.ai.auto_reply.prompt", "接话策略", "textarea", "告诉模型什么情况下适合接话、何时保持安静。沿用当前聊天模型、人设和上下文；最多 2000 个字符，不能为空。策略计入自动接话的输入预算，较长时请相应提高输入预算；正常 @ 提问不加载这段策略。", { full: true, rows: 6 }),
    field("plugins.ai.auto_reply.cooldown_seconds", "主动发言冷却（秒）", "number", "每个已开启聊天独立计时；设为 0 时不限制主动发言间隔。", { min: 0, max: 3600 }),
    field("plugins.ai.auto_reply.debounce_ms", "消息合并等待（毫秒）", "number", "收到新消息后等待这段时间，将短时间内的连续发言合并判断。1000 毫秒 = 1 秒。", { min: 100, max: 10000 }),
    field("plugins.ai.auto_reply.max_wait_ms", "最长合并等待（毫秒）", "number", "持续有新消息时，最迟在这段时间后判断是否接话；不能短于消息合并等待。", { min: 100, max: 30000 }),
    field("plugins.ai.auto_reply.max_messages", "每批最多新消息数", "number", "单批最多合并这么多条新消息，后续消息进入下一批。", { min: 1, max: 32 }),
    field("plugins.ai.auto_reply.max_age_seconds", "待接话消息有效期（秒）", "number", "超过有效期的批次不再主动回复；应覆盖最长合并等待。只限制待接话的新消息，不截断完整聊天上下文、摘要或长期记忆。", { min: 5, max: 600, full: true }),
  ];
  const learningFields = [
    field("plugins.ai.learning.enabled", "启用学习笔记", "check", "默认关闭。开启后仅从同一机器人账号的群聊取材，在群聊间共享交流经验；保存并应用后生效。", { full: true }),
    field("plugins.ai.learning.semantic_recall", "启用笔记向量召回", "check", "默认开启，仅在学习笔记和向量服务同时启用时发出请求。按向量与文字相关性选取笔记；关闭、无索引或服务不可用时回退文字和最近笔记。笔记向量请求独立限额为每小时 120 次。", { full: true }),
    field("plugins.ai.learning.recall_limit", "每轮最多召回笔记数", "number", "默认 8 条，范围 1–128。向量、文字匹配和最近笔记回退共用此上限，只加入独立预算内的完整条目；锁定只保护编辑，不保证每轮注入。", { min: 1, max: 128, full: true }),
    field("plugins.ai.learning.prompt", "管理员学习规则", "textarea", "规定可以学习的交流经验和常用术语，最多 4000 个字符，可留空。群聊内容只作为素材，不能修改管理员规则。", { full: true, rows: 5, optional: true }),
    field("plugins.ai.learning.min_interval_seconds", "最短学习间隔（秒）", "number", "每个机器人账号的后台学习间隔，默认 900 秒。", { min: 60, max: 86400 }),
    field("plugins.ai.learning.max_batch_messages", "每批最多素材消息数", "number", "只读取符合条件的群聊消息。", { min: 1, max: 100 }),
    field("plugins.ai.learning.max_changes_per_run", "每轮最多笔记变更数", "number", "限制一次自动学习可新增、修改或删除的笔记数量。", { min: 1, max: 20 }),
    field("plugins.ai.learning.max_note_chars", "整份笔记字符上限", "number", "当前账号全部有效笔记的总字符上限，人工编辑和自动学习共用。", { min: 100, max: 10000 }),
    field("plugins.ai.learning.injection_budget", "笔记输入额度（计入最终总上限）", "number", "默认 12000。只在开启记忆读取的群聊中提供笔记；普通聊天输入预算加上这份独立额外预算构成最终总上限，不占原有问题和历史空间。设为 0 暂停注入，后台仍可学习。", { min: 0, max: 64000, full: true }),
    field("limits.learning_requests_per_hour", "每小时学习请求上限", "number", "第三个独立额度池，与前台聊天、后台个人记忆整理分别计数。", { min: 1, max: 1000 }),
    field("limits.learning_requests_per_day", "每 24 小时学习请求上限", "number", "滚动 24 小时统计，不能低于每小时上限。", { min: 1, max: 10000 }),
    field("limits.learning_max_input_budget", "学习输入预算（估算）", "number", "后台学习规则、群聊素材与已有笔记共用的输入上限。", { min: 2048, max: 65536 }),
    field("limits.learning_max_output_tokens", "学习输出 token 上限", "number", "限制后台模型输出的笔记变更内容。", { min: 256, max: 8192 }),
  ];
  const contextFields = [
    field("plugins.ai.context.enabled", "跨群与私聊接续", "check", "同一机器人账号、同一用户可以接续最近完成的问答。群聊背景保留出处与发言者，不会归为当前用户的记忆。保存并应用后生效。", { full: true }),
    field("plugins.ai.context.recent_seconds", "近期问答有效期（秒）", "number", "默认 7200 秒，即最近 2 小时；最多保留 7 天的接续范围。", { min: 60, max: 604800 }),
    field("plugins.ai.context.max_turns", "最多召回问答轮数", "number", "默认 3 轮；只读取已经成功发送的完整问答。", { min: 1, max: 32 }),
    field("plugins.ai.context.neighbors_before", "核心问答之前的群消息数", "number", "默认前 10 条。设为 0 不读取前文。", { min: 0, max: 50 }),
    field("plugins.ai.context.neighbors_after", "核心问答之后的群消息数", "number", "默认后 5 条。设为 0 不读取后文，始终截止于当前请求之前。", { min: 0, max: 50 }),
    field("plugins.ai.context.neighbor_seconds", "前后文各自时间窗口（秒）", "number", "同时限制数量和时间，默认向前、向后各 120 秒；重叠背景会去重。", { min: 1, max: 3600, full: true }),
    field("plugins.ai.context.recall_budget", "近期问答预算上限（估算）", "number", "默认 3000，包含群聊背景，占普通输入预算、最多为其一半；优先保留完整问答。", { min: 256, max: 64000 }),
    field("plugins.ai.context.neighbor_budget", "群聊背景预算上限（估算）", "number", "默认 1500，包含在近期问答预算内，不能超过近期问答预算。设为 0 停止注入背景；空间不足时先减少背景。", { min: 0, max: 64000 }),
  ];
  const groups = [
    { title: "模型接口", description: "兼容 Chat Completions 的远程 API；获取模型列表不会保存密钥或发送聊天请求。", models: true, fields: [
      field("llm.chat_completions_url", "API 地址", "text", "可填写基础地址或 /v1 后获取模型列表，成功后补全聊天接口路径。直接手填模型时，请使用服务商的完整聊天接口地址。", { full: true, placeholder: "https://…/v1 或 https://…/v1/chat/completions" }),
      { secret: "llm_api_key", label: "模型 API 密钥" },
      field("llm.model", "手动填写模型名称", "text", "使用服务商提供的准确模型 ID；获取列表不会替换当前模型。"),
      field("llm.max_concurrent_requests", "最大并发请求", "number", "前台聊天和后台整理共用请求额度。", { min: 1 }),
      field("llm.request_timeout_secs", "单次请求超时（秒）", "number", "", { min: 1 }),
      field("llm.tool_calling", "启用原生工具调用", "check", "模型需支持工具调用；工具仍由宿主检查权限和执行结果。"),
      field("llm.vision", "启用图片识别", "check", "识别当前触发消息及其引用中的图片，合计最多 4 张；模型需支持视觉输入。关闭后按文字处理。"),
    ] },
    { title: "向量接口（Embedding）", description: "独立于聊天模型，用于文字记忆、学习笔记和表情图片检索。测试使用两条合成输入，不读取聊天历史。", embedding: true, fields: [
      field("embedding.enabled", "启用向量服务", "check", "保存并应用后启用。关闭时个人记忆继续使用文字检索，学习笔记回退文字和最近笔记。", { full: true }),
      field("embedding.embeddings_url", "向量 API 地址", "text", "填写完整 embeddings 接口地址；更换地址时需填写该服务的密钥。", { full: true, placeholder: "https://api.siliconflow.cn/v1/embeddings" }),
      { secret: "embedding_api_key", label: "向量 API 密钥" },
      field("embedding.model", "向量模型", "text", "文字和图片需使用同一个支持多模态向量的模型。", { placeholder: "Qwen/Qwen3-VL-Embedding-8B" }),
      field("embedding.dimensions", "向量维度", "number", "建议 1024；模型需支持所选维度。更改后会重新建立对应索引。", { min: 64, max: 4096 }),
      field("embedding.request_timeout_secs", "向量请求超时（秒）", "number", "包括排队时间；请求串行处理。连接测试每次最多等待 12 秒。", { min: 1, max: 30 }),
    ] },
    { title: "表情回复", description: "由模型按需选择当前群内的表情图片，需同时启用聊天和向量服务。", fields: [
      field("stickers.enabled", "启用表情库", "check", "控制自动收集和表情回复，仍受群内 Bot 开关和权限限制。"),
      field("stickers.auto_collect", "自动保存群内表情图片", "check", "从允许的群消息中收集表情，供该群后续回复使用。"),
      field("stickers.send_cooldown_secs", "表情回复间隔（秒）", "number", "限制同一群内表情回复的频率。", { min: 1, max: 86400 }),
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
    ] },
    { title: "自动接话", description: "全局策略仅用于已在「群与好友」单独开启自动接话的聊天；每个群聊和私聊默认关闭。保存并应用后生效。真实 @、关键词和引用机器人消息的明确触发仍优先，不受自动接话的合并等待与冷却限制。", autoReply: true, fields: autoReplyFields },
    { title: "跨会话接续", description: "以完整问答为核心，按需附带前 10 条、后 5 条群聊消息。数量、时间和预算都可以调整；召回资料保留来源隔离。", fields: contextFields },
    { title: "学习笔记", description: "独立记录同一机器人账号的群聊交流经验，不替代个人记忆，也不从私聊取材。可在「学习笔记」页人工管理、锁定和查看历史；来源被遗忘的内容不能通过回滚复活。", fields: learningFields },
    { title: "人设", description: "从列表选择默认人设，也可编辑自定义内容。会话可单独选择已保存的人设。", personas: true, fields: [
      field("plugins.ai.defaults.persona", "默认人设", "persona", "选择后随配置一起保存；下方可预览模板和编辑自定义人设。", { full: true }),
    ] },
    { title: "上下文与记忆", description: "摘要、长期记忆读取和写入分别控制；输入预算采用保守字节估算。", fields: [
      field("plugins.ai.defaults.input_budget", "输入预算（估算）", "number", "", { min: 1024, max: 262144 }),
      field("plugins.ai.defaults.output_budget", "回复 token 上限", "number", "模型可能将思考与工具参数一并计入；额度过低会截断工具调用。回复仍受消息长度限制。", { min: 64, max: 32768 }),
      field("plugins.ai.defaults.recent_messages", "最近历史消息数", "number", "设为 0 时不载入最近原文。", { min: 0, max: 4096 }),
      field("plugins.ai.defaults.allowed_tools", "允许模型使用的工具", "tools", "自动读取已注册工具，保存并应用后生效。不勾选即禁用全部工具；关闭插件会保留已有选择。", { full: true }),
      field("plugins.ai.defaults.summary", "生成并使用历史摘要", "check", "关闭后不生成或注入摘要，按历史条数读取原文。"),
      field("plugins.ai.global_memory_shared", "全域长期记忆共享", "check", "默认开启：同一机器人账号内，同一 QQ 的记忆在所有群聊和私聊共享。关闭后仅在来源会话可用。应用开关变更会重置短期上下文和摘要，保留长期记忆与原始消息。", { full: true }),
      field("plugins.ai.defaults.memory_read", "检索长期记忆", "check", "将当前用户的相关记忆加入回答；检索范围由全域共享开关控制。"),
      field("plugins.ai.defaults.reply_to_bot", "回复机器人消息时触发 AI", "check", "在群内引用回复当前机器人账号的消息时，无需再次 @ 或输入关键词。插件命令仍优先处理。"),
      field("plugins.ai.defaults.memory_write", "自动提炼长期记忆", "check", "只接受带有效原文证据的事实变更。"),
      field("plugins.ai.read_group_background", "读取群内背景消息", "check", "未触发的群消息也可进入上下文，与长期记忆学习独立。"),
      field("plugins.ai.learn_all_group_messages", "学习群内背景消息", "check", "未触发的群消息也可用于长期记忆；仍受记忆写入开关约束。"),
      field("plugins.ai.defaults.paused", "默认暂停自动聊天", "check", "可在「记忆与上下文」恢复或只放行一次回复。"),
    ] },
    { title: "联网搜索", description: "将查询关键词发送至 Tavily，可能产生费用。配置完成后启用；AI 使用还需勾选 web_search 工具，会话独立工具设置继续生效。", search: true, fields: [
      field("plugins.web_search.enabled", "启用联网搜索", "check", "提供 /搜索 和 /search 命令；群/好友可单独关闭。"),
      field("plugins.web_search.provider", "搜索供应商", "select", "", { choices: [["tavily", "Tavily"]] }),
      { secret: "search_api_key", label: "Tavily API 密钥" },
      field("plugins.web_search.max_results", "最多结果数", "number", "", { min: 1, max: 5 }),
      field("plugins.web_search.request_timeout_secs", "搜索超时（秒）", "number", "须短于工具总超时；超时不重试。", { min: 1, max: 12 }),
      field("plugins.web_search.max_response_bytes", "响应字节上限", "number", "", { min: 4096, max: 1048576 }),
      field("plugins.web_search.max_output_bytes", "工具输出字节上限", "number", "", { min: 2048, max: 8192 }),
      field("plugins.web_search.max_concurrent_requests", "搜索并发上限", "number", "", { min: 1, max: 16 }),
      field("plugins.web_search.max_calls_per_turn", "每轮搜索上限", "number", "同轮相同参数复用结果；失败请求也计入额度。", { min: 1, max: 2 }),
      field("plugins.web_search.user_per_hour", "每人每小时搜索上限", "number", "", { min: 1, max: 100000 }),
      field("plugins.web_search.conversation_per_hour", "每个会话每小时搜索上限", "number", "", { min: 1, max: 100000 }),
      field("plugins.web_search.global_per_hour", "全局每小时搜索上限", "number", "", { min: 1, max: 100000 }),
    ] },
    { title: "插件", description: "启用需要的功能。提醒可由 AI 直接创建和取消，也可选择要求再次确认。", fields: [
      field("plugins.ai.enabled", "AI 聊天", "check", "启用后需配置有效的模型地址和名称。"),
      field("plugins.clock.enabled", "时间查询", "check", "提供 /clock 指令和 clock_now 工具。"),
      field("plugins.reminder.enabled", "提醒", "check", "提供提醒创建、查询和取消功能。"),
      field("plugins.reminder.llm_writes_require_confirmation", "AI 创建和取消提醒需要再次确认", "check", "默认关闭：按用户请求直接执行。开启后先展示预览，由本人确认。"),
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
      field("plugins.ai.memory_input_budget", "整理输入预算（估算）", "number", "默认 8000，供后台提炼与摘要整理使用；普通聊天的输入预算单独配置。", { min: 3072, max: 262144 }),
      field("plugins.ai.memory_output_budget", "整理输出 token 上限", "number", "", { min: 256, max: 8192 }),
      field("plugins.ai.max_memories_per_conversation", "长期记忆存储容量", "number", "全域共享开启时按同一机器人账号内每个 QQ 计；关闭时按来源会话计。降低容量不会删除已有记忆。", { min: 1 }),
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
    return Object.fromEntries([...secretInputs].map(([name, controls]) => [name, { clear: controls.clear, value: controls.value.value, undoValue: controls.undoValue }]));
  }

  function describeSecret(name) {
    const meta = state.secrets[name];
    if (!meta) return "未读取到密钥状态。";
    if (meta.env_set) return "当前由环境变量提供。文件中的修改不会覆盖环境变量。";
    if (meta.file_set) return "配置文件已设置；密钥不会回传到页面。";
    if (meta.effective_set) return `当前已配置（来源：${String(meta.source || "服务端")}）。`;
    return "当前未设置。";
  }

  function secretField(definition, pending) {
    const wrapper = element("div", "field");
    const id = `secret-${definition.secret}`;
    const label = element("label", "", definition.label);
    label.htmlFor = id;
    wrapper.append(label);
    const input = document.createElement("input");
    input.id = id; input.type = "password"; input.autocomplete = "off";
    input.spellcheck = false; input.setAttribute("aria-label", `${definition.label}新值`);
    input.placeholder = "留空保持原密钥，填写即更新"; input.value = pending?.value || "";
    const clearButton = element("button", "text-button secret-clear", "清除已保存密钥"); clearButton.type = "button";
    const status = element("p", "hint secret-state");
    const controls = { value: input, clear: Boolean(pending?.clear), undoValue: pending?.undoValue || "", clearButton, status, label: definition.label };
    const refresh = () => {
      clearButton.textContent = controls.clear ? "撤销清除" : "清除已保存密钥";
      status.textContent = describeSecret(definition.secret) + (controls.clear ? " 已标记清除文件中的密钥，保存后生效。" : " 留空保持原密钥，输入后随配置保存。");
    };
    wrapper.append(input, clearButton, status); secretInputs.set(definition.secret, controls); refresh();
    const changed = () => { if (definition.secret === "llm_api_key") invalidateModels(); if (definition.secret === "embedding_api_key") invalidateEmbeddingTest(); if (definition.secret === "search_api_key") invalidateSearchTest(); refresh(); markDirty(); };
    clearButton.addEventListener("click", () => {
      if (controls.clear) { controls.clear = false; input.value = controls.undoValue; controls.undoValue = ""; }
      else { controls.clear = true; controls.undoValue = input.value; input.value = ""; }
      changed();
    });
    input.addEventListener("input", () => { controls.clear = false; controls.undoValue = ""; changed(); });
    return wrapper;
  }

  function secretChange(controls) {
    if (controls.clear) return { action: "clear" };
    const value = controls.value.value;
    if (!value) return { action: "keep" };
    if (!value.trim()) throw new Error(`「${controls.label}」不能只包含空白；留空可保持原密钥。`);
    return { action: "set", value };
  }

  function invalidateModels() {
    state.models.version++; state.models.items = []; state.models.manual = true; state.models.message = ""; state.models.kind = "";
    renderModels();
  }
  function renderModels() {
    const model = inputs.get("llm.model"), list = $("model-list"), current = model?.control.value || "";
    list.replaceChildren();
    if (!state.models.items.includes(current)) {
      const option = element("option", "", current ? `当前手动值：${current}` : "请选择模型（保留空值直到选择）"); option.value = current; list.append(option);
    }
    for (const id of state.models.items) { const option = element("option", "", id); option.value = id; list.append(option); }
    list.value = current;
    $("models-select-row").hidden = !state.models.items.length;
    $("manual-model").textContent = state.models.manual ? "收起手动输入" : "手动输入模型名称";
    if (model) model.wrapper.hidden = !state.models.manual && Boolean(state.models.items.length);
    $("models-message").textContent = state.models.message || (state.models.loading ? "正在获取模型列表…" : "按需获取，可继续手动填写模型。列表中的模型是否支持聊天，以服务商说明为准。");
    $("models-message").className = "hint models-message" + (state.models.kind === "error" ? " error-text" : "");
    $("fetch-models").textContent = state.models.loading ? "获取中…" : "获取模型列表";
    updateControls();
  }
  function modelsCurrent(request) {
    try {
      return state.authenticated && state.session === request.session && state.models.version === request.version && state.revision === request.revision
        && inputs.get("llm.chat_completions_url")?.control.value.trim() === request.url
        && canonical(secretChange(secretInputs.get("llm_api_key"))) === request.key;
    } catch { return false; }
  }
  async function fetchModels() {
    if (!state.authenticated || !state.config || state.models.loading || state.busy || state.jsonDirty || state.pendingOperation || state.stale) return;
    let request;
    try {
      const url = inputs.get("llm.chat_completions_url").control.value.trim(), key = secretChange(secretInputs.get("llm_api_key"));
      if (!url) throw new Error("请先填写 API 地址。");
      request = { session: state.session, version: state.models.version, revision: state.revision, url, key: canonical(key) };
      state.models.loading = true; state.models.message = ""; state.models.kind = ""; renderModels();
      const result = await api("/api/llm/models", "POST", { revision: request.revision, url, api_key: key });
      if (!modelsCurrent(request)) return;
      const ids = result.models;
      if (!Array.isArray(ids) || ids.length > 4096 || ids.some((id) => typeof id !== "string" || !id || id.trim() !== id || new TextEncoder().encode(id).length > 256 || /[\p{Cc}]/u.test(id))) throw new Error("服务返回的模型列表格式无效，请手动填写模型名称。");
      const endpoint = new URL(result.chat_completions_url), origin = new URL(url);
      if (!["http:", "https:"].includes(endpoint.protocol) || endpoint.origin !== origin.origin || endpoint.username || endpoint.password || endpoint.search || endpoint.hash) throw new Error("服务返回的聊天接口地址无效，原地址已保留。");
      state.models.items = [...new Set(ids)]; state.models.manual = !ids.length;
      const control = inputs.get("llm.chat_completions_url").control;
      if (control.value !== result.chat_completions_url) { control.value = result.chat_completions_url; markDirty(); }
      const source = { temporary: "本次使用填写的新密钥。", environment: "本次使用环境变量中的密钥。", file: "本次使用已保存密钥。", none: "本次未使用密钥。" }[result.key_source] || "";
      state.models.message = (ids.length ? `已获取 ${state.models.items.length} 个模型，请从列表选择；当前模型保持不变。` : "服务未返回可用模型，请手动填写模型名称。") + source
        + (result.environment_overrides_saved_key ? " 保存后，环境变量仍优先于文件中的密钥。" : "");
    } catch (error) {
      if (request && !modelsCurrent(request)) return;
      if (error.status === -1 || error.code === "auth_required") return;
      state.models.message = error.message || "未能获取模型列表，请手动填写模型名称。"; state.models.kind = "error";
      if (error.code === "models_stale") state.models.message += " 请先保留本页草稿，再重新载入配置。";
      if (error.code === "csrf_invalid") report(error);
    } finally {
      state.models.loading = false; renderModels();
    }
  }

  function invalidateEmbeddingTest() {
    state.embedding.version++; state.embedding.message = ""; state.embedding.kind = "";
    renderEmbeddingTest();
  }
  function renderEmbeddingTest() {
    if (!embeddingControls) return;
    embeddingControls.button.textContent = state.embedding.loading ? "正在测试…" : "测试文字和图片向量";
    embeddingControls.message.textContent = state.embedding.message || (state.embedding.loading ? "正在发送合成文字和图片，请稍候…" : "点击会发出两次向量请求，可能产生少量费用；测试成功后仍需保存并应用配置。");
    embeddingControls.message.className = "hint" + (state.embedding.kind === "error" ? " error-text" : "");
    updateControls();
  }
  function embeddingDraft() {
    const url = inputs.get("embedding.embeddings_url").control.value.trim();
    const model = inputs.get("embedding.model").control.value.trim();
    const number = (path, min, max) => {
      const control = inputs.get(path).control, value = Number(control.value);
      if (!control.value.trim() || !Number.isSafeInteger(value) || value < min || value > max || !control.validity.valid) throw new Error("请填写有效的向量维度和超时。");
      return value;
    };
    if (!url || !model) throw new Error("请先填写向量 API 地址和模型名称。");
    return { url, model, dimensions: number("embedding.dimensions", 64, 4096), request_timeout_secs: number("embedding.request_timeout_secs", 1, 30), api_key: secretChange(secretInputs.get("embedding_api_key")) };
  }
  function embeddingCurrent(request) {
    try {
      return state.authenticated && state.session === request.session && state.embedding.version === request.version && state.revision === request.revision
        && !state.jsonDirty && canonical(embeddingDraft()) === request.draft;
    } catch { return false; }
  }
  async function testEmbedding() {
    if (!state.authenticated || !state.config || state.embedding.loading || state.busy || state.jsonDirty || state.pendingOperation || state.stale) return;
    let request;
    try {
      const draft = embeddingDraft();
      request = { session: state.session, version: state.embedding.version, revision: state.revision, draft: canonical(draft) };
      state.embedding.loading = true; state.embedding.message = ""; state.embedding.kind = ""; renderEmbeddingTest();
      const result = await api("/api/embedding/test", "POST", { revision: request.revision, ...draft });
      if (!embeddingCurrent(request)) return;
      if (result.text_ok !== true || result.image_ok !== true || result.dimensions !== draft.dimensions) throw new Error("向量测试返回的维度或能力结果无效。");
      const source = { temporary: "本次使用新填写的密钥。", environment: "本次使用环境变量中的密钥。", file: "本次使用已保存密钥。", none: "" }[result.key_source] || "";
      state.embedding.message = `测试通过：文字和图片均返回有效的 ${result.dimensions} 维向量。${source}配置尚未由此次测试保存。`
        + (result.environment_overrides_saved_key ? " 保存后，环境变量仍优先于文件密钥。" : "");
    } catch (error) {
      if (request && !embeddingCurrent(request)) return;
      if (error.status === -1 || error.code === "auth_required") return;
      state.embedding.message = error.message || "向量连接测试未通过。"; state.embedding.kind = "error";
      if (error.code === "csrf_invalid") report(error);
    } finally {
      state.embedding.loading = false; renderEmbeddingTest();
    }
  }

  function invalidateSearchTest() {
    state.search.version++; state.search.message = ""; state.search.kind = ""; renderSearchTest();
  }
  function renderSearchTest() {
    if (!searchControls) return;
    searchControls.button.textContent = state.search.loading ? "正在测试…" : "测试搜索连接";
    searchControls.message.textContent = state.search.message || (state.search.loading ? "正在检索测试关键词…" : "测试固定关键词 Rust programming language，发出一次可能计费的请求，不保存配置。环境变量 QKBOT_SEARCH_API_KEY 优先于文件密钥。");
    searchControls.message.className = "hint" + (state.search.kind === "error" ? " error-text" : "");
    updateControls();
  }
  async function testSearch() {
    if (!state.authenticated || !state.config || state.search.loading || state.busy || state.jsonDirty || state.pendingOperation || state.stale) return;
    const request = { session: state.session, version: state.search.version, revision: state.revision };
    const current = () => state.authenticated && state.session === request.session && state.search.version === request.version && state.revision === request.revision && !state.jsonDirty;
    try {
      state.search.loading = true; state.search.message = ""; state.search.kind = ""; renderSearchTest();
      const response = await api("/api/search/test", "POST", { revision: request.revision, api_key: secretChange(secretInputs.get("search_api_key")) });
      if (!current()) return;
      if (response.provider !== "tavily" || !Number.isInteger(response.result_count) || response.result_count < 0 || response.result_count > 1) throw new Error("搜索测试返回无效结果。");
      state.search.message = `连接成功，收到 ${response.result_count} 条结果。配置尚未保存或应用。` + (response.environment_overrides_saved_key ? " 保存后环境变量仍优先于文件密钥。" : "");
    } catch (error) {
      if (!current() || error.status === -1 || error.code === "auth_required") return;
      state.search.message = error.message || "搜索测试失败"; state.search.kind = "error";
      if (error.code === "csrf_invalid") report(error);
    } finally { state.search.loading = false; renderSearchTest(); }
  }

  function validatePersonaMap(value, requireText = true) {
    if (!value || typeof value !== "object" || Array.isArray(value) || Object.entries(value).some(([name, text]) => !name.trim() || typeof text !== "string" || (requireText && !text.trim()))) throw new Error("自定义人设需要有效名称和非空正文。");
  }

  function validateToolCatalog(value) {
    if (!Array.isArray(value) || value.length > 4096 || value.some((tool) => !tool || typeof tool.name !== "string" || !tool.name || typeof tool.description !== "string" || typeof tool.plugin_id !== "string" || !tool.plugin_id || !["read", "network_read", "async_read", "local_write", "reply"].includes(tool.effect)) || new Set(value.map((tool) => tool.name)).size !== value.length) throw new Error("工具目录格式无效，请重新载入配置。");
  }
  function toolUnavailable(tool, mode) {
    if (!tool) return "未注册的旧工具，可取消选择";
    const config = mode === "global" ? state.draft : state.config;
    const plugins = config?.plugins;
    const configured = plugins && Object.hasOwn(plugins, tool.plugin_id) && plugins[tool.plugin_id]?.enabled === true;
    const enabled = mode === "global" ? inputs.get(`plugins.${tool.plugin_id}.enabled`)?.control.checked ?? configured : configured;
    if (!enabled) return "所属插件已关闭";
    if (mode === "conversation" && !config?.plugins?.ai?.defaults?.allowed_tools?.includes(tool.name)) return "未在已保存的全局配置中启用";
    return "";
  }
  function toolPickerBlocked(mode) {
    return state.busy || !state.authenticated || !state.config || Boolean(state.pendingOperation)
      || (mode === "global" ? state.jsonDirty : !state.memory.detail || state.memory.conflict);
  }
  function updateToolPicker(control) {
    const picker = control.toolPicker; if (!picker) return;
    for (const [name, row] of picker.rows) row.checkbox.disabled = toolPickerBlocked(picker.mode) || (Boolean(row.unavailable) && !picker.selected.has(name));
    const unavailable = [...picker.selected].filter((name) => picker.rows.get(name)?.unavailable).length;
    picker.summary.textContent = picker.selected.size ? `已勾选 ${picker.selected.size} 项` + (unavailable ? `，其中 ${unavailable} 项当前不可用；已有选择保留，可取消。` : "。") : "未勾选任何工具：禁用全部工具。";
    const toolCalling = picker.mode === "global" ? inputs.get("llm.tool_calling")?.control.checked ?? state.draft?.llm?.tool_calling : state.config?.llm?.tool_calling;
    if (toolCalling === false) picker.summary.textContent += " 原生工具调用已关闭，勾选项暂不会用于模型调用。";
  }
  function renderToolPicker(control, selected, mode) {
    const selection = new Set(selected ?? control.toolPicker?.selected ?? []);
    const catalog = new Map(state.toolCatalog.map((tool) => [tool.name, tool]));
    const names = [...catalog.keys(), ...[...selection].filter((name) => !catalog.has(name))];
    const rows = new Map(), summary = element("p", "hint tool-summary"); summary.setAttribute("aria-live", "polite");
    control.toolPicker = { selected: selection, mode, rows, summary }; control.replaceChildren();
    for (const name of names) {
      const tool = catalog.get(name), unavailable = toolUnavailable(tool, mode);
      const label = element("label", "tool-option" + (unavailable ? " unavailable" : ""));
      const checkbox = document.createElement("input"); checkbox.type = "checkbox"; checkbox.checked = selection.has(name); checkbox.dataset.toolName = name;
      checkbox.setAttribute("aria-label", name + (unavailable ? "，" + unavailable : ""));
      const copy = element("span", "tool-copy"); copy.append(element("strong", "tool-name", name));
      if (tool) {
        const effectLabel = { read: "只读", network_read: "联网查询 · 可能计费", async_read: "异步只读", local_write: "本地写入", reply: "本轮回复" }[tool.effect];
        copy.append(element("span", "tool-meta", `${effectLabel} · ${tool.plugin_id}`));
        copy.append(element("span", "tool-description", tool.description));
      }
      if (unavailable) copy.append(element("span", "tool-warning", unavailable));
      label.append(checkbox, copy); control.append(label); rows.set(name, { checkbox, unavailable });
      checkbox.addEventListener("change", () => {
        if (toolPickerBlocked(mode) || (checkbox.checked && toolUnavailable(catalog.get(name), mode))) { checkbox.checked = selection.has(name); updateToolPicker(control); return; }
        if (checkbox.checked) selection.add(name); else selection.delete(name);
        updateToolPicker(control);
        if (mode === "global") markDirty(); else { state.memory.settingsDirty = true; updateControls(); }
      });
    }
    if (!names.length) control.append(element("p", "hint", "当前没有已注册的工具。"));
    control.append(summary); updateToolPicker(control);
  }
  function refreshToolPickers() {
    const global = inputs.get("plugins.ai.defaults.allowed_tools")?.control;
    if (global) renderToolPicker(global, undefined, "global");
    const conversation = settingInputs.get("allowed_tools")?.input;
    if (conversation) renderToolPicker(conversation, undefined, "conversation");
  }
  function readToolPicker(control) { return [...control.toolPicker.selected]; }
  function personaChoices(config) {
    return new Map([...Object.entries(state.personaTemplates), ...Object.entries(config?.plugins?.ai?.personas || {})]);
  }
  function renderAutoReplyBudget() {
    if (!autoReplyBudgetControl || !state.draft) return;
    const prompt = (inputs.get("plugins.ai.auto_reply.prompt")?.control.value ?? "").trim();
    const name = inputs.get("plugins.ai.defaults.persona")?.control.value ?? "";
    const expression = personaChoices(state.draft).get(name);
    const budget = Number(inputs.get("plugins.ai.defaults.input_budget")?.control.value);
    if (typeof expression !== "string" || !Number.isSafeInteger(budget) || budget <= 0) {
      autoReplyBudgetControl.className = "hint";
      autoReplyBudgetControl.textContent = "选择有效的默认人设并填写输入预算后，可在这里查看自动接话的预算估算。";
      return;
    }
    // The persona is JSON embedded inside a system message, so count its outer
    // JSON escaping too. This matches the server's conservative UTF-8 budget.
    const jsonBytes = (value) => new TextEncoder().encode(JSON.stringify(value)).length;
    const variable = jsonBytes(prompt) - 2 + jsonBytes(JSON.stringify({ name, expression })) - jsonBytes(JSON.stringify({ name: "", expression: "" }));
    const complete = state.autoReplyBaseEstimate !== null;
    const estimate = (state.autoReplyBaseEstimate ?? 0) + variable;
    const remaining = budget - estimate;
    const warning = estimate >= budget ? "已达到或超出输入预算，可能无法自动接话。"
      : estimate >= budget * 0.75 || remaining < 1000 ? "接近输入预算，剩余空间可能不足。" : "";
    autoReplyBudgetControl.className = warning || !complete ? "notice warning" : "hint";
    autoReplyBudgetControl.textContent = warning + `${complete ? "策略/人设及固定规则" : "仅策略/人设"}约占 ${estimate} / ${budget} 输入预算，剩余 ${Math.max(0, remaining)}${remaining < 0 ? `（超出 ${-remaining}）` : ""}。`
      + (complete ? "" : "未计入宿主固定规则开销，实际占用更高，无法判断预算是否足够。")
      + "还需容纳当前消息、历史、摘要、记忆和工具。这里是保守估算，不是模型精确 token；会话单独覆盖预算或人设时，以该会话设置为准。";
  }
  function requirePersona(config, name, label = "人设") {
    if (!personaChoices(config).has(name)) throw new Error(`「${label}」已失效，请从列表重新选择。`);
  }
  function fillPersonaSelect(control, selected, config) {
    const choices = personaChoices(config); control.replaceChildren();
    if (!choices.has(selected)) {
      const missing = element("option", "", selected ? `已失效：${selected}（请重新选择）` : "请选择人设");
      missing.value = selected ?? ""; missing.disabled = true; control.append(missing);
    }
    for (const name of choices.keys()) {
      const custom = Object.hasOwn(config?.plugins?.ai?.personas || {}, name);
      const option = element("option", "", name + (custom ? " · 自定义" : " · 内置模板")); option.value = name; control.append(option);
    }
    control.value = selected ?? ""; control.setAttribute("aria-invalid", String(!choices.has(selected)));
  }
  function refreshPersonaSelectors() {
    const defaults = inputs.get("plugins.ai.defaults.persona")?.control;
    if (defaults) fillPersonaSelect(defaults, defaults.value, state.draft);
    const conversation = settingInputs.get("persona")?.input;
    if (conversation) fillPersonaSelect(conversation, conversation.value, state.config);
    const allGroups = $("group-persona-select");
    fillPersonaSelect(allGroups, allGroups.value, state.config);
    if (!allGroups.value) allGroups.setAttribute("aria-invalid", "false");
  }
  function renderPersonaLibrary() {
    const choices = personaChoices(state.draft), list = $("persona-list"); list.replaceChildren();
    if (!choices.has(state.personaSelected)) state.personaSelected = choices.has(state.draft?.plugins?.ai?.defaults?.persona) ? state.draft.plugins.ai.defaults.persona : choices.keys().next().value ?? null;
    for (const name of choices.keys()) {
      const custom = Object.hasOwn(state.draft.plugins.ai.personas, name);
      const option = element("option", "", name + (custom ? " · 自定义" : " · 内置模板")); option.value = name; list.append(option);
    }
    list.value = state.personaSelected ?? ""; renderPersonaPreview();
  }
  function renderPersonaPreview() {
    const name = state.personaSelected, choices = personaChoices(state.draft);
    const custom = name != null && Object.hasOwn(state.draft?.plugins?.ai?.personas || {}, name);
    $("persona-title").textContent = name == null ? "人设正文" : name;
    $("persona-text").value = choices.get(name) ?? "";
    badge("persona-source", name == null ? "选择人设" : custom ? "自定义" : "内置模板", custom ? "good" : "neutral");
    $("persona-editor-hint").textContent = custom ? "编辑内容会保留在本页草稿，切换人设不会丢失。随配置保存并应用后生效。" : "内置模板可直接使用；需要调整时，先另存为自定义。";
    updateControls();
  }
  function createPersona(copy = false) {
    if (!state.draft || state.busy || state.jsonDirty || state.pendingOperation) return;
    const name = $("persona-new-name").value.trim(), choices = personaChoices(state.draft);
    if (!name) throw new Error("请填写新自定义人设的名称。");
    if (Object.hasOwn(state.draft.plugins.ai.personas, name)) throw new Error("同名自定义人设已存在，请从列表选择编辑，或换一个名称。");
    if (copy && !choices.has(state.personaSelected)) throw new Error("请先选择要另存的人设。");
    if (choices.has(name) && !window.confirm("保存同名自定义后将优先使用自定义正文。确定继续？")) return;
    Object.defineProperty(state.draft.plugins.ai.personas, name, { value: copy ? choices.get(state.personaSelected) : "", enumerable: true, writable: true, configurable: true });
    state.personaSelected = name; $("persona-new-name").value = ""; $("persona-error").textContent = "";
    renderPersonaLibrary(); refreshPersonaSelectors(); markDirty();
  }
  function deletePersona() {
    const name = state.personaSelected;
    if (!state.draft || state.busy || state.jsonDirty || state.pendingOperation || !Object.hasOwn(state.draft.plugins.ai.personas, name)) return;
    if (!window.confirm(`删除自定义人设「${name}」？${Object.hasOwn(state.personaTemplates, name) ? "同名内置模板将恢复使用。" : "引用它的默认或会话人设需要重新选择。"}保存并应用配置后生效。`)) return;
    delete state.draft.plugins.ai.personas[name]; $("persona-error").textContent = "";
    renderPersonaLibrary(); refreshPersonaSelectors(); markDirty();
  }

  function configField(definition) {
    const wrapper = element("div", `field${definition.full ? " full" : ""}${definition.type === "check" ? " check-field" : ""}`);
    const id = `field-${definition.path.replaceAll(".", "-")}`;
    const label = element("label", "", definition.label); label.htmlFor = id;
    let control;
    if (definition.type === "tools") {
      control = element("div", "tool-choices"); control.setAttribute("role", "group"); control.setAttribute("aria-label", definition.label);
    } else if (["lines", "ids", "json", "textarea"].includes(definition.type)) {
      control = document.createElement("textarea");
      control.rows = definition.rows ?? (definition.type === "json" ? 5 : 3);
      control.spellcheck = false;
      if (definition.type === "textarea") control.required = !definition.optional;
    } else if (definition.type === "persona") {
      control = document.createElement("select"); control.required = true;
      fillPersonaSelect(control, get(state.draft, definition.path), state.draft);
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
    if (definition.type === "tools") renderToolPicker(control, value, "global");
    else if (definition.type === "check") control.checked = Boolean(value);
    else if (definition.type === "json") control.value = JSON.stringify(value ?? {}, null, 2);
    else if (["lines", "ids"].includes(definition.type)) control.value = Array.isArray(value) ? value.join("\n") : "";
    else control.value = value == null ? "" : String(value);
    const range = definition.path.startsWith("plugins.web_search.") && definition.type === "number" ? `允许范围：${definition.min}–${definition.max}。` : "";
    const hint = element("p", "hint", range + definition.hint);
    hint.id = `${id}-hint`; control.setAttribute("aria-describedby", hint.id);
    if (definition.type === "check") { const copy = element("div"); copy.append(label, hint); wrapper.append(control, copy); }
    else { wrapper.append(label, control); if (range || definition.hint) wrapper.append(hint); }
    inputs.set(definition.path, { control, definition, wrapper });
    control.addEventListener("input", () => {
      if (definition.path === "llm.chat_completions_url") invalidateModels();
      if (definition.path === "llm.model") renderModels();
      if (definition.path.startsWith("embedding.")) invalidateEmbeddingTest();
      if (definition.path.startsWith("plugins.web_search.")) invalidateSearchTest();
      if (definition.path.startsWith("plugins.") && definition.path.endsWith(".enabled")) refreshToolPickers();
      markDirty();
    });
    if (definition.type === "persona") control.addEventListener("change", () => { control.setAttribute("aria-invalid", String(!personaChoices(state.draft).has(control.value))); markDirty(); });
    return wrapper;
  }

  function validatePluginCatalog(catalog) {
    if (!Array.isArray(catalog) || catalog.length > 64) throw new Error("插件目录格式无效。");
    const ids = new Set();
    for (const item of catalog) {
      if (!item || typeof item.id !== "string" || typeof item.name !== "string" || typeof item.description !== "string" || !["rust", "nonebot"].includes(item.runtime) || ids.has(item.id)) throw new Error("插件目录字段缺失或重复。");
      ids.add(item.id);
    }
  }
  function validatePluginShape(config) {
    const object = (value) => value && typeof value === "object" && !Array.isArray(value);
    const native = config.plugins?.native;
    if (native !== undefined || state.draft?.plugins?.native !== undefined) {
      if (!object(native) || !Array.isArray(native.enabled) || !native.enabled.every((id) => typeof id === "string")) throw new Error("JSON 字段 plugins.native.enabled 必须为插件 ID 数组。");
    }
    const bridge = config.nonebot;
    if (bridge !== undefined || state.draft?.nonebot !== undefined) {
      if (!object(bridge) || typeof bridge.enabled !== "boolean" || !Array.isArray(bridge.plugins)) throw new Error("JSON 字段 nonebot 必须包含启用状态和插件数组。");
      const seen = new Set();
      for (const item of bridge.plugins) {
        if (!object(item) || typeof item.id !== "string" || seen.has(item.id) || typeof item.enabled !== "boolean") throw new Error("JSON 字段 nonebot.plugins 缺少唯一 ID 或启用状态。");
        for (const key of ["module", "name", "description"]) if (Object.hasOwn(item, key) && typeof item[key] !== "string") throw new Error(`JSON 字段 nonebot.plugins.${key} 必须为文字。`);
        seen.add(item.id);
      }
    }
  }
  function objectJson(control, label) {
    let value;
    try { value = JSON.parse(control.value); } catch { throw new Error(`${label}不是有效的 JSON。`); }
    if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(`${label}必须为 JSON 对象。`);
    return value;
  }
  function pluginCheck(parent, id, title, hint, enabled, map) {
    const row = element("label", "plugin-option"), input = document.createElement("input"), copy = element("span");
    input.type = "checkbox"; input.checked = enabled; input.dataset.pluginId = id;
    input.setAttribute("aria-label", title + "全局启用"); map.set(id, input);
    const loaded = element("span", "plugin-load-state"); loaded.dataset.pluginLoaded = id;
    copy.append(element("strong", "", title), loaded, element("span", "hint", hint));
    row.append(input, copy); parent.append(row); input.addEventListener("change", markDirty);
  }
  function readExternalDefinitions() {
    let definitions;
    try { definitions = JSON.parse(pluginControls.definitions.value); } catch { throw new Error("NoneBot 插件定义不是有效的 JSON 数组。"); }
    if (!Array.isArray(definitions) || definitions.length > 32) throw new Error("NoneBot 插件定义应为最多32项的 JSON 数组。");
    const seen = new Set();
    return definitions.map((item) => {
      if (!item || typeof item !== "object" || Array.isArray(item) || typeof item.id !== "string" || seen.has(item.id)) throw new Error("NoneBot 插件定义缺少唯一 id。");
      if (Object.hasOwn(item, "enabled")) throw new Error("NoneBot 定义中的 enabled 请移除，使用上方勾选控制启用。");
      seen.add(item.id); return clone(item);
    });
  }
  function refreshExternalPlugins() {
    const controls = pluginControls, previous = new Map([...controls.external].map(([id, input]) => [id, input.checked]));
    const definitions = readExternalDefinitions(); controls.external.clear(); controls.externalList.replaceChildren();
    for (const item of definitions) pluginCheck(controls.externalList, item.id, item.name || item.id, item.module || "请在高级区填写模块名", previous.get(item.id) ?? false, controls.external);
    if (!definitions.length) controls.externalList.append(element("p", "hint", "尚未配置 NoneBot 插件。在高级区添加插件定义后刷新此列表。"));
    renderPluginLoadedStates();
  }
  function renderPluginConfig(pending) {
    pluginControls = null;
    if (!state.draft?.plugins?.native) return;
    const native = state.draft.plugins.native, bridge = state.draft.nonebot;
    const card = element("section", "card plugin-config"); card.dataset.configGroup = "tools";
    card.append(element("h2", "", "插件功能"), element("p", "muted", "勾选是待保存的全局配置，实际运行以「当前运行已启用」为准。保存并应用成功后才会生效；会话开关只控制群或好友是否允许使用。"));
    const nativeList = element("div", "plugin-grid"), nativeChecks = new Map();
    const known = new Map(state.pluginCatalog.filter((entry) => entry.runtime === "rust" && entry.id !== "web_search").map((entry) => [entry.id, entry]));
    for (const id of native.enabled || []) if (!known.has(id)) known.set(id, { id, name: id + "（已保存但未注册）", description: "保留原选择，可取消后修复配置。" });
    for (const item of known.values()) pluginCheck(nativeList, item.id, item.name, item.description, (native.enabled || []).includes(item.id), nativeChecks);
    card.append(nativeList);
    const details = element("details", "plugin-advanced"), summary = element("summary", "", "高级参数 · 管理员、MC、群互动与 NoneBot");
    const advanced = element("div", "plugin-advanced-body"); details.append(summary, advanced);
    const nativeOptions = clone(native); delete nativeOptions.enabled;
    const nativeJson = document.createElement("textarea"); nativeJson.rows = 10; nativeJson.spellcheck = false; nativeJson.id = "native-options-json"; nativeJson.value = JSON.stringify(nativeOptions, null, 2);
    const nativeLabel = element("label", "", "原生插件参数 JSON"); nativeLabel.htmlFor = nativeJson.id;
    advanced.append(nativeLabel, element("p", "hint", "superusers 为可管理图片餐单的 QQ 号；mc_servers 配置服务器；today_waifu、poke 和 fakemsg 保留各插件参数。此处不填写 enabled。"), nativeJson);
    const controls = { native: nativeChecks, nativeJson, external: new Map() }; pluginControls = controls;
    nativeJson.addEventListener("input", markDirty);
    if (bridge) {
      const enabled = document.createElement("input"); enabled.type = "checkbox"; enabled.id = "nonebot-enabled"; enabled.checked = Boolean(bridge.enabled);
      const label = element("label", "plugin-option"); label.append(enabled, element("span", "", "启用 NoneBot 兼容服务"));
      card.append(label, element("p", "hint", "兼容插件需要启动本机 NoneBot Python 服务。这里只配置已安装模块，不下载或执行任意插件源码。"));
      card.append(secretField({ secret: "nonebot_token", label: "NoneBot 连接令牌" }, pending.nonebot_token));
      const externalList = element("div", "plugin-grid"); card.append(externalList);
      const definitions = document.createElement("textarea"); definitions.id = "nonebot-definitions-json"; definitions.rows = 8; definitions.spellcheck = false;
      definitions.value = JSON.stringify((bridge.plugins || []).map((entry) => { const item = clone(entry); delete item.enabled; return item; }), null, 2);
      const options = clone(bridge); delete options.enabled; delete options.token; delete options.plugins;
      const optionsJson = document.createElement("textarea"); optionsJson.id = "nonebot-options-json"; optionsJson.rows = 6; optionsJson.spellcheck = false; optionsJson.value = JSON.stringify(options, null, 2);
      const definitionsLabel = element("label", "", "NoneBot 插件定义 JSON"); definitionsLabel.htmlFor = definitions.id;
      const optionsLabel = element("label", "", "NoneBot 服务参数 JSON"); optionsLabel.htmlFor = optionsJson.id;
      const refresh = element("button", "button secondary", "采用定义并刷新勾选列表"); refresh.type = "button";
      advanced.append(definitionsLabel, element("p", "hint", "保留 id、module、name、description、commands、url_hosts 等完整字段；启用状态由上方勾选控制。新添加的定义默认关闭。"), definitions, refresh, optionsLabel, optionsJson);
      Object.assign(controls, { enabled, externalList, definitions, optionsJson });
      for (const entry of bridge.plugins || []) controls.external.set(entry.id, { checked: Boolean(entry.enabled) });
      refreshExternalPlugins();
      enabled.addEventListener("change", markDirty); definitions.addEventListener("input", markDirty); optionsJson.addEventListener("input", markDirty);
      refresh.addEventListener("click", () => { try { refreshExternalPlugins(); markDirty(); } catch (error) { report(error); } });
    }
    card.append(details); $("config-groups").append(card);
  }
  function readPluginConfig(config) {
    if (!pluginControls) return;
    const options = objectJson(pluginControls.nativeJson, "原生插件参数");
    if (Object.hasOwn(options, "enabled")) throw new Error("原生参数中的 enabled 请移除，使用插件勾选列表。");
    config.plugins.native = { ...options, enabled: [...pluginControls.native].filter(([, input]) => input.checked).map(([id]) => id) };
    if (pluginControls.enabled) {
      const bridge = objectJson(pluginControls.optionsJson, "NoneBot 服务参数");
      for (const key of ["enabled", "token", "plugins"]) if (Object.hasOwn(bridge, key)) throw new Error(`NoneBot 服务参数中的 ${key} 请使用对应的独立控件。`);
      config.nonebot = { ...bridge, token: "", enabled: pluginControls.enabled.checked, plugins: readExternalDefinitions().map((entry) => ({ ...entry, enabled: pluginControls.external.get(entry.id)?.checked ?? false })) };
    }
  }

  function renderFields(pending = {}) {
    const personaManager = $("persona-manager"), modelPicker = $("model-picker");
    state.models.version++; state.models.items = []; state.models.manual = true; state.models.message = ""; state.models.kind = "";
    state.embedding.version++; state.embedding.message = ""; state.embedding.kind = ""; embeddingControls = null;
    state.search.version++; state.search.message = ""; state.search.kind = ""; searchControls = null;
    inputs.clear(); secretInputs.clear(); autoReplyBudgetControl = null;
    $("config-groups").replaceChildren();
    groups.forEach((group, index) => {
      const card = element("section", "card"); card.dataset.configGroup = configGroupCategory(group);
      const heading = element("div", "config-section-head");
      heading.append(element("span", "section-number", String(index + 1).padStart(2, "0")));
      const title = element("div"); title.append(element("h2", "", group.title), element("p", "", group.description)); heading.append(title);
      const grid = element("div", "field-grid");
      for (const definition of group.fields) {
        grid.append(definition.secret ? secretField(definition, pending[definition.secret]) : configField(definition));
        if (group.models && definition.path === "llm.model") grid.append(modelPicker);
      }
      card.append(heading, grid);
      if (group.autoReply) {
        autoReplyBudgetControl = element("p", "hint"); autoReplyBudgetControl.id = "auto-reply-budget";
        autoReplyBudgetControl.setAttribute("role", "status"); autoReplyBudgetControl.setAttribute("aria-live", "polite"); autoReplyBudgetControl.setAttribute("aria-atomic", "true");
        card.append(autoReplyBudgetControl);
      }
      if (group.embedding) {
        const row = element("div", "field full"), button = element("button", "secondary", "测试文字和图片向量"), message = element("p", "hint");
        button.type = "button"; button.id = "embedding-test"; message.id = "embedding-test-message";
        button.addEventListener("click", () => { void testEmbedding(); });
        row.append(button, message); card.append(row); embeddingControls = { button, message };
      }
      if (group.search) {
        const row = element("div", "field full"), button = element("button", "secondary", "测试搜索连接"), message = element("p", "hint");
        button.type = "button"; button.id = "search-test"; message.id = "search-test-message";
        button.addEventListener("click", () => { void testSearch(); });
        row.append(button, message); card.append(row); searchControls = { button, message };
      }
      if (group.personas) card.append(personaManager); $("config-groups").append(card);
    });
    renderPluginConfig(pending);
    filterConfig(state.configCategory);
    const web = state.config.web || {};
    $("web-readonly").textContent = `管理服务：${String(web.listen || "未配置")} · Secure Cookie：${web.secure_cookie ? "开启" : "关闭"} · web 分组只读；监听地址、数据库路径和日志过滤级别的变更需要重启整个程序。`;
    renderPersonaLibrary(); renderModels(); renderEmbeddingTest(); renderSearchTest(); refreshToolPickers(); renderAutoReplyBudget(); updateControls();
  }

  function readForm() {
    const config = clone(state.draft);
    for (const { control, definition: def } of inputs.values()) {
      if (def.type === "tools") { put(config, def.path, readToolPicker(control)); continue; }
      let value = control.value.trim();
      if (def.type === "persona") { value = control.value; requirePersona(config, value, def.label); }
      else if (def.type === "check") value = control.checked;
      else if (def.type === "number" || def.type === "nullableNumber") {
        if (def.type === "nullableNumber" && value === "") value = null;
        else {
          const numeric = Number(value);
          if (value === "" || !Number.isSafeInteger(numeric) || !control.validity.valid || (def.min !== undefined && numeric < def.min) || (def.max !== undefined && numeric > def.max)) {
            const range = def.min !== undefined && def.max !== undefined ? `${def.min}–${def.max} 范围内` : def.min !== undefined ? `不小于 ${def.min}` : "有效范围内";
            throw new Error(`「${def.label}」需要填写${range}的整数。`);
          }
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
    readPluginConfig(config);
    validatePersonaMap(config.plugins.ai.personas); validateImmutable(config); validateAutoReply(config); validateLearning(config); validateContext(config);
    return config;
  }

  function validateAutoReply(config) {
    const policy = get(config, "plugins.ai.auto_reply");
    if (!policy || typeof policy !== "object" || Array.isArray(policy)) throw new Error("自动接话配置不完整，请重新载入配置。");
    if (typeof policy.prompt !== "string" || !policy.prompt.trim() || [...policy.prompt.trim()].length > 2000) throw new Error("「接话策略」需要填写 1–2000 个字符，不能只含空白。");
    for (const def of autoReplyFields.filter((entry) => entry.type === "number")) {
      const value = get(config, def.path);
      if (!Number.isSafeInteger(value) || value < def.min || value > def.max) throw new Error(`「${def.label}」需要填写 ${def.min}–${def.max} 范围内的整数。`);
    }
    if (policy.max_wait_ms < policy.debounce_ms) throw new Error("「最长合并等待」不能短于「消息合并等待」。");
    if (policy.max_age_seconds * 1000 < policy.max_wait_ms) throw new Error("「待接话消息有效期」不能短于「最长合并等待」，请注意秒与毫秒的单位。");
    policy.prompt = policy.prompt.trim();
  }
  function validateLearning(config) {
    const learning = get(config, "plugins.ai.learning");
    if (!learning || typeof learning !== "object" || Array.isArray(learning)) throw new Error("学习笔记配置不完整，请重新载入配置。");
    for (const def of learningFields.filter((entry) => entry.type === "check")) {
      if (typeof get(config, def.path) !== "boolean") throw new Error(`「${def.label}」需要填写 true 或 false。`);
    }
    if (typeof learning.prompt !== "string" || [...learning.prompt.trim()].length > 4000) throw new Error("「管理员学习规则」最多 4000 个字符。");
    for (const def of learningFields.filter((entry) => entry.type === "number")) {
      const value = get(config, def.path);
      if (!Number.isSafeInteger(value) || value < def.min || value > def.max) throw new Error(`「${def.label}」需要填写 ${def.min}–${def.max} 范围内的整数。`);
    }
    if (config.limits.learning_requests_per_day < config.limits.learning_requests_per_hour) throw new Error("「每 24 小时学习请求上限」不能低于每小时上限。");
    learning.prompt = learning.prompt.trim();
  }

  function validateContext(config) {
    const policy = get(config, "plugins.ai.context");
    if (!policy || typeof policy.enabled !== "boolean") throw new Error("跨会话接续配置不完整，请重新载入配置。");
    for (const def of contextFields.filter((entry) => entry.type === "number")) {
      const value = get(config, def.path);
      if (!Number.isSafeInteger(value) || value < def.min || value > def.max) throw new Error(`「${def.label}」需要填写 ${def.min}–${def.max} 范围内的整数。`);
    }
    if (policy.neighbor_budget > policy.recall_budget) throw new Error("群聊背景预算不能超过近期问答预算。");
  }

  function configGroupCategory(group) {
    if (group.fields === contextFields) return "context";
    if (["学习笔记", "上下文与记忆"].includes(group.title)) return "knowledge";
    if (["人设", "会话与触发", "自动接话"].includes(group.title)) return "conversation";
    if (["模型接口", "向量接口（Embedding）", "Milky 消息接口", "QQ 登录管理"].includes(group.title)) return "connection";
    return "tools";
  }

  function filterConfig(category) {
    if (!["all", "connection", "context", "knowledge", "conversation", "tools"].includes(category)) return;
    state.configCategory = category;
    document.querySelectorAll("[data-config-group]").forEach((node) => { node.hidden = category !== "all" && node.dataset.configGroup !== category; });
    document.querySelectorAll("[data-config-category]").forEach((node) => {
      const active = node.dataset.configCategory === category;
      node.classList.toggle("active", active); node.setAttribute("aria-pressed", String(active));
    });
  }

  function openConfigSection(category) {
    filterConfig(category); showView("config");
    $("config-categories").scrollIntoView?.({ block: "start", behavior: "instant" });
  }

  function validateImmutable(config) {
    if (!config || typeof config !== "object" || Array.isArray(config)) throw new Error("配置必须是 JSON 对象。");
    if (canonical(config.web) !== canonical(state.config.web)) throw new Error("web 分组不能在线修改，请恢复此分组，或通过本机配置文件修改后重启管理服务。");
    for (const path of ["llm.api_key", "embedding.api_key", "plugins.web_search.api_key", "milky.access_token", "qq_login.token", "nonebot.token"]) {
      const value = get(config, path);
      if (value != null && value !== "") throw new Error(`请勿在 JSON 的 ${path} 中填入密钥；使用对应的密钥控件设置。`);
    }
    for (const key of Object.keys(state.config)) {
      if (!Object.hasOwn(config, key)) throw new Error(`完整 JSON 缺少 ${key} 分组；请保留全部配置字段。`);
    }
  }

  function secretChanges() {
    const changes = {};
    for (const [name, controls] of secretInputs) changes[name] = secretChange(controls);
    return changes;
  }

  function markDirty() {
    state.dirty = true;
    renderAutoReplyBudget();
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
    validatePersonaMap(get(config, "plugins.ai.personas"));
    validatePluginShape(config);
    // Validate represented fields before replacing the UI, so malformed nested
    // objects cannot silently disappear into the editable form.
    for (const group of groups) for (const def of group.fields) {
      if (def.secret) continue;
      const value = get(config, def.path);
      const valid = def.type === "check" ? typeof value === "boolean"
        : def.type === "number" ? Number.isSafeInteger(value)
          : def.type === "nullableNumber" ? value === null || Number.isSafeInteger(value)
            : def.type === "ids" ? Array.isArray(value) && value.every((entry) => Number.isSafeInteger(entry) && entry > 0)
              : ["lines", "tools"].includes(def.type) ? Array.isArray(value) && value.every((entry) => typeof entry === "string")
                : def.type === "json" ? value && typeof value === "object" && !Array.isArray(value) && Object.values(value).every((entry) => typeof entry === "string")
                  : typeof value === "string";
      if (!valid) throw new Error(`JSON 字段 ${def.path} 的类型不正确。`);
      if (def.type === "select" && !def.choices.some(([key]) => key === value)) throw new Error(`JSON 字段 ${def.path} 的选项无效。`);
    }
    validateAutoReply(config); validateLearning(config); validateContext(config);
    const pending = secretSnapshot();
    state.draft = config; state.jsonDirty = false; state.formInvalid = false; state.dirty = true;
    renderFields(pending); syncJson();
  }

  function configApplyFailure() {
    if (state.applyError) return state.applyError;
    if (state.applyFailure?.revision === state.revision) return state.applyFailure.message;
    return state.status?.phase === "error" ? String(state.status.message || "机器人启动失败。") : "";
  }
  function renderConfigState() {
    const failure = configApplyFailure(), unsaved = state.dirty || state.jsonDirty;
    const applied = state.revision && state.status?.applied_revision === state.revision && ["running", "waiting_login"].includes(state.status?.phase);
    const pending = state.pendingOperation?.kind === "runtime" && state.pendingOperation.goal === "running";
    const label = !state.config ? "正在读取配置" : unsaved ? "未保存" : state.stale ? "保存状态待核对" : failure ? state.applyError ? "不可应用" : "应用失败" : applied && !pending ? "已应用" : "已保存待应用";
    let detail = !state.config ? "等待服务器返回配置。" : unsaved ? "当前修改仅保留在本页；保存和应用成功后才会生效。"
      : pending ? "配置已保存，应用结果尚未确认；请查询结果，勿重复提交。"
        : failure ? "配置已保存但未生效。修正下方问题后保存并应用。"
          : applied ? "当前机器人已应用此版本。" : "已保存到文件，当前机器人尚未应用此版本。";
    if (state.status?.applied_revision) detail += " 当前运行版本：" + state.status.applied_revision;
    $("dirty-label").textContent = label; $("config-state").textContent = label;
    $("config-state").className = "badge " + (failure ? "bad" : applied && !unsaved && !pending ? "good" : "wait");
    $("config-state-detail").textContent = detail;
    $("config-apply-error").hidden = !failure;
    $("config-apply-error").textContent = failure ? "已保存配置的应用问题：" + failure + "\n请按提示修正后保存并应用；当前勾选不代表插件已经加载。" : "";
    $("revision-state").textContent = label;
    $("revision-detail").textContent = detail;
  }
  function renderPluginLoadedStates() {
    const loaded = Array.isArray(state.status?.loaded_plugins) ? new Set(state.status.loaded_plugins) : null;
    document.querySelectorAll("[data-plugin-loaded]").forEach((node) => {
      const active = loaded?.has(node.dataset.pluginLoaded);
      const external = state.pluginCatalog.some((entry) => entry.id === node.dataset.pluginLoaded && entry.runtime === "nonebot") || pluginControls?.external.has(node.dataset.pluginLoaded);
      node.textContent = active == null ? "当前运行状态待确认" : active ? external ? "当前运行已启用桥接；Python 连接另行确认" : "当前运行已启用" : "当前运行未启用";
      node.className = "plugin-load-state " + (active ? "loaded" : "not-loaded");
    });
  }
  function configSaveFeedback(message = "", kind = "", phase = "") {
    state.configSave = { message, kind, phase }; updateControls();
  }
  function renderConfigSave() {
    const saving = state.configSave, pending = state.pendingOperation;
    const checking = pending && ["config", "runtime"].includes(pending.kind);
    const message = checking ? pending.label + "的响应未确认，请查询结果；不会重复提交。" + (saving.kind === "error" ? " " + saving.message : "")
      : state.stale ? saving.message || "保存状态待核对。请先保留当前编辑，再重新载入服务器配置。" : saving.message;
    const node = $("config-save-message"); node.textContent = message;
    node.hidden = !message; node.className = "config-save-message" + (checking || state.stale ? " warning-text" : saving.kind ? " " + saving.kind + "-text" : "");
    $("config-check-operation").hidden = !checking;
    $("config-check-operation").disabled = state.busy || !state.authenticated;
    for (const id of ["save-config", "save-config-top", "save-apply"]) {
      $(id).textContent = saving.phase || (id === "save-apply" ? "保存并应用" : "保存当前配置");
      $(id).setAttribute("aria-busy", String(Boolean(saving.phase)));
      $(id).setAttribute("aria-describedby", "config-save-message");
    }
  }
  function updateControls() {
    const unavailable = state.busy || !state.authenticated || !state.config;
    const pending = Boolean(state.pendingOperation);
    document.querySelectorAll("[data-action]").forEach((node) => { node.disabled = unavailable; });
    document.querySelectorAll("[data-memory-write]").forEach((node) => { node.disabled = unavailable || pending || state.memory.conflict || state.memory.settingsDirty; });
    $("config-fields").disabled = unavailable || state.jsonDirty || pending;
    $("config-json").disabled = unavailable || pending || (!state.jsonDirty && state.formInvalid);
    renderConfigState(); renderPluginLoadedStates();
    for (const id of ["save-config", "save-config-top", "save-apply"]) $(id).disabled = unavailable || pending || state.stale;
    const modelsBlocked = unavailable || state.jsonDirty || pending || state.stale;
    $("fetch-models").disabled = modelsBlocked || state.models.loading;
    $("model-list").disabled = modelsBlocked; $("manual-model").disabled = modelsBlocked;
    if (embeddingControls) embeddingControls.button.disabled = modelsBlocked || state.embedding.loading;
    if (searchControls) searchControls.button.disabled = modelsBlocked || state.search.loading;
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
    const personaBlocked = unavailable || state.jsonDirty || pending;
    const customPersona = state.draft?.plugins?.ai?.personas && Object.hasOwn(state.draft.plugins.ai.personas, state.personaSelected);
    $("persona-list").disabled = personaBlocked; $("persona-new-name").disabled = personaBlocked;
    $("persona-text").disabled = personaBlocked; $("persona-text").readOnly = !customPersona;
    $("persona-new").disabled = personaBlocked; $("persona-copy").disabled = personaBlocked || !personaChoices(state.draft).has(state.personaSelected);
    $("persona-delete").disabled = personaBlocked || !customPersona;
    $("group-persona-select").disabled = unavailable || pending || state.groupPersona.saving;
    $("apply-group-persona").disabled = unavailable || pending || state.groupPersona.saving || !personaChoices(state.config).has($("group-persona-select").value);
    $("apply-group-persona").textContent = state.groupPersona.saving ? "正在处理…" : "应用到全部群聊";
    $("group-persona-message").textContent = state.groupPersona.message || "点击应用时会核对 QQ 账号与群数量，再确认操作范围。";
    $("group-persona-message").className = "hint group-persona-message" + (state.groupPersona.kind === "error" ? " inline-error" : "");
    for (const control of [inputs.get("plugins.ai.defaults.allowed_tools")?.control, settingInputs.get("allowed_tools")?.input]) if (control) updateToolPicker(control);
    updateContactControls();
    updateStickerControls();
    updateLearningControls();
    renderConfigSave();
  }

  async function loadConfig() {
    const result = await api("/api/config");
    if (!result.config || typeof result.revision !== "string") throw new Error("配置响应缺少 config 或 revision。");
    if (result.apply_error != null && typeof result.apply_error !== "string") throw new Error("配置应用校验结果无效。");
    validatePersonaMap(result.persona_templates || {}, false); validatePersonaMap(get(result.config, "plugins.ai.personas"), false);
    validateToolCatalog(result.tool_catalog ?? []); state.toolCatalog = clone(result.tool_catalog ?? []);
    state.personaTemplates = clone(result.persona_templates || {});
    validatePluginCatalog(result.plugin_catalog || []); state.pluginCatalog = clone(result.plugin_catalog || []);
    state.config = clone(result.config); state.draft = clone(result.config);
    const baseEstimate = result.auto_reply_budget?.base_estimate;
    state.autoReplyBaseEstimate = Number.isSafeInteger(baseEstimate) && baseEstimate >= 0 ? baseEstimate : null;
    state.revision = result.revision; state.secrets = result.secrets || {}; state.applyError = result.apply_error || null;
    if (state.applyFailure?.revision !== state.revision) state.applyFailure = null;
    state.dirty = false; state.jsonDirty = false; state.formInvalid = false; state.stale = false;
    if (!state.configSave.phase) state.configSave = { phase: "", message: "", kind: "" };
    resumeQqPolling(); clearQr();
    $("config-warning").hidden = true;
    $("saved-revision").textContent = "已保存版本：" + state.revision;
    renderFields(); refreshPersonaSelectors(); syncJson(); renderStatus(); renderQq(); scheduleQq(0);
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
    if (state.learning.snapshot && status.online && status.bot_id != null && state.learning.snapshot.bot_id !== String(status.bot_id)) { invalidateLearning(); state.learning.conflict = Boolean(state.learning.dirty); state.learning.message = "机器人账号已变化，请刷新学习笔记。"; renderLearning(); }
    if (state.context.snapshot && status.online && status.bot_id != null && state.context.snapshot.bot_id !== String(status.bot_id)) { invalidateContextOverview(); state.context.message = "机器人账号已变化，请刷新接续记录。"; renderContextOverview(); }
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
    if (state.applyFailure?.revision === state.revision && status.applied_revision === state.revision && ["running", "waiting_login"].includes(status.phase)) state.applyFailure = null;
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
      ["学习笔记", String(result.used?.learning ?? 0) + " / " + String(result.limits?.learning_requests_per_hour ?? "—"), "最近 1 小时 · 第三个独立额度池；滚动 24 小时 " + String(result.used?.learning_day ?? 0) + " / " + String(result.limits?.learning_requests_per_day ?? "—")],
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
    try {
      const result = await api("/api/status");
      if (session !== state.session || !state.authenticated) return;
      state.status = result; renderStatus(); if (manual) notice("运行状态已更新。");
    }
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
    const previous = state.qq;
    if (manual) resumeQqPolling();
    if (state.qqPolling || state.qqPollingPaused) return;
    const version = state.qqQueryVersion, session = state.session; state.qqPolling = true;
    try {
      const result = await api("/api/qq/status" + (manual ? "?force=true" : ""));
      if (version !== state.qqQueryVersion) return;
      if (state.contacts.botId) {
        if (result.online && String(result.uin) !== state.contacts.botId) invalidateContacts("QQ 账号已变化，请刷新联系人列表。", "offline");
        else if (!result.online && previous?.online && String(previous.uin) === state.contacts.botId) {
          const c = state.contacts;
          if (c.saving) invalidateContacts("QQ 已离线，开关结果需要恢复连接后刷新确认。", "offline");
          else { c.version++; c.loading = false; c.phase = "cached"; c.message = "QQ 已离线，显示该账号的上次列表；仍可关闭 Bot，恢复连接并刷新后才能开启。"; renderContacts(); }
        }
      }
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
    invalidateScopePlugins();
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
    document.querySelectorAll("[data-contact-toggle], [data-contact-auto-reply]").forEach((input) => { input.disabled = blocked || (c.phase !== "ready" && !(c.phase === "cached" && input.checked)) || c.loading || Boolean(c.saving); });
    document.querySelectorAll("[data-contact-plugins]").forEach((input) => { input.disabled = blocked || c.phase !== "ready" || c.loading || Boolean(c.saving); });
    updateScopePluginControls();
  }
  function renderContacts() {
    const c = state.contacts, name = c.scene === "group" ? "群聊" : "好友";
    $("contacts-account").textContent = c.botId ? (c.phase === "cached" ? "上次读取 QQ " : "QQ ") + c.botId : "尚未读取";
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
      label.append(input, element("span", "contact-switch-track"), element("span", "contact-switch-label", c.saving === scene + ":" + row.id ? "保存中…" : row.enabled ? "Bot 已开启" : "Bot 已关闭"));
      const autoLabel = element("label", "contact-switch contact-auto-reply"), autoInput = document.createElement("input");
      autoInput.type = "checkbox"; autoInput.checked = row.auto_reply_enabled; autoInput.setAttribute("role", "switch"); autoInput.dataset.contactAutoReply = row.id;
      autoInput.setAttribute("aria-label", title + "（" + row.id + "）自动接话开关");
      autoInput.addEventListener("change", () => { if (version === state.contacts.version) void toggleContact(scene, row.id, autoInput.checked, true); });
      autoLabel.append(autoInput, element("span", "contact-switch-track"), element("span", "contact-switch-label", c.saving === "auto:" + scene + ":" + row.id ? "保存中…" : row.auto_reply_enabled ? "自动接话 开" : "自动接话 关"));
      const plugins = element("button", "text-button accent contact-plugins", "插件"); plugins.type = "button"; plugins.dataset.contactPlugins = row.id;
      plugins.setAttribute("aria-label", title + "（" + row.id + "）插件开关");
      plugins.addEventListener("click", () => { if (version === state.contacts.version) void openScopePlugins(scene, row.id, title); });
      const actions = element("div", "contact-actions"); actions.append(plugins, autoLabel, label);
      item.append(identity, actions); list.append(item);
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
        if (!row || !contactId(row.id) || typeof row.name !== "string" || typeof row.remark !== "string" || typeof row.enabled !== "boolean" || typeof row.auto_reply_enabled !== "boolean" || seen.has(row.id)) throw new Error("联系人数据不完整或存在重复 ID，请刷新重试。");
        seen.add(row.id);
      }
    }
  }
  function invalidateScopePlugins() {
    const scope = state.scopePlugins; scope.version++; scope.query = null; scope.rows = []; scope.loading = false; scope.saving = false; scope.uncertain = false;
    $("scope-plugins-list").replaceChildren(); $("scope-plugins-message").textContent = "";
    if ($("scope-plugins-dialog").open) $("scope-plugins-dialog").close();
  }
  function updateScopePluginControls() {
    const s = state.scopePlugins, blocked = !state.authenticated || state.busy || s.loading || s.saving;
    $("refresh-scope-plugins").disabled = blocked || !s.query;
    document.querySelectorAll("[data-scope-plugin]").forEach((input) => { input.disabled = blocked || s.uncertain; });
  }
  function validateScopePlugins(result, query) {
    if (!result || result.bot_id !== query.bot_id || result.scene !== query.scene || result.peer_id !== query.peer_id || !Array.isArray(result.plugins) || result.plugins.length > 64) throw new Error("插件列表不属于当前选择的账号和会话。");
    const seen = new Set();
    for (const row of result.plugins) {
      if (!row || typeof row.id !== "string" || typeof row.name !== "string" || typeof row.description !== "string" || typeof row.enabled !== "boolean" || typeof row.globally_enabled !== "boolean" || !Number.isSafeInteger(row.revision) || row.revision < 0 || seen.has(row.id)) throw new Error("插件开关数据无效或重复。");
      seen.add(row.id);
    }
  }
  function renderScopePlugins() {
    const s = state.scopePlugins, list = $("scope-plugins-list"); list.replaceChildren();
    for (const row of s.rows) {
      const label = element("label", "plugin-option"), input = document.createElement("input"), copy = element("span");
      input.type = "checkbox"; input.checked = row.enabled; input.dataset.scopePlugin = row.id;
      input.setAttribute("aria-label", row.name + "在此会话启用");
      const inactive = row.group_only && s.query?.scene !== "group" ? " · 仅群聊生效" : "";
      const availability = element("span", "plugin-load-state " + (row.globally_enabled ? "loaded" : "not-loaded"),
        (row.enabled ? "会话已允许" : "会话已禁止") + " · " + (row.globally_enabled ? "当前运行已启用" : "当前运行未启用"));
      copy.append(element("strong", "", row.name + inactive), availability, element("span", "hint", row.description + (row.runtime === "nonebot" ? " 桥接启用不代表 Python 服务已连接。" : "")));
      input.addEventListener("change", () => { void toggleScopePlugin(row.id, input.checked); });
      label.append(input, copy); list.append(label);
    }
    updateScopePluginControls();
  }
  async function openScopePlugins(scene, peerId, title) {
    invalidateScopePlugins();
    state.scopePlugins.query = { bot_id: state.contacts.botId, scene, peer_id: peerId };
    $("scope-plugins-title").textContent = title + " · 插件";
    $("scope-plugins-scope").textContent = `QQ ${state.contacts.botId} · ${scene === "group" ? "群" : "好友"} ${peerId}`;
    $("scope-plugins-dialog").showModal(); await loadScopePlugins();
  }
  async function loadScopePlugins() {
    const s = state.scopePlugins;
    if (!s.query || s.loading || s.saving || !state.authenticated) return;
    const query = clone(s.query), version = ++s.version, session = state.session;
    s.loading = true; $("scope-plugins-message").textContent = "正在读取当前会话的插件开关…"; updateScopePluginControls();
    try {
      const result = await api("/api/plugins/scope?" + new URLSearchParams(query).toString());
      if (version !== s.version || session !== state.session || !$("scope-plugins-dialog").open) return;
      validateScopePlugins(result, query); s.rows = result.plugins; s.uncertain = false;
      $("scope-plugins-message").textContent = "会话默认允许插件；取消勾选会立即记入黑名单。允许使用不等于当前运行已启用，未启用的插件需先保存并应用全局配置。";
    } catch (error) {
      if (version !== s.version || session !== state.session) return;
      s.uncertain = true; $("scope-plugins-message").textContent = error.message;
    } finally {
      if (version === s.version && session === state.session) { s.loading = false; renderScopePlugins(); }
    }
  }
  async function toggleScopePlugin(plugin, enabled) {
    const s = state.scopePlugins, row = s.rows.find((item) => item.id === plugin);
    if (!s.query || !row || s.loading || s.saving || s.uncertain || !state.authenticated || state.busy) return;
    const query = clone(s.query), version = s.version, session = state.session;
    s.saving = true; updateScopePluginControls(); $("scope-plugins-message").textContent = "正在保存插件开关…";
    try {
      const result = await api("/api/plugins/scope", "POST", { ...query, plugin, enabled, revision: row.revision });
      if (version !== s.version || session !== state.session || !$("scope-plugins-dialog").open) return;
      validateScopePlugins(result, query); s.rows = result.plugins;
      const current = s.rows.find((item) => item.id === plugin);
      $("scope-plugins-message").textContent = !enabled ? `${row.name} 已在此会话禁止，开关已保存。`
        : `${row.name} 已允许在此会话使用，开关已保存。` + (current?.globally_enabled ? "当前运行已启用。" + (current.runtime === "nonebot" ? " Python 连接需另行确认。" : "")
          : "但当前运行未启用；请先修正配置问题，再保存并应用全局配置。" + (configApplyFailure() ? " " + configApplyFailure() : ""));
    } catch (error) {
      if (version !== s.version || session !== state.session) return;
      s.uncertain = true;
      $("scope-plugins-message").textContent = (uncertain(error) ? "保存结果尚未确认，不会重复提交。请点「刷新核对」查看服务器状态。 " : "开关未能确认，请刷新核对后再操作。 ") + error.message;
    } finally {
      if (version === s.version && session === state.session) { s.saving = false; renderScopePlugins(); }
    }
  }
  async function loadContacts(force = false) {
    const c = state.contacts;
    if (!state.authenticated || state.view !== "contacts" || c.loading || c.saving) return;
    const session = state.session, version = ++c.version;
    const previous = ["ready", "cached"].includes(c.phase) && c.botId ? { botId: c.botId, groups: c.groups, friends: c.friends, fetchedAt: c.fetchedAt, generation: c.generation } : null;
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
      if (previous && previous.generation === c.generation && ([502, 503].includes(error.status) || error.status === 0)) {
        Object.assign(c, previous); c.phase = "cached";
        c.message = "连接暂不可用，显示该账号的上次列表；仍可关闭 Bot 或自动接话，恢复连接并刷新后才能开启。";
      }
    } finally {
      if (contactsCurrent(version, session)) { c.loading = false; renderContacts(); }
    }
  }
  async function toggleContact(scene, peerId, enabled, autoReply = false) {
    const c = state.contacts;
    if (state.view !== "contacts" || !state.authenticated || state.busy || (c.phase !== "ready" && !(c.phase === "cached" && enabled === false)) || c.loading || c.saving || typeof enabled !== "boolean") { renderContacts(); return; }
    const row = (scene === "group" ? c.groups : scene === "friend" ? c.friends : []).find((item) => item.id === peerId);
    const field = autoReply ? "auto_reply_enabled" : "enabled";
    if (!row || row[field] === enabled) return;
    const version = c.version, session = state.session, botId = c.botId, previous = row[field];
    c.saving = (autoReply ? "auto:" : "") + scene + ":" + peerId; c.message = ""; row[field] = enabled; renderContacts();
    try {
      const result = await api(autoReply ? "/api/qq/contacts/auto-reply" : "/api/qq/contacts/toggle", "POST", { bot_id: botId, scene, peer_id: peerId, enabled });
      if (!contactsCurrent(version, session)) return;
      if (result.bot_id !== botId || result.scene !== scene || result.peer_id !== peerId || result.enabled !== enabled) throw new Error("开关响应与当前账号或会话不一致。");
      c.message = `${row.remark || row.name || row.id} 的${autoReply ? "自动接话" : " Bot"}已${enabled ? "开启" : "关闭"}，即时生效。` + (autoReply ? (enabled ? " AI 可根据对话选择接话或保持安静；Bot 和 AI 插件也需开启。" : scene === "friend" ? " 私聊仍可正常直接对话。" : " 群聊仍可通过 @、关键词或已开启的引用回复触发。") : "") + (c.phase === "cached" ? " 当前显示上次列表；恢复连接并刷新后才能开启。" : "");
    } catch (error) {
      if (!contactsCurrent(version, session)) return;
      row[field] = previous;
      if (error.status === 409) { invalidateContacts("QQ 账号或联系人已变化，旧列表已停用，请刷新后再操作。", "error"); return; }
      c.phase = "uncertain"; c.message = "开关未确认，已还原页面显示。请刷新列表核对实际状态。 " + error.message;
      if (error.code === "csrf_invalid") report(error);
    } finally {
      if (contactsCurrent(version, session)) { c.saving = null; renderContacts(); }
    }
  }
  const stickerScope = (value) => typeof value === "string" && /^[1-9]\d*:group:[1-9]\d*$/u.test(value);
  const stickerInteger = (value, minimum = 0) => Number.isSafeInteger(value) && value >= minimum;
  const stickerBytes = (value) => value >= 1048576 ? `${(value / 1048576).toFixed(1)} MB` : value >= 1024 ? `${(value / 1024).toFixed(1)} KB` : `${value} B`;
  const stickerScopeLabel = (scope) => { const [bot, , group] = scope.split(":"); return `群 ${group} · 机器人 ${bot}`; };
  function stickerCurrent(request) {
    return state.authenticated && state.session === request.session && state.view === "stickers" && state.stickers.version === request.version
      && (request.scope === undefined || state.stickers.scope === request.scope);
  }
  function stickerImageUrl(item, scope, variant) {
    return `/api/stickers/items/${item.id}/image?` + new URLSearchParams({ scope, variant, revision: String(item.revision) });
  }
  function closeStickerPreview() {
    state.stickers.preview = null; $("sticker-preview-image").removeAttribute("src"); $("sticker-preview-image").hidden = true;
    $("sticker-preview-detail").textContent = ""; $("sticker-preview-message").textContent = "";
    if ($("sticker-preview-dialog").open) $("sticker-preview-dialog").close();
  }
  function clearStickerCards() {
    document.querySelectorAll(".sticker-image").forEach((image) => image.removeAttribute("src"));
    $("sticker-grid").replaceChildren(); closeStickerPreview();
  }
  function invalidateStickers(reset = false) {
    const s = state.stickers;
    s.version++; s.groupRequest++; s.itemRequest++; s.groupsLoading = false; s.loading = false;
    s.rows = []; s.index = null; s.message = ""; s.kind = ""; clearStickerCards();
    if (reset) { s.groups = []; s.scope = ""; s.groupPage = 1; s.totalGroupPages = 1; s.page = 1; s.totalPages = 1; s.total = 0; s.operation = null; renderStickerGroups(); }
    renderStickerStatus();
  }
  function updateStickerControls() {
    const s = state.stickers, blocked = !state.authenticated || state.busy;
    $("refresh-stickers").disabled = blocked || s.groupsLoading || s.loading;
    $("sticker-group").disabled = blocked || s.groupsLoading;
    $("reindex-sticker-group").disabled = blocked || s.loading || !s.scope || !s.total || Boolean(s.operation);
    $("stickers-prev").disabled = blocked || s.loading || !s.scope || s.page <= 1;
    $("stickers-next").disabled = blocked || s.loading || !s.scope || s.page >= s.totalPages;
    $("sticker-groups-prev").disabled = blocked || s.groupsLoading || s.groupPage <= 1;
    $("sticker-groups-next").disabled = blocked || s.groupsLoading || s.groupPage >= s.totalGroupPages;
    document.querySelectorAll("[data-sticker-write]").forEach((node) => { node.disabled = blocked || s.loading || Boolean(s.operation); });
    document.querySelectorAll("[data-sticker-preview]").forEach((node) => { node.disabled = blocked || s.loading; });
    $("retry-sticker-operation").disabled = blocked || !s.operation || s.operation.sending;
  }
  function renderStickerGroups() {
    const s = state.stickers, select = $("sticker-group"); select.replaceChildren();
    const empty = element("option", "", s.groups.length ? "请选择群" : "当前没有已保存图片的群"); empty.value = ""; select.append(empty);
    if (s.scope && !s.groups.some((group) => group.scope === s.scope)) { const selected = element("option", "", `当前选择：${stickerScopeLabel(s.scope)}`); selected.value = s.scope; select.append(selected); }
    for (const group of s.groups) { const option = element("option", "", `${stickerScopeLabel(group.scope)} · ${group.count} 张`); option.value = group.scope; select.append(option); }
    select.value = s.scope; $("sticker-group-pagination").hidden = s.totalGroupPages <= 1;
    $("sticker-groups-page").textContent = `群列表第 ${s.groupPage} / ${s.totalGroupPages} 页`;
  }
  function renderStickerStatus() {
    const s = state.stickers, group = s.groups.find((item) => item.scope === s.scope);
    $("sticker-summary").textContent = group ? `${stickerScopeLabel(group.scope)} · ${group.count} 张 · ${stickerBytes(group.bytes)} · 待处理 ${group.pending} · 失败 ${group.failed}` : s.scope ? `${stickerScopeLabel(s.scope)} · ${s.total} 张` : "选择群后查看图片。";
    $("sticker-index-info").textContent = s.index ? (s.index.enabled ? `向量模型：${s.index.model} · ${s.index.dimensions} 维。` : "向量服务未启用。")
      + (s.running && s.index.enabled ? " 重建任务由后台逐张处理，刷新可查看进度。" : " 重建请求可排队，机器人和向量服务运行后处理。") : "";
    $("sticker-message").textContent = s.message || (s.groupsLoading ? "正在读取群列表…" : s.loading ? "正在读取图片…" : (s.scope && !s.rows.length) || (!s.scope && !s.groups.length) ? "本群暂无图片；在群内发送 QQ 表情可自动收集，普通图片请引用并发送 /save。" : "点击图片查看原图；GIF 可在预览中播放。");
    $("sticker-message").className = s.kind === "error" ? "inline-error" : "muted";
    $("stickers-page").textContent = s.scope ? `图片第 ${s.page} / ${s.totalPages} 页 · 共 ${s.total} 张` : "尚未选择群";
    const operation = s.operation; $("sticker-operation").hidden = !operation;
    $("sticker-operation-text").textContent = operation ? `${stickerScopeLabel(operation.payload.scope)}：${operation.action === "delete" ? "删除图片" : "重建索引"}${operation.sending ? "正在提交…" : "结果尚未确认。点击核对会沿用原操作号，不会创建重复操作。"} 操作号：${operation.payload.request_id}` : "";
    updateStickerControls();
  }
  async function loadStickerGroups(page = state.stickers.groupPage) {
    if (!state.authenticated || state.view !== "stickers" || !stickerInteger(page, 1)) return false;
    const s = state.stickers, request = { session: state.session, version: s.version, sequence: ++s.groupRequest };
    s.groupsLoading = true; s.message = ""; s.kind = ""; renderStickerStatus();
    try {
      const result = await api("/api/stickers/groups?" + new URLSearchParams({ page: String(page) }));
      if (!stickerCurrent(request) || request.sequence !== s.groupRequest) return false;
      if (!Array.isArray(result.groups) || result.groups.length > 20 || result.page !== page || !stickerInteger(result.total) || !stickerInteger(result.total_pages)
        || result.groups.some((group) => !stickerScope(group.scope) || group.scope !== `${group.bot_id}:group:${group.group_id}` || [group.count, group.bytes, group.pending, group.failed].some((value) => !stickerInteger(value)))
        || new Set(result.groups.map((group) => group.scope)).size !== result.groups.length) throw new Error("图库群列表格式无效，请刷新核对。");
      if (page > Math.max(1, result.total_pages)) return await loadStickerGroups(Math.max(1, result.total_pages));
      s.groups = result.groups; s.groupPage = page; s.totalGroupPages = Math.max(1, result.total_pages);
      renderStickerGroups(); return true;
    } catch (error) {
      if (!stickerCurrent(request) || request.sequence !== s.groupRequest) return false;
      s.message = error.message; s.kind = "error"; if (["reauth_required", "csrf_invalid"].includes(error.code)) report(error); return false;
    } finally { if (state.session === request.session && request.sequence === s.groupRequest) { s.groupsLoading = false; renderStickerStatus(); } }
  }
  async function selectStickerGroup(scope) {
    if (!state.authenticated || state.view !== "stickers" || (scope && scope !== state.stickers.scope && !state.stickers.groups.some((group) => group.scope === scope))) return;
    const s = state.stickers; s.version++; s.itemRequest++; s.loading = false; s.scope = scope; s.rows = []; s.page = 1; s.totalPages = 1; s.total = 0; s.index = null; s.message = ""; s.kind = "";
    clearStickerCards(); $("sticker-group").value = scope; renderStickerStatus();
    if (scope) await loadStickerItems(1);
  }
  async function changeStickerGroupPage(page) {
    if (state.stickers.groupsLoading || page < 1 || page > state.stickers.totalGroupPages) return;
    await loadStickerGroups(page);
  }
  async function refreshStickers() {
    if (!state.authenticated || state.view !== "stickers") return;
    const session = state.session;
    if (await loadStickerGroups() && session === state.session && state.stickers.scope) await loadStickerItems(state.stickers.page);
  }
  async function loadStickerItems(page = state.stickers.page) {
    const s = state.stickers;
    if (!state.authenticated || state.view !== "stickers" || !stickerScope(s.scope) || !stickerInteger(page, 1)) return;
    const request = { session: state.session, version: s.version, scope: s.scope, sequence: ++s.itemRequest };
    s.loading = true; s.rows = []; s.message = ""; s.kind = ""; clearStickerCards(); renderStickerStatus();
    try {
      const result = await api("/api/stickers/items?" + new URLSearchParams({ scope: request.scope, page: String(page) }));
      if (!stickerCurrent(request) || request.sequence !== s.itemRequest) return;
      if (result.scope !== request.scope || !Array.isArray(result.items) || result.items.length > 20 || result.page !== page || !stickerInteger(result.total) || !stickerInteger(result.total_pages)
        || typeof result.running !== "boolean" || typeof result.index?.enabled !== "boolean" || typeof result.index?.model !== "string" || !stickerInteger(result.index?.dimensions, 1)
        || result.items.some((item) => !stickerInteger(item.id, 1) || !stickerInteger(item.revision, 1) || !stickerInteger(item.bytes) || !stickerInteger(item.created_at) || !stickerInteger(item.updated_at)
          || !["image/jpeg", "image/png", "image/gif", "image/webp"].includes(item.mime) || !["ready", "pending", "failed"].includes(item.index_status) || (item.scope !== undefined && item.scope !== request.scope))
        || new Set(result.items.map((item) => item.id)).size !== result.items.length) throw new Error("图片列表范围或格式无效，未显示图片，请刷新核对。");
      if (page > Math.max(1, result.total_pages)) { await loadStickerItems(Math.max(1, result.total_pages)); return; }
      s.rows = result.items; s.page = page; s.total = result.total; s.totalPages = Math.max(1, result.total_pages); s.index = result.index; s.running = result.running;
      renderStickerCards(request);
    } catch (error) {
      if (!stickerCurrent(request) || request.sequence !== s.itemRequest) return;
      s.message = error.message; s.kind = "error"; if (["reauth_required", "csrf_invalid"].includes(error.code)) report(error);
    } finally { if (stickerCurrent(request) && request.sequence === s.itemRequest) { s.loading = false; renderStickerStatus(); } }
  }
  function renderStickerCards(request) {
    const grid = $("sticker-grid"); grid.replaceChildren();
    for (const item of state.stickers.rows) {
      const card = element("article", "sticker-card"), preview = element("button", "sticker-thumbnail"), image = element("img", "sticker-image");
      preview.type = "button"; preview.dataset.stickerPreview = String(item.id); preview.setAttribute("aria-label", `预览图片 ${item.id}`);
      image.alt = `表情 ${item.id} 的缩略图`; image.loading = "lazy"; image.decoding = "async"; image.referrerPolicy = "no-referrer";
      image.src = stickerImageUrl(item, request.scope, "thumbnail"); preview.append(image);
      const unavailable = element("span", "hint", "缩略图无法读取，请刷新核对。"); unavailable.hidden = true; preview.append(unavailable);
      image.addEventListener("error", () => { if (stickerCurrent(request) && request.sequence === state.stickers.itemRequest && image.isConnected) { image.hidden = true; unavailable.hidden = false; } });
      preview.addEventListener("click", () => openStickerPreview(item));
      const body = element("div", "sticker-card-body"), header = element("div", "card-top");
      header.append(element("strong", "", `图片 #${item.id}`), element("span", `badge ${item.index_status === "ready" ? "good" : item.index_status === "failed" ? "bad" : "wait"}`, { ready: "索引就绪", pending: "等待索引", failed: "索引失败" }[item.index_status]));
      body.append(header, element("p", "hint", `${item.mime.slice(6).toUpperCase()} · ${stickerBytes(item.bytes)}`), element("p", "hint", "入库 " + dateText(item.created_at)));
      if (item.source_available === false) body.append(element("p", "hint danger-text", "来源已失效，不能用于回复；可删除清理。"));
      const actions = element("div", "button-row");
      for (const [action, label, style] of [["reindex", "重建索引", "accent"], ["delete", "删除图片", "danger-text"]]) {
        const button = element("button", `text-button ${style}`, label); button.type = "button"; button.dataset.stickerWrite = action; button.dataset.stickerId = String(item.id);
        button.addEventListener("click", () => { void mutateSticker(action, item); }); actions.append(button);
      }
      body.append(actions); card.append(preview, body); grid.append(card);
    }
    updateStickerControls();
  }
  function openStickerPreview(item) {
    const s = state.stickers;
    if (!state.authenticated || state.view !== "stickers" || s.loading || !s.rows.some((row) => row.id === item.id && row.revision === item.revision)) return;
    closeStickerPreview();
    const preview = { session: state.session, version: s.version, scope: s.scope, id: item.id, revision: item.revision }; s.preview = preview;
    const image = $("sticker-preview-image"); image.hidden = false; image.referrerPolicy = "no-referrer"; image.alt = `群 ${s.scope.split(":")[2]} 的表情 ${item.id}`;
    $("sticker-preview-title").textContent = `原图预览 · #${item.id}`;
    $("sticker-preview-detail").textContent = `${stickerScopeLabel(s.scope)} · ${item.mime.slice(6).toUpperCase()} · ${stickerBytes(item.bytes)}`;
    $("sticker-preview-message").textContent = "正在加载原图…";
    image.onload = () => { if (s.preview === preview && stickerCurrent(preview)) $("sticker-preview-message").textContent = item.mime === "image/gif" ? "GIF 原图正在播放。" : ""; };
    image.onerror = () => { if (s.preview === preview && stickerCurrent(preview)) { image.hidden = true; image.removeAttribute("src"); $("sticker-preview-message").textContent = "原图无法读取，可能已删除或版本变化，请关闭后刷新图库。"; } };
    image.src = stickerImageUrl(item, s.scope, "original"); $("sticker-preview-dialog").showModal();
  }
  async function mutateSticker(action, item) {
    const s = state.stickers;
    if (!state.authenticated || state.view !== "stickers" || state.busy || s.operation || s.loading || !stickerScope(s.scope) || !["delete", "reindex"].includes(action)) return;
    if (item && !s.rows.some((row) => row.id === item.id && row.revision === item.revision)) return;
    if (action === "delete" && !item) return;
    const label = stickerScopeLabel(s.scope);
    const confirmation = action === "delete" ? `确定删除${label}中的图片 #${item.id}？图片与索引会从本群图库移除。` : item ? "重建这张图片的向量索引？后台会调用向量 API，产生请求用量。" : `重建${label}的全部 ${s.total} 张图片索引？后台会逐张调用向量 API，产生请求用量。`;
    if (!window.confirm(confirmation)) return;
    const payload = { request_id: requestId(), scope: s.scope }; if (item) Object.assign(payload, { id: item.id, revision: item.revision });
    s.operation = { action, payload, session: state.session, sending: false }; await retryStickerOperation();
  }
  async function retryStickerOperation() {
    const s = state.stickers, operation = s.operation;
    if (!state.authenticated || !operation || operation.sending || operation.session !== state.session || state.busy) return;
    const request = { session: state.session, version: s.version, scope: operation.payload.scope }; operation.sending = true; renderStickerStatus();
    try {
      const result = await api(`/api/stickers/${operation.action}`, "POST", operation.payload);
      if (state.session !== operation.session || s.operation !== operation) return;
      if (result.request_id !== operation.payload.request_id || result.scope !== operation.payload.scope || result.action !== operation.action || !stickerInteger(result.affected) || typeof result.receipt !== "string") throw new ApiError("操作响应格式无效，结果尚未确认。", 200, "invalid_response");
      s.operation = null;
      if (stickerCurrent(request)) {
        const message = operation.action === "delete" ? `已删除 ${result.affected} 张图片。` : `已提交 ${result.affected} 张图片的索引重建，尚未完成；刷新查看处理进度。`;
        await refreshStickers();
        if (stickerCurrent(request)) { const refreshError = s.kind === "error" ? " 列表刷新未完成：" + s.message : ""; s.message = message + refreshError; s.kind = refreshError ? "error" : ""; }
      }
    } catch (error) {
      if (state.session !== operation.session || s.operation !== operation) return;
      if (uncertain(error)) { operation.sending = false; }
      else { s.operation = null; }
      if (stickerCurrent(request)) { s.message = error.status === 409 || error.status === 404 ? "图片或图库已变化，操作未完成，请刷新后再试。" : error.message; s.kind = "error"; }
      if (["reauth_required", "csrf_invalid"].includes(error.code)) report(error);
    } finally { if (state.session === operation.session) { if (s.operation === operation) operation.sending = false; renderStickerStatus(); } }
  }

  const learningKinds = { experience: "交流经验", correction: "纠正记录", term: "用语解释" };
  function updateLearningControls() {
    const s = state.learning, blocked = state.busy || !state.authenticated || s.loading || s.saving || Boolean(state.pendingOperation);
    $("refresh-learning").disabled = blocked;
    $("new-learning").disabled = blocked || !s.snapshot || s.conflict;
    $("save-learning").disabled = blocked || !s.snapshot || s.conflict || !s.editing;
    $("cancel-learning-edit").disabled = state.busy || s.saving || Boolean(state.pendingOperation);
    document.querySelectorAll("[data-learning-write]").forEach((node) => { node.disabled = blocked || !s.snapshot || s.conflict || node.dataset.learningUnavailable === "true"; });
    document.querySelectorAll("#learning-editor-form input,#learning-editor-form textarea,#learning-editor-form select").forEach((node) => { node.disabled = blocked; });
  }
  function invalidateLearning(clear = false) {
    const s = state.learning; s.version++; s.loading = false; s.snapshot = null; s.message = "";
    $("learning-list").replaceChildren(); $("learning-history").replaceChildren();
    $("learning-account").textContent = "尚未读取"; $("learning-summary").textContent = "刷新后读取服务器确认的账号。";
    if (clear) { s.saving = false; s.editing = null; s.dirty = false; s.conflict = false; $("edit-learning-text").value = ""; $("learning-editor").hidden = true; }
    $("learning-message").textContent = ""; updateLearningControls();
  }
  function learningCurrent(version, session) { return state.authenticated && state.view === "learning" && state.learning.version === version && state.session === session; }
  function validateLearningSnapshot(value, expected) {
    const integer = (n, min = 0) => Number.isSafeInteger(n) && n >= min;
    const scope = (s) => s === "shared" || (typeof s === "string" && new RegExp("^" + value.bot_id + ":group:[1-9][0-9]*$").test(s));
    const note = (n) => n && integer(n.id, 1) && integer(n.version, 1) && Object.hasOwn(learningKinds, n.kind) && typeof n.text === "string" && scope(n.scope) && typeof n.locked === "boolean" && typeof n.deleted === "boolean";
    if (!contactId(value.bot_id) || (expected && value.bot_id !== expected) || !integer(value.revision) || !integer(value.max_note_chars, 100) || value.max_note_chars > 10000 || typeof value.learning_enabled !== "boolean"
      || !Array.isArray(value.notes) || !Array.isArray(value.history) || value.notes.some((n) => !note(n) || typeof n.invalidated !== "boolean" || !Array.isArray(n.sources) || n.sources.some((source) => typeof source.source_key !== "string" || typeof source.quote !== "string" || !scope(source.scope)))
      || value.history.some((n) => !note(n) || !integer(n.note_id, 1) || typeof n.rollback_allowed !== "boolean") || new Set(value.notes.map((n) => n.id)).size !== value.notes.length) throw new Error("学习笔记范围或格式无效，请刷新核对。");
  }
  function learningButton(label, action, unavailable = false) {
    const button = element("button", "text-button accent", label); button.type = "button";
    button.dataset.learningWrite = ""; button.dataset.learningUnavailable = String(unavailable);
    button.addEventListener("click", () => { if (!button.disabled) void runAction(action); }); return button;
  }
  function renderLearning() {
    const s = state.learning, snapshot = s.snapshot;
    $("learning-message").textContent = s.message || (s.loading ? "正在读取学习笔记…" : "");
    if (!snapshot) { updateLearningControls(); return; }
    $("learning-account").textContent = "QQ " + snapshot.bot_id;
    const used = snapshot.notes.filter((note) => !note.deleted && !note.invalidated).reduce((sum, note) => sum + [...note.text].length, 0);
    $("learning-summary").textContent = `自动学习${snapshot.learning_enabled ? "已开启" : "已关闭"} · 版本 ${snapshot.revision} · 有效笔记共 ${used} / ${snapshot.max_note_chars} 个字符。人工管理即时生效。最多保留 128 条有效笔记、512 条含已删除记录，每条保留最近 32 个版本；超出记录容量时，会清理最旧的已删除或来源失效记录及其历史。`;
    const list = $("learning-list"), history = $("learning-history"); list.replaceChildren(); history.replaceChildren();
    if (!snapshot.notes.length) list.append(element("p", "empty-state", "当前账号还没有学习笔记。可人工新增，或在配置中开启自动学习。"));
    for (const note of snapshot.notes) {
      const card = element("article", "memory-record"), top = element("div", "memory-record-top");
      top.append(element("strong", "", `${learningKinds[note.kind]} #${note.id}`), element("span", "hint", `版本 ${note.version} · ${note.scope === "shared" ? "同账号群聊共享" : "群 " + note.scope.split(":")[2]} · ${note.invalidated ? "来源已失效" : note.deleted ? "已删除" : note.locked ? "已锁定" : "可自动更新"}`));
      card.append(top, element("p", "memory-text", note.text));
      if (note.sources.length) {
        const details = element("details", "evidence-details"); details.append(element("summary", "", "学习来源（" + note.sources.length + "）"));
        for (const source of note.sources) details.append(element("p", "hint learning-source", `${source.scope} · ${source.source_key}\n${source.quote}`));
        card.append(details);
      }
      const actions = element("div", "button-row");
      actions.append(learningButton("编辑", () => openLearningEditor(note), note.deleted || note.invalidated), learningButton(note.locked ? "解除锁定" : "锁定", () => mutateLearning({ op: "lock", id: note.id, version: note.version, locked: !note.locked }), note.deleted || note.invalidated), learningButton("删除", async () => { if (window.confirm(`删除笔记 #${note.id}？记录和历史按页面所示容量保留，可恢复的历史可以回滚；超过容量后，旧的已删除记录及其历史会被清理。`)) await mutateLearning({ op: "delete", id: note.id, version: note.version }); }, note.deleted || note.invalidated));
      card.append(actions); list.append(card);
    }
    if (!snapshot.history.length) history.append(element("p", "empty-state", "尚无修改历史。"));
    for (const entry of snapshot.history) {
      const card = element("article", "memory-record");
      const current = snapshot.notes.find((note) => note.id === entry.note_id);
      card.append(element("strong", "", `笔记 #${entry.note_id} · 版本 ${entry.version}`), element("p", "hint", `${dateText(entry.created_at)} · ${learningKinds[entry.kind]} · ${entry.deleted ? "删除版本" : entry.locked ? "已锁定" : "未锁定"}`), element("p", "memory-text", entry.text));
      card.append(learningButton("回滚到此版本", async () => { if (window.confirm(`将笔记 #${entry.note_id} 回滚到版本 ${entry.version}？这会创建一个新版本。`)) await mutateLearning({ op: "rollback", id: entry.note_id, version: current.version, history_id: entry.id }); }, !entry.rollback_allowed || !current));
      if (!entry.rollback_allowed) card.append(element("p", "hint", "此历史不可回滚，来源可能已遗忘或撤回。"));
      history.append(card);
    }
    updateLearningControls();
  }
  async function loadLearning() {
    if (!state.authenticated || state.view !== "learning" || state.learning.loading) return;
    const s = state.learning, version = ++s.version, session = state.session, expected = s.snapshot?.bot_id;
    s.loading = true; s.message = ""; renderLearning();
    try {
      const result = await api("/api/learning" + (expected ? "?" + new URLSearchParams({ bot_id: expected }) : ""));
      if (!learningCurrent(version, session)) return;
      validateLearningSnapshot(result, expected);
      if (s.dirty && s.editing && (s.editing.bot_id !== result.bot_id || s.editing.revision !== result.revision)) s.conflict = true;
      else if (!s.dirty) s.conflict = false;
      s.snapshot = result; s.message = s.conflict ? "笔记已变化，编辑仍保留。请复制内容，取消编辑后重新选择笔记。" : "";
    } catch (error) {
      if (!learningCurrent(version, session)) return;
      s.snapshot = null; s.conflict = Boolean(s.dirty); s.message = error.message;
      $("learning-list").replaceChildren(); $("learning-history").replaceChildren();
      $("learning-account").textContent = "账号待核对"; $("learning-summary").textContent = "刷新后重新读取服务器确认的账号与笔记。";
      if (["reauth_required", "csrf_invalid"].includes(error.code)) report(error);
    } finally { if (learningCurrent(version, session)) { s.loading = false; renderLearning(); } }
  }
  function openLearningEditor(note = null) {
    const s = state.learning;
    if (!s.snapshot || s.loading || s.saving || s.conflict || state.pendingOperation || (s.dirty && !window.confirm("放弃尚未保存的笔记编辑？"))) return;
    s.editing = { ...(note ? clone(note) : {}), bot_id: s.snapshot.bot_id, revision: s.snapshot.revision }; s.dirty = false;
    $("learning-editor-title").textContent = note ? `编辑笔记 #${note.id}` : "新增笔记";
    $("learning-editor-scope").textContent = `机器人 ${s.snapshot.bot_id} · ${note?.scope && note.scope !== "shared" ? "群 " + note.scope.split(":")[2] : "同账号群聊共享"}。人工笔记默认锁定，不含聊天来源。已有来源依赖会保留。`;
    $("edit-learning-kind").value = note?.kind || "experience"; $("edit-learning-text").value = note?.text || ""; $("edit-learning-locked").checked = note?.locked ?? true;
    $("learning-editor-error").textContent = ""; $("learning-editor").hidden = false; updateLearningControls();
  }
  function cancelLearningEdit() {
    const s = state.learning; if (s.saving || state.pendingOperation || (s.dirty && !window.confirm("放弃尚未保存的笔记编辑？"))) return;
    s.editing = null; s.dirty = false; s.conflict = false; $("learning-editor").hidden = true; $("edit-learning-text").value = ""; updateLearningControls();
  }
  async function saveLearning() {
    const s = state.learning, edit = s.editing, text = $("edit-learning-text").value.trim();
    if (!edit || !s.snapshot || !text || [...text].length > s.snapshot.max_note_chars) throw new Error(`笔记内容需要填写 1–${s.snapshot?.max_note_chars ?? 2000} 个字符。`);
    const otherChars = s.snapshot.notes.filter((note) => !note.deleted && !note.invalidated && note.id !== edit.id).reduce((sum, note) => sum + [...note.text].length, 0);
    if (otherChars + [...text].length > s.snapshot.max_note_chars) throw new Error(`全部有效笔记不能超过 ${s.snapshot.max_note_chars} 个字符；其他笔记已占 ${otherChars} 个。`);
    const action = { op: edit.id ? "update" : "add", kind: $("edit-learning-kind").value, text, locked: $("edit-learning-locked").checked, scope: edit.scope || "shared" };
    if (edit.id) Object.assign(action, { id: edit.id, version: edit.version });
    await mutateLearning(action, true);
  }
  async function mutateLearning(action, closeEditor = false) {
    const s = state.learning;
    if (!s.snapshot || s.loading || s.saving || s.conflict || state.pendingOperation) throw new Error("请先刷新笔记并核对未完成操作。");
    if (closeEditor && (s.editing?.bot_id !== s.snapshot.bot_id || s.editing?.revision !== s.snapshot.revision)) throw new Error("编辑版本已变化，请重新载入笔记。");
    const version = s.version, session = state.session;
    const payload = { bot_id: s.snapshot.bot_id, revision: s.snapshot.revision, request_id: requestId(), action };
    const validate = (result) => { if (result.request_id !== payload.request_id || result.bot_id !== payload.bot_id || result.status !== "completed" || !Number.isSafeInteger(result.revision) || result.revision <= payload.revision) throw new ApiError("学习操作响应范围无效，请查询原操作结果。", 200, "invalid_response"); };
    const complete = async () => {
      if (!learningCurrent(version, session)) return;
      if (closeEditor) { s.editing = null; s.dirty = false; $("learning-editor").hidden = true; $("edit-learning-text").value = ""; }
      await loadLearning(); s.message = s.snapshot ? "笔记已保存。" : "笔记已保存，但刷新列表失败：" + s.message; renderLearning();
    };
    s.saving = true; updateLearningControls();
    try { const result = await api("/api/learning/mutate", "POST", payload); validate(result); await complete(); }
    catch (error) {
      if (state.session !== session) return;
      if (uncertain(error)) { trackOperation({ kind: "learning", label: "学习笔记操作", requestId: payload.request_id, botId: payload.bot_id, validate, complete }); return; }
      if (learningCurrent(version, session)) { s.conflict = error.status === 409; s.message = error.status === 409 ? "笔记已变化，编辑仍保留；请刷新后核对。" : error.message; $("learning-editor-error").textContent = s.message; renderLearning(); }
      throw error;
    } finally { if (state.session === session) { s.saving = false; updateLearningControls(); } }
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
    try {
      if (!stop && state.applyError) throw new Error(state.applyError);
      await api(stop ? "/api/runtime/stop" : "/api/runtime/apply", "POST", {});
      if (!stop) state.applyFailure = null;
    }
    catch (error) {
      if (uncertain(error)) {
        trackOperation({ kind: "runtime", label: stop ? "断开连接" : "应用配置", goal: stop ? "stopped" : "running", revision: state.revision });
        notice((stop ? "断开结果待确认。" : "配置已保存，应用结果尚未确认。") + error.message, "warning"); return false;
      }
      if (!stop && state.authenticated) { state.applyFailure = { revision: state.revision, message: error.message }; updateControls(); }
      throw error;
    }
    notice(stop ? "已提交断开请求。" : "配置已保存，已提交应用请求；生效状态会自动更新。"); await refreshStatus(); return true;
  }
  async function saveConfig(apply) {
    if (state.pendingOperation || state.stale) { updateControls(); return; }
    const session = state.session; let saved = false, refreshed = false;
    configSaveFeedback("正在校验并保存配置，请稍候。", "", "正在保存…");
    try {
      if (state.jsonDirty) adoptJson();
      const config = readForm(), secrets = secretChanges();
      const logChanged = config.app.log !== state.config.app.log, databaseChanged = config.app.database !== state.config.app.database;
      try { await api("/api/config", "PUT", { revision: state.revision, config, secrets }); }
      catch (error) {
        if (uncertain(error)) { state.stale = true; trackOperation({ kind: "config", label: "保存配置", previousRevision: state.revision, config }); return; }
        throw error;
      }
      saved = true; configSaveFeedback("配置已保存，正在读取保存后的版本。", "", "正在核对…");
      await loadConfig(); refreshed = true;
      if (databaseChanged) notice("配置已保存。数据库路径已改变，请重启整个程序后再使用；不会在当前进程中应用。", "warning");
      else if (apply) {
        configSaveFeedback("配置已保存，正在应用到机器人。", "", "正在应用…");
        try { if (await applyRuntime() && logChanged) notice("配置已保存并提交应用。日志过滤级别仍需要重启整个程序。", "warning"); }
        catch (error) { error.message = "配置已保存但未生效。应用失败：" + error.message; throw error; }
      } else notice(state.applyError ? "配置已保存但未生效。当前不可应用：" + state.applyError
        : "配置已保存，应用后生效。" + (logChanged ? " 日志过滤级别需要重启整个程序。" : ""), state.applyError || logChanged ? "warning" : "success");
      configSaveFeedback($("notice-text").textContent, state.applyError || logChanged || databaseChanged ? "warning" : "success");
    } catch (error) {
      if (session === state.session) {
        if (saved && !refreshed) { state.stale = true; error.message = "配置已保存，但读取保存结果失败。请重新载入核对后再继续。" + error.message; }
        const message = error.code === "reauth_required" ? "请完成密码重新验证，再点击保存。当前编辑已保留。" : error.message || "保存未完成，请检查配置。";
        configSaveFeedback(message, "error");
      }
      throw error;
    } finally {
      if (session === state.session) { state.configSave.phase = ""; updateControls(); }
    }
  }
  async function checkOperation() {
    const operation = state.pendingOperation; if (!operation) return;
    if (operation.kind === "learning") {
      const result = await api("/api/learning/operations/" + encodeURIComponent(operation.requestId) + "?" + new URLSearchParams({ bot_id: operation.botId }));
      if (result.status === "completed") { operation.validate(result); clearOperation(); await operation.complete(result); }
      else notice("尚未查到学习笔记操作的完成记录，请稍后再次查询；不会重复提交。", "warning");
    } else if (operation.kind === "memory") {
      const result = await api("/api/memory/operations/" + encodeURIComponent(operation.requestId));
      if (result.status === "completed") { if (operation.validate) operation.validate(result); clearOperation(); await operation.complete(result); }
      else notice("尚未查到完成记录。请稍后再次查询，不要重新提交。操作号：" + operation.requestId, "warning");
    } else if (operation.kind === "config") {
      const result = await api("/api/config");
      clearOperation(); state.stale = true;
      $("config-warning").hidden = false;
      $("config-warning").textContent = result.revision !== operation.previousRevision && canonical(result.config) === canonical(operation.config)
        ? "服务器已有匹配的配置。密钥不可回读，请核对密钥状态后重新载入；不会自动应用。"
        : "已取得服务器当前版本，但无法确认这次保存。当前编辑仍保留，请先复制再重新载入核对。";
      notice($("config-warning").textContent, "warning"); updateControls();
      configSaveFeedback($("config-warning").textContent, "warning");
    } else {
      state.status = await api("/api/status"); renderStatus();
      const done = operation.goal === "stopped" ? state.status.phase === "stopped" : state.status.applied_revision === operation.revision && ["running", "waiting_login"].includes(state.status.phase);
      if (done || state.status.phase === "error") {
        if (done && operation.goal === "running") state.applyFailure = null;
        clearOperation(); notice(done ? "运行状态已确认。" : String(state.status.message || "运行时应用失败。"), done ? "success" : "error");
      }
      else notice("当前状态：" + String(state.status.phase) + "。请稍后再次查询。", "warning");
    }
  }
  const knowledgeViews = ["context", "memory", "learning"];
  function invalidateContextOverview() {
    const context = state.context;
    context.version++; context.loading = false; context.snapshot = null; context.message = "";
    $("context-recent-list").replaceChildren(); $("context-counts").replaceChildren();
    renderContextOverview();
  }
  function validateContextOverview(value) {
    if (!value || !(value.bot_id === null || contactId(value.bot_id)) || typeof value.enabled !== "boolean" || !value.counts
      || ["turns", "memories", "notes"].some((key) => !Number.isSafeInteger(value.counts[key]) || value.counts[key] < 0)
      || !Array.isArray(value.recent) || value.recent.length > 100 || (value.message != null && typeof value.message !== "string")) throw new Error("接续概览格式无效，请刷新核对。");
    validateContext({ plugins: { ai: { context: value.config } } });
    const ids = new Set();
    for (const turn of value.recent) {
      if (!turn || !Number.isSafeInteger(turn.id) || turn.id < 1 || ids.has(turn.id) || !contactId(turn.user_id) || !contactId(turn.peer_id)
        || !["group", "friend"].includes(turn.scene) || typeof turn.conversation_id !== "string" || !Number.isSafeInteger(turn.completed_at)
        || typeof turn.request_key !== "string" || typeof turn.reply_key !== "string"
        || ["user_text", "assistant_text"].some((key) => turn[key] != null && typeof turn[key] !== "string")) throw new Error("问答来源格式无效，请刷新核对。");
      if (!turn.conversation_id.startsWith(`${value.bot_id}:${turn.scene}:${turn.peer_id}:`)) throw new Error("问答来源会话与当前账号不一致，请刷新核对。");
      ids.add(turn.id);
    }
    if (value.bot_id === null && value.recent.length) throw new Error("尚未确认机器人账号，不能展示问答来源。");
  }
  function contextDuration(seconds) {
    if (seconds % 86400 === 0) return seconds / 86400 + " 天";
    if (seconds % 3600 === 0) return seconds / 3600 + " 小时";
    if (seconds % 60 === 0) return seconds / 60 + " 分钟";
    return seconds + " 秒";
  }
  function renderContextOverview() {
    const context = state.context, snapshot = context.snapshot, policy = snapshot?.config;
    badge("context-enabled", !snapshot ? "待同步" : snapshot.enabled ? "已开启" : "已关闭", !snapshot ? "neutral" : snapshot.enabled ? "good" : "neutral");
    $("context-window").textContent = policy ? `最近 ${contextDuration(policy.recent_seconds)} · ${policy.max_turns} 轮问答` : "等待同步已应用设置";
    $("context-window-detail").textContent = snapshot ? `当前运行配置${snapshot.bot_id ? " · 机器人 QQ " + snapshot.bot_id : " · 尚未确认机器人账号"}。群聊与私聊按同一用户接续。` : "读取当前运行配置后，展示接续范围与来源。";
    $("context-neighbors").textContent = policy ? `群聊背景 · 前 ${policy.neighbors_before} / 后 ${policy.neighbors_after} 条` : "群聊背景 · 待同步";
    $("context-time-window").textContent = policy ? `前后各 ${contextDuration(policy.neighbor_seconds)}` : "时间范围 · 待同步";
    const saved = state.config?.plugins?.ai;
    const ordinaryBudget = saved?.defaults.input_budget, notesAllowed = saved?.learning?.enabled && saved?.defaults.memory_read;
    $("context-input-budget").textContent = saved ? (ordinaryBudget + (notesAllowed ? saved.learning.injection_budget : 0)).toLocaleString("zh-CN") : "—";
    $("context-ordinary-budget").textContent = saved ? ordinaryBudget.toLocaleString("zh-CN") : "—";
    $("context-recall-budget").textContent = saved?.context ? saved.context.recall_budget.toLocaleString("zh-CN") : "—";
    $("context-neighbor-budget").textContent = saved?.context ? saved.context.neighbor_budget.toLocaleString("zh-CN") : "—";
    $("context-learning-budget").textContent = saved?.learning ? saved.learning.injection_budget.toLocaleString("zh-CN") : "—";
    $("context-budget-note").textContent = "已保存的全局默认，应用后生效。普通输入加上允许注入的笔记额度构成最终上限；接续占普通输入、最多一半，群聊背景包含在接续内。会话可覆盖普通输入预算。";
    $("refresh-context-overview").disabled = !state.authenticated || context.loading;
    $("refresh-context-overview").textContent = context.loading ? "正在同步…" : "刷新记录";
    $("context-overview-message").textContent = context.message || (context.loading ? "正在读取当前账号的问答来源…" : snapshot?.message || "");
    $("context-counts").replaceChildren(); $("context-recent-list").replaceChildren();
    if (!snapshot) return;
    for (const [key, name, detail] of [["turns", "预览问答", "本次读取的有效记录"], ["memories", "个人记忆", "事实、偏好、任务与事件"], ["notes", "经验笔记", "交流经验、纠正与用语"]]) {
      const item = element("div", "context-count"), copy = element("div");
      copy.append(element("span", "", name), element("p", "hint", detail));
      item.append(element("strong", "", snapshot.counts[key].toLocaleString("zh-CN")), copy); $("context-counts").append(item);
    }
    if (!snapshot.recent.length) {
      $("context-recent-list").append(element("div", "context-empty", snapshot.bot_id ? "还没有可预览的问答。机器人成功发送回复后，会在这里显示对应的来源记录。" : "连接机器人后，这里会显示当前 QQ 账号的近期问答。"));
      return;
    }
    for (const turn of snapshot.recent) {
      const item = element("details", "context-turn"), heading = element("summary"), source = element("span", "context-turn-origin");
      source.append(element("span", "origin-icon", turn.scene === "group" ? "群" : "聊"), element("strong", "", `${turn.scene === "group" ? "群 " : "私聊 "}${turn.peer_id}`));
      const meta = element("span", "context-turn-meta", `用户 ${turn.user_id} · ${dateText(turn.completed_at)}`);
      heading.append(source, meta, element("span", "context-turn-expand", "查看来源")); item.append(heading);
      const body = element("div", "context-turn-body"), tags = element("div", "source-tags");
      tags.append(element("span", "", "机器人 " + snapshot.bot_id), element("span", "", "会话 " + turn.conversation_id), element("span", "", "已完成问答 #" + turn.id));
      body.append(tags);
      for (const [title, text, key] of [["用户原文", turn.user_text, turn.request_key], ["机器人回复", turn.assistant_text, turn.reply_key]]) {
        const quote = element("div", "context-quote");
        quote.append(element("span", "section-label", title), element("p", "", text == null ? "原文当前不可用。" : text || "（无文字内容）"), element("code", "source-key", key)); body.append(quote);
      }
      body.append(element("p", "hint", "原文为有长度限制的预览。群聊前后文将在召回时按已应用的窗口与读取权限选取，并标注各条消息的发言者。"));
      item.append(body); $("context-recent-list").append(item);
    }
  }
  async function loadContextOverview() {
    const context = state.context;
    if (!state.authenticated || state.view !== "context" || context.loading) return;
    const version = ++context.version, session = state.session;
    const current = () => state.authenticated && state.view === "context" && context.version === version && state.session === session;
    context.loading = true; context.snapshot = null; context.message = ""; renderContextOverview();
    try {
      const result = await api("/api/context/overview");
      if (!current()) return;
      validateContextOverview(result);
      if (result.bot_id != null && state.status?.online && state.status.bot_id != null && result.bot_id !== String(state.status.bot_id)) throw new Error("机器人账号已变化，请刷新接续记录。");
      context.snapshot = result;
    } catch (error) {
      if (!current()) return;
      context.message = error.message || "读取问答来源失败，请重试。";
      if (["reauth_required", "csrf_invalid"].includes(error.code)) report(error);
    } finally { if (current()) { context.loading = false; renderContextOverview(); } }
  }

  function showView(view) {
    if (!["overview", "qq", "contacts", "config", "context", "memory", "learning", "stickers"].includes(view)) return;
    if (state.view === "contacts" && view !== "contacts") invalidateContacts();
    if (state.view === "stickers" && view !== "stickers") invalidateStickers();
    if (state.view === "learning" && view !== "learning") invalidateLearning();
    if (state.view === "context" && view !== "context") invalidateContextOverview();
    state.view = view;
    document.querySelectorAll(".view").forEach((node) => { node.hidden = node.id !== "view-" + view; });
    document.querySelectorAll(".nav-item").forEach((node) => { const current = node.dataset.view === view || node.dataset.view === "context" && knowledgeViews.includes(view); node.classList.toggle("active", current); if (current) node.setAttribute("aria-current", "page"); else node.removeAttribute("aria-current"); });
    document.querySelectorAll(".workspace-tab").forEach((node) => { const current = node.dataset.view === view; node.classList.toggle("active", current); if (current) node.setAttribute("aria-current", "page"); else node.removeAttribute("aria-current"); });
    $("knowledge-header").hidden = !knowledgeViews.includes(view);
    $("page-name").textContent = "工作空间 / " + ({ overview: "运行概览", qq: "QQ 登录", contacts: "群与好友", config: "设置与人设", context: "上下文与知识", memory: "上下文与知识", learning: "上下文与知识", stickers: "图库管理" }[view]);
    if (view === "memory" && !state.memory.list.length) void runAction(loadConversations);
    if (view === "qq") scheduleQq(0);
    if (view === "contacts" && state.contacts.phase !== "ready") void loadContacts();
    if (view === "stickers") void refreshStickers();
    if (view === "learning") void loadLearning();
    if (view === "context") void loadContextOverview();
  }
  const hasEdits = () => state.dirty || state.jsonDirty || state.memory.settingsDirty || state.memory.editorDirty || state.learning.dirty;
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
    state.config = null; state.draft = null; state.status = null; state.qq = null; state.runtimePolling = false; state.applyError = null; state.applyFailure = null;
    state.configSave = { phase: "", message: "", kind: "" };
    state.autoReplyBaseEstimate = null; autoReplyBudgetControl = null;
    state.personaTemplates = {}; state.personaSelected = null; state.toolCatalog = []; state.pluginCatalog = []; pluginControls = null; $("persona-list").replaceChildren(); $("persona-text").value = ""; $("persona-new-name").value = "";
    state.groupPersona = { saving: false, message: "", kind: "" }; $("group-persona-select").replaceChildren(); $("group-persona-select").value = "";
    state.secrets = {}; state.dirty = false; state.jsonDirty = false; state.pendingOperation = null; resumeQqPolling();
    invalidateContacts(); state.contacts.query = ""; $("contacts-search").value = "";
    invalidateStickers(true);
    invalidateLearning(true);
    invalidateContextOverview();
    $("config-fields").append($("persona-manager"), $("model-picker"));
    inputs.clear(); secretInputs.clear(); $("config-groups").replaceChildren(); $("config-json").value = "";
    invalidateModels(); invalidateEmbeddingTest(); invalidateSearchTest();
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
    field("persona", "会话人设", "persona", "选择已保存的自定义人设或内置模板，不受配置页未保存草稿影响。"),
    field("input_budget", "输入预算（保守估算）", "number", "", { min: 1024, max: 262144 }),
    field("output_budget", "输出 token 上限", "number", "模型可能将思考与工具参数一并计入；额度过低会截断工具调用。", { min: 64, max: 32768 }),
    field("recent_messages", "最近历史条数", "number", "", { min: 0, max: 4096 }),
    field("allowed_tools", "允许的工具", "tools", "只可勾选全局允许且所属插件已启用的工具。不勾选即禁用全部工具；修改全局工具或插件后，请先保存并应用，再配置会话。"),
    field("summary", "生成并使用摘要", "check", "关闭后不生成或注入摘要；保留原文按历史条数读取。"),
    field("memory_read", "读取长期记忆", "check", "将当前范围内相关记忆加入回复上下文。"),
    field("reply_to_bot", "回复机器人消息时触发 AI", "check", "引用当前机器人账号的消息即可触发；插件命令仍优先处理。"),
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
  function memoryRangeDescription(conversation) {
    return conversation.global_memory_shared
      ? "全域共享：机器人 " + conversation.bot_id + " 的所有群聊和私聊" + (conversation.owner_id ? " · QQ " + conversation.owner_id : " · 按 QQ 归属，可在下方筛选人物") + "。编辑、停用与遗忘会影响该记忆在所有会话中的使用；摘要和短期上下文仍属于选中会话。"
      : "按来源会话隔离：仅显示并维护当前会话的长期记忆。可在配置管理中开启全域长期记忆共享。";
  }
  function memoryMatchesSelection(record) {
    const selected = state.memory.detail || state.memory.selected;
    if (!selected) return false;
    if (selected.memory_scope) return record.memory_scope === selected.memory_scope && typeof record.conversation_id === "string" && (!selected.owner_id || record.subject_id === selected.owner_id);
    return record.conversation_id === selected.id;
  }
  function memorySourceTitle(record) {
    const match = String(record.source_scope || record.conversation_id).match(/^(\d+):(group|friend):(\d+)(?::|$)/u);
    if (!match) return "来源会话 " + record.conversation_id;
    return "来源：机器人 " + match[1] + " · " + (match[2] === "group" ? "群 " : "私聊 ") + match[3] + (record.source_owner_id && match[2] === "group" ? " · 成员 " + record.source_owner_id : "");
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
      button.append(element("strong", "", scopeTitle(conversation)), element("span", "hint", "机器人 " + conversation.bot_id + " · " + (conversation.mode === "user" ? "成员隔离上下文" : "共享上下文") + " · " + conversation.memory_count + (conversation.global_memory_shared ? " 条全域记忆" : " 条会话记忆") + (conversation.is_current === false ? " · 历史" : "")));
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
    $("memory-range-description").textContent = memoryRangeDescription(detail);
    badge("selected-scope-badge", detail.is_current === false ? "历史会话" : detail.settings.paused ? "已暂停" : "可聊天", detail.is_current === false ? "neutral" : detail.settings.paused ? "wait" : "good");
    $("pause-conversation").textContent = detail.settings.paused ? "恢复聊天" : "暂停聊天";
    $("once-conversation").textContent = detail.settings.once ? "已放行一次" : "只回复一次";
    $("conversation-mode").value = detail.scope_mode || detail.mode;
    renderSettings(); if ($("context-details").open) renderContext(detail); updateControls();
  }
  function renderSettings() {
    settingInputs.clear(); const box = $("conversation-settings-grid"); box.replaceChildren();
    for (const def of settingDefinitions) {
      const wrapper = element("div", "field" + (def.type === "check" ? " check-field" : def.type === "tools" ? " full" : ""));
      const label = element("label", "", def.label); label.htmlFor = "setting-" + def.path;
      const input = document.createElement(def.type === "tools" ? "div" : def.type === "persona" ? "select" : def.type === "lines" ? "textarea" : "input"); input.id = label.htmlFor;
      if (!["lines", "persona", "tools"].includes(def.type)) input.type = def.type === "check" ? "checkbox" : def.type;
      input.autocomplete = "off"; const value = state.memory.detail.settings[def.path];
      if (def.type === "tools") { input.className = "tool-choices"; input.setAttribute("role", "group"); input.setAttribute("aria-label", def.label); renderToolPicker(input, value, "conversation"); }
      else if (def.type === "persona") fillPersonaSelect(input, value, state.config);
      else if (def.type === "check") input.checked = Boolean(value);
      else input.value = def.type === "lines" ? (value || []).join("\n") : String(value ?? "");
      if (def.type === "number") { input.min = String(def.min); input.max = String(def.max); input.step = "1"; input.required = true; }
      if (def.path === "persona") input.required = true;
      const hint = element("p", "hint", def.hint);
      if (def.type === "check") { const copy = element("div"); copy.append(label, hint); wrapper.append(input, copy); }
      else wrapper.append(label, input, hint);
      input.addEventListener("input", () => { state.memory.settingsDirty = true; updateControls(); });
      if (def.type === "persona") input.addEventListener("change", () => { input.setAttribute("aria-invalid", String(!personaChoices(state.config).has(input.value))); state.memory.settingsDirty = true; updateControls(); });
      settingInputs.set(def.path, { input, def }); box.append(wrapper);
    }
  }
  function readSettings() {
    const result = clone(state.memory.detail.settings);
    for (const [key, { input, def }] of settingInputs) {
      if (def.type === "tools") result[key] = readToolPicker(input);
      else if (def.type === "persona") { requirePersona(state.config, input.value, "会话人设"); result[key] = input.value; }
      else if (def.type === "check") result[key] = input.checked;
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
    if (state.memory.detail?.memory_scope && (result.access_conversation_id !== id || result.memory_scope !== state.memory.detail.memory_scope || result.global_memory_shared !== state.memory.detail.global_memory_shared)) throw new Error("记忆响应的会话范围不一致，请重新载入当前会话。");
    if ((result.items || []).some((item) => !memoryMatchesSelection(item) || (state.memory.detail?.memory_scope && item.access_conversation_id !== id))) throw new Error("记忆响应的会话范围不一致，已停止显示。");
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
      card.append(top, element("p", "memory-text", record.text), element("p", "hint", memorySourceTitle(record)));
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
    if (!memoryMatchesSelection(result) || result.conversation_id !== record.conversation_id || result.subject_id !== record.subject_id) throw new Error("证据不属于当前选中的记忆范围。");
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
  function validateGroupPersonaResult(result, payload) {
    if (!result || result.status !== "completed" || result.request_id !== payload.request_id || result.bot_id !== payload.bot_id || result.persona !== payload.persona
      || !["affected_groups", "affected_conversations"].every((key) => Number.isSafeInteger(result[key]) && result[key] >= 0)
      || !Array.isArray(result.scopes) || result.scopes.some((scope) => typeof scope !== "string" || !scope.startsWith(payload.bot_id + ":group:") || !contactId(scope.slice((payload.bot_id + ":group:").length)))) {
      throw new ApiError("批量人设响应未能确认，请查询操作结果。", 200, "invalid_response");
    }
  }
  async function finishGroupPersona(result) {
    const session = state.session;
    let message = `QQ ${result.bot_id} 的 ${result.affected_groups} 个群已使用「${result.persona}」，更新了 ${result.affected_conversations} 个已有会话。好友、全局默认、历史和记忆保持不变。`;
    const selected = state.memory.detail || state.memory.selected;
    if (selected?.scene === "group" && result.scopes.includes(selected.scope)) {
      state.memory.version++; state.memory.conflict = true; state.memory.preview = null;
      $("conversation-conflict").hidden = false; if ($("forget-dialog").open) $("forget-dialog").close();
      if (state.memory.settingsDirty || state.memory.editorDirty) {
        message += " 当前会话的未保存编辑已保留，请重新载入后再继续。";
      } else {
        try { await reloadSelected(); } catch (error) {
          if (session !== state.session) return;
          message += " 当前会话详情刷新失败，请重新载入：" + error.message;
        }
      }
    }
    if (session !== state.session) return;
    try { await loadConversations(); } catch (error) {
      if (session !== state.session) return;
      message += " 会话列表刷新失败，请手动刷新：" + error.message;
    }
    if (session !== state.session) return;
    state.groupPersona.message = message; state.groupPersona.kind = "success"; updateControls(); notice(message);
  }
  async function applyGroupPersona() {
    if (!state.authenticated || !state.config || state.pendingOperation || state.groupPersona.saving) return;
    const persona = $("group-persona-select").value;
    requirePersona(state.config, persona, "群聊人设");
    const session = state.session;
    state.groupPersona.saving = true; state.groupPersona.kind = "";
    state.groupPersona.message = "正在核对当前 QQ 账号和全部群列表……"; updateControls();
    try {
      const contacts = await api("/api/qq/contacts?force=true");
      validateContacts(contacts);
      if (session !== state.session) return;
      if (!contacts.groups.length) { state.groupPersona.message = "当前登录 QQ 没有已加入的群，无需修改。"; return; }
      if (!window.confirm(`将 QQ ${contacts.bot_id} 当前已加入的全部 ${contacts.groups.length} 个群设为「${persona}」？\n包括按成员隔离的会话及这些群以后新建的会话；好友、全局默认、历史和记忆保持不变。`)) {
        state.groupPersona.message = "已取消，没有修改群聊人设。"; return;
      }
      const payload = { request_id: requestId(), bot_id: contacts.bot_id, persona };
      const validate = (result) => validateGroupPersonaResult(result, payload);
      const complete = async (result) => { if (session === state.session && state.authenticated) await finishGroupPersona(result); };
      state.groupPersona.message = `正在为 QQ ${payload.bot_id} 的全部群应用「${persona}」……`; updateControls();
      let result;
      try {
        result = await api("/api/ai/groups/persona", "POST", payload); validate(result);
      } catch (error) {
        if (session !== state.session) return;
        if (uncertain(error)) {
          state.groupPersona.message = "批量修改结果待确认，请使用上方「查询操作结果」；不会重复提交。";
          trackOperation({ kind: "memory", label: "全部群聊人设操作", requestId: payload.request_id, validate, complete }); return;
        }
        throw error;
      }
      await complete(result);
    } catch (error) {
      if (session !== state.session) return;
      state.groupPersona.message = error.message || "群聊人设操作未完成。"; state.groupPersona.kind = "error"; throw error;
    } finally {
      if (session === state.session) { state.groupPersona.saving = false; updateControls(); }
    }
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
    $("memory-editor-scope").textContent = (record ? memorySourceTitle(record) : "来源：" + scopeTitle(state.memory.detail)) + (state.memory.detail.global_memory_shared ? " · 此 QQ 的记忆在同账号所有群聊和私聊共享。" : "。") + "人工记录默认锁定，模型不会覆盖。";
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
    if ((hasEdits() || state.pendingOperation || state.stickers.operation) && !window.confirm("有未保存编辑或待查询的操作。确定退出并清空本页内容？")) return;
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
    if (id === "sticker-preview-dialog") closeStickerPreview();
    $(id).close(); updateControls();
  }
  document.querySelectorAll("[data-close]").forEach((button) => button.addEventListener("click", () => closeDialog(button.dataset.close)));
  document.querySelectorAll("dialog").forEach((dialog) => dialog.addEventListener("cancel", (event) => { event.preventDefault(); closeDialog(dialog.id); }));
  $("dismiss-notice").addEventListener("click", () => { $("notice").hidden = true; });
  $("check-operation").addEventListener("click", () => { void runAction(checkOperation); });
  $("config-check-operation").addEventListener("click", () => { void runAction(async () => { try { await checkOperation(); configSaveFeedback($("notice-text").textContent, state.pendingOperation || state.stale ? "warning" : "success"); } catch (error) { configSaveFeedback(error.message, "error"); throw error; } }); });
  document.querySelectorAll("[data-view]").forEach((button) => button.addEventListener("click", () => showView(button.dataset.view)));
  document.querySelectorAll("[data-config-category]").forEach((button) => button.addEventListener("click", () => filterConfig(button.dataset.configCategory)));
  document.querySelectorAll("[data-config-section]").forEach((button) => button.addEventListener("click", () => openConfigSection(button.dataset.configSection)));
  $("refresh-context-overview").addEventListener("click", () => { void loadContextOverview(); });
  document.querySelector(".brand").addEventListener("click", (event) => { event.preventDefault(); showView("overview"); });
  $("config-form").addEventListener("submit", (event) => event.preventDefault());
  $("persona-list").addEventListener("change", () => { state.personaSelected = $("persona-list").value; $("persona-error").textContent = ""; renderPersonaPreview(); });
  $("group-persona-select").addEventListener("change", () => {
    $("group-persona-select").setAttribute("aria-invalid", String(!personaChoices(state.config).has($("group-persona-select").value)));
    if (!state.pendingOperation && !state.groupPersona.saving) { state.groupPersona.message = ""; state.groupPersona.kind = ""; }
    updateControls();
  });
  $("apply-group-persona").addEventListener("click", () => { void runAction(applyGroupPersona); });
  $("persona-text").addEventListener("input", () => {
    if (state.busy || state.jsonDirty || state.pendingOperation || !Object.hasOwn(state.draft?.plugins?.ai?.personas || {}, state.personaSelected)) return;
    Object.defineProperty(state.draft.plugins.ai.personas, state.personaSelected, { value: $("persona-text").value, enumerable: true, writable: true, configurable: true });
    $("persona-error").textContent = ""; markDirty();
  });
  for (const [id, action] of [["persona-copy", () => createPersona(true)], ["persona-new", () => createPersona(false)], ["persona-delete", deletePersona]]) $(id).addEventListener("click", () => { try { action(); } catch (error) { $("persona-error").textContent = error.message; } });
  $("refresh-status").addEventListener("click", () => { void runAction(() => refreshStatus(true)); });
  $("refresh-qq").addEventListener("click", () => { void runAction(() => refreshQq(true)); });
  $("refresh-contacts").addEventListener("click", () => { void loadContacts(true); });
  $("refresh-stickers").addEventListener("click", () => { void refreshStickers(); });
  $("sticker-group").addEventListener("change", () => { void selectStickerGroup($("sticker-group").value); });
  $("reindex-sticker-group").addEventListener("click", () => { void mutateSticker("reindex"); });
  $("retry-sticker-operation").addEventListener("click", () => { void retryStickerOperation(); });
  for (const [id, direction] of [["stickers-prev", -1], ["stickers-next", 1]]) $(id).addEventListener("click", () => { void loadStickerItems(state.stickers.page + direction); });
  for (const [id, direction] of [["sticker-groups-prev", -1], ["sticker-groups-next", 1]]) $(id).addEventListener("click", () => { void changeStickerGroupPage(state.stickers.groupPage + direction); });
  $("refresh-scope-plugins").addEventListener("click", () => { void loadScopePlugins(); });
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
  $("save-config-top").addEventListener("click", () => { void runAction(() => saveConfig(false)); });
  $("fetch-models").addEventListener("click", () => { void fetchModels(); });
  $("model-list").addEventListener("change", () => {
    if (!state.models.items.includes($("model-list").value)) return;
    inputs.get("llm.model").control.value = $("model-list").value; markDirty();
  });
  $("manual-model").addEventListener("click", () => { state.models.manual = !state.models.manual; renderModels(); });
  $("save-apply").addEventListener("click", () => { void runAction(() => saveConfig(true)); });
  $("get-qr").addEventListener("click", () => { void runAction(getQr); });
  $("advanced").addEventListener("toggle", () => { if ($("advanced").open && !state.jsonDirty && state.config) syncJson(); });
  $("config-json").addEventListener("input", () => { state.jsonDirty = true; state.dirty = true; invalidateModels(); invalidateEmbeddingTest(); $("json-state").textContent = "JSON 有未采用修改，分组表单已暂时锁定。"; updateControls(); });
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
  $("refresh-learning").addEventListener("click", () => { void loadLearning(); });
  $("new-learning").addEventListener("click", () => { if (!state.busy) openLearningEditor(); });
  $("cancel-learning-edit").addEventListener("click", cancelLearningEdit);
  $("learning-editor-form").addEventListener("input", () => { state.learning.dirty = true; });
  $("learning-editor-form").addEventListener("submit", (event) => { event.preventDefault(); void runAction(async () => { try { await saveLearning(); } catch (error) { $("learning-editor-error").textContent = error.message; throw error; } }); });
  $("context-details").addEventListener("toggle", () => { if ($("context-details").open) void runAction(loadContext); });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) { stopTimers(); $("poll-state").textContent = "页面隐藏，已暂停轮询"; }
    else if (state.authenticated) { $("poll-state").textContent = "自动同步"; void refreshStatus(); void refreshQq(); }
  });
  window.addEventListener("beforeunload", (event) => { if (hasEdits() || state.pendingOperation || state.stickers.operation) { event.preventDefault(); event.returnValue = ""; } });
  window.addEventListener("pagehide", stopTimers);
  window.addEventListener("pageshow", (event) => { if (event.persisted) { clearSession(); void bootstrap(); } });
  void bootstrap();
})();
