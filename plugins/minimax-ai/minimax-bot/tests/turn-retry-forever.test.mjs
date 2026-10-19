// 暂时性错误的无限重试（2026-10-19）。
//
// 起因：Host 的 `toLLMRetryDecision()` 把通用 5xx 归进 50113 这个「兜底桶」
// 并 fail-closed —— 但本插件看得到错误消息文本，能把「服务端抖了一下」
// 与「服务端明确拒绝」分开。实测那批 50113 全是暂时性故障
// （网络切换 / 500 / 超时 / 520 / 502 / 服务不可用）。
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';

import { Store } from '../miniapp/node/store.mjs';
import { MemoryStore } from '../miniapp/node/memory.mjs';
import { Team } from '../miniapp/node/team.mjs';

/** 等待引擎里的回合全部结束。 */
async function until(predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error('The expected engine state did not arrive');
    await new Promise((r) => setTimeout(r, 5));
  }
}

function idle(engine) {
  return (
    engine.running.size === 0 &&
    engine.tickets.size === 0 &&
    engine.groupLoops.size === 0 &&
    engine.inbox.batches.size === 0 &&
    engine.tasks.runs.size === 0 &&
    engine.tasks.deliveries.size === 0
  );
}
import { Engine, isTransientErrorMessage, turnRetryDelayMs } from '../miniapp/node/engine.mjs';

async function fixture(t, opts = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'bot-retry-forever-'));
  const store = new Store(dir);
  const memory = new MemoryStore(dir);
  await store.init();
  await memory.init();
  const events = [];
  const engine = new Engine({
    store,
    memory,
    logger: { warn() {} },
    broadcast: (event) => events.push(event),
    ...opts,
  });
  const team = new Team({ store, memory, engine, broadcast() {} });
  t.after(async () => {
    await rm(dir, { recursive: true, force: true });
  });
  return { engine, store, memory, events, team };
}

test('暂时性错误消息被识别为可重试', () => {
  for (const m of [
    'net::ERR_NETWORK_CHANGED',
    'Internal server error. Please retry your request. (500)',
    '请求处理超时，请稍后重试 (2066)',
    'unknown error, 520 (1000)',
    'open platform service unavailable, please retry',
    '502 Bad Gateway',
    'The model is busy right now',
  ]) {
    assert.equal(isTransientErrorMessage(m), true, `应当可重试：${m}`);
  }
});

test('确定性失败不被当成暂时性错误', () => {
  for (const m of [
    '',
    undefined,
    '当前已达到 Token Plan 用量上限 (2067)',
    'HTTP 400 Bad Request',
    'HTTP 404 Not Found',
    'invalid request',
  ]) {
    assert.equal(isTransientErrorMessage(m), false, `不该重试：${m}`);
  }
});

test('无限重试的退避封顶在 60 秒', () => {
  const d1 = turnRetryDelayMs(1);
  const d5 = turnRetryDelayMs(5);
  const d100 = turnRetryDelayMs(100);
  assert.ok(d5 > d1, '退避应随次数增长');
  assert.ok(d100 <= 60_000 + 1000, `必须封顶：${d100}`);
});

test('暂时性错误会一直重试到成功（不受 2 次窗口限制）', async (t) => {
  const { engine, store } = await fixture(t, {
    // 有限重试窗口设为空：这样如果代码走的是「有限重试」分支就一次都不重试，
    // 只有走「无限重试」分支才会继续。
    retryDelaysMs: [],
    // 退避调到毫秒级：测试不该真的等 8/16/32 秒
    retryBaseMs: 5,
    retryCapMs: 50,
  });
  const bot = { id: 'bot_retry', name: 'RetryBot', role: 'test', isMain: true };
  store.state.bots = [bot];
  const chatId = `bot:${bot.id}`;

  let attempts = 0;
  engine.agent = {
    async run(input, options) {
      attempts += 1;
      if (attempts < 4) {
        // 前 3 次：暂时性错误（Host 判 retryable=false，但消息是 500）
        throw Object.assign(new Error('Internal server error. Please retry your request. (500)'), {
          code: 'AGENT_UNAVAILABLE',
          retryable: false,
        });
      }
      options?.onEvent?.({ type: 'turn.started', sessionId: 'mvs_x', turnId: 'turn-x' });
      return { sessionId: 'mvs_x', status: 'finished', text: 'Recovered' };
    },
  };

  await engine.sendUserMessage(chatId, 'hello');
  try {
    await until(() => idle(engine), 3000);
  } catch (e) {
    console.log('  诊断 attempts =', attempts, '| running =', engine.running.size,
      '| tickets =', engine.tickets.size, '| groupLoops =', engine.groupLoops.size,
      '| inbox =', engine.inbox.batches.size, '| tasks.runs =', engine.tasks.runs.size,
      '| tasks.deliveries =', engine.tasks.deliveries.size);
    throw e;
  }
  assert.equal(attempts, 4, '应当重试到第 4 次才成功');
});

test('确定性失败不会重试（额度到顶）', async (t) => {
  const { engine, store } = await fixture(t, { retryDelaysMs: [10, 20] });
  const bot = { id: 'bot_quota', name: 'QuotaBot', role: 'test', isMain: true };
  store.state.bots = [bot];
  const chatId = `bot:${bot.id}`;

  let attempts = 0;
  engine.agent = {
    async run() {
      attempts += 1;
      throw Object.assign(new Error('当前已达到 Token Plan 用量上限 (2067)'), {
        code: 'AGENT_UNAVAILABLE',
        retryable: false,
      });
    },
  };

  await engine.sendUserMessage(chatId, 'hello');
  await until(() => idle(engine));
  assert.equal(attempts, 1, '额度类失败必须一次就停，不能重试');
});
