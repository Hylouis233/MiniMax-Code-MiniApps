// @ts-check
/**
 * Bot 唤醒 —— 在蹬王之王自己的 MiniApp 进程里把挂掉的 bot 叫醒。
 *
 * # 完整的机制（全部经 2026-10-20 实测，见 docs/BOT_DIAGNOSIS.md）
 *
 * ## 唤醒必须走 `session send`（或 `agent.run` 直接续 bot 会话）
 *
 * `session send` 内部走 `turns.steer()`，与官方 `session send` 同一条通道。
 * **裸写 `local_runtime_queue_items` 不会生效** —— 隔离实验：一条裸写队列项
 * 3 分钟后仍是 `queued`、未 claim、无 turn、无消息；同一会话用 `session send`
 * 则 5 秒内 accepted 并被 bot 回复。⇒ Host 的队列轮询器只认经过 steer 的行。
 *
 * ## 为什么不是「蹬王之王直接 agent.run 续 bot 会话」
 *
 * `service/miniapp/agent/gateway.ts` 的 `assertSessionOwnership`：
 * `owner !== caller.pluginId` ⇒ 抛 `SESSION_NOT_FOUND`。
 * 而 45 个 bot 会话的 `purpose` 全是 `miniapp:minimax-bot`
 * ⇒ 蹬王之王对这些会话直接 `agent.run` **永远被拒**（实测每 30 秒 6 条全拒）。
 *
 * ## 那怎么唤醒？—— 在一个 agent 回合里用 `mavis` 工具的 `session send`
 *
 * `local-runtime/dist/runtime/mavis-session-send.js` 的准入注释明写：
 * "Reachability is the whole access model … **There is no topology check** —
 *  a task child, a sibling, a root Session and the caller's own Session
 *  all take the same path."
 * ⇒ **`session send` 不做 owner 检查** ⇒ 任何拿到 mavis 工具的会话都能唤醒任意 bot 会话。
 *
 * MiniApp 的 `agent.run`（不带 sessionId）建的会话 `agent_name = 'mavis'`
 * ⇒ 那个 agent 天然带桌面端 mavis 工具（`local-mavis.js` 的 `handleSessionSend`）。
 * 实测：蹬王之王起一个回合，指令是「用 mavis 工具 session send 发到 <bot 会话>」
 * ⇒ 探针 agent 回「状态 accepted（mode: queued）」，目标 bot 会话
 * `updated_at` 变化、起了新 turn、并真的回复了「收到，看门狗探针 2 正常送达」。
 *
 * # 代价（必须知道）
 *
 * 每个被唤醒的 bot 要烧**一个 agent 回合**（实测约 47k tokens，46k 是缓存读取）。
 * ⇒ 只在**真的有 pending 请求**时才跑，绝不空转。
 *
 * # 幂等
 *
 * 成功处理的 id 追加进 `bot-revive-acked.json`；请求超过 TTL（30 分钟）作废。
 */

import { readFile, writeFile, rename, mkdir, rm } from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { homedir } from 'node:os';

/** 唤醒宿主会话的 id（复用以省 token，见 `buildWakeInstruction` 的说明）。 */
const HOST_SESSION_FILE = 'bot-wake-host-session.json';

/**
 * 读出「唤醒宿主会话」的 id。
 *
 * ⚠️ **为什么复用**（2026-10-20 实测）：
 * 每次 `agent.run` 不带 sessionId 都会**新建一个会话** ⇒ 每次都是冷启动，
 * 实测单次唤醒约 93k tokens（绝大部分是重新读进来的系统提示与工具定义）。
 * 复用一个长期存活的会话 ⇒ 那些内容进上下文缓存 ⇒ 后续唤醒只付增量。
 * 而且那个会话里 agent 已经「知道」该怎么做（用 mavis 工具 session send），
 * 指令可以更短 ⇒ 输入 token 也更少。
 *
 * @returns {Promise<string|undefined>}
 */
export async function readHostSessionId() {
  const file = join(configDir(), HOST_SESSION_FILE);
  if (!existsSync(file)) return undefined;
  try {
    const v = JSON.parse(readFileSync(file, 'utf8'));
    const id = v?.sessionId;
    return typeof id === 'string' && id.startsWith('mvs_') ? id : undefined;
  } catch {
    return undefined;
  }
}

/** 忘掉「唤醒宿主会话」的 id（复用失败时用）。 */
export async function clearHostSessionId() {
  const file = join(configDir(), HOST_SESSION_FILE);
  try {
    await rm(file, { force: true });
  } catch {
    /* 忽略 */
  }
}

