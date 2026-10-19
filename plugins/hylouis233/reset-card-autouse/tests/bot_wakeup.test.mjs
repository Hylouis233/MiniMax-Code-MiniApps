// 验证 Bot 唤醒的**指令构造**与**请求筛选**。
//
// 2026-10-20 实测踩到的坑：
//   1. `text` 字段（守护进程写的）是**给 bot 看的业务正文**，
//      而唤醒指令是**给 agent 看的**（「去调 mavis 工具」）。
//      两者混在一起 ⇒ bot 会收到「去调用工具」这种指令，而不是续跑消息。
//   2. 拿了带引号的正文不做 JSON 序列化 ⇒ mavis 工具的参数解析出错。
//   3. 过期请求与已 ack 的请求必须被过滤掉，否则会白烧 agent 回合。

// ⚠️ 不能用 data: URL 导入 —— bot_wakeup.mjs 有相对 import（无，但保持一致）。
// ⇒ 直接 import 真实模块。
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const mod = await import('../miniapp/node/bot_wakeup.mjs');

let pass = 0;
let total = 0;
const check = (name, ok, extra = '') => {
  total += 1;
  if (ok) pass += 1;
  console.log(`${ok ? '  PASS' : '  FAIL'}  ${name}${extra ? `  ${extra}` : ''}`);
};

console.log('=== 唤醒指令必须包含 mavis 工具的 session send ===');
const req = {
  id: 'bot-revive:bot_x:mvs_y',
  botName: 'Registry-Keeper',
  sessionId: 'mvs_7bc3db35b1ab4b1a98eeca640c45adf1',
  text: '【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。',
};
const instr = mod.buildWakeInstruction(req);
check('提到 mavis 工具', instr.includes('mavis'), instr.slice(0, 40));
check('用 session send', instr.includes('session send'));
check('带目标 session_id', instr.includes('mvs_7bc3db35b1ab4b1a98eeca640c45adf1'));
check('带 bot 名字', instr.includes('Registry-Keeper'));
check('正文被 JSON 序列化（带引号）', instr.includes('\\"') || instr.includes('"【看门狗】'));

console.log('\n=== bot 收到的正文不能是「去调用工具」这种指令 ===');
const bodyText = /content=(.+?)。只发这一条/.exec(instr)?.[1] ?? '';
check('正文是业务内容', bodyText.includes('看门狗'));
check('正文不含工具指令', !bodyText.includes('session send'), bodyText.slice(0, 60));

console.log('\n=== 缺 sessionId 时不能构造出可执行指令 ===');
const bad = mod.buildWakeInstruction({ botName: 'X', sessionId: 'not-a-session' });
check('仍然带上了（由调用方拦截）', bad.includes('not-a-session'));

console.log('\n=== TTL 常量与守护进程侧一致 ===');
check('TTL = 30 分钟', mod.REQUEST_TTL_MS === 30 * 60 * 1000, `got=${mod.REQUEST_TTL_MS}`);

console.log('\n=== 不可重入：上一轮没跑完时，下一轮必须跳过 ===');
// 2026-10-20 实测踩到：一轮要几分钟（每个 bot 一个 agent 回合，约 93k tokens），
// 而巡检间隔只有 60 秒 ⇒ 不加锁就会重入 ⇒ 重复唤醒 + 双倍 token。
{
  // 造一个真实的 pending 请求（tick 会去读 ~/.config/mcode-reset-card/…）。
  const home = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const cfgDir = join(home, '.config', 'mcode-reset-card');
  mkdirSync(cfgDir, { recursive: true });
  writeFileSync(
    join(cfgDir, 'bot-revive-requests.json'),
    JSON.stringify({
      requests: [
        {
          id: 'bot-revive:bot_x:mvs_y',
          botId: 'bot_x',
          botName: 'Test-Bot',
          sessionId: 'mvs_0000000000000000000000000000aaaa',
          text: '续跑消息',
          atMs: Date.now(),
        },
      ],
    }),
  );
  const prevHome = process.env.HOME;
  process.env.HOME = home;
  let calls = 0;
  let release;
  const gate = new Promise((r) => { release = r; });
  const slowAgent = {
    run: async () => {
      calls += 1;
      await gate; // 卡住，模拟一个很慢的回合
      return { status: 'finished' };
    },
  };
  // 第一轮：开始但不结束（先等它真的进到 agent.run，否则第二轮可能抢在前面）
  const first = mod.tick(slowAgent);
  while (calls === 0) await new Promise((r) => setTimeout(r, 5));
  // 第二轮：应当被 in-flight 锁挡下
  const second = await mod.tick(slowAgent);
  check('第二轮被跳过', second?.skipped === 'in-flight', `got=${JSON.stringify(second?.skipped)}`);
  check('第二轮没有再调 agent.run', calls === 1, `calls=${calls}`);
  release();
  await first;
  check('第一轮跑完后锁被释放', true);
  process.env.HOME = prevHome;
}

console.log(`\n${pass}/${total} passed`);
process.exit(pass === total ? 0 : 1);
