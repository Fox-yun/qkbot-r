// Local smoke measurement, not a production benchmark. Uses only Node built-ins.
// Run after cargo build --release: node scripts/perf-smoke.mjs
import http from 'node:http';
import { createHash, randomUUID } from 'node:crypto';
import { spawn, execFile } from 'node:child_process';
import { mkdir, writeFile, access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';
import { promisify } from 'node:util';
import { performance } from 'node:perf_hooks';

const execFileAsync = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const executable = path.join(root, 'target', 'release', process.platform === 'win32' ? 'qkbot.exe' : 'qkbot');
const scratchRoot = path.join(root, 'target', 'perf-smoke');
const runDir = path.join(scratchRoot, `${new Date().toISOString().replaceAll(':', '-')}-${randomUUID().slice(0, 8)}`);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const round = value => Math.round(value * 100) / 100;
const mib = bytes => round(bytes / 1048576);
const botId = 10001;
const groups = [20001, 20002];
const modelDelayMs = 500;
const samples = [];
const pending = new Map();
const latencies = [];
const failures = [];
const sockets = new Set();
let socket;
let processHandle;
let modelRequests = 0;
let replyCount = 0;
let outgoingSequence = 50000;
let logTail = '';
let stopSampling = false;
let sampler;
let startupResolve;
let startupReject;
const startup = new Promise((resolve, reject) => { startupResolve = resolve; startupReject = reject; });
startup.catch(() => {});

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
      const opcode = buffered[0] & 0x0f;
      const masked = (buffered[1] & 0x80) !== 0;
      let length = buffered[1] & 0x7f;
      let offset = 2;
      if (length === 127) { connection.destroy(); return; }
      if (length === 126) {
        if (buffered.length < 4) return;
        length = buffered.readUInt16BE(2); offset = 4;
      }
      const payloadOffset = offset + (masked ? 4 : 0);
      if (buffered.length < payloadOffset + length) return;
      const payload = Buffer.from(buffered.subarray(payloadOffset, payloadOffset + length));
      if (masked) for (let i = 0; i < payload.length; i++) payload[i] ^= buffered[offset + (i % 4)];
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
    if (request.url === '/v1/chat/completions') {
      modelRequests++;
      const current = body.messages.filter(message => message.role === 'user').at(-1)?.content ?? '';
      const id = /PERF-\d+/.exec(current)?.[0];
      if (!id || !pending.has(id)) throw new Error('mock model cannot identify current input');
      await sleep(modelDelayMs);
      return respond(response, {
        id: `mock-${id}`, object: 'chat.completion', model: 'local-smoke-model',
        choices: [{ index: 0, finish_reason: 'stop', message: { role: 'assistant', content: `${id} 已收到。这是本机模拟响应，仅用于测量程序自身占用。` } }],
        usage: { prompt_tokens: 100, completion_tokens: 25, total_tokens: 125 },
      });
    }
    if (request.url === '/api/send_group_message') {
      const text = body.message?.map(segment => segment.data?.text ?? '').join('') ?? '';
      const id = /PERF-\d+/.exec(text)?.[0];
      const waiting = pending.get(id);
      if (!waiting || waiting.group !== body.group_id) {
        failures.push({ reason: 'unexpected bot receipt', group: body.group_id, text: text.slice(0, 200) });
      } else {
        pending.delete(id);
        replyCount++;
        latencies.push(performance.now() - waiting.started);
        waiting.resolve();
      }
      return respond(response, ok({ message_seq: outgoingSequence++, time: Math.floor(Date.now() / 1000) }));
    }
    throw new Error(`unexpected local mock route: ${request.url}`);
  } catch (error) {
    failures.push({ reason: error.message });
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
  connection.on('error', error => failures.push({ reason: `local websocket: ${error.message}` }));
  startupResolve();
});

async function sample(phase) {
  if (process.platform !== 'win32') throw new Error('this resource sampler currently requires Windows');
  const pid = processHandle.pid;
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error('invalid child PID');
  // Only a validated numeric PID is interpolated; no paths, credentials or user text enter shell code.
  const command = `$p=Get-Process -Id ${pid} -ErrorAction Stop; [pscustomobject]@{working_set_bytes=$p.WorkingSet64;private_bytes=$p.PrivateMemorySize64;peak_working_set_bytes=$p.PeakWorkingSet64;cpu_seconds=$p.TotalProcessorTime.TotalSeconds} | ConvertTo-Json -Compress`;
  const { stdout } = await execFileAsync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', command], {
    windowsHide: true, timeout: 10000, maxBuffer: 16384,
  });
  const value = { phase, elapsed_ms: performance.now(), ...JSON.parse(stdout.trim()) };
  samples.push(value);
  return value;
}

async function within(promise, timeoutMs, message) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), timeoutMs); })]); }
  finally { clearTimeout(timer); }
}

async function send(group, number) {
  const id = `PERF-${number}`;
  const reply = new Promise(resolve => pending.set(id, { group, started: performance.now(), resolve }));
  socket.write(frame(1, JSON.stringify({
    time: Math.floor(Date.now() / 1000), self_id: botId, event_type: 'message_receive',
    data: { message_scene: 'group', peer_id: group, sender_id: 30000 + group, message_seq: number,
      segments: [{ type: 'mention', data: { user_id: botId } }, { type: 'text', data: { text: `${id} 小葵，请简短介绍上下文与长期记忆有什么区别。` } }] },
  })));
  await within(reply, 15000, `no mock reply for ${id}`);
}

