// 真实行为验证：用本地 mock server 实测重试行为。
// 不用真账号、不触网外部接口 —— 只验证「哪些错误会重试、退避是否生效」。
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../miniapp/node/server.mjs', import.meta.url), 'utf8');
const body = src.replace(/export async function start[\s\S]*$/, '');
const mod = await import(
  'data:text/javascript;base64,' +
    Buffer.from(
      body +
        '\nexport { isRetryableStatus, retryDelayMs, RETRY_CAP_MS, fetchWithRetry };',
    ).toString('base64')
);
const { fetchWithRetry, RETRY_CAP_MS } = mod;

let hits = 0;
let mode = 'ok';
const server = createServer((req, res) => {
  hits += 1;
  if (mode === '500') {
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end('{"base_resp":{"status_code":-1,"status_msg":"internal"}}');
  } else if (mode === '429') {
    res.writeHead(429, { 'Content-Type': 'application/json' });
    res.end('{"base_resp":{"status_code":-1,"status_msg":"overloaded"}}');
  } else if (mode === '404') {
    res.writeHead(404);
    res.end('nope');
  } else {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end('{"ok":true}');
  }
});

await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;
const url = `http://127.0.0.1:${port}/probe`;

let pass = 0;
let total = 0;
const check = (name, ok, extra = '') => {
  total += 1;
  if (ok) pass += 1;
  console.log(`${ok ? '  PASS' : '  FAIL'}  ${name}${extra ? `  ${extra}` : ''}`);
};

// ── 1) 500 ⇒ 会重试，并在第 3 次成功时返回 ──
hits = 0;
mode = '500';
let calls = 0;
const origFetch = globalThis.fetch;
globalThis.fetch = async (u, init) => {
  calls += 1;
  if (calls >= 3) mode = 'ok'; // 前两次 500，第三次成功
  return origFetch(u, init);
};
const r500 = await fetchWithRetry(url, {}, 'GET probe');
check(
  '500 会重试直到成功',
  r500.status === 200 && calls === 3,
  `调用 ${calls} 次，最终 status=${r500.status}`,
);

// ── 2) 持续 500 ⇒ 确实在不停重试（不提前放弃）──
// 前 3 次都返回 500，第 4 次才成功 —— 如果实现里有「重试次数上限」，
// 这里会在第 3 次之前就返回 500，测试会失败。
calls = 0;
mode = '500';
globalThis.fetch = async (u, init) => {
  calls += 1;
  if (calls >= 4) mode = 'ok';
  return origFetch(u, init);
};
const rLoop = await fetchWithRetry(url, {}, 'GET probe');
check(
  '持续 500 会一直重试（不提前放弃）',
  rLoop.status === 200 && calls >= 4,
  `调用 ${calls} 次，最终 status=${rLoop.status}`,
);

// ── 3) 429 ⇒ 也会重试 ──
calls = 0;
mode = '429';
globalThis.fetch = async (u, init) => {
  calls += 1;
  if (calls >= 2) mode = 'ok';
  return origFetch(u, init);
};
const r429 = await fetchWithRetry(url, {}, 'GET probe');
check('429 会重试', r429.status === 200 && calls === 2, `调用 ${calls} 次`);

// ── 4) 404 ⇒ 立即返回，不重试 ──
calls = 0;
mode = '404';
globalThis.fetch = async (u, init) => {
  calls += 1;
  return origFetch(u, init);
};
const r404 = await fetchWithRetry(url, {}, 'GET probe');
check('404 不重试（快速失败）', r404.status === 404 && calls === 1, `调用 ${calls} 次`);

globalThis.fetch = origFetch;
server.close();

console.log(`\n${pass}/${total} passed`);
process.exit(pass === total ? 0 : 1);
