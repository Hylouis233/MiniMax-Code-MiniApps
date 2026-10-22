// The automatic retry windows of a failed turn: the real exported defaults, and how the engine
// picks and applies them.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Store } from '../miniapp/node/store.mjs';
import { MemoryStore } from '../miniapp/node/memory.mjs';
import { Engine, LONG_TURN_RETRY_DELAYS_MS, TURN_RETRY_DELAYS_MS } from '../miniapp/node/engine.mjs';
import { setLocale } from '../miniapp/node/i18n.mjs';

async function fixture(t, deps = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'bot-turn-retry-window-'));
  const store = new Store(dir);
  const memory = new MemoryStore(dir);
  await store.init();
  await memory.init();
  setLocale('en');
  const events = [];
  const engine = new Engine({ store, memory, logger: { warn() {} }, broadcast: (event) => events.push(structuredClone(event)), ...deps });
  t.after(async () => {
    engine.dispose();
    await store.writeChain;
    await rm(dir, { recursive: true, force: true });
  });
  return { store, engine, events };
}

/** An Agent whose every turn starts and then fails with no retry verdict. */
function alwaysFailing(calls) {
  return {
    async run(input, options) {
      calls.count += 1;
      options.onEvent({ type: 'turn.started', sessionId: 's1', turnId: `t${calls.count}` });
      return { sessionId: 's1', turnId: `t${calls.count}`, status: 'error', error: { code: 'TURN_FAILED', message: 'The Agent turn failed.' } };
    },
  };
}

test('the default window is two retries after 8 s and 20 s, and a new engine uses it', async (t) => {
  assert.deepEqual(TURN_RETRY_DELAYS_MS, [8000, 20000]);
  assert.ok(Object.isFrozen(TURN_RETRY_DELAYS_MS));
  const { store, engine } = await fixture(t);
  assert.equal(store.state.settings.longRetry, false);
  assert.equal(engine.turnRetryDelays(), TURN_RETRY_DELAYS_MS);
});

test('the long window is ten capped retries, 488 seconds in all, used only when the setting is on', async (t) => {
  assert.deepEqual(LONG_TURN_RETRY_DELAYS_MS, [8000, 20000, 40000, 60000, 60000, 60000, 60000, 60000, 60000, 60000]);
  assert.ok(Object.isFrozen(LONG_TURN_RETRY_DELAYS_MS));
  assert.equal(LONG_TURN_RETRY_DELAYS_MS.reduce((sum, ms) => sum + ms, 0), 488_000);
  const { store, engine } = await fixture(t);
  store.state.settings.longRetry = true;
  assert.equal(engine.turnRetryDelays(), LONG_TURN_RETRY_DELAYS_MS);
  store.state.settings.longRetry = false;
  assert.equal(engine.turnRetryDelays(), TURN_RETRY_DELAYS_MS);
});

for (const longRetry of [false, true]) {
  test(`a failing turn runs once plus one retry per wait of the ${longRetry ? 'long' : 'default'} window, and shows n of max`, async (t) => {
    const { store, engine, events } = await fixture(t, { retryDelaysMs: [1, 1], longRetryDelaysMs: [1, 1, 1, 1, 1] });
    store.state.settings.longRetry = longRetry;
    const retries = longRetry ? 5 : 2;
    const calls = { count: 0 };
    engine.agent = alwaysFailing(calls);
    const bot = store.state.bots[0];
    const chatId = `bot:${bot.id}`;
    const result = await engine.runBotTurn({ bot, chatId, text: 'Hello', kind: 'private' });
    assert.equal(result.status, 'error');
    assert.equal(calls.count, retries + 1);
    assert.equal((await store.getMessages(chatId)).at(-1).retried, retries);
    const shown = events
      .map((event) => event.message?.live)
      .filter((live) => live?.key === 'live.retrying')
      .map((live) => `${live.params.n}/${live.params.max}`);
    assert.deepEqual([...new Set(shown)], Array.from({ length: retries }, (_, i) => `${i + 2}/${retries + 1}`));
  });
}

test('an engine given no retry waits never retries, even with the long window on', async (t) => {
  const { store, engine } = await fixture(t, { retryDelaysMs: [], longRetryDelaysMs: [] });
  store.state.settings.longRetry = true;
  const calls = { count: 0 };
  engine.agent = alwaysFailing(calls);
  const bot = store.state.bots[0];
  await engine.runBotTurn({ bot, chatId: `bot:${bot.id}`, text: 'Hello', kind: 'private' });
  assert.equal(calls.count, 1);
});
