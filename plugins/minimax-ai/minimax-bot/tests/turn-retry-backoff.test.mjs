// 回合级重试：重试窗口 + 暂时性错误判定。
//
// 2026-10-20 收敛（依据上游 review 意见）：
//   - 上游 0.7.4 的窗口是 `[8000, 20000]` ⇒ **只重试 2 次**
//     ⇒ 一次持续 30 秒的服务端抖动就让 bot **直接死掉**，只能靠人手点一下。
//   - 但「无限重试」又与维护者「给连续重试设个上限」的要求相反。
//   ⇒ 收敛成：**默认窗口延长到 10 档（8s→20s→40s→60s×6，约 7.5 分钟）**，仍然有上限。
//
// ⚠️ 同时守住两条边界：
//   1. 400 / 404 / 422 **不重试**（重试它们只是把同一次拒绝重复十遍）
//   2. 正常消息（「用了 400 tokens」「第 404 步」）不会被误判成错误
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isTransientErrorMessage } from '../miniapp/node/engine.mjs';

/** 与 engine.mjs 里的默认窗口保持一致（改一处就要同步这里）。 */
const DEFAULT_WINDOW_MS = [8000, 20000, 40000, 60000, 60000, 60000, 60000, 60000, 60000, 60000];

test('默认重试窗口：10 档、指数退避、封顶 60 秒，总窗口约 8 分钟', () => {
  assert.equal(DEFAULT_WINDOW_MS.length, 10, '必须有明确的上限（不是无限重试）');
  assert.equal(DEFAULT_WINDOW_MS[0], 8000);
  assert.equal(DEFAULT_WINDOW_MS[1], 20000);
  assert.equal(DEFAULT_WINDOW_MS[2], 40000);
  for (const d of DEFAULT_WINDOW_MS.slice(3)) assert.equal(d, 60000, '之后一律封顶 60s');
  const total = DEFAULT_WINDOW_MS.reduce((a, b) => a + b, 0);
  assert.ok(total >= 7 * 60_000 && total <= 9 * 60_000, `总窗口应约 8 分钟，实际 ${total}ms`);
});

test('暂时性错误会被认出来（服务端 / 网络故障）', () => {
  for (const m of [
    'net::ERR_NETWORK_CHANGED',
    'Internal server error (500)',
    '502 Bad Gateway',
    'service unavailable',
    '请求处理超时 (2066)',
    'unknown error, 520 (1000)',
    'rate limit exceeded',
    'The model is busy right now',
    'HTTP 500',
    'status 502',
  ]) {
    assert.equal(isTransientErrorMessage(m), true, `应判为暂时性：${m}`);
  }
});

test('400 / 404 / 422 不重试（重试只是把同一次拒绝重复十遍）', () => {
  for (const m of [
    'HTTP 400 Bad Request',
    'HTTP 404 Not Found',
    'code=422',
    '400 Bad Request',
    '404 Not Found',
  ]) {
    assert.equal(isTransientErrorMessage(m), false, `不该重试：${m}`);
  }
});

test('正常消息不会被误判成错误', () => {
  for (const m of [
    '用了 400 tokens',
    '第 404 步完成',
    '剩余 422 积分',
    '当前已达到 Token Plan 用量上限 (2067)',
    '积分已耗尽',
    '',
    undefined,
  ]) {
    assert.equal(isTransientErrorMessage(m), false, `不该判为暂时性：${m}`);
  }
});
