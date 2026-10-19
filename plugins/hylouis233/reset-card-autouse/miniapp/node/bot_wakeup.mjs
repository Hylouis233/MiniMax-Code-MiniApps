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

import { readFile, writeFile, rename, mkdir } from 'node:fs/promises';
import { existsSync, readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { homedir } from 'node:os';

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

/** 请求的存活上限（毫秒）。与蹬王之王守护进程侧一致。 */
export const REQUEST_TTL_MS = 30 * 60 * 1000;

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
  const now = Date.now();
  return (Array.isArray(parsed?.requests) ? parsed.requests : []).filter((r) => {
    const id = String(r?.id ?? '');
    if (!id || acked.has(id)) return false;
    const at = Number(r?.atMs ?? 0);
    return now - at <= REQUEST_TTL_MS;
  });
}

/** 已处理的请求 id。 */
function readAcked() {
  try {
    const v = JSON.parse(readFileSync(ackedFile(), 'utf8'));
    return new Set(Array.isArray(v?.acked) ? v.acked.map(String) : []);
  } catch {
    return new Set();
  }
}

/** 把处理过的 id 追加进 acked 列表（原子写）。 */
export async function appendAcked(ids) {
  if (!ids.length) return;
  const acked = readAcked();
  for (const id of ids) acked.add(id);
  const list = [...acked].slice(-200);
  const file = ackedFile();
  await mkdir(dirname(file), { recursive: true });
  const tmp = `${file}.tmp`;
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
      const r = await agent.run({ text: buildWakeInstruction(req) });
      // `finished` = 那个 agent 回合跑完了（它已经把 session send 发出去）。
      // 回合里 agent 若报告失败，status 仍可能是 finished ⇒ 再看错误字段。
      const errText = r?.error?.message ?? '';
      if (r?.status === 'finished' && !errText) {
        done.push(req.id);
        // ⚠️ 立刻 ack，不要等整批跑完 ——
        // 一轮要几分钟（每个 bot 一个 agent 回合），而巡检间隔只有 60 秒。
        // 不立刻 ack 的话，下一轮会读到同一批未 ack 的请求 ⇒ 重复唤醒 + 双倍 token。
        await appendAcked([req.id]);
      } else {
        failed.push({ id: req.id, detail: `唤醒回合未成功：${errText || r?.status || '未知'}` });
      }
    } catch (e) {
      failed.push({ id: req.id, detail: `唤醒异常：${e?.message ?? e}` });
    }
  }
  return { checked: requests.length, revived: done.length, failed };
}
