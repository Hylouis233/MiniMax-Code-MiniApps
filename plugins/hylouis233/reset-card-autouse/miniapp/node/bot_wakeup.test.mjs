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
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const mod = await import('./bot_wakeup.mjs');

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

console.log('\n=== 判据常量与守护进程侧一致 ===');
// ⚠️ 判据改成「按尝试次数」后，30 分钟 TTL 常量已被删除（时间只留 24 小时兜底）。
// 两侧必须一致，否则会「一边有效一边无效」⇒ 请求被反复重写或永不投递。
check('最多尝试 3 次', mod.MAX_ATTEMPTS === 3, `got=${mod.MAX_ATTEMPTS}`);
check('时间兜底 24 小时', mod.MAX_REQUEST_AGE_MS === 24 * 60 * 60 * 1000, `got=${mod.MAX_REQUEST_AGE_MS}`);

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
  // 真正验证「锁已释放」：造一条**新**请求（第一轮那条已被 ack，
  // 第二轮读不到是设计意图，不能拿来验证锁）。
  const raw2 = JSON.parse(readFileSync(join(home, '.config', 'mcode-reset-card', 'bot-revive-requests.json'), 'utf8'));
  raw2.requests.push({
    id: 'bot-revive:bot_x:mvs_second',
    botId: 'bot_x',
    botName: 'Test-Bot',
    sessionId: 'mvs_0000000000000000000000000000dddd',
    text: '续跑消息',
    atMs: Date.now(),
  });
  writeFileSync(join(home, '.config', 'mcode-reset-card', 'bot-revive-requests.json'), JSON.stringify(raw2, null, 2));
  const before3 = calls;
  const third = await mod.tick(slowAgent);
  check('第一轮跑完后锁已释放（第三轮不再被跳过）', third?.skipped !== 'in-flight', `skipped=${third?.skipped}`);
  check('第三轮确实调了 agent.run', calls > before3, `calls=${calls} before=${before3}`);
  process.env.HOME = prevHome;
}

console.log('\n=== 面板关久了，请求不能因超时被丢掉（按尝试次数封顶）===');
// 2026-10-20 实测的坑：纯时间 TTL ⇒ 面板关掉 → 没人消费 → 30 分钟后请求全过期
// ⇒ 面板再打开时已经没有请求可发，bot 一直挂着没人叫醒。
{
  const home2 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d2 = join(home2, '.config', 'mcode-reset-card');
  mkdirSync(d2, { recursive: true });
  const OLD = Date.now() - 3 * 60 * 60 * 1000; // 3 小时前
  writeFileSync(
    join(d2, 'bot-revive-requests.json'),
    JSON.stringify({
      requests: [
        { id: 'old-but-fresh', botName: 'A', sessionId: 'mvs_0000000000000000000000000000aaaa', text: 'x', atMs: OLD },
        { id: 'tried-3-times', botName: 'B', sessionId: 'mvs_0000000000000000000000000000bbbb', text: 'x', atMs: Date.now() },
        { id: 'tried-1-time', botName: 'C', sessionId: 'mvs_0000000000000000000000000000cccc', text: 'x', atMs: Date.now() },
      ],
    }),
  );
  // 尝试次数单独存一个文件（避免与守护进程并发写同一个文件互相覆盖）。
  writeFileSync(
    join(d2, 'bot-revive-attempts.json'),
    JSON.stringify({ attempts: { 'tried-3-times': 3, 'tried-1-time': 1 } }),
  );
  const prevHome2 = process.env.HOME;
  process.env.HOME = home2;
  const reqs = await mod.readWakeRequests();
  const ids = reqs.map((r) => r.id).sort();
  check('3 小时前的请求仍然有效（面板关了很久也能补上）', ids.includes('old-but-fresh'), ids.join(','));
  check('试满 3 次的请求被作废', !ids.includes('tried-3-times'));
  check('只试过 1 次的仍有效', ids.includes('tried-1-time'));
  process.env.HOME = prevHome2;
}

