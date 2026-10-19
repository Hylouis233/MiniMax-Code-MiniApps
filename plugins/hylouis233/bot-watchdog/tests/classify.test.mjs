// 看门狗的分类与退避判定测试：纯函数，不触网。
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../miniapp/node/classify.mjs', import.meta.url), 'utf8');
const mod = await import('data:text/javascript;base64,' + Buffer.from(src).toString('base64'));
const { classifyError, backoffMs, isIdle, Verdict } = mod;

let pass = 0;
let total = 0;
const check = (name, ok, extra = '') => {
  total += 1;
  if (ok) pass += 1;
  console.log(`${ok ? '  PASS' : '  FAIL'}  ${name}${extra ? `  ${extra}` : ''}`);
};

console.log('=== 健康：无错误码也无消息 ===');
// 2026-10-19 实测：44 个 bot 会话里 29 个属于这一类 —— 它们只是闲置很久，
// 并不是挂掉。此前一律判成 self-healing ⇒ 会去「重新启用」一批没坏的 bot。
check('(0, 空消息) => healthy', classifyError(0, '').verdict === Verdict.HEALTHY);
check(
  '(0, cron 被删) => needs-user（配置变更，不是崩溃）',
  classifyError(0, 'Cron definition was deleted before execution').verdict === Verdict.NEEDS_USER,
);

console.log('=== 需人工处理：重试无用 ===');
check(
  '42212 ⇒ needs-user',
  classifyError(42212, '已达到 Token Plan 用量上限').verdict === Verdict.NEEDS_USER,
);
check(
  '50110 积分耗尽 ⇒ needs-user',
  classifyError(50110, 'credits exhausted').verdict === Verdict.NEEDS_USER,
);
check(
  '50112 鉴权失效 ⇒ needs-user',
  classifyError(50112, 'auth failed').verdict === Verdict.NEEDS_USER,
);
check(
  '无错误码但文案是额度上限 ⇒ needs-user',
  classifyError(0, '当前已达到 Token Plan 用量上限 (2067)').verdict === Verdict.NEEDS_USER,
);

console.log('\n=== 可自愈：应当自动重新启用 ===');
check(
  '50113 / HTTP 500 ⇒ self-healing',
  classifyError(50113, 'Internal server error. Please retry your request. (500)').verdict ===
    Verdict.SELF_HEALING,
);
check(
  '50111 / 429 ⇒ self-healing',
  classifyError(50111, 'rate limited').verdict === Verdict.SELF_HEALING,
);
check(
  '50151 / 529 ⇒ self-healing',
  classifyError(50151, 'overloaded').verdict === Verdict.SELF_HEALING,
);
check(
  '50150 / TPM 限流 ⇒ self-healing',
  classifyError(50150, 'rate limit exceeded(TPM) (1039)').verdict === Verdict.SELF_HEALING,
);
check(
  '网络切换 ⇒ self-healing',
  classifyError(50113, 'net::ERR_NETWORK_CHANGED').verdict === Verdict.SELF_HEALING,
);
check(
  '无错误码但文案是网络错误 ⇒ self-healing',
  classifyError(0, 'net::ERR_NETWORK_CHANGED').verdict === Verdict.SELF_HEALING,
);
check(
  '未知错误 ⇒ 倾向 self-healing（宁可多试一次）',
  classifyError(0, '某未知错误').verdict === Verdict.SELF_HEALING,
);

console.log('\n=== 退避必须封顶 ===');
const d1 = backoffMs(1, 5000, 60000);
const d10 = backoffMs(10, 5000, 60000);
const d100 = backoffMs(100, 5000, 60000);
check('退避随次数增长', d10 > d1, `d1=${d1} d10=${d10}`);
check('退避封顶在 cap+抖动内', d100 <= 61000, `d100=${d100}`);
check('无限次也不会超过 cap', d100 < 120000);

console.log('\n=== 闲置判定：error_code 可能是陈旧值 ===');
const now = 1_000_000_000;
check('刚有活动 ⇒ 不算闲置', isIdle(now - 10_000, now, 90_000) === false);
check('90 秒没活动 ⇒ 算闲置', isIdle(now - 90_000, now, 90_000) === true);
check('从未活动过 ⇒ 算闲置', isIdle(0, now, 90_000) === true);

console.log(`\n${pass}/${total} passed`);
process.exit(pass === total ? 0 : 1);
