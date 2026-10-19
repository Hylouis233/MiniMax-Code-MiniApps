// @ts-check
/**
 * Bot 看门狗 — 错误分类与「该不该自动重新启用」的判定。
 *
 * 这个文件是**纯函数**，不碰网络、不碰数据库，可以直接单测。
 *
 * # 为什么要分类
 *
 * MiniMax Bot 挂掉有两种性质完全不同的原因：
 *
 *   - **可自愈**：HTTP 500（服务端内部错误）、网络切换、429/529（负载过高）。
 *     这类通常几十秒内自己就好了 ⇒ 自动重新启用是合理的。
 *   - **需人工**：Token Plan 用量到顶（42212）、积分耗尽（50110）、鉴权失效（50112）。
 *     这类**重试一万次也不会成功** ⇒ 自动重试只会白白烧 token，
 *     正确做法是**明确告诉用户缺什么**。
 *
 * 把两者混为一谈，正是「bot 反复挂掉」这个体验的来源。
 */

/**
 * 错误分类。
 *
 * - `self-healing`：可自动重新启用（500 / 网络 / 429 / 529 / 其它未知瞬时错误）
 * - `needs-user`：重试无用，必须人工介入（额度到顶 / 积分耗尽 / 鉴权失效）
 * - `healthy`：没有错误
 */
export const Verdict = {
  HEALTHY: 'healthy',
  SELF_HEALING: 'self-healing',
  NEEDS_USER: 'needs-user',
};

/** 来自 `packages/shared/src/llm-error-classifier.ts` 的协议错误码。 */
export const ErrorCode = {
  /** USAGE_LIMIT_EXCEEDED — Token Plan 用量到顶（上游 2056 / 2067）。 */
  USAGE_LIMIT_EXCEEDED: 42212,
  /** LLM_CREDITS_EXHAUSTED — 积分耗尽。 */
  LLM_CREDITS_EXHAUSTED: 50110,
  /** LLM_RATE_LIMITED — HTTP 429。 */
  LLM_RATE_LIMITED: 50111,
  /** LLM_AUTH_ERROR — HTTP 401/403。 */
  LLM_AUTH_ERROR: 50112,
  /** LLM_UPSTREAM_ERROR — 通用 4xx/5xx 兜底（HTTP 500 落这里）。 */
  LLM_UPSTREAM_ERROR: 50113,
  /** LLM_CLUSTER_OVERLOADED — HTTP 529。 */
  LLM_CLUSTER_OVERLOADED: 50151,
  /** LLM_TPM_RATE_LIMITED — 供应商 TPM/RPM 短时限流。 */
  LLM_TPM_RATE_LIMITED: 50150,
};

/**
 * 需要人工处理的错误码。
 *
 * 这些重试没有意义：
 *   - 42212：Token Plan 用量到顶 ⇒ 要用户升级套餐或补充积分
 *   - 50110：积分耗尽 ⇒ 要用户充值
 *   - 50112：鉴权失效 ⇒ 要用户重新登录
 */
const NEEDS_USER_CODES = new Set([
  ErrorCode.USAGE_LIMIT_EXCEEDED,
  ErrorCode.LLM_CREDITS_EXHAUSTED,
  ErrorCode.LLM_AUTH_ERROR,
]);

/** 明确可自愈的错误码。 */
const SELF_HEALING_CODES = new Set([
  ErrorCode.LLM_UPSTREAM_ERROR, // 50113 —— HTTP 500 等，Host 故意 fail-closed 了
  ErrorCode.LLM_RATE_LIMITED, // 50111 —— 429
  ErrorCode.LLM_CLUSTER_OVERLOADED, // 50151 —— 529
  ErrorCode.LLM_TPM_RATE_LIMITED, // 50150 —— TPM/RPM 短时限流，几秒就恢复
]);

/**
 * 错误消息里的关键字 —— 用来在没有结构化错误码时兜底判断。
 *
 * Host 有时只给文本消息（例如 `net::ERR_NETWORK_CHANGED`、
 * 「已达到 Token Plan 用量上限」），没有 code。
 */
const NEEDS_USER_TEXT = [
  '用量上限',
  'usage limit',
  'quota',
  '余额不足',
  '积分不足',
  'credit.*exhaust',
  '登录态',
  'unauthorized',
  'forbidden',
  '401',
  '403',
];

const SELF_HEALING_TEXT = [
  'network_changed',
  'err_network',
  'econnreset',
  'etimedout',
  'socket hang up',
  'internal server error',
  '服务不可用',
  'service unavailable',
  'overloaded',
  'rate limit',
  'too many requests',
  'bad gateway',
  'gateway timeout',
];

