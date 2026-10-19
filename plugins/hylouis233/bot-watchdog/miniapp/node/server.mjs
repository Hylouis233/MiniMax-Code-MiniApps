// @ts-check
/**
 * Bot 看门狗 — 监测 MiniMax Bot 的会话，挂掉了就自动重新启用。
 *
 * # 解决什么问题
 *
 * MiniMax Bot 的会话在遇到错误后会**新建一个会话**继续，于是表现为
 * 「bot 反复挂掉」：每挂一次多一个会话，界面上又是一条错误通知。
 *
 * 2026-10-19 实测（完整取证见 `docs/DIAGNOSIS.md`）：一天里 20+ 个
 * `purpose='miniapp:minimax-bot'` 的会话带错误码，两类：
 *   - 50113 / HTTP 500 / 网络切换 ⇒ **可自愈**，只是 Host 故意 fail-closed 没重试
 *   - 42212 Token Plan 到顶 ⇒ **重试无用**，必须人工补充额度
 *
 * # 唤醒通道：为什么是「Agent 两段跳」
 *
 * 候选通道与实测结果：
 *
 * | 通道 | 能否唤醒别的 Mini App 拥有的会话 | 结论 |
 * | --- | --- | --- |
 * | `context.agent.run({ sessionId })` | ❌ | 规范明确：「Each Mini App owns its sessions and can continue only those」 |
 * | 直连对方的 MCP HTTP 端点 | ❌ | Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到 |
 * | **`agent.run` 起一个 Agent，让它调 MCP 的 `message_bot`** | ✅ | 2026-10-19 实测：13 秒内唤起挂着 42212 的 bot 并拿到回复 |
 *
 * ⇒ 看门狗自己声明 `mcpServers` 指向 minimax-bot，
 * 再用 `agent.run` 起一个「看门狗 Agent」，让它通过 MCP 的 `message_bot`
 * 把续跑消息送给挂掉的 bot。**复用原会话，不新建。**
 *
 * # 关键约束
 *
 * - **绝不新建会话** —— 那正是「反复挂掉」的表象本身。
 * - **额度类错误不自动重试** —— 重试解决不了 42212，只会烧 token。
 * - **判据要回读** —— 会话上的 error_code 可能是陈旧值（上一轮 500 留下的，
 *   运行时没清）。必须同时确认「最近没有新活动」，否则会误伤正在干活的会话。
 */

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { existsSync, readFileSync, mkdirSync, writeFileSync, renameSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { homedir } from 'node:os';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { Verdict, classifyError, backoffMs, isIdle } from './classify.mjs';

const execFileAsync = promisify(execFile);

// ── 路径 ────────────────────────────────────────────────────────────────────
const HOME = homedir();
/** MiniMax Bot 的插件数据目录（它的 state.json 在这里）。 */
const BOT_STATE = join(
  HOME, '.minimax', 'v2', 'plugin-data', 'liveboards', 'minimax-bot', 'state.json',
);
/** 运行时状态库：会话状态与错误码都在这里。 */
const RUNTIME_DB = join(HOME, '.minimax', 'v2', 'sqlite', 'runtime-state.sqlite');
/** 本程序自己的状态（退避计时）。 */
const WATCHDOG_STATE = join(
  HOME, '.minimax', 'v2', 'plugin-data', 'liveboards', 'bot-watchdog', 'state.json',
);

/** 监测间隔（毫秒）。 */
const POLL_MS = 30_000;
/** 超过多久没有任何活动就算「闲下来」了。 */
const IDLE_MS = 90_000;
/** 自动重新启用的退避基数与上限。 */
const BACKOFF_BASE_MS = 5_000;
const BACKOFF_CAP_MS = 60_000;

/**
 * 读 MiniMax Bot 的 state.json，取出所有 bot → sessionId 映射。
 *
 * 结构：`sessions[chatId][botId] = { sessionId, ... }`
 * @returns {Promise<Array<{botId: string, chatId: string, sessionId: string}>>}
 */
export async function readBotSessions() {
  if (!existsSync(BOT_STATE)) return [];
  let raw;
  try {
    raw = await readFile(BOT_STATE, 'utf8');
  } catch {
    return [];
  }
  let state;
  try {
    state = JSON.parse(raw);
  } catch {
    return [];
  }
  // botId → 名字（MCP 的 message_bot 用**名字**寻址，不是 id）
  const names = new Map();
  for (const b of Array.isArray(state?.bots) ? state.bots : []) {
    if (b && typeof b.id === 'string' && typeof b.name === 'string') {
      names.set(b.id, b.name);
    }
  }

  const out = [];
  for (const [chatId, bots] of Object.entries(state?.sessions ?? {})) {
    if (!bots || typeof bots !== 'object') continue;
    for (const [botId, info] of Object.entries(bots)) {
      const sessionId = info?.sessionId;
      if (typeof sessionId === 'string' && sessionId.startsWith('mvs_')) {
        out.push({ botId, botName: names.get(botId) ?? botId, chatId, sessionId });
      }
    }
  }
  return out;
}

/**
 * 查一批会话的当前状态。
 *
 * 最后活动时间取 `local_runtime_message_rows` 里该会话最近一条消息的时间 ——
 * 这是「这个会话现在还在动吗」的权威依据（`error_code` 可能是陈旧值）。
 *
 * @param {string[]} sessionIds
 */
export async function readSessionStates(sessionIds) {
  const result = new Map();
  if (sessionIds.length === 0) return result;
  const ids = sessionIds.map((id) => `'${id.replace(/'/g, "''")}'`).join(',');
  const sql = `SELECT s.session_id AS sid, s.status AS status,
       IFNULL(s.error_code, 0) AS error_code, IFNULL(s.error_message, '') AS error_message,
       IFNULL((SELECT MAX(m.created_at_ms) FROM local_runtime_message_rows m
               WHERE m.session_id = s.session_id), 0) AS last_activity
     FROM local_runtime_sessions s WHERE s.session_id IN (${ids});`;
  try {
    const r = await execFileAsync('/usr/bin/sqlite3', ['-json', RUNTIME_DB, sql], {
      maxBuffer: 8 << 20,
    });
    for (const row of JSON.parse(r.stdout || '[]')) {
      result.set(row.sid, {
        status: String(row.status ?? ''),
        errorCode: Number(row.error_code ?? 0),
        errorMessage: String(row.error_message ?? ''),
        lastActivity: Number(row.last_activity ?? 0),
      });
    }
  } catch {
    /* 读不到就当没查到 */
  }
  return result;
}

/** 读本程序的状态。 */
function readWatchdogState() {
  try {
    return JSON.parse(readFileSync(WATCHDOG_STATE, 'utf8'));
  } catch {
    return { attempts: {} };
  }
}

/** 写本程序的状态（原子写）。 */
function writeWatchdogState(state) {
  try {
    mkdirSync(dirname(WATCHDOG_STATE), { recursive: true });
    const tmp = `${WATCHDOG_STATE}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`, 'utf8');
    renameSync(tmp, WATCHDOG_STATE);
  } catch {
    /* 状态写不进去不影响主流程 */
  }
}

