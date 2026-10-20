// Isolated native-process measurement. No real QQ, provider, or production config.
// Node >=22.13 (node:sqlite); Windows or Linux resource sampling.
// node scripts/perf-smoke.mjs --binary target/release/qkbot.exe --output target/perf-smoke/latest.json
// Linux: node scripts/perf-smoke.mjs --container-image localhost/qkbot-r:bench --output target/perf-smoke/podman.json
import http from 'node:http';
import net from 'node:net';
import { createHash, randomUUID } from 'node:crypto';
import { spawn, execFile } from 'node:child_process';
import { mkdir, writeFile, readFile, stat, access } from 'node:fs/promises';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';
import { promisify } from 'node:util';
import { performance } from 'node:perf_hooks';

const execFileAsync = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const options = {};
for (let i = 2; i < process.argv.length; i += 2) {
  if (!['--binary', '--output', '--container-image'].includes(process.argv[i]) || !process.argv[i + 1]) {
    throw new Error('Usage: node scripts/perf-smoke.mjs [--binary PATH | --container-image IMAGE] [--output PATH]');
  }
  options[process.argv[i]] = process.argv[i + 1];
}
const executable = path.resolve(root, options['--binary'] ?? `target/release/qkbot${process.platform === 'win32' ? '.exe' : ''}`);
const containerImage = options['--container-image'];
if (containerImage && (process.platform !== 'linux' || options['--binary'])) throw new Error('Container mode requires Linux and cannot combine with --binary');
const containerName = `qkbot-perf-${randomUUID().slice(0, 8)}`;
const output = path.resolve(root, options['--output'] ?? 'target/perf-smoke/latest.json');
const runDir = path.join(root, 'target', 'perf-smoke', `${new Date().toISOString().replaceAll(':', '-')}-${randomUUID().slice(0, 8)}`);
const database = path.join(runDir, 'smoke.sqlite3');
const nativePlugins = ['help', 'checkin', 'wordle', 'kfc', 'meal', 'daily_partner', 'mc_status', 'sticker_save', 'mock_chat_image', 'mock_forward', 'nudge', 'mute_roulette'];
const botId = 10001;
const groups = [20001, 20002];
const modelDelayMs = 500;
const setupCode = `isolated-benchmark-${randomUUID()}`;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const round = value => Math.round(value * 100) / 100;
const mib = bytes => round(bytes / 1048576);
const started = performance.now();
const samples = [], phases = [], receipts = [], images = [], failures = [], modelCalls = [];
const sockets = new Set(), pending = new Map(), recalledGroups = new Set();
let socket, processHandle, measuredPid, sampler, adminEndpoint, mediaPort, authCookie, csrfToken, containerMetadata;
let phaseName = 'startup', stopSampling = false, sequence = 1, outgoingSequence = 50000;
let linuxTicks = 100, linuxPageSize = 4096, report, logTail = '', shutdownConfirmed = false;
let startupResolve, startupReject;
const startup = new Promise((resolve, reject) => { startupResolve = resolve; startupReject = reject; });
startup.catch(() => {});