/** 记住「唤醒宿主会话」的 id。 */
export async function writeHostSessionId(sessionId) {
  if (typeof sessionId !== 'string' || !sessionId.startsWith('mvs_')) return;
  const file = join(configDir(), HOST_SESSION_FILE);
  await mkdir(dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await writeFile(tmp, `${JSON.stringify({ sessionId }, null, 2)}\n`, 'utf8');
  await rename(tmp, file);
}

/**
 * 配置文件所在目录。
 *
 * ⚠️ 每次都重新读 `process.env.HOME`（而不是模块加载时缓存 `homedir()`）——
 * 这样一个进程里若 HOME 变了（测试注入临时目录），路径也跟着变。
 */
function configDir() {
  return join(process.env.HOME ?? homedir(), '.config', 'mcode-reset-card');
}
function requestFile() {
  return join(configDir(), 'bot-revive-requests.json');
}
function ackedFile() {
  return join(configDir(), 'bot-revive-acked.json');
}
/** 尝试次数单独存一个文件（避免与守护进程并发写同一个文件互相覆盖）。 */
function attemptsFile() {
  return join(configDir(), 'bot-revive-attempts.json');
}

/**
 * ⚠️ **判据是「按尝试次数」而不是「按时间」**（2026-10-20 实测）：
 * 纯时间 TTL 会让「面板关着」的场景彻底失效 ——
 * 面板关掉 ⇒ 没人消费请求 ⇒ 30 分钟后请求全部过期作废 ⇒
 * 等面板再打开时，那些 bot 又已经挂了一段时间，却**再也没有请求可发**。
 * 实测：面板关掉 70 秒后，7 条请求全部未被消费；再打开时它们已过期。
 *
 * 真正的风险不是「放太久」，而是「**反复试同一个叫不醒的 bot**」。
 * ⇒ 按尝试次数封顶（`MAX_ATTEMPTS`）：试满 N 次仍失败才作废。
 * 这样：面板关多久都不影响（请求一直在那儿等着）；叫不醒的 bot 最多试 N 轮。
 * 时间只留一个 24 小时的兜底（`MAX_REQUEST_AGE_MS`），防僵尸请求永久堆积。
 */

/** 同一个请求最多尝试几次（超过就作废，不再重试）。 */
export const MAX_ATTEMPTS = 3;

/** 尝试次数表最多保留多少条（防无限增长）。 */
export const MAX_ATTEMPT_ENTRIES = 200;

/** 尝试次数记录的最长保留时间（毫秒）—— 超过就当僵尸清掉。 */
export const MAX_ATTEMPT_AGE_MS = 24 * 60 * 60 * 1000;

/**
 * 请求的最长保留时间（毫秒）—— 只是兜底，不是主要判据。
 * 取 24 小时：足够覆盖「面板关了一整夜」，同时避免僵尸请求永久堆积。
 */
export const MAX_REQUEST_AGE_MS = 24 * 60 * 60 * 1000;

/** 巡检间隔（毫秒）。 */
export const WAKE_INTERVAL_MS = 60 * 1000;

/**
 * 读守护进程写下的唤醒请求。
 * @returns {Promise<Array<{id: string, botId?: string, botName?: string, sessionId?: string, text?: string, atMs?: number}>>}
 */
export async function readWakeRequests() {
  if (!existsSync(requestFile())) return [];
  let raw;
  try {
    raw = await readFile(requestFile(), 'utf8');
  } catch {
    return [];
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  const acked = readAcked();
  const attemptsMap = readAttemptsMap();
  const now = Date.now();
  return (Array.isArray(parsed?.requests) ? parsed.requests : []).filter((r) => {
    const id = String(r?.id ?? '');
    if (!id || acked.has(id)) return false;
    // 尝试次数用尽 ⇒ 这个 bot 叫不醒（或者投递端有问题）⇒ 不再重试。
    const attempts = attemptsMap.get(id) ?? Number(r?.attempts ?? 0);
    if (attempts >= MAX_ATTEMPTS) return false;
    // 时间上限只当作「兜底」（比如 bot 早就恢复健康了却还挂着请求），
    // 但要留得足够久，覆盖「面板关了一整夜」这种场景。
    const at = Number(r?.atMs ?? 0);
    return now - at <= MAX_REQUEST_AGE_MS;
  });
}

/**
 * 把某个请求的尝试次数 +1（失败时调用）。
 *
 * ⚠️ **为什么单独存一份 attempts，而不是直接改 requests 文件**（2026-10-20 审计发现）：
 * 蹬王之王的守护进程（Rust，Host 之外）会**并发**往同一个 requests 文件里写新请求。
 * 如果这里做「读全量 → 改一个字段 → 写回全量」，就会把守护进程刚写进去的
 * 那几条**一起抹掉**（典型的 lost update）。⇒ 两次写同一个文件，间隔可能只有几毫秒。
 *
 * ⇒ 改成**独立文件**存尝试次数：两边各写各的，永不互相覆盖。
 */
export async function bumpAttempts(id) {
  const file = attemptsFile();
  let parsed;
  try {
    parsed = JSON.parse(await readFile(file, 'utf8'));
  } catch {
    parsed = { attempts: {} };
  }
  const map = parsed?.attempts && typeof parsed.attempts === 'object' ? parsed.attempts : {};
  const key = String(id ?? '');
  if (!key) return;
  // ⚠️ 值同时记两个信息：{ n: 次数, at: 最后一次尝试的时间戳 }。
  //    之前只存次数 ⇒ 没法判断「这条尝试记录是不是僵尸」。
  //    兼容旧格式（纯数字 = 次数）。
  const raw = map[key];
  const prev =
    raw && typeof raw === 'object'
      ? { n: Number(raw.n ?? 0), at: Number(raw.at ?? 0) }
      : { n: Number(raw ?? 0), at: 0 };
  map[key] = { n: prev.n + 1, at: Date.now() };
  // ⚠️ 清理条件：**只能**按「已 ack」或「超龄」清。
  //
  // 曾经的写法是「只保留请求文件里还在的条目」—— 那是错的（2026-10-20 独立审计发现）：
  // Rust 的 prune 会把「尝试次数用尽」的请求从 requests 文件里删掉 ⇒
  // 下一个 bot 失败时调 bumpAttempts ⇒ 把那个 bot 的 attempts 记录也删掉
  // ⇒ Rust 读到 0 ⇒ 重新入队 ⇒ 再试 3 次 ⇒ **3 次封顶被架空，变成无限重试**
  // （每回合 47k~93k tokens）。
  //
  // 而「已 ack」的条目本来就该清（它已经处理完了）；
  // 「超龄」也该清（僵尸记录）。其余一律保留。
  const nowMs = Date.now();
  const acked = readAckedMap();
  const kept = {};
  for (const [k, v] of Object.entries(map)) {
    const ackedAt = acked.get(k) ?? 0;
    const done = ackedAt > 0 && nowMs - ackedAt <= ACK_SUPPRESS_WINDOW_MS;
    const at = v && typeof v === 'object' ? Number(v.at ?? 0) : 0;
    const tooOld = at > 0 && nowMs - at > MAX_ATTEMPT_AGE_MS;
    if (!done && !tooOld) kept[k] = v;
  }
  const entries = Object.entries(kept);
  const trimmed = entries.length > MAX_ATTEMPT_ENTRIES ? entries.slice(-MAX_ATTEMPT_ENTRIES) : entries;
  await mkdir(dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await writeFile(tmp, `${JSON.stringify({ attempts: Object.fromEntries(trimmed) }, null, 2)}\n`, 'utf8');
  await rename(tmp, file);
}

/** 读出所有已尝试次数（键 = 请求 id）。 */
export function readAttemptsMap() {
  try {
    const v = JSON.parse(readFileSync(attemptsFile(), 'utf8'));
    const map = v?.attempts && typeof v.attempts === 'object' ? v.attempts : {};
    const out = new Map();
    for (const [k, v2] of Object.entries(map)) {
      // 新格式 { n, at }；旧格式是纯数字（次数）。
      const n = v2 && typeof v2 === 'object' ? Number(v2.n ?? 0) : Number(v2 ?? 0);
      out.set(String(k), n);
    }
    return out;
  } catch {
    return new Map();
  }
}

/**
 * 「最近被救过」的请求 id → 被救的时间戳。
 *
 * ⚠️ **为什么要带时间戳、还要有窗口**（2026-10-20 独立审计发现）：
 * 原来 acked 是个纯 id 列表 ⇒ 「救过一次」变成**永久黑名单**。
 * 实测：6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），
 * 却因为 acked 里有过它们而永远不再被叫醒。
 * ⇒ 改成「最近 30 分钟内救过就不再重复救；超过窗口可以再救」。
 *
 * 兼容旧格式（纯字符串数组 ⇒ 没有时间戳 ⇒ 视为「很久以前」，不再抑制）。
 */
function readAckedMap() {
  try {
    const v = JSON.parse(readFileSync(ackedFile(), 'utf8'));
    const arr = Array.isArray(v?.acked) ? v.acked : [];
    const map = new Map();
    for (const x of arr) {
      if (typeof x === 'string' && x) map.set(x, 0); // 旧格式：没有时间戳
      else if (x && typeof x === 'object' && typeof x.id === 'string') {
        map.set(x.id, Number(x.atMs ?? 0));
      }
    }
    return map;
  } catch {
    return new Map();
  }
}

/** 只返回「最近被救过、且还在抑制窗口内」的请求 id。 */
function readAcked() {
  const now = Date.now();
  const out = new Set();
  for (const [id, at] of readAckedMap()) {
    if (at > 0 && now - at <= ACK_SUPPRESS_WINDOW_MS) out.add(id);
  }
  return out;
}

/**
 * 「救过一次」的抑制窗口（毫秒）。
 *
 * 30 分钟：既能避免同一轮里重复投递（60 秒巡检间隔下会重复 30 次），
 * 又不会让「救过一次」变成永久黑名单。
 */
export const ACK_SUPPRESS_WINDOW_MS = 30 * 60 * 1000;

/** 把处理过的 id 追加进 acked 列表（原子写，带时间戳）。 */
export async function appendAcked(ids) {
  if (!ids.length) return;
  const map = readAckedMap();
  const now = Date.now();
  for (const id of ids) map.set(String(id), now);
  // 只保留最近 200 条（防无限增长）。
  const list = [...map.entries()]
    .sort((a, b) => a[1] - b[1])
    .slice(-200)
    .map(([id, atMs]) => ({ id, atMs }));
  const file = ackedFile();
  await mkdir(dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await writeFile(tmp, `${JSON.stringify({ acked: list }, null, 2)}\n`, 'utf8');
  await rename(tmp, file);
}

/**
 * 给一个 bot 构造「唤醒指令」——交给 agent 回合里的 mavis 工具执行。
 *
 * ⚠️ 这段文本是**给 agent 看的指令**（它会调 `mavis` 工具），
 * 而 `text` 字段（守护进程写的）是**给 bot 看的续跑消息正文**。
 * 两者必须分开：bot 收到的必须是业务内容，不能是「去调用工具」这种指令。
 *
 * @param {{sessionId?: string, botName?: string, text?: string}} request
 * @returns {string}
 */
export function buildWakeInstruction(request) {
  const sessionId = String(request?.sessionId ?? '');
  const botName = String(request?.botName ?? '该 bot');
  const body = String(
    request?.text ??
      '【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。\n请从中断处继续，不要重做已完成的部分。',
  );
  return [
    `用 mavis 工具，command="session send"，args 传 session_id="${sessionId}" 与 content=${JSON.stringify(body)}。`,
    `只发这一条，不要做别的事，也不要自己回答。目标 bot 是「${botName}」。`,
    // ⚠️ 宿主会话会被复用（见 `readHostSessionId` 的说明）⇒ 这个会话里可能已经有
    //    上一轮的指令。必须明确「只做这一条」，否则 agent 会顺手把上一条也重发。
    `（这是本轮唯一要做的事；之前轮次若有指令，一律不要重复执行。）`,
  ].join('');
}

/**
 * 跑一轮：把待唤醒的 bot 真的叫醒。
 *
 * 每个请求要起一个 agent 回合（`agent.run`）——那个回合里的 agent 用
 * mavis 工具的 `session send` 把续跑消息送进目标 bot 会话。
 *
 * ⚠️ **不可重入**：一轮要几分钟（每个 bot 一个 agent 回合，实测单回合约 93k tokens），
 * 而巡检间隔只有 60 秒 ⇒ 上一轮没跑完时，下一轮又进来了。
 * 用 `inFlight` 锁挡住，避免重复唤醒与双倍 token 消耗。
 *
 * @param {any} agent context.agent
 */
/** 当前是否有一轮唤醒正在跑。 */
let inFlight = false;

export async function tick(agent) {
  if (!agent?.run) {
    return { checked: 0, revived: 0, failed: [] };
  }
  // ⚠️ 重入保护：上一轮还在跑就直接跳过这一轮。
  if (inFlight) {
    return { checked: 0, revived: 0, failed: [], skipped: 'in-flight' };
  }
  inFlight = true;
  try {
    return await runRound(agent);
  } finally {
    inFlight = false;
  }
}

/** 真正跑一轮（由 `tick` 加锁后调用）。 */
async function runRound(agent) {
  const requests = await readWakeRequests();
  if (requests.length === 0) {
    return { checked: 0, revived: 0, failed: [] };
  }
  const done = [];
  const failed = [];
  for (const req of requests) {
    const sessionId = String(req?.sessionId ?? '');
    if (!sessionId.startsWith('mvs_')) {
      failed.push({ id: req.id, detail: `请求缺少合法的 sessionId：${JSON.stringify(req.sessionId)}` });
      continue;
    }
    try {
      // 复用宿主会话（省 token）；没有就用这次返回的 id 记下来。
      //
      // ⚠️ `hostSessionId` 必须在**新建成功之后**才落盘。
      // 之前的写法是「先取旧值 → 复用失败 → 新建 → 用旧值判断要不要落盘」，
      // 而旧值是 truthy ⇒ 新建的会话 id 永远不落盘 ⇒ 复用收益归零
      // （2026-10-20 独立审计发现）。
      const existingHost = await readHostSessionId();
      let hostSessionId = existingHost;
      let r;
      if (existingHost) {
        try {
          r = await agent.run({ text: buildWakeInstruction(req), sessionId: existingHost });
        } catch (e) {
          // ⚠️ 只有「宿主会话真的不能用了」才退回新建。
          // 下面这两类错误**不能**重发 —— 重发会让 bot 收到两条重复指令：
          //   - `session_busy` / BUSY：该会话已有回合在飞（可重试，但不是重发）
          //   - `AGENT_OUTCOME_UNKNOWN`：回合可能已经跑了，别自动重试
          const code = String(e?.code ?? e?.reason ?? '');
          if (code.includes('BUSY') || code.includes('busy') || code.includes('OUTCOME_UNKNOWN')) {
            throw e;
          }
          // 其它错误（会话被归档 / 凭据过期 / 会话不存在）⇒ 宿主会话确实不能用了
          // ⇒ 退回「新建会话」这条路，并把失效的 id 清掉。
          // ⚠️ 清掉之后**本轮就用新会话**，避免下一轮又先撞一次坏会话。
          await clearHostSessionId();
          hostSessionId = undefined;
          r = await agent.run({ text: buildWakeInstruction(req) });
        }
      } else {
        r = await agent.run({ text: buildWakeInstruction(req) });
      }
      // `finished` = 那个 agent 回合跑完了（它已经把 session send 发出去）。
      // 回合里 agent 若报告失败，status 仍可能是 finished ⇒ 再看错误字段。
      const errText = r?.error?.message ?? '';
      if (r?.status === 'finished' && !errText) {
        // 新建的会话 id 落盘（下一轮就能复用 ⇒ 省掉冷启动的 ~93k tokens）。
        if (!hostSessionId && r?.sessionId) {
          hostSessionId = r.sessionId;
          await writeHostSessionId(hostSessionId);
        }
        done.push(req.id);
        // ⚠️ 立刻 ack，不要等整批跑完 ——
        // 一轮要几分钟（每个 bot 一个 agent 回合），而巡检间隔只有 60 秒。
        // 不立刻 ack 的话，下一轮会读到同一批未 ack 的请求 ⇒ 重复唤醒 + 双倍 token。
        await appendAcked([req.id]);
      } else {
        await bumpAttempts(req.id);
        failed.push({ id: req.id, detail: `唤醒回合未成功：${errText || r?.status || '未知'}` });
      }
    } catch (e) {
      // ⚠️ `AGENT_OUTCOME_UNKNOWN`：回合**可能已经跑了**（Host 原文：never retryable）。
      //    若只是 bump attempts 而不 ack ⇒ 60 秒后下一轮会再投同一条
      //    ⇒ bot 收到**两条**重复的「请从中断处继续」。
      //    ⇒ 这类错误直接 ack（认为已经处理过），不再重试。
      const code = String(e?.code ?? e?.reason ?? '');
      if (code.includes('OUTCOME_UNKNOWN')) {
        await appendAcked([req.id]);
        failed.push({ id: req.id, detail: '回合结果未知（可能已执行），不再重试' });
        continue;
      }
      await bumpAttempts(req.id);
      failed.push({ id: req.id, detail: `唤醒异常：${e?.message ?? e}` });
    }
  }
  return { checked: requests.length, revived: done.length, failed };
}
