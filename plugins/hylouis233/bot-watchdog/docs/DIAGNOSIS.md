# MiniMax Bot 反复挂掉 · 根因诊断

> 诊断日期：2026-10-19。对象：`MiniMax-Code-MiniApps` 里的 `plugins/minimax-ai/minimax-bot`。

## 一、现象

`local_runtime_sessions` 里 `purpose='miniapp:minimax-bot'` 的会话**每报错一次就新建一个**，
10-17 那天几十分钟内堆了 20+ 个：

| session_id | status | error_code | error_message | 创建时间 |
| --- | --- | --- | --- | --- |
| `mvs_2b5491c4078f4852a7df5e1eb775c4a9` | **error** | 42212 | 当前已达到 Token Plan 用量上限…(2067) | 10-17 06:41 |
| `mvs_5fbb398903b34da181425babc4f5f184` | **aborted** | 50113 | Internal server error. Please retry your request. (500) | 10-17 05:37 |
| `mvs_36eec08efa684a1d8b98a2a9057ba490` | idle | 50113 | net::ERR_NETWORK_CHANGED | 10-17 03:37 |
| `mvs_7be887cac0114136a5c183ab5394c5bf` | idle | 50113 | open platform service unavailable, please retry | 10-17 06:43 |
| `mvs_4471ae616d184875af41cf8e7d779b3b` | idle | 42212 | 当前已达到 Token Plan 用量上限…(2067) | 10-19 17:45 |
| `mvs_9c990771bb5944f8935ff5fa64225974` | idle | 50113 | net::ERR_NETWORK_CHANGED | 10-19 21:24 |

**两类错误码**（来自 `packages/shared/src/llm-error-classifier.ts` 的 `LLM_ERROR_STATUS_CODES`）：

| 码 | 含义 | 上游映射 |
| --- | --- | --- |
| **42212** | `USAGE_LIMIT_EXCEEDED` — Token Plan 用量到顶 | 2056 / 2067 |
| **50113** | `LLM_UPSTREAM_ERROR` — 通用 4xx/5xx 兜底 | HTTP 500 等 |
| 50110 | `LLM_CREDITS_EXHAUSTED` | 1400010161 |
| 50111 | `LLM_RATE_LIMITED` | HTTP 429 |
| 50112 | `LLM_AUTH_ERROR` | HTTP 401/403 |
| 50151 | `LLM_CLUSTER_OVERLOADED` | HTTP 529 |

## 二、根因

### 根因 1：500 被**故意**判为「不可重试」

`toLLMRetryDecision()`（`llm-error-classifier.ts:345`）的判定顺序：

```ts
if (status === 429) return { retryable: true, reason: 'rate_limited' };
if (status === 529) return { retryable: true, reason: 'overloaded' };
return { retryable: false, reason: metricKind };   // ← 500 落这里
```

而 `toLLMMetricErrorKind()` 里 `status >= 500` → `'server_error'`。

**⇒ HTTP 500（⇒ 50113）`retryable: false`。**
注释写得很直白：

> Strict transport-retry policy shared by all hosts that opt into framework retry.
> Product quota, credits, auth and explicit cancellation always win over an outer
> transient-looking HTTP status. **Only the bounded allowlist below is retryable;
> generic 5xx and unknown failures deliberately fail closed.**

可重试的白名单只有：**429 / 529 / timeout / network**。
500 虽然是典型的暂时性错误，但**不在白名单里**。

### 根因 2：42212 是**产品额度上限**，重试也无用

`usage_limit` 在 `toLLMRetryDecision()` 的**第一个**分支就被判 `retryable: false`。
Token Plan 额度到顶时，重试多少次都不会成功 —— 除非额度被补充（重置卡/积分）。

### 根因 3：minimax-bot 自己的重试只有 2 次，且窗口极短

`miniapp/node/engine.mjs:36`：

```js
const TURN_RETRY_DELAYS_MS = [8000, 20000];   // 8s、20s，一共只重试 2 次
const NO_RETRY_REASONS = new Set(['auth', 'quota', 'context_too_long', 'blocked', 'invalid_request']);
```

⇒ 一次失败最多 3 次尝试、跨度 28 秒。
而 42212（额度到顶）**要等用户补充额度** —— 28 秒的重试窗口对它毫无意义；
500（服务端抖动）**有时能在 28 秒内自愈**，但 500 压根不在 Host 的可重试白名单里，
`retryable=false` ⇒ **minimax-bot 自己那 2 次重试也不会触发**。

### 根因 4：客户端没有「重新启用」入口

`miniapp/client/` 里搜不到任何 `重新启用` / `reenable` / `reactivate` 入口。
bot 挂掉后，用户只能**手动再发一条消息**才能唤起它。

## 三、结论

| 错误 | 性质 | 现有行为 | 是否合理 |
| --- | --- | --- | --- |
| 42212 Token Plan 到顶 | 产品额度上限，需人工补充 | 立即失败，不重试 | **合理** —— 重试无用 |
| 50113 / HTTP 500 | 服务端内部错误，通常**暂时性** | 立即失败，不重试 | **不合理** —— 500 被 fail-closed 误伤 |
| net::ERR_NETWORK_CHANGED | 本地网络切换 | 立即失败 | **不合理** —— 属于可重试白名单里的 `network` |
| 429 / 529 | 负载过高 | 可重试，但只有 2 次、28 秒窗口 | 窗口偏短 |

**核心问题**：500 与网络切换这两类**本来可以自愈**的错误被一次判定就打死了，
用户看到的就是「bot 反复挂掉」——每次挂掉都表现为一个新会话 + 一条错误通知。

## 四、方案

新增一个 MiniApp「Bot 看门狗」：

1. **持续监测** `purpose='miniapp:minimax-bot'` 的会话，找出 `status IN ('error','aborted')`
   或 `error_code IS NOT NULL` 的那些。
2. **分类**：
   - **可自愈类**（50113 / HTTP 500 / 网络错误 / 429 / 529）⇒ 退避后自动重新启用
   - **需人工类**（42212 额度到顶 / 50110 积分耗尽 / 50112 鉴权失败）⇒ **不自动重试**，
     只在面板上明确提示「需要补充额度 / 重新登录」
3. **重新启用** = 向该 bot 的会话发一条续跑消息（走官方 `session send` 通道），
   而不是新建会话。
4. **退避封顶**（如 60 秒）+ 抖动 + 可中断，避免把接口打爆。
5. **幂等**：同一会话在退避期内不重复触发；已被人工处理（会话转为 idle/active 且无错误）则撤销待办。

## 五、关键约束（实现时必须遵守）

- **不能新建会话**：那正是「反复挂掉」的表象。必须复用原会话。
- **额度类错误不自动重试**：重试解决不了 42212，硬重试只会烧 token。
- **判据要基于回读**：读到的 `error_code` 可能是上一轮留下的陈旧值
  （2026-10-19 在量化会话上就踩过：会话 `error_code=50113` 是陈旧的，
  而它正在跑一个 20 多分钟的正常长回合）。
  ⇒ 必须同时确认「该会话当前没有新活动」。