console.log('\n=== v1.6：已投递的请求永远不再投（acked 永久化）===');
// 2026-10-20 独立审计 + 实测发现：acked 原来带 30 分钟窗口，
// 窗口一过，**已经投递过的请求又变回「未处理」**
// ⇒ 投递端每 30 分钟把同一条请求重投一次（每回合 ≈47k tokens），
// 而且会打断已经恢复正常的 bot。
{
  const home3 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d3 = join(home3, '.config', 'mcode-reset-card');
  mkdirSync(d3, { recursive: true });
  const LONG_AGO = Date.now() - 60 * 60 * 1000; // 1 小时前（远超 30 分钟窗口）
  writeFileSync(
    join(d3, 'bot-revive-requests.json'),
    JSON.stringify({
      requests: [
        { id: 'bot-revive:b1:mvs_1:111', botName: 'A', sessionId: 'mvs_0000000000000000000000000000aaaa', text: 'x', atMs: LONG_AGO },
        { id: 'bot-revive:b2:mvs_2:222', botName: 'B', sessionId: 'mvs_0000000000000000000000000000bbbb', text: 'x', atMs: Date.now() },
      ],
    }),
  );
  // 1 小时前投递过第一条（带时间戳）。
  writeFileSync(
    join(d3, 'bot-revive-acked.json'),
    JSON.stringify({ acked: [{ id: 'bot-revive:b1:mvs_1:111', atMs: LONG_AGO }] }),
  );
  const prevHome3 = process.env.HOME;
  process.env.HOME = home3;
  const reqs3 = await mod.readWakeRequests();
  const ids3 = reqs3.map((r) => r.id);
  check('超过 30 分钟窗口，已投递过的请求**仍然不投**', !ids3.includes('bot-revive:b1:mvs_1:111'), ids3.join(','));
  check('没投递过的照常投', ids3.includes('bot-revive:b2:mvs_2:222'), ids3.join(','));
  process.env.HOME = prevHome3;
}

console.log('\n=== v1.6：尝试次数按 bot 键记（换实例不重置计数）===');
// 守护进程每写一次新实例就是一个新 id ⇒ 若按 id 计次数，
// 每次换实例都重置 ⇒ 3 次封顶被架空 ⇒ 无限重试。
{
  const home4 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d4 = join(home4, '.config', 'mcode-reset-card');
  mkdirSync(d4, { recursive: true });
  writeFileSync(
    join(d4, 'bot-revive-requests.json'),
    JSON.stringify({
      requests: [
        { id: 'bot-revive:b1:mvs_1:999', botName: 'A', sessionId: 'mvs_0000000000000000000000000000aaaa', text: 'x', atMs: Date.now() },
      ],
    }),
  );
  // 3 次尝试记在 **bot 键**上（不是实例 id）。
  writeFileSync(
    join(d4, 'bot-revive-attempts.json'),
    JSON.stringify({ attempts: { 'bot-revive:b1:mvs_1': { n: 3, at: Date.now() } } }),
  );
  const prevHome4 = process.env.HOME;
  process.env.HOME = home4;
  const reqs4 = await mod.readWakeRequests();
  check('同一 bot 的新实例同样被 3 次封顶挡住', reqs4.length === 0, `剩 ${reqs4.length} 条`);
  process.env.HOME = prevHome4;
}

console.log('\n=== v1.6：bot 键的推导规则必须与 Rust 侧逐字一致 ===');
{
  check('带实例后缀 ⇒ 取前三段', mod.botKeyFromRequestId('bot-revive:b1:mvs_1:111') === 'bot-revive:b1:mvs_1');
  check('不带实例后缀 ⇒ 原样', mod.botKeyFromRequestId('bot-revive:b1:mvs_1') === 'bot-revive:b1:mvs_1');
  check('形状不对 ⇒ 原样返回（不瞎折）', mod.botKeyFromRequestId('weird') === 'weird');
}

console.log('\n=== v1.6 二轮审计 F1：失败计数不能被「曾被救过」清掉 ===');
// 回归：acked 永久化之后，「按 acked 清理 attempts」会把**曾经被救过**的 bot
// 的失败计数一起清掉 ⇒ 3 次封顶失效 ⇒ 该请求永远「活」⇒ 每 60 秒重投一次。
{
  const home5 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d5 = join(home5, '.config', 'mcode-reset-card');
  mkdirSync(d5, { recursive: true });
  // 这个 bot 曾经被救过（acked 里有它）。
  writeFileSync(
    join(d5, 'bot-revive-acked.json'),
    JSON.stringify({ acked: [{ id: 'bot-revive:b1:mvs_1:111', atMs: Date.now() }] }),
  );
  // 已经有 2 次失败记录（bot 键）。
  writeFileSync(
    join(d5, 'bot-revive-attempts.json'),
    JSON.stringify({ attempts: { 'bot-revive:b1:mvs_1': { n: 2, at: Date.now() } } }),
  );
  const prevHome5 = process.env.HOME;
  process.env.HOME = home5;
  // 再失败一次 ⇒ 计数必须是 3，而不是被清成 0/1。
  await mod.bumpAttempts('bot-revive:b1:mvs_1');
  const map = JSON.parse(readFileSync(join(d5, 'bot-revive-attempts.json'), 'utf8')).attempts ?? {};
  check(
    '失败计数累加到 3（没被 acked 清掉）',
    map['bot-revive:b1:mvs_1']?.n === 3,
    JSON.stringify(map['bot-revive:b1:mvs_1']),
  );
  check('计数表没被清空', Object.keys(map).length >= 1, `剩 ${Object.keys(map).length} 条`);
  process.env.HOME = prevHome5;
}