/**
 * 唤醒一个挂掉的 bot。
 *
 * 通过 `agent.run` 起一个「看门狗 Agent」，让它用 MCP 的 `message_bot`
 * 把续跑消息投给目标 bot —— 这条路会**复用目标 bot 的原会话**。
 *
 * @param {any} agent context.agent
 * @param {string} botId 目标 bot
 * @param {string} botName 目标 bot 名（MCP 的 `to` 参数用名字）
 * @returns {Promise<{ok: boolean, detail: string}>}
 */
export async function reviveBot(agent, botId, botName) {
  if (!agent?.run) {
    return { ok: false, detail: '运行时未提供 agent 访问能力（需在 miniapp.json 声明 agentAccess）' };
  }
  const prompt = [
    '用 minimax-bot 的 message_bot 工具，给 bot「' + botName + '」发送下面这条消息。',
    '只发这一条，不要做别的事，也不要自己回答。',
    '',
    '消息正文：',
    '【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。',
    '请从中断处继续，不要重做已完成的部分。',
  ].join('\n');
  try {
    const r = await agent.run({ text: prompt });
    if (r?.status === 'finished') {
      const t = String(r?.text ?? '');
      if (/error|错误|失败|not found|unknown/i.test(t)) {
        return { ok: false, detail: `看门狗 Agent 回复异常：${t.slice(0, 200)}` };
      }
      return { ok: true, detail: t.slice(0, 200) || '已投递' };
    }
    return { ok: false, detail: `看门狗 Agent 未成功：${r?.error?.message ?? r?.status ?? '未知'}` };
  } catch (e) {
    return { ok: false, detail: `看门狗 Agent 异常：${/** @type {any} */ (e)?.message ?? e}` };
  }
}