/**
 * 把错误归类。
 *
 * @param {number|null|undefined} code 协议错误码（可能为空）
 * @param {string} message 错误消息文本
 * @returns {{verdict: string, reason: string}} verdict 与一句人话理由
 */
export function classifyError(code, message) {
  const raw = String(message ?? '').trim();
  const text = raw.toLowerCase();
  const numeric = typeof code === 'number' ? code : Number(code) || 0;

  // ⚠️ 2026-10-19 实测踩到的坑：**没有错误码也没有错误消息 ⇒ 健康**。
  // 44 个 bot 会话里有 29 个属于这一类 —— 它们只是闲置很久（最后活动在
  // 两天前），并不是挂掉。此前这里一律返回 self-healing ⇒ 看门狗会去
  // 「重新启用」一批根本没坏的 bot，纯属打扰。
  if (!numeric && !raw) {
    return { verdict: Verdict.HEALTHY, reason: '没有错误' };
  }
  // 「Cron definition was deleted before execution」= 那个 cron 被删了，
  // 这是**配置变更**不是 bot 崩溃 ⇒ 重新启用没有意义，交给人看。
  if (/cron definition was deleted/i.test(raw)) {
    return { verdict: Verdict.NEEDS_USER, reason: '这个会话的定时任务已被删除，需要重新安排' };
  }

  if (numeric && NEEDS_USER_CODES.has(numeric)) {
    return { verdict: Verdict.NEEDS_USER, reason: describeNeedsUser(numeric) };
  }
  if (numeric && SELF_HEALING_CODES.has(numeric)) {
    return { verdict: Verdict.SELF_HEALING, reason: '服务端临时错误，稍后重试通常能恢复' };
  }
  // 没有结构化错误码 ⇒ 退回文本匹配
  if (NEEDS_USER_TEXT.some((re) => new RegExp(re, 'i').test(text))) {
    return { verdict: Verdict.NEEDS_USER, reason: '额度或登录态需要你处理，重试无用' };
  }
  if (SELF_HEALING_TEXT.some((re) => new RegExp(re, 'i').test(text))) {
    return { verdict: Verdict.SELF_HEALING, reason: '网络或服务端临时故障，稍后重试通常能恢复' };
  }
  // 未知错误：倾向自愈（宁可多试一次，也不要让 bot 永久瘫着）
  return { verdict: Verdict.SELF_HEALING, reason: '未知错误，按可恢复处理并退避重试' };
}

/** 需要人工处理时，给用户一句能照着做的话。 */
export function describeNeedsUser(code) {
  switch (code) {
    case ErrorCode.USAGE_LIMIT_EXCEEDED:
      return 'Token Plan 用量已达上限：升级套餐，或购买积分并开启积分自动消耗';
    case ErrorCode.LLM_CREDITS_EXHAUSTED:
      return '积分已耗尽：充值积分后才能继续';
    case ErrorCode.LLM_AUTH_ERROR:
      return '登录态已失效：重新打开 MiniMax Code 登录';
    default:
      return '需要你手动处理（重试无用）';
  }
}

/**
 * 退避时长：指数增长 + 抖动，**封顶** `capMs`。
 *
 * 封顶是必须的：无限重试若不封顶，第 40 次要等 12 天，等于没有重试。
 * 抖动也是必须的：否则并发重试会形成「惊群」，同时打在同一时刻。
 */
export function backoffMs(attempt, baseMs = 5000, capMs = 60000) {
  const exp = Math.min(baseMs * 2 ** Math.max(0, attempt - 1), capMs);
  return exp + Math.floor(Math.random() * 1000);
}

/**
 * 判断一个会话「现在是不是真的没在动」。
 *
 * ⚠️ 2026-10-19 实测踩到的坑：会话上的 `error_code` 可能是**陈旧值** ——
 * 上一轮 500 留下的，运行时没清。于是「有 error_code」并不等于「现在还挂着」。
 * 那个会话当时正在跑一个 20 多分钟的正常长回合。
 * ⇒ 必须同时确认「最近没有新活动」。
 *
 * @param {number} lastActivityMs 该会话最近一次活动（消息/回合）的时间
 * @param {number} nowMs 当前时间
 * @param {number} idleMs 超过多久算「没在动」
 */
export function isIdle(lastActivityMs, nowMs, idleMs = 90000) {
  if (!lastActivityMs) return true; // 从来没活动过 ⇒ 视为闲置
  return nowMs - lastActivityMs >= idleMs;
}