console.log('\n=== v1.6：投递成功时失败计数归零 ===');
{
  const home6 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d6 = join(home6, '.config', 'mcode-reset-card');
  mkdirSync(d6, { recursive: true });
  writeFileSync(
    join(d6, 'bot-revive-attempts.json'),
    JSON.stringify({ attempts: { 'bot-revive:b1:mvs_1': { n: 2, at: Date.now() } } }),
  );
  const prevHome6 = process.env.HOME;
  process.env.HOME = home6;
  await mod.clearAttemptsOnSuccess('bot-revive:b1:mvs_1');
  const map = JSON.parse(readFileSync(join(d6, 'bot-revive-attempts.json'), 'utf8')).attempts ?? {};
  check('成功后失败计数归零', !('bot-revive:b1:mvs_1' in map), JSON.stringify(map));
  process.env.HOME = prevHome6;
}

console.log('\n=== 尝试次数单独存一个文件（不与守护进程抢同一个文件）===');
// 2026-10-20 审计发现：投递端与守护进程会并发写 requests 文件。
// 若投递端做「读全量 → 改字段 → 写回全量」，会把守护进程刚写进去的条目抹掉。
{
  const home3 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d3 = join(home3, '.config', 'mcode-reset-card');
  mkdirSync(d3, { recursive: true });
  writeFileSync(
    join(d3, 'bot-revive-requests.json'),
    JSON.stringify({ requests: [{ id: 'r1', botName: 'A', sessionId: 'mvs_0000000000000000000000000000aaaa', text: 'x', atMs: Date.now() }] }),
  );
  const prevHome3 = process.env.HOME;
  process.env.HOME = home3;
  await mod.bumpAttempts('r1');
  // 模拟守护进程此刻写入了新条目（追加到同一个文件）。
  const raw = JSON.parse(readFileSync(join(d3, 'bot-revive-requests.json'), 'utf8'));
  raw.requests.push({ id: 'r2', botName: 'B', sessionId: 'mvs_0000000000000000000000000000bbbb', text: 'x', atMs: Date.now() });
  writeFileSync(join(d3, 'bot-revive-requests.json'), JSON.stringify(raw, null, 2));
  // 再 bump 一次：绝不能把 r2 抹掉。
  await mod.bumpAttempts('r1');
  const after = JSON.parse(readFileSync(join(d3, 'bot-revive-requests.json'), 'utf8'));
  check('requests 文件里 r2 还在（没有 lost update）', after.requests.some((r) => r.id === 'r2'), JSON.stringify(after.requests.map((r) => r.id)));
  check('requests 文件里 r1 还在', after.requests.some((r) => r.id === 'r1'));
  const att = JSON.parse(readFileSync(join(d3, 'bot-revive-attempts.json'), 'utf8'));
  check('尝试次数记在独立文件里', att.attempts?.r1?.n === 2, JSON.stringify(att.attempts));
  process.env.HOME = prevHome3;
}

console.log('\n=== 会话复用：第二轮必须复用同一个会话（省掉冷启动的 ~93k tokens）===');
{
  const home4 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d4 = join(home4, '.config', 'mcode-reset-card');
  mkdirSync(d4, { recursive: true });
  const reqFile4 = join(d4, 'bot-revive-requests.json');
  const mkReq = (id) => ({
    id, botId: 'bot_x', botName: 'Test-Bot',
    sessionId: 'mvs_0000000000000000000000000000' + id.slice(-4),
    text: '续跑消息', atMs: Date.now(),
  });
  writeFileSync(reqFile4, JSON.stringify({ requests: [mkReq('req-aaaa')] }));
  const prevHome4 = process.env.HOME;
  process.env.HOME = home4;

  const used = [];
  const fakeAgent = {
    run: async ({ text, sessionId }) => {
      used.push({ text, sessionId });
      return { status: 'finished', sessionId: sessionId ?? 'mvs_hostnew0000000000000000000000000' };
    },
  };
  // 第一轮：没有宿主会话 ⇒ 新建
  await mod.tick(fakeAgent);
  check('第一轮是新建会话（没传 sessionId）', used[0]?.sessionId === undefined, `sessionId=${used[0]?.sessionId}`);
  check('宿主会话 id 已落盘', (await mod.readHostSessionId()) === 'mvs_hostnew0000000000000000000000000');
  // 第二轮：复用
  writeFileSync(reqFile4, JSON.stringify({ requests: [mkReq('req-bbbb')] }));
  await mod.tick(fakeAgent);
  check('第二轮复用了宿主会话', used[1]?.sessionId === 'mvs_hostnew0000000000000000000000000', `sessionId=${used[1]?.sessionId}`);
  process.env.HOME = prevHome4;
}

