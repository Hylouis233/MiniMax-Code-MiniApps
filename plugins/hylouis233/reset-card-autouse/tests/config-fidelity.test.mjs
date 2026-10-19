// 验证 MiniApp 的配置归一化**不会丢字段**。
//
// 2026-10-19 两次真实事故：
//   1. `normalizeConfig()` 漏了 `resumeCronId` ⇒ 用户改任何设置都会把 cron id
//      抹掉 ⇒ 恢复机制静默失效。
//   2. 同类问题差点发生在 `botGuard` 上。
//
// ⇒ 这条测试把**每个**配置字段都过一遍：读进去、再存出来，必须一字不差。
// 直接 import 真实模块 —— 它 import 了 ./i18n.mjs 等相对路径模块，
// data: URL 方式无法解析相对路径。
const mod = await import('../miniapp/node/server.mjs');

let pass = 0;
let total = 0;
const check = (name, ok, extra = '') => {
  total += 1;
  if (ok) pass += 1;
  console.log(`${ok ? '  PASS' : '  FAIL'}  ${name}${extra ? `  ${extra}` : ''}`);
};

const full = {
  schemaVersion: 1,
  cardTriggerPercent: 97,
  cardSkipBeforeResetMinutes: 45,
  quotaLevels: { tight: 82, low: 96 },
  goalPause: { enabled: true, triggerPercent: 93, resumeAtPercent: 3 },
  resumeCronId: 'cf98b82e-306b-490c-9e27-141ee44fab40',
  botGuard: { enabled: true, idleMs: 120000, backoffBaseMs: 7000, backoffCapMs: 45000 },
};

const norm = mod.normalizeConfig(full);
const c = norm.config;

console.log('=== 顶层字段必须一字不差地保留 ===');
for (const key of [
  'schemaVersion',
  'cardTriggerPercent',
  'cardSkipBeforeResetMinutes',
  'quotaLevels',
  'goalPause',
  'resumeCronId',
  'botGuard',
]) {
  const got = JSON.stringify(c[key]);
  const want = JSON.stringify(full[key]);
  check(`${key} 保留`, got === want, got === want ? '' : `got=${got} want=${want}`);
}

console.log('\n=== botGuard 的子字段也要保留 ===');
for (const key of ['enabled', 'idleMs', 'backoffBaseMs', 'backoffCapMs']) {
  const got = c.botGuard?.[key];
  const want = full.botGuard[key];
  check(`botGuard.${key} 保留`, got === want, `got=${got} want=${want}`);
}

console.log('\n=== 缺字段时用默认值，而不是丢字段 ===');
const c2 = mod.normalizeConfig({ cardTriggerPercent: 90 }).config;
check('缺失时 resumeCronId 存在', typeof c2.resumeCronId === 'string');
check('缺失时 botGuard 存在且默认关闭', c2.botGuard && c2.botGuard.enabled === false);

console.log('\n=== 非法值被夹到合法区间（不静默通过）===');
const c3 = mod.normalizeConfig({ botGuard: { enabled: true, idleMs: 1, backoffCapMs: 99999999 } }).config;
check('idleMs 夹到下限 10000', c3.botGuard.idleMs === 10000, `got=${c3.botGuard.idleMs}`);
check('backoffCapMs 夹到上限 3600000', c3.botGuard.backoffCapMs === 3600000, `got=${c3.botGuard.backoffCapMs}`);

console.log(`\n${pass}/${total} passed`);
process.exit(pass === total ? 0 : 1);