try {
  await access(executable);
  await mkdir(runDir, { recursive: true });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const endpoint = `http://127.0.0.1:${server.address().port}`;
  const configPath = path.join(runDir, 'config.toml');
  await writeFile(configPath, `[app]
database = "smoke.sqlite3"
log = "warn"
[milky]
url = "${endpoint}"
bot_id = ${botId}
[llm]
chat_completions_url = "${endpoint}/v1/chat/completions"
model = "local-smoke-model"
api_key = "mock-only"
tool_calling = true
max_concurrent_requests = 2
[access]
groups = [${groups.join(', ')}]
[plugins.ai]
enabled = true
compact_after_messages = 1000
compact_idle_secs = 3600
[plugins.ai.defaults]
input_budget = 6000
output_budget = 800
recent_messages = 24
memory_read = true
memory_write = true
summary = true
allowed_tools = ["clock_now"]
[plugins.clock]
enabled = true
[plugins.reminder]
enabled = false
[limits]
send_cooldown_secs = 0
sends_per_hour = 120
`, 'utf8');
  processHandle = spawn(executable, ['--config', configPath], {
    cwd: root, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, QKBOT_API_KEY: 'mock-only', QKBOT_MILKY_TOKEN: 'mock-only', NO_PROXY: '127.0.0.1,localhost,::1', no_proxy: '127.0.0.1,localhost,::1' },
  });
  const capture = data => { logTail = (logTail + data.toString()).slice(-16000); };
  processHandle.stdout.on('data', capture); processHandle.stderr.on('data', capture);
  processHandle.on('error', startupReject);
  processHandle.on('exit', (code, signal) => startupReject(new Error(`bot stopped before readiness: ${code ?? signal}; ${logTail}`)));
  await within(startup, 15000, `bot did not connect to local event mock; ${logTail}`);
  await sleep(750);
  for (let i = 0; i < 3; i++) { await sample('idle'); await sleep(250); }
  const start = performance.now();
  const cpuStart = samples.at(-1).cpu_seconds;
  sampler = (async () => { while (!stopSampling) { await sample('load'); await sleep(250); } })();
  sampler.catch(() => {}); // The awaited sampler below reports errors after cleanup can run.
  await within(Promise.all(groups.map(async (group, groupIndex) => {
    for (let n = 0; n < 10; n++) await send(group, 1 + groupIndex * 10 + n);
  })), 45000, 'local load phase timed out');
  const elapsedMs = performance.now() - start;
  stopSampling = true;
  await sampler;
  const final = await sample('after_load');
  if (failures.length || replyCount !== 20 || modelRequests !== 20) throw new Error(`incomplete smoke test: ${JSON.stringify({ replyCount, modelRequests, failures })}`);
  const idleSamples = samples.filter(value => value.phase === 'idle');
  const loadSamples = samples.filter(value => value.phase === 'load');
  const sortedLatencies = [...latencies].sort((a, b) => a - b);
  const report = {
    measured_at: new Date().toISOString(), executable, platform: `${process.platform}/${process.arch}`, os_release: os.release(),
    logical_cpus: os.cpus().length, configuration: { groups: 2, messages: 20, concurrent_conversations: 2, model_mock_delay_ms: modelDelayMs, cooldown_seconds: 0 },
    replies: replyCount, model_requests: modelRequests, elapsed_ms: round(elapsedMs),
    latency_ms: { min: round(sortedLatencies[0]), median: round((sortedLatencies[9] + sortedLatencies[10]) / 2), p95: round(sortedLatencies[18]), max: round(sortedLatencies.at(-1)) },
    latency_samples_ms: latencies.map(round),
    idle_rss_mib: mib(idleSamples.map(value => value.working_set_bytes).sort((a, b) => a - b)[1]),
    sampled_load_peak_rss_mib: mib(Math.max(...loadSamples.map(value => value.working_set_bytes))),
    sampled_load_peak_private_mib: mib(Math.max(...loadSamples.map(value => value.private_bytes))),
    cpu_seconds_delta: round(final.cpu_seconds - cpuStart),
    observed_lifetime_peak_rss_mib: mib(final.peak_working_set_bytes),
    limitations: ['Only the bot child process is measured; mocks, PowerShell, QQ and Milky are excluded.', 'Local fixed-delay JSON replies; no real model, TLS, tools or background memory extraction workload.', 'Short run with empty initial long-term memory; sampled memory peaks are not a long-term capacity guarantee.', 'The isolated benchmark child is terminated after measurement; its temporary database is not production data.'],
    samples: samples.map(value => ({ ...value, elapsed_ms: round(value.elapsed_ms - start) })),
  };
  await writeFile(path.join(runDir, 'metrics.json'), JSON.stringify(report, null, 2));
  await writeFile(path.join(scratchRoot, 'latest.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify({ ...report, samples: undefined, report_path: path.join(scratchRoot, 'latest.json') }, null, 2));
} catch (error) {
  console.error(error.message);
  if (logTail) console.error(logTail);
  process.exitCode = 1;
} finally {
  stopSampling = true;
  if (sampler) await sampler.catch(() => {});
  if (processHandle && processHandle.exitCode === null) {
    processHandle.kill();
    await within(new Promise(resolve => processHandle.once('exit', resolve)), 5000, 'mock child did not exit').catch(() => {});
  }
  for (const connection of sockets) connection.destroy();
  if (server.listening) await new Promise(resolve => server.close(resolve));
}