/**
 * 跑一轮监测。
 * @param {any} agent
 */
export async function tick(agent) {
  const now = Date.now();
  const bots = await readBotSessions();
  const states = await readSessionStates(bots.map((b) => b.sessionId));
  const wd = readWatchdogState();
  wd.attempts ??= {};

  let healthy = 0;
  let needsUser = 0;
  let revived = 0;
  const items = [];

  for (const bot of bots) {
    const st = states.get(bot.sessionId);
    if (!st) continue;

    // 已经恢复活动（error_code 可能只是陈旧值）⇒ 撤销待办，不打扰它。
    const active = !isIdle(st.lastActivity, now, IDLE_MS);
    const prev = wd.attempts[bot.sessionId];
    if (active) {
      healthy += 1;
      if (prev) delete wd.attempts[bot.sessionId];
      continue;
    }

    const { verdict, reason } = classifyError(st.errorCode, st.errorMessage);

    if (verdict === Verdict.HEALTHY) {
      healthy += 1;
      if (prev) delete wd.attempts[bot.sessionId];
      continue;
    }

    if (verdict === Verdict.NEEDS_USER) {
      needsUser += 1;
      items.push({
        botId: bot.botId,
        botName: bot.botName,
        sessionId: bot.sessionId,
        verdict,
        reason,
        errorCode: st.errorCode,
        errorMessage: st.errorMessage,
        action: 'needs-user',
      });
      // 不自动重试，也不登记退避（免得额度补上后还在等退避）
      continue;
    }

    // ── 可自愈：退避后重新启用 ──
    const wait = backoffMs(prev?.count ?? 0, BACKOFF_BASE_MS, BACKOFF_CAP_MS);
    if (prev && now - (prev.at ?? 0) < wait) continue; // 还在退避窗口里

    const count = (prev?.count ?? 0) + 1;
    wd.attempts[bot.sessionId] = { at: now, count, reason };

    const res = await reviveBot(agent, bot.botId, bot.botName);
    if (res.ok) {
      revived += 1;
      items.push({
        botId: bot.botId,
        botName: bot.botName,
        sessionId: bot.sessionId,
        verdict,
        reason,
        errorCode: st.errorCode,
        errorMessage: st.errorMessage,
        action: 'revived',
        detail: res.detail,
      });
    } else {
      items.push({
        botId: bot.botId,
        botName: bot.botName,
        sessionId: bot.sessionId,
        verdict,
        reason,
        errorCode: st.errorCode,
        errorMessage: st.errorMessage,
        action: 'revive-failed',
        detail: res.detail,
      });
    }
  }

  writeWatchdogState(wd);
  return { checked: bots.length, healthy, needsUser, revived, items, atMs: now };
}

/** @type {any} */
let lastReport = { checked: 0, healthy: 0, needsUser: 0, revived: 0, items: [], atMs: 0 };

export async function start(context) {
  const clientHtml = await readFile(join(context.pluginRoot, 'miniapp/client/index.html'));
  const server = createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    const send = (code, body) => {
      res.writeHead(code, { 'Content-Type': 'application/json; charset=utf-8' });
      res.end(JSON.stringify(body));
    };
    if (url.pathname === '/api/state') {
      send(200, lastReport);
      return;
    }
    if (url.pathname === '/api/tick') {
      tick(context.agent)
        .then((r) => {
          lastReport = r;
          send(200, r);
        })
        .catch((e) => send(500, { error: String(e?.message ?? e) }));
      return;
    }
    if (url.pathname === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(clientHtml);
      return;
    }
    send(404, { error: 'not found' });
  });

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, context.listen.host, resolve);
  });
  context.logger.info('botWatchdog.listening');

  const timer = setInterval(() => {
    tick(context.agent)
      .then((r) => {
        lastReport = r;
        if (r.revived > 0 || r.needsUser > 0) {
          context.logger.warn('botWatchdog.report', {
            revived: r.revived,
            needsUser: r.needsUser,
          });
        }
      })
      .catch((e) => context.logger.warn('botWatchdog.tick failed', { error: String(e?.message ?? e) }));
  }, POLL_MS);
  timer.unref?.();

  const dispose = () => {
    clearInterval(timer);
    server.close();
  };
  context.signal.addEventListener('abort', dispose, { once: true });
  return { dispose };
}