console.log('\n=== 复用失败要退回新建，且新会话 id 必须落盘 ===');
{
  const home5 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d5 = join(home5, '.config', 'mcode-reset-card');
  mkdirSync(d5, { recursive: true });
  writeFileSync(join(d5, 'bot-revive-requests.json'), JSON.stringify({
    requests: [{ id: 'req-cccc', botId: 'bot_x', botName: 'T', sessionId: 'mvs_0000000000000000000000000000cccc', text: 'x', atMs: Date.now() }],
  }));
  // 预置一个「已失效」的宿主会话
  writeFileSync(join(d5, 'bot-wake-host-session.json'), JSON.stringify({ sessionId: 'mvs_stale000000000000000000000000000' }));
  const prevHome5 = process.env.HOME;
  process.env.HOME = home5;

  const seen = [];
  const flakyAgent = {
    run: async ({ sessionId }) => {
      seen.push(sessionId);
      if (sessionId === 'mvs_stale000000000000000000000000000') throw new Error('Session not found');
      return { status: 'finished', sessionId: 'mvs_fresh000000000000000000000000000' };
    },
  };
  await mod.tick(flakyAgent);
  check('先试了失效的宿主会话', seen[0] === 'mvs_stale000000000000000000000000000', `seen=${JSON.stringify(seen)}`);
  check('失败后退回新建会话', seen[1] === undefined, `seen=${JSON.stringify(seen)}`);
  check('新会话 id 已落盘（否则复用收益归零）', (await mod.readHostSessionId()) === 'mvs_fresh000000000000000000000000000', `got=${await mod.readHostSessionId()}`);
  process.env.HOME = prevHome5;
}

console.log('\n=== BUSY / OUTCOME_UNKNOWN 不能重发（否则 bot 会收到两条指令）===');
{
  const home6 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d6 = join(home6, '.config', 'mcode-reset-card');
  mkdirSync(d6, { recursive: true });
  writeFileSync(join(d6, 'bot-revive-requests.json'), JSON.stringify({
    requests: [{ id: 'req-dddd', botId: 'bot_x', botName: 'T', sessionId: 'mvs_0000000000000000000000000000dddd', text: 'x', atMs: Date.now() }],
  }));
  writeFileSync(join(d6, 'bot-wake-host-session.json'), JSON.stringify({ sessionId: 'mvs_host0000000000000000000000000000' }));
  const prevHome6 = process.env.HOME;
  process.env.HOME = home6;
  const calls6 = [];
  const busyAgent = {
    run: async ({ sessionId }) => {
      calls6.push(sessionId);
      const e = new Error('session busy');
      e.code = 'BUSY';
      e.retryable = true;
      throw e;
    },
  };
  await mod.tick(busyAgent);
  check('BUSY 时只调了一次（没有重发）', calls6.length === 1, `calls=${calls6.length}`);
  check('宿主会话 id 没被清掉（BUSY 不是「会话坏了」）', (await mod.readHostSessionId()) === 'mvs_host0000000000000000000000000000');
  process.env.HOME = prevHome6;
}

console.log('\n=== 每条成功即 ack（不等整批跑完）===');
{
  const home7 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d7 = join(home7, '.config', 'mcode-reset-card');
  mkdirSync(d7, { recursive: true });
  writeFileSync(join(d7, 'bot-revive-requests.json'), JSON.stringify({
    requests: [
      { id: 'req-e1', botId: 'b', botName: 'T', sessionId: 'mvs_0000000000000000000000000000e001', text: 'x', atMs: Date.now() },
      { id: 'req-e2', botId: 'b', botName: 'T', sessionId: 'mvs_0000000000000000000000000000e002', text: 'x', atMs: Date.now() },
    ],
  }));
  const prevHome7 = process.env.HOME;
  process.env.HOME = home7;
  let n = 0;
  let ackedDuringFirst = null;
  const slowAgent = {
    run: async () => {
      n += 1;
      if (n === 2) {
        // 第二条**开始前**检查：这时第一条已经完成并 ack 了。
        // ⚠️ 断言不能写在 run 内部（run 返回前还没 ack）——
        //    第一轮审计指出原来那样写会「0 条断言」（readFileSync 先抛 ENOENT）。
        const pf = join(d7, 'bot-revive-acked.json');
        ackedDuringFirst = existsSync(pf) ? JSON.parse(readFileSync(pf, 'utf8')).acked : null;
      }
      return { status: 'finished', sessionId: 'mvs_host000000000000000000000000000' };
    },
  };
  await mod.tick(slowAgent);
  const ackedFile7 = join(d7, 'bot-revive-acked.json');
  const finalAcked = existsSync(ackedFile7) ? JSON.parse(readFileSync(ackedFile7, 'utf8')).acked : [];
  check('第一条成功后立刻 ack（不等整批跑完）', ackedDuringFirst !== null && ackedDuringFirst.some((x) => (typeof x === 'string' ? x : x.id) === 'req-e1'), 'ackedDuringFirst=' + JSON.stringify(ackedDuringFirst));
  check('两条都进了 ack 列表', finalAcked.length >= 2, 'count=' + finalAcked.length);
  process.env.HOME = prevHome7;
}

