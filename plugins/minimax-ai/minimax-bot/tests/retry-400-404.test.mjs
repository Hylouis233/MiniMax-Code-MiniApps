// 直接 import 真实的 engine.mjs —— 它 import 了 ./i18n.mjs 等相对路径模块，
// data: URL 方式无法解析相对路径（ERR_UNSUPPORTED_RESOLVE_REQUEST）。
const { isTransientErrorMessage: t } = await import('../miniapp/node/engine.mjs');

const cases = [
  ['HTTP 400 Bad Request', true],
  ['HTTP 404 Not Found', true],
  ['status code: 404', true],
  ['code=422', true],
  ['400 Bad Request', true],
  ['404 Not Found', true],
  ['Internal server error (500)', true],
  ['net::ERR_NETWORK_CHANGED', true],
  ['unknown error, 520 (1000)', true],
  ['502 Bad Gateway', true],
  ['The model is busy right now', true],
  ['用了 400 tokens', false],
  ['第 404 步完成', false],
  ['剩余 422 积分', false],
  ['', false],
  [undefined, false],
  ['当前已达到 Token Plan 用量上限 (2067)', false],
  ['积分已耗尽', false],
];

let bad = 0;
for (const [msg, want] of cases) {
  const got = t(msg);
  if (got !== want) {
    bad += 1;
    console.log('FAIL:', JSON.stringify(msg), 'want', want, 'got', got);
  }
}
console.log(bad === 0 ? `${cases.length}/${cases.length} 全部符合预期` : `${bad} 条不符`);
process.exit(bad === 0 ? 0 : 1);
