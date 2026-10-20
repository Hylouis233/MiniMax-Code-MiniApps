// 静态守卫：抓「用了但没声明」的标识符。
//
// 2026-10-20 实测踩到的坑（v1.2 引入，v1.6 才发现）：
//   `doBuildSnapshot()` 里把 `platform` 写成了 `platformRes`
//   ⇒ `platformRes.fromCache` 抛 ReferenceError
//   ⇒ **整个看板挂掉**：额度、决策、重置卡、积分全拿不到
//     （`buildSnapshot()` 一抛异常，所有调用方都拿到错）。
//   而这类 bug 零测试覆盖 —— 单测测的是纯函数 `decide()`，
//   取数那层（`doBuildSnapshot`）从来没被跑过。
//
// ⇒ 这个测试用「声明集合 vs 使用集合」的静态比对把这类错挡住。
import { readFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
// ⚠️ 插件包里的布局不同：测试在 `tests/`，源码在 `../miniapp/node/`。
// ⇒ 两个位置都找一遍（否则插件包里跑这个测试会直接崩）。
const serverPath = [
  join(here, 'server.mjs'),
  join(here, '..', 'miniapp', 'node', 'server.mjs'),
].find((p) => existsSync(p));
if (!serverPath) {
  console.error('  FAIL  找不到 server.mjs（既不在本目录，也不在 ../miniapp/node/）');
  process.exit(1);
}
const source = readFileSync(serverPath, 'utf8');

let pass = 0;
let total = 0;
const check = (name, ok, extra = '') => {
  total += 1;
  if (ok) pass += 1;
  console.log(`${ok ? '  PASS' : '  FAIL'}  ${name}${extra ? `  ${extra}` : ''}`);
};

/** 收集源码里「声明过」的名字。 */
function collectDeclared(src) {
  const declared = new Set();
  const push = (n) => {
    if (n && /^[A-Za-z_$][\w$]*$/.test(n)) declared.add(n);
  };
  // const/let/var 声明（含解构）
  for (const m of src.matchAll(/\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)/g)) push(m[1]);
  for (const m of src.matchAll(/\b(?:const|let|var)\s*\[([^\]]+)\]\s*=/g)) {
    for (const part of m[1].split(',')) push(part.trim().split(/[:=]/)[0].trim());
  }
  for (const m of src.matchAll(/\b(?:const|let|var)\s*\{([^}]*)\}\s*=/g)) {
    for (const part of m[1].split(',')) push(part.trim().split(/[:=]/)[0].trim());
  }
  // 函数 / 类
  for (const m of src.matchAll(/\bfunction\s+([A-Za-z_$][\w$]*)/g)) push(m[1]);
  for (const m of src.matchAll(/\bclass\s+([A-Za-z_$][\w$]*)/g)) push(m[1]);
  // import { a, b as c }
  for (const m of src.matchAll(/import\s*\{([^}]*)\}\s*from/g)) {
    for (const part of m[1].split(',')) {
      const bits = part.trim().split(/\s+as\s+/);
      push((bits[1] ?? bits[0] ?? '').trim());
    }
  }
  // 函数参数（粗略：抓 (a, b) / ({ a, b })
  for (const m of src.matchAll(/\(([^()]*)\)\s*(=>|\{)/g)) {
    for (const part of m[1].split(',')) push(part.trim().replace(/[{}\[\]]/g, '').split(/[:=]/)[0].trim());
  }
  return declared;
}

const declared = collectDeclared(source);

/** 已知全局/内建（不是我们声明的，但合法）。 */
const GLOBALS = new Set([
  'console', 'JSON', 'Math', 'Date', 'Object', 'Array', 'String', 'Number', 'Boolean',
  'Promise', 'Map', 'Set', 'Error', 'TypeError', 'RangeError', 'Symbol', 'RegExp',
  'process', 'globalThis', 'Buffer', 'URL', 'URLSearchParams', 'Intl', 'BigInt',
  'setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'fetch', 'AbortController',
  'TextEncoder', 'TextDecoder', 'structuredClone', 'crypto', 'performance', 'queueMicrotask',
  'undefined', 'null', 'true', 'false', 'NaN', 'Infinity', 'parseInt', 'parseFloat',
  'isNaN', 'isFinite', 'encodeURIComponent', 'decodeURIComponent', 'require', 'module',
  'window', 'document', 'navigator', 'location', 'localStorage', 'sessionStorage', 'fetch',
]);

console.log('=== 取数层的「缓存结果」属性访问必须都有声明 ===');
// `cached()` 返回 { value, fromCache }，Promise.all 解构出来的名字就是这些结果的持有者。
// 这里专门盯 `.fromCache` —— 踩坑的那一行就是 `platformRes.fromCache`。
const fromCacheUses = [...source.matchAll(/\b([A-Za-z_$][\w$]*)\.fromCache\b/g)].map((m) => m[1]);
check('源码里确实有 .fromCache 的用法（守卫不是空转）', fromCacheUses.length > 0, `出现 ${fromCacheUses.length} 次`);

const undeclared = [...new Set(fromCacheUses)].filter((n) => !declared.has(n) && !GLOBALS.has(n));
check(
  '所有 .fromCache 的持有者都声明过',
  undeclared.length === 0,
  undeclared.length ? `未声明：${undeclared.join(', ')}` : '（全部已声明）',
);

console.log('\n=== Promise.all 解构出来的名字都被用对（不许改名后残留旧名） ===');
// 逐个检查：解构出的每个名字在文件里至少被引用一次（防止改了名字忘了另一处）。
const destructureBlocks = [...source.matchAll(/\b(?:const|let|var)\s*\[([^\]]+)\]\s*=\s*await Promise\.all/g)];
const allNames = destructureBlocks.flatMap((m) =>
  m[1].split(',').map((p) => p.trim()).filter(Boolean),
);
check('至少找到一个 Promise.all 解构', allNames.length > 0, `${allNames.length} 个名字`);
const unusedNames = allNames.filter(
  (n) => !/^[A-Za-z_$][\w$]*$/.test(n) || (source.split(new RegExp(`\\b${n}\\b`)).length - 1) < 2,
);
check('解构出来的名字都被用到', unusedNames.length === 0, unusedNames.join(', '));

console.log('\n=== 拼写错误兜底：常见的手滑模式 ===');
// 已知的坏模式：把 `xxxRes` 当成 `xxx` 用（本次踩坑就是 platformRes vs platform）。
const suspects = [...source.matchAll(/\b([A-Za-z_$][\w$]*Res)\b/g)].map((m) => m[1]);
const badRes = [...new Set(suspects)].filter((n) => !declared.has(n) && !GLOBALS.has(n));
check('没有「声明过但不存在」的 *Res 变量', badRes.length === 0, badRes.join(', '));

console.log(`\n${pass}/${total} passed`);
if (pass !== total) process.exitCode = 1;