async function within(promise, timeoutMs, message) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), timeoutMs); })]); }
  finally { clearTimeout(timer); }
}
async function until(fn, timeoutMs, message) {
  const deadline = performance.now() + timeoutMs;
  while (performance.now() < deadline) {
    const result = await fn();
    if (result) return result;
    await sleep(500);
  }
  throw new Error(message);
}
function frame(opcode, data = Buffer.alloc(0)) {
  const payload = Buffer.isBuffer(data) ? data : Buffer.from(data);
  if (payload.length > 65535) throw new Error('mock frame too large');
  const header = Buffer.alloc(payload.length < 126 ? 2 : 4);
  header[0] = 0x80 | opcode;
  header[1] = payload.length < 126 ? payload.length : 126;
  if (header.length === 4) header.writeUInt16BE(payload.length, 2);
  return Buffer.concat([header, payload]);
}
function readClientFrames(connection) {
  let buffered = Buffer.alloc(0);
  connection.on('data', data => {
    buffered = Buffer.concat([buffered, data]);
    while (buffered.length >= 2) {
      const opcode = buffered[0] & 0x0f, masked = (buffered[1] & 0x80) !== 0;
      let length = buffered[1] & 0x7f, offset = 2;
      if (length === 127) { connection.destroy(); return; }
      if (length === 126) {
        if (buffered.length < 4) return;
        length = buffered.readUInt16BE(2); offset = 4;
      }
      const payloadOffset = offset + (masked ? 4 : 0);
      if (buffered.length < payloadOffset + length) return;
      const payload = Buffer.from(buffered.subarray(payloadOffset, payloadOffset + length));
      if (masked) for (let i = 0; i < payload.length; i++) payload[i] ^= buffered[offset + i % 4];
      buffered = buffered.subarray(payloadOffset + length);
      if (opcode === 9) connection.write(frame(10, payload));
      if (opcode === 8) { connection.end(frame(8, payload)); return; }
    }
  });
}
function respond(response, data, status = 200) {
  response.writeHead(status, { 'Content-Type': 'application/json' });
  response.end(JSON.stringify(data));
}
const ok = data => ({ status: 'ok', retcode: 0, data });
const completion = (content, toolCalls) => ({
  id: `mock-${randomUUID()}`, object: 'chat.completion', model: 'local-smoke-model',
  choices: [{ index: 0, finish_reason: toolCalls ? 'tool_calls' : 'stop',
    message: { role: 'assistant', content: content ?? null, ...(toolCalls ? { tool_calls: toolCalls } : {}) } }],
  usage: { prompt_tokens: 100, completion_tokens: 25, total_tokens: 125 },
});
async function model(body, response) {
  const user = body.messages.filter(message => message.role === 'user').at(-1)?.content ?? '';
  let parsed;
  try { parsed = JSON.parse(user); } catch {}
  if (parsed && Array.isArray(parsed.sources)) {
    modelCalls.push({ kind: 'memory', conversation: parsed.guard.conversation_id, source_count: parsed.sources.length,
      elapsed_ms: round(performance.now() - started), input_json_bytes: Buffer.byteLength(JSON.stringify(body)) });
    const source = parsed.sources.find(source => source.role === 'user' && source.memory_eligible && source.text.includes('我喜欢白茶'));
    const existing = parsed.allowed_memories.find(memory => memory.text === '偏好饮品为白茶' && memory.subject_id === source?.subject_id);
    const changes = source && parsed.memory_write ? [{
      op: 'upsert', id: existing?.id ?? null, subject_id: source.subject_id, kind: 'preference',
      text: '偏好饮品为白茶', evidence: [{ source_key: source.key, quote: '我喜欢白茶' }], ttl_days: null,
    }] : [];
    await sleep(modelDelayMs);
    return respond(response, completion(JSON.stringify({
      summary: parsed.summary_enabled ? '本地测试会话讨论了茶饮偏好与程序资源。' : '', changes,
    })));
  }
  const current = parsed?.text ?? user;
  const marker = /(?:PERF|TOOL|RECALL)-\d+/.exec(current)?.[0];
  const waiting = [...pending.values()].find(value => value.marker === marker);
  if (!waiting) throw new Error('model request does not belong to the isolated input workload');
  const afterTool = body.messages.at(-1)?.role === 'tool';
  const tool = marker.startsWith('TOOL-');
  modelCalls.push({ kind: tool ? (afterTool ? 'tool_followup' : 'tool_request') : marker.startsWith('RECALL-') ? 'recall' : 'chat',
    elapsed_ms: round(performance.now() - started), input_json_bytes: Buffer.byteLength(JSON.stringify(body)) });
  if (marker.startsWith('RECALL-')) {
    // This sentence exists only in the extracted memory, not in raw input or summary.
    if (!JSON.stringify(body.messages).includes('偏好饮品为白茶')) throw new Error('expected extracted memory was not recalled');
    recalledGroups.add(waiting.group);
  }
  await sleep(modelDelayMs);
  if (tool && !afterTool) {
    if (!body.tools?.some(tool => tool.function?.name === 'clock_now')) throw new Error('clock tool was not available');
    return respond(response, completion(null, [{
      id: `clock-${marker}`, type: 'function', function: { name: 'clock_now', arguments: '{}' },
    }]));
  }
  if (tool && !body.messages.some(message => message.role === 'tool' && message.content)) throw new Error('clock result missing');
  return respond(response, completion(`${marker} 已收到。本机模拟响应完成。`));
}
async function receiveImage(segment) {
  const url = new URL(segment.data.uri);
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || Number(url.port) !== mediaPort) {
    throw new Error('refusing non-owned image URL');
  }
  const response = await fetch(url, { signal: AbortSignal.timeout(10000), redirect: 'error' });
  if (!response.ok) throw new Error(`local rendered image returned ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  if (bytes.length < 24 || !bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) throw new Error('rendered media is not PNG');
  const item = { phase: phaseName, bytes: bytes.length, width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
  images.push(item);
  return item;
}
const server = http.createServer(async (request, response) => {
  try {
    const chunks = [];
    let length = 0;
    for await (const chunk of request) {
      length += chunk.length;
      if (length > 512 * 1024) throw new Error('mock request exceeds bound');
      chunks.push(chunk);
    }
    const body = JSON.parse(Buffer.concat(chunks).toString() || '{}');
    if (request.url === '/api/get_login_info') return respond(response, ok({ uin: botId, nickname: 'local-smoke' }));
    if (request.url === '/api/get_group_member_info') return respond(response, ok({ member: {
      user_id: body.user_id, group_id: body.group_id, nickname: '测试成员', card: '', role: 'admin',
      sex: 'unknown', title: '', level: 0, join_time: 0, last_sent_time: 0,
    } }));
    if (request.url === '/v1/chat/completions') return await model(body, response);
    if (request.url === '/api/send_group_message') {
      const waiting = pending.get(body.group_id);
      const text = body.message?.map(segment => segment.data?.text ?? '').join('') ?? '';
      if (!waiting || (waiting.marker && !text.includes(waiting.marker))) throw new Error('unexpected bot receipt');
      const fetched = [];
      for (const segment of body.message ?? []) if (segment.type === 'image') fetched.push(await receiveImage(segment));
      const receipt = { phase: phaseName, group: body.group_id, label: waiting.label,
        latency_ms: round(performance.now() - waiting.started), image_count: fetched.length };
      receipts.push(receipt);
      pending.delete(body.group_id);
      respond(response, ok({ message_seq: outgoingSequence++, time: Math.floor(Date.now() / 1000) }));
      waiting.resolve({ ...receipt, text });
      return;
    }
    throw new Error(`unexpected local mock route: ${request.url}`);
  } catch (error) {
    failures.push(error.message);
    if (!response.headersSent) respond(response, { error: 'local mock failure' }, 500);
    else response.end();
  }
});
server.on('connection', connection => {
  sockets.add(connection);
  connection.on('close', () => sockets.delete(connection));
});
server.on('upgrade', (request, connection) => {
  if (request.url !== '/event' || typeof request.headers['sec-websocket-key'] !== 'string') { connection.destroy(); return; }
  const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  connection.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  socket = connection;
  readClientFrames(connection);
  connection.on('error', error => failures.push(`local websocket: ${error.message}`));
  startupResolve();
});

async function freePort() {
  const probe = net.createServer();
  await new Promise((resolve, reject) => { probe.once('error', reject); probe.listen(0, '127.0.0.1', resolve); });
  const port = probe.address().port;
  await new Promise(resolve => probe.close(resolve));
  return port;
}
async function sample(phase = phaseName) {
  const pid = measuredPid;
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('invalid child PID');
  let counters;
  if (process.platform === 'win32') {
    // Only a validated numeric PID enters shell code.
    const command = `$p=Get-Process -Id ${pid} -ErrorAction Stop; [pscustomobject]@{resident_bytes=$p.WorkingSet64;private_bytes=$p.PrivateMemorySize64;lifetime_peak_resident_bytes=$p.PeakWorkingSet64;cpu_seconds=$p.TotalProcessorTime.TotalSeconds} | ConvertTo-Json -Compress`;
    const { stdout } = await execFileAsync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
      windowsHide: true, timeout: 10000, maxBuffer: 16384,
    });
    counters = JSON.parse(stdout.trim());
  } else if (process.platform === 'linux') {
    const [status, rawStat, statm] = await Promise.all([
      readFile(`/proc/${pid}/status`, 'utf8'), readFile(`/proc/${pid}/stat`, 'utf8'), readFile(`/proc/${pid}/statm`, 'utf8'),
    ]);
    const fields = rawStat.slice(rawStat.lastIndexOf(')') + 2).trim().split(/\s+/);
    const pages = statm.trim().split(/\s+/).map(Number);
    counters = {
      resident_bytes: Number(/^VmRSS:\s+(\d+)/m.exec(status)?.[1] ?? pages[1] * linuxPageSize / 1024) * 1024,
      private_bytes: null, // Linux statm shared subtraction is not Windows PrivateMemorySize64.
      lifetime_peak_resident_bytes: Number(/^VmHWM:\s+(\d+)/m.exec(status)?.[1] ?? 0) * 1024,
      cpu_seconds: (Number(fields[11]) + Number(fields[12])) / linuxTicks,
    };
  } else throw new Error('Resource sampler supports Windows and Linux only');
  const value = { phase, elapsed_ms: round(performance.now() - started), ...counters };
  samples.push(value);
  return value;
}
function distribution(values) {
  const sorted = [...values].sort((a, b) => a - b), n = sorted.length;
  if (!n) return null;
  return { min: round(sorted[0]), median: round(n % 2 ? sorted[(n - 1) / 2] : (sorted[n / 2 - 1] + sorted[n / 2]) / 2),
    p95: round(sorted[Math.ceil(n * .95) - 1]), max: round(sorted.at(-1)) };
}
async function phase(name, action) {
  phaseName = name;
  const before = await sample(), receiptIndex = receipts.length, modelIndex = modelCalls.length, imageIndex = images.length;
  const wallStart = performance.now();
  const value = await action();
  const wall = performance.now() - wallStart;
  const after = await sample();
  const own = samples.filter(sample => sample.phase === name);
  const privateValues = own.map(sample => sample.private_bytes).filter(value => value !== null);
  const result = {
    name, elapsed_ms: round(wall), cpu_seconds: round(after.cpu_seconds - before.cpu_seconds),
    cpu_equivalent_one_core_percent: round((after.cpu_seconds - before.cpu_seconds) / ((after.elapsed_ms - before.elapsed_ms) / 1000) * 100),
    resident_mib_median: mib(distribution(own.map(sample => sample.resident_bytes)).median),
    sampled_peak_resident_mib: mib(Math.max(...own.map(sample => sample.resident_bytes))),
    sampled_peak_private_mib: privateValues.length ? mib(Math.max(...privateValues)) : null,
    replies: receipts.length - receiptIndex, model_requests: modelCalls.length - modelIndex, images: images.length - imageIndex,
    latency_ms: distribution(receipts.slice(receiptIndex).map(receipt => receipt.latency_ms)),
  };
  phases.push(result);
  console.log(JSON.stringify({ phase: result }));
  return value;
}
async function send(group, text, label, mention = false) {
  if (pending.has(group)) throw new Error('only one pending test input per group');
  const marker = /(?:PERF|TOOL|RECALL)-\d+/.exec(text)?.[0];
  const reply = new Promise(resolve => pending.set(group, { group, marker, label, started: performance.now(), resolve }));
  const time = Math.floor(Date.now() / 1000), actor = 30000 + group;
  socket.write(frame(1, JSON.stringify({
    time, self_id: botId, event_type: 'message_receive',
    data: { message_scene: 'group', peer_id: group, sender_id: actor, message_seq: sequence++, time,
      group_member: { user_id: actor, group_id: group, nickname: '测试成员', card: '群内显示名', role: 'admin' },
      segments: [...(mention ? [{ type: 'mention', data: { user_id: botId } }] : []), { type: 'text', data: { text } }] },
  })));
  return await within(reply, 45000, `reply timed out: ${label}`);
}
async function admin(route, method = 'GET', body) {
  const response = await fetch(`${adminEndpoint}${route}`, {
    method, headers: { ...(authCookie ? { Cookie: authCookie } : {}),
      ...(csrfToken ? { 'X-CSRF-Token': csrfToken } : {}), ...(body ? { 'Content-Type': 'application/json' } : {}) },
    ...(body ? { body: JSON.stringify(body) } : {}), signal: AbortSignal.timeout(20000),
  });
  if (!response.ok) throw new Error(`isolated admin ${route} returned ${response.status}`);
  const json = await response.json();
  return { json, cookie: response.headers.get('set-cookie')?.split(';')[0] };
}
function inspectDatabase() {
  const db = new DatabaseSync(database, { readOnly: true });
  try {
    const count = (table, condition = '1') => db.prepare(`SELECT COUNT(*) AS n FROM ${table} WHERE ${condition}`).get().n;
    return {
      messages: count('messages'), conversations: count('ai_conversations'), memories: count('ai_memories'),
      conversations_with_summary: count('ai_conversations', "summary != ''"),
      successful_clock_tools: count('tool_runs', "tool_name='clock_now' AND status='succeeded'"),
      reminders: count('reminder_jobs'), cancelled_reminders: count('reminder_jobs', "status='cancelled'"),
      wordle_games: count('wordle_games'), meal_items: count('meal_menu'), native_receipts: count('native_receipts'),
      outbox: db.prepare('SELECT state,COUNT(*) AS count FROM outbox GROUP BY state').all(),
      logical_database_bytes: db.prepare('PRAGMA page_count').get().page_count * db.prepare('PRAGMA page_size').get().page_size,
    };
  } finally { db.close(); }
}
async function fileSize(file) {
  try { return (await stat(file)).size; } catch (error) { if (error.code === 'ENOENT') return 0; throw error; }
}
async function stopChild() {
  if (!processHandle || processHandle.exitCode !== null || processHandle.signalCode !== null) return;
  if (authCookie) {
    try {
      await admin('/api/runtime/stop', 'POST');
      await until(async () => (await admin('/api/status')).json.phase === 'stopped', 15000, 'isolated Host stop timed out');
      shutdownConfirmed = true;
    } catch (error) { failures.push(error.message); }
  }
  const ended = new Promise(resolve => processHandle.once('exit', resolve));
  if (containerImage) {
    await execFileAsync('podman', ['stop', '--time', '10', containerName], { timeout: 20000 });
  } else processHandle.kill('SIGTERM');
  await within(ended, 10000, 'isolated child did not exit');
}

try {
  if (!containerImage) await access(executable);
  else {
    const inspected = JSON.parse((await execFileAsync('podman', ['image', 'inspect', containerImage], { timeout: 20000 })).stdout)[0];
    containerMetadata = { image: containerImage, id: inspected.Id, image_bytes: inspected.Size,
      source_revision: inspected.Labels?.['org.opencontainers.image.revision'] ?? inspected.Config?.Labels?.['org.opencontainers.image.revision'] ?? null,
      runtime_version: (await execFileAsync('podman', ['--version'])).stdout.trim(),
      network: 'host', user_namespace: 'keep-id:uid=10001,gid=10001', binary: '/usr/local/bin/qkbot' };
  }
  await mkdir(runDir, { recursive: true });
  if (process.platform === 'linux') {
    linuxTicks = Number((await execFileAsync('getconf', ['CLK_TCK'])).stdout.trim());
    linuxPageSize = Number((await execFileAsync('getconf', ['PAGESIZE'])).stdout.trim());
    if (!(linuxTicks > 0 && linuxPageSize > 0)) throw new Error('invalid Linux clock/page size');
  }
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const endpoint = `http://127.0.0.1:${server.address().port}`;
  const adminPort = await freePort();
  do { mediaPort = await freePort(); } while (mediaPort === adminPort);
  adminEndpoint = `http://127.0.0.1:${adminPort}`;
  const configPath = path.join(runDir, 'config.toml');
  await writeFile(configPath, `[app]
database = "smoke.sqlite3"
timezone = "Asia/Shanghai"
log = "warn"
[web]
enabled = true
listen = "127.0.0.1:${adminPort}"
token = "${setupCode}"
[qq_login]
enabled = false
[milky]
url = "${endpoint}"
bot_id = ${botId}
[llm]
chat_completions_url = "${endpoint}/v1/chat/completions"
model = "local-smoke-model"
api_key = "mock-only"
tool_calling = true
max_concurrent_requests = 2
[plugins.ai]
enabled = true
compact_after_messages = 8
compact_idle_secs = 30
memory_input_budget = 6000
memory_output_budget = 1600
[plugins.ai.defaults]
input_budget = 6000
output_budget = 800
recent_messages = 4
memory_read = true
memory_write = true
summary = true
allowed_tools = ["clock_now"]
[plugins.clock]
enabled = true
[plugins.reminder]
enabled = true
[plugins.native]
enabled = ${JSON.stringify(nativePlugins)}
meal_cooldown_secs = 0
[nonebot]
enabled = false
listen = "127.0.0.1:${mediaPort}"
media_dir = ${JSON.stringify(containerImage ? '/bench/media' : path.join(runDir, 'media'))}
plugins = []
[limits]
send_cooldown_secs = 0
sends_per_hour = 600
`, 'utf8');
  const env = { ...process.env };
  for (const name of Object.keys(env)) {
    if (name.startsWith('QKBOT_') || /^(?:HTTP|HTTPS|ALL|NO)_PROXY$/i.test(name)) delete env[name];
  }
  Object.assign(env, { QKBOT_API_KEY: 'mock-only', QKBOT_MILKY_TOKEN: 'mock-only',
    NO_PROXY: '127.0.0.1,localhost,::1', no_proxy: '127.0.0.1,localhost,::1' });
  const launchCommand = containerImage ? 'podman' : executable;
  const launchArgs = containerImage ? [
    'run', '--rm', '--name', containerName, '--network=host', '--userns=keep-id:uid=10001,gid=10001',
    '--volume', `${runDir}:/bench:Z`, '--workdir', '/bench',
    '--env', 'QKBOT_API_KEY=mock-only', '--env', 'QKBOT_MILKY_TOKEN=mock-only',
    '--env', 'NO_PROXY=127.0.0.1,localhost,::1',
    '--entrypoint', '/usr/local/bin/qkbot', containerImage, '--config', '/bench/config.toml',
  ] : ['--config', configPath];
  processHandle = spawn(launchCommand, launchArgs, { cwd: runDir, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'], env });
  const capture = data => { logTail = (logTail + data.toString()).slice(-16000).replaceAll(setupCode, '[isolated setup code]'); };
  processHandle.stdout.on('data', capture); processHandle.stderr.on('data', capture);
  processHandle.on('error', startupReject);
  processHandle.on('exit', (code, signal) => startupReject(new Error(`isolated bot exited: ${code ?? signal}`)));
  await within(startup, 20000, 'isolated bot did not connect to mock');
  const startupMs = performance.now() - started;
  measuredPid = processHandle.pid;
  if (containerImage) {
    const pids = (await execFileAsync('podman', ['top', containerName, 'hpid'], { timeout: 10000 })).stdout.trim().split(/\r?\n/).slice(1).map(value => Number(value.trim()));
    if (pids.length !== 1 || !Number.isSafeInteger(pids[0]) || pids[0] <= 0) throw new Error('container must have exactly one measurable bot process');
    measuredPid = pids[0];
    const hash = (await execFileAsync('podman', ['exec', containerName, 'sha256sum', '/usr/local/bin/qkbot'], { timeout: 10000 })).stdout.trim().split(/\s+/)[0];
    const bytes = Number((await execFileAsync('podman', ['exec', containerName, 'stat', '-c', '%s', '/usr/local/bin/qkbot'], { timeout: 10000 })).stdout.trim());
    if (!/^[a-f0-9]{64}$/.test(hash) || !Number.isSafeInteger(bytes) || bytes <= 0) throw new Error('invalid container executable identity');
    containerMetadata.binary_sha256 = hash; containerMetadata.binary_bytes = bytes;
  }
  sampler = (async () => { while (!stopSampling) { await sample(); await sleep(250); } })();
  sampler.catch(() => {});
  await phase('idle_before_admin_setup', () => sleep(2500));
  await phase('admin_password_setup', async () => {
    const grant = await admin('/api/auth/setup', 'POST', { code: setupCode, password: `Local-benchmark-${randomUUID()}` });
    authCookie = grant.cookie; csrfToken = grant.json.csrf_token;
    if (!authCookie || !csrfToken) throw new Error('isolated admin setup did not return a session');
  });
  const status = (await admin('/api/status')).json;
  if (!status.online || !nativePlugins.every(id => status.loaded_plugins.includes(id))) throw new Error('not all native plugins loaded');
  await phase('idle_after_admin_setup', () => sleep(2500));
  await phase('chat_20_two_groups', async () => {
    await Promise.all(groups.map(async (group, index) => {
      for (let n = 0; n < 10; n++) await send(group, `PERF-${index * 10 + n} 我喜欢白茶。请简短说明上下文和长期记忆的区别。`, 'chat', true);
    }));
  });
  await phase('clock_tool_two_groups', async () => {
    await Promise.all(groups.map((group, index) => send(group, `TOOL-${index} 请调用工具查询现在时间。`, 'clock tool', true)));
  });
  await phase('reminder_create_list_cancel', async () => {
    const created = await send(groups[0], '/remind add 3600 BENCH-REMINDER', 'reminder create');
    const id = /[a-f0-9]{32}/i.exec(created.text)?.[0];
    if (!id) throw new Error('reminder create returned no ID');
    const listed = await send(groups[0], '/remind list', 'reminder list');
    if (!listed.text.includes('BENCH-REMINDER')) throw new Error('created reminder not listed');
    await send(groups[0], `/remind cancel ${id}`, 'reminder cancel');
  });
  await phase('wordle_first_image', async () => {
    const reply = await send(groups[0], '/wordle -l 5', 'wordle start');
    if (!reply.image_count) throw new Error('Wordle did not render an image');
  });
  await phase('wordle_guess_warm', async () => {
    const reply = await send(groups[0], '/wordle apple', 'wordle guess');
    if (!reply.image_count) throw new Error('Wordle guess did not render an image');
    await send(groups[0], '/wordle_stop', 'wordle stop');
  });
  await phase('calendar_warm', async () => {
    const reply = await send(groups[0], '/签到', 'deer calendar');
    if (!reply.image_count) throw new Error('deer did not render a calendar');
  });
  await phase('meal_menu_warm', async () => {
    const reply = await send(groups[0], '/查看菜单 菜品', 'meal menu');
    if (!reply.image_count) throw new Error('meal did not render its menu');
  });
  await phase('memory_maintenance_wait_and_extract', async () => {
    // Production maintenance cadence (60 seconds) is unchanged.
    let loggedAt = 0;
    await until(() => {
      const value = inspectDatabase();
      if (performance.now() - loggedAt > 20000) {
        console.log(JSON.stringify({ waiting_for_maintenance: { memories: value.memories, summaries: value.conversations_with_summary } }));
        loggedAt = performance.now();
      }
      return value.memories >= 2 && value.conversations_with_summary >= 2;
    }, 140000, 'background memory/summary did not commit for both groups');
  });
  await phase('memory_recall_two_groups', async () => {
    await Promise.all(groups.map((group, index) => send(group, `RECALL-${index} 我的白茶偏好是什么？`, 'memory recall', true)));
    if (recalledGroups.size !== 2) throw new Error('both groups must recall committed memories');
  });
  await phase('idle_after_all_features', () => sleep(3000));
  const db = inspectDatabase();
  if (db.successful_clock_tools !== 2 || db.cancelled_reminders !== 1 || failures.length) throw new Error(`workload verification failed: ${JSON.stringify(failures)}`);
  report = {
    schema_version: 2, measured_at: new Date().toISOString(),
    build: containerImage ? { executable: containerMetadata.binary, bytes: containerMetadata.binary_bytes, sha256: containerMetadata.binary_sha256 }
      : { executable: path.relative(root, executable).replaceAll('\\', '/'), bytes: (await stat(executable)).size,
        sha256: createHash('sha256').update(await readFile(executable)).digest('hex') },
    container: containerMetadata ?? null,
    environment: { platform: `${process.platform}/${process.arch}`, os_release: os.release(), cpu_model: os.cpus()[0]?.model,
      logical_cpus: os.cpus().length, host_total_memory_bytes: os.totalmem(), node: process.version,
      resident_metric: process.platform === 'win32' ? 'Windows WorkingSet64' : 'Linux /proc VmRSS',
      private_metric: process.platform === 'win32' ? 'Windows PrivateMemorySize64 (commit; not resident)' : 'not measured',
      sampler_interval_ms: 250, sampler_note: '250 ms sleep plus one sampler command/read; actual sample timestamps retained' },
    configuration: { native_plugins: nativePlugins, loaded_plugins: status.loaded_plugins, ai: true, clock: true, reminder: true,
      web: true, nonebot_external: false, groups: 2, model_mock_delay_ms: modelDelayMs, cooldown_seconds: 0,
      input_budget: 6000, output_budget: 800, recent_messages: 4, compact_after_messages: 8, compact_idle_secs: 30,
      memory_input_budget: 6000, memory_output_budget: 1600, initial_database: 'empty' },
    startup_to_mock_ws_ms: round(startupMs), phases, receipts, images, model_calls: modelCalls, database_counts: db,
    lifetime_peak_resident_mib: mib(Math.max(...samples.map(sample => sample.lifetime_peak_resident_bytes))),
    sampled_peak_private_mib: process.platform === 'win32' ? mib(Math.max(...samples.map(sample => sample.private_bytes))) : null,
    total_child_cpu_seconds: samples.at(-1).cpu_seconds, samples,
    limitations: [
      'Only the isolated bot process is measured; Node/PowerShell, QQ/Milky, real model servers, Python/browser and container runtime/VM overhead are excluded.',
      'Container mode, when selected, measures podman top HPID via host /proc; it is not cgroup memory or whole-container resource accounting. The deployment entrypoint is bypassed with an explicit synthetic config.',
      'All 12 Rust native plugins are loaded, but only Wordle, deer calendar and meal-menu image paths are exercised; external-network plugins are not benchmarked.',
      'Mock model waits 500 ms per request; chat/tool/memory latencies include that wait. Summarizer output is deterministic, not an intelligence-quality evaluation.',
      'The memory phase includes the unchanged production 60-second maintenance cadence and a shortened configured 30-second idle threshold.',
      'Admin password initialization is a separate first-run Argon2 workload; its peak is included in the lifetime peak.',
      'Fresh database, 2 active conversations and 20 initial chat turns; this is not sustained-load, database-scale, reconnect or failure benchmarking.',
      'Image timings include actual font/dictionary loading, rendering, PNG encoding and fetching the controlled local media URL.',
      'Windows working set and private commit are different measures; sampled peaks may miss short transients. No memory-size acceptance threshold.',
      'Temporary config, auth database and message database remain in target/perf-smoke; they contain synthetic test data only.',
    ],
  };
} catch (error) {
  console.error(error.message);
  if (logTail) console.error(logTail);
  process.exitCode = 1;
} finally {
  stopSampling = true;
  if (sampler) await sampler.catch(error => { failures.push(`sampler: ${error.message}`); });
  try { await stopChild(); } catch (error) { console.error(error.message); process.exitCode = 1; }
  if (containerImage) {
    // Only the unique container created by this run is eligible for cleanup.
    await execFileAsync('podman', ['rm', '--force', '--ignore', containerName], { timeout: 20000 }).catch(() => {});
  }
  for (const connection of sockets) connection.destroy();
  if (server.listening) await new Promise(resolve => server.close(resolve));
}
if (report && !process.exitCode) {
  report.shutdown = { host_stopped_via_admin: shutdownConfirmed,
    isolated_process_exited: processHandle.exitCode !== null || processHandle.signalCode !== null };
  report.storage = { after_process_exit: { sqlite_bytes: await fileSize(database), wal_bytes: await fileSize(`${database}-wal`),
    shm_bytes: await fileSize(`${database}-shm`) } };
  // Windows terminates the now-stopped control panel rather than delivering Unix
  // SIGTERM. Recover/checkpoint only this synthetic database after its child exits,
  // reporting both physical states rather than treating the small main file as all data.
  const checkpoint = new DatabaseSync(database);
  try { checkpoint.exec('PRAGMA wal_checkpoint(TRUNCATE)'); } finally { checkpoint.close(); }
  report.storage.after_explicit_checkpoint = { sqlite_bytes: await fileSize(database), wal_bytes: await fileSize(`${database}-wal`),
    shm_bytes: await fileSize(`${database}-shm`) };
  report.elapsed_total_ms = round(performance.now() - started);
  report.failures = failures;
  if (failures.length || !shutdownConfirmed) throw new Error(`benchmark cleanup/verification incomplete: ${JSON.stringify(failures)}`);
  await mkdir(path.dirname(output), { recursive: true });
  await writeFile(path.join(runDir, 'metrics.json'), JSON.stringify(report, null, 2));
  await writeFile(output, JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ report_path: output, replies: receipts.length, model_requests: modelCalls.length,
    images: images.length, startup_to_mock_ws_ms: report.startup_to_mock_ws_ms, database_counts: report.database_counts,
    storage: report.storage, shutdown: report.shutdown, build: report.build, lifetime_peak_resident_mib: report.lifetime_peak_resident_mib }, null, 2));
}