console.log('\n=== OUTCOME_UNKNOWN：回合可能已跑 ⇒ 不重投，直接 ack ===');
{
  const home9 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d9 = join(home9, '.config', 'mcode-reset-card');
  mkdirSync(d9, { recursive: true });
  writeFileSync(join(d9, 'bot-revive-requests.json'), JSON.stringify({
    requests: [{ id: 'req-unknown', botId: 'b', botName: 'T', sessionId: 'mvs_0000000000000000000000000000u001', text: 'x', atMs: Date.now() }],
  }));
  const prevHome9 = process.env.HOME;
  process.env.HOME = home9;
  let calls9 = 0;
  const unknownAgent = {
    run: async () => {
      calls9 += 1;
      const e = new Error('outcome unknown');
      e.code = 'AGENT_OUTCOME_UNKNOWN';
      throw e;
    },
  };
  await mod.tick(unknownAgent);
  check('只调了一次（没有立刻重发）', calls9 === 1, 'calls=' + calls9);
  const acked9 = existsSync(join(d9, 'bot-revive-acked.json'))
    ? JSON.parse(readFileSync(join(d9, 'bot-revive-acked.json'), 'utf8')).acked
    : [];
  check('直接 ack（下一轮不会重投 ⇒ bot 不会收到两条）', acked9.some((x) => (typeof x === 'string' ? x : x.id) === 'req-unknown'), JSON.stringify(acked9));
  await mod.tick(unknownAgent);
  check('第二轮没有再投（被 ack 挡住）', calls9 === 1, 'calls=' + calls9);
  process.env.HOME = prevHome9;
}

console.log('\n=== 尝试次数表不能无限增长（Rust 不清理它）===');
{
  const home8 = mkdtempSync(join(tmpdir(), 'expiry-wake-'));
  const d8 = join(home8, '.config', 'mcode-reset-card');
  mkdirSync(d8, { recursive: true });
  // 请求文件里只剩一条；attempts 表里塞一堆「已经没有请求」的条目。
  writeFileSync(join(d8, 'bot-revive-requests.json'), JSON.stringify({
    requests: [{ id: 'keep-me', botName: 'T', sessionId: 'mvs_0000000000000000000000000000aaaa', text: 'x', atMs: Date.now() }],
  }));
  const stale = {};
  for (let i = 0; i < 50; i += 1) stale[`gone-${i}`] = 1;
  stale['keep-me'] = 2;
  writeFileSync(join(d8, 'bot-revive-attempts.json'), JSON.stringify({ attempts: stale }));
  const prevHome8 = process.env.HOME;
  process.env.HOME = home8;
  await mod.bumpAttempts('keep-me');
  const after = JSON.parse(readFileSync(join(d8, 'bot-revive-attempts.json'), 'utf8'));
  check('仍然有效的请求被保留', after.attempts?.['keep-me']?.n === 3, JSON.stringify(after.attempts?.['keep-me']));
  // ⚠️ **不能**按「requests 文件里还在不在」来清 attempts ——
  //    Rust 的 prune 会删掉「尝试次数用尽」的请求 ⇒ 这里的记录也会被连带删掉
  //    ⇒ 3 次封顶被架空，变成无限重试（2026-10-20 独立审计发现）。
  check('即使请求不在 requests 文件里，attempts 记录也保留（3 次封顶不被架空）', 'gone-0' in (after.attempts ?? {}), `条目数=${Object.keys(after.attempts ?? {}).length}`);
  process.env.HOME = prevHome8;
}

console.log(`\n${pass}/${total} passed`);
process.exit(pass === total ? 0 : 1);
