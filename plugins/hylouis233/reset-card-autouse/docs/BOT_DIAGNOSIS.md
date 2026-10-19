# Bot 看门狗 · 唤醒通道实测排查手册

> 诊断日期：2026-10-19 首版 / **2026-10-20 重写为「唤醒通道排查手册」**。
> 对象：`MiniMax-Code-MiniApps` 里的 `plugins/minimax-ai/minimax-bot`。
>
> 这份手册回答一个问题：**「bot 挂掉了，为什么没被叫醒？」**
> 按「**现象 → 查什么表/什么日志 → 怎么判断**」的格式写，每一条判据都来自实测。

---

## 零、先跑这 5 步（90% 的问题在这里面）

```bash
cd ~/.config/mcode-reset-card

# ① 判定有没有发生？  —— 看守护进程日志
grep "Bot 看门狗" /tmp/mcode-reset-card.log | tail -20

# ② 请求写进去了吗？ —— 待投递的请求
cat bot-revive-requests.json

# ③ 投递方处理了吗？ —— 已处理的请求（幂等表）
cat bot-revive-acked.json

# ④ 目标会话起回合了吗？ —— 权威判据
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT turn_id,status,source,datetime(accepted_at_ms/1000,'unixepoch','localtime') acc
   FROM local_runtime_turn_ingress WHERE session_id='<bot会话id>'
   ORDER BY accepted_at_ms DESC LIMIT 5;"

# ⑤ MiniApp 进程活着吗？ —— 投递方是它，不是守护进程
ps aux | grep -i "expiry-guardian\|server.mjs" | grep -v grep
```

| 走到哪一步断了 | 去 [一、四段链路](#一唤醒通道的完整链路) |
| --- | --- |
| ① 没有「Bot 看门狗」这行 | 判定层 ⇒ [现象 A](#现象-a日志里根本没有bot-看门狗这行) |
| ① 有、但 ② 文件里没有 | 写文件失败 ⇒ [现象 A](#现象-a日志里根本没有bot-看门狗这行) |
| ② 有、③ 没有 | 投递方没跑 ⇒ [现象 B](#现象-b请求文件里有但acked-里没有) |
| ③ 有、④ 没有新回合 | 消息没进目标会话 ⇒ [现象 C](#现象-cacked-里有但目标会话没起新回合) |
| ④ 有、状态 `failed` | 回合本身失败 ⇒ [现象 D](#现象-d回合起来了但状态是-failed) |
| ④ 有、`completed`，bot 却像没收到 | bot 侧没接茬 ⇒ [现象 E](#现象-e回合completed但bot-像是没收到) |

---

## 一、唤醒通道的完整链路

唤醒要经过**四段**。每一段都有独立的证据，断在哪段就在哪段查。

```
┌─ ① 判定 ─────────────────────────────────────────────────────────┐
│ Rust 守护进程（Host 之外 · launchd · 无 token）                   │
│   · 读 bot 会话状态 + 最后活动时间                                │
│   · classify_error() 分「可自愈 / 需人工」                        │
│   · 可自愈 ⇒ 写 bot-revive-requests.json                          │
│   证据：日志「Bot 看门狗：<名> 因暂时性错误（<文案>）挂掉，已写入唤醒请求」 │
│        请求文件里出现 id = bot-revive:<botId>:<sessionId>          │
└───────────────────────────────────────────────────────────────────┘
                              │ 文件交接
                              ▼
┌─ ② 读取 ─────────────────────────────────────────────────────────┐
│ 蹬王之王自己的 MiniApp（Host 之内 · 60 秒一轮）                  │
│   · 读 bot-revive-requests.json                                   │
│   · 跳过：已在 acked 里的 / 超过 30 分钟 TTL 的 / id 不合法的      │
│   证据：ack 表不含该 id，且请求文件里它还没过期                     │
└───────────────────────────────────────────────────────────────────┘
                              ▼
┌─ ③ 起回合 ───────────────────────────────────────────────────────┐
│   · agent.run({ text: 「用 mavis 工具，command="session send"…」 })│
│   · 这个回合的会话 agent_name = 'mavis' ⇒ 天然带桌面端 mavis 工具    │
│   证据：local_runtime_sessions 里多出一条 purpose='miniapp:expiry-guardian'│
│        的会话，且它有 1 个 turn                                    │
│   ⚠️ 代价：实测 ≈ 93k tokens（input 42k / output 374 / cache read 51k）│
└───────────────────────────────────────────────────────────────────┘
                              ▼
┌─ ④ 送达 ─────────────────────────────────────────────────────────┐
│   · 回合里的 agent 调 mavis 工具的 session send                    │
│   · session send 内部走 turns.steer()，与官方 session send 同一条路 │
│   证据：目标会话 local_runtime_turn_ingress 多出一条新 turn          │
│        目标会话 local_runtime_message_rows 多出一条 role='user' 的   │
│        「【看门狗】上一个回合因内部错误中断…」                       │
└───────────────────────────────────────────────────────────────────┘
```

> 💡 **一条经验**：**第 ④ 段之前的一切都只是「准备」**。
> 只有目标会话真的多了一条 turn，才叫「唤醒了」。

---

## 二、必须知道的三条底层事实

这三条是 2026-10-19 / 2026-10-20 实测出来的，**解释了上面每一处「为什么」**。

### 事实 1：唤醒必须走 `session send`，裸写队列项无效

**隔离实验**：往一个挂着 50113 的 bot 会话直接写一条 `local_runtime_queue_items` 行，
3 分钟后仍是 `queued`、未 claim、无 turn、无消息；
**同一会话**用官方 `session send`，5 秒内 accepted 并被 bot 回复。

⇒ **Host 的队列轮询器只认经过 steer 的行**。绕开 steer 塞行 = 白写。

⚠️ **反向陷阱**：**队列表的状态列不能当「有没有被送达」的判据**。
实测 5 行 `session send` 写进去的行，**送达成功后仍然停在 `status='queued'`、`claim_id=NULL`**，
而对应的回合确实跑完并回复了。
真正的送达记录在 `data_json.deliveryAttempts[]` 里：

```sql
-- 送达过的行：有 deliveryAttempts
SELECT session_id, status, IFNULL(claim_id,'<null>'),
       json_array_length(json_extract(data_json,'$.deliveryAttempts')) AS attempts
FROM local_runtime_queue_items
WHERE session_id='<bot会话id>' ORDER BY created_at_ms DESC;
```

- `attempts > 0` ⇒ 有人 claim 过 ⇒ 起过回合
- `attempts = 0`（或没有这个键）⇒ **没人 claim ⇒ 没起过回合**

### 事实 2：不能直接 `agent.run` 续别的 Mini App 的会话

`service/miniapp/agent/gateway.ts` 的 `assertSessionOwnership`（第 828 行）：

```ts
if (owner !== caller.pluginId) {
  throw new MiniAppAgentError('SESSION_NOT_FOUND', 'Mini App Agent session not found');
}
```

实测库里 **45 个** bot 会话的 `purpose` 全是 `miniapp:minimax-bot`
⇒ 蹬王之王对它们直接 `agent.run` **永远被拒**（实测每 30 秒 6 条全拒）。

### 事实 3：`session send` 不做 owner 检查 ⇒ 这是唯一能用的绕法

`local-runtime/dist/runtime/mavis-session-send.js` 的准入注释原文：

> Reachability is the whole access model: the caller identity comes from the
> runtime context, the target must be resolvable through the canonical Session
> query, and everything after that is TurnSystem admission. **There is no
> topology check** — a task child, a sibling, a root Session and the caller's
> own Session all take the same path.

⇒ 任何拿到 mavis 工具的会话都能唤醒任意 bot 会话。
而 MiniApp 的 `agent.run`（**不带** `sessionId`）建的会话 `agent_name = 'mavis'`
⇒ 那个 agent 天然带桌面端 mavis 工具。

⇒ **结论：能用的路径是「起一个自己的 agent 回合，让回合里的 agent 用 mavis 工具的 `session send`」。**

---

## 三、排查手册

### 现象 A：日志里根本没有「Bot 看门狗」这行

**查什么**

```bash
# 守护进程还活着吗？
launchctl list | grep reset-card
tail -20 /tmp/mcode-reset-card.log
tail -20 /tmp/mcode-reset-card.err

# bot 会话本身的状态
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT session_id,status,IFNULL(error_code,'-'),IFNULL(error_message,'')
   FROM local_runtime_sessions WHERE purpose='miniapp:minimax-bot' AND error_code IS NOT NULL;"
```

**怎么判断**

| 观察到 | 结论 |
| --- | --- |
| 守护进程没在跑 | `launchctl kickstart -k "gui/$(id -u)/com.local.mcode-reset-card"` |
| 守护进程在跑，但**没有任何 bot 会话带错误码** | **没有 bot 挂掉**。这是正常的 —— 44 个 bot 会话里有 29 个是「无错误码、无错误消息」，它们只是闲置很久 |
| 有 bot 带错误码，但日志没提它 | 看 `botGuard.enabled` 是否为 `false`（配置被误抹 ⇒ 看门狗静默失效，无任何报错） |
| 错误码是 **42212 / 50110 / 50112** | **这是设计如此**：需人工，不自动重试。日志里会有一行「需要你处理」 |
| 错误码是 **50113 / 50150 / 50151** | 应该出现「因暂时性错误挂掉」那行。没有 ⇒ 判定层被跳过，看 `botGuard.idleMs`（默认 90 秒）是否被调得过大 |

---

### 现象 B：请求文件里有，但 acked 里没有

**查什么**

```bash
cat ~/.config/mcode-reset-card/bot-revive-requests.json   # 待投递
cat ~/.config/mcode-reset-card/bot-revive-acked.json      # 已处理
```

**怎么判断**

| 观察到 | 结论 | 怎么办 |
| --- | --- | --- |
| 请求的 `atMs` 距今 **超过 30 分钟** | **TTL 过期**，投递方会直接跳过 | 守护进程下一轮会重新写一条新的（同一个 id 会被覆盖），等 60 秒 |
| `sessionId` 不是 `mvs_` 开头 | 投递方会判为非法请求并记 `botWakeFailed` | 查 minimax-bot 的 `state.json` 里会话映射 |
| MiniApp 进程**没在跑** | **投递方不在** ⇒ 永远不会被处理 | 打开 MCode 的「蹬王之王」面板，进程会被 `ensureMiniAppStarted` 按需拉起 |
| MiniApp 在跑，但面板是**刚打开的** | 巡检是 **60 秒**一轮 | 等 60 秒再看 |
| 请求 id 在 acked 里但 bot 还是没反应 | 投递方会把「已 ack 的 id」**整个跳过**（`readWakeRequests()` 过滤 `acked.has(id)`）⇒ 这条请求**不会被再投一次** | 让守护进程重写请求（见下一行的 id 语义） |
| 目标会话**正卡在一个没结束的回合**里 | `session send` 只能**排队**，不会打断 | 等那个回合结束，或在 MCode 里手动开一次 |

> ⚠️ **同一个 bot 可能被投递两次**（实测，见第四节）：请求 id 是
> `bot-revive:<botId>:<sessionId>`，**同一个 bot+会话恒定**；
> 而 ack 表要到**整批处理完**才写（`tick()` 循环结束后才 `appendAcked`）。
> 守护进程在这段窗口里会把它当成「还没处理」，写一条**同 id** 的新请求。
> ⇒ 看到同一个 bot 收到两条「【看门狗】」**不是 bug**，bot 侧会按「从中断处继续」处理，
> 不会重复交付。**但它会多烧一个 agent 回合**（≈93k tokens）。

> ⚠️ **投递方不是重入安全的**：`setInterval(60s)` 直接调 `botWakeTick()`，没有「上一轮还在跑」的闸门。
> 一批 7 个请求、每个回合 10–60 秒 ⇒ **单轮就超过 60 秒** ⇒ 下一轮会**并发**再跑一次，
> 两轮读到同一份请求文件 ⇒ 重复投递 + 重复烧 token。
> 实测 2026-10-20 00:48:51–00:50:44（113 秒）内起了 **14 个**唤醒回合，
> 而这一轮只有 **7 个** bot 需要唤醒。
> ⇒ 查「为什么 token 涨得比预期快」时，先看这个。

> 💡 **确认 MiniApp 进程**：它是投递方。守护进程活着 ≠ 有人投递。

---

### 现象 C：acked 里有，但目标会话没起新回合

**查什么**

```bash
# 目标会话最近的回合
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT turn_id,status,source,datetime(accepted_at_ms/1000,'unixepoch','localtime') acc,
          datetime(completed_at_ms/1000,'unixepoch','localtime') done
   FROM local_runtime_turn_ingress WHERE session_id='<bot会话id>'
   ORDER BY accepted_at_ms DESC LIMIT 5;"

# 目标会话最近的消息（看「【看门狗】」有没有作为 user 消息落进去）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT role,datetime(created_at_ms/1000,'unixepoch','localtime') t,substr(data_json,1,160)
   FROM local_runtime_message_rows WHERE session_id='<bot会话id>'
   ORDER BY created_at_ms DESC LIMIT 6;"
```

**怎么判断**

| 观察到 | 结论 |
| --- | --- |
| `turn_ingress` 里**没有新 turn**，但 `message_rows` 里**有**「【看门狗】」 | 消息进去了，回合没起来 ⇒ 查该会话是不是正忙（`status='started'`） |
| `turn_ingress` 里没有新 turn，`message_rows` 也没有 | **根本没送达**。回查第 ④ 段的 `deliveryAttempts`（见事实 1） |
| 有新 turn 但 `status='accepted'` 且 `completed_at_ms` 为空 | 回合**还在跑**。回合可以跑 20 多分钟且完全正常 —— 判死活要看 `message_rows` 的 `MAX(created_at_ms)`，**不是**看 `accepted` 超了多久 |
| 有新 turn 但 `source` 不是 `turn` | 不是正常入口起的回合，多半是别的插件在动这个会话 |

---

### 现象 D：回合起来了，但状态是 `failed`

**查什么**

```bash
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT status,IFNULL(error_code,'-'),IFNULL(error_message,''),
          datetime(accepted_at_ms/1000,'unixepoch','localtime') acc
   FROM local_runtime_sessions WHERE session_id='<bot会话id>';"
```

**怎么判断**

| `error_code` | 含义 | 是不是我们的锅 |
| --- | --- | --- |
| **50150** | TPM / RPM 限流 | ❌ **不是**。是正常业务错误 —— 唤醒本身成功了，是 bot 自己那轮被限流。**实测 2026-10-20 00:49 的 7 次唤醒里有 3 次是它** |
| **50113** | 通用 4xx/5xx 兜底（HTTP 500） | 可能是上游抖动，等下一轮退避重试 |
| **50151** | 集群过载（HTTP 529） | 上游过载，等下一轮 |
| **42212 / 50110 / 50112** | 额度到顶 / 积分耗尽 / 登录失效 | 🔴 **需要人工**。看门狗不会自动重试这类，日志里会有「需要你处理」 |

> 💡 **「失败」≠「没唤醒」**。回合能起来就说明投递链路是通的。
> 失败的是 bot 自己那一轮。

---

### 现象 E：回合 `completed`，但 bot 像是没收到

**查什么**

```bash
# bot 的回复正文
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT role,datetime(created_at_ms/1000,'unixepoch','localtime') t,substr(data_json,1,400)
   FROM local_runtime_message_rows WHERE session_id='<bot会话id>'
     AND created_at_ms > <唤醒时间戳>
   ORDER BY created_at_ms LIMIT 8;"
```

**怎么判断**

这是**正常且正确**的行为。看门狗发的是**续跑消息**，不是新任务：

> 「【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。
> 请从中断处继续，不要重做已完成的部分。」

实测 2026-10-20 00:49，bot 的第一条回复是：

> 「先确认中断点在哪、且有没有新派活落进台账——不重做已交付的部分。」

⇒ 它**先核对**自己到底有没有半成品，再决定要不要接着干。**这是对的**：
看门狗不该替 bot 决定做什么。
如果它回「没有中断的工作要续」，那就是它确认过确实没半成品 —— **不是唤醒失败**。

---

### 现象 F：日志被刷屏

**查什么**

```bash
wc -l /tmp/mcode-reset-card.log
cat ~/.config/mcode-reset-card/bot-notified.json
```

**怎么判断**

2026-10-19 实测：不加去重表时，日志**每 60 秒**把同样 9 个 bot 的同样内容各刷一遍，
**40 分钟 463 行**。

v1.4 已修：新增 `bot-notified.json` 去重表，**键 = `botId|错误码|错误文案`**。
同一个 bot 的同一个错误**只提示一次**；错误一变就是新问题，值得再提示。

⇒ 如果你还在看到刷屏，说明去重表没写成功（权限 / 目录不存在），
或者**错误文案在变**（比如文案里带动态内容 ⇒ 每次都是新键 ⇒ 去重失效）。

---

### 现象 G：「需要你处理」一直不消失

**查什么**

```bash
cat ~/.config/mcode-reset-card/bot-notified.json
```

**怎么判断**

「需要你处理」是**终态提示**，不是待办队列：

| 错误码 | 你需要做什么 |
| --- | --- |
| **42212** Token Plan 用量到顶 | 升级 Token Plan 套餐，或购买积分补充用量并**开启积分自动消耗** |
| **50110** 积分耗尽 | 充值积分 |
| **50112** 登录态失效 | 重新打开 MiniMax Code 登录 |
| `Cron definition was deleted` | 那个会话的**定时任务**被删了，需要重新安排（这是配置变更，不是崩溃） |

处理完之后，错误码变了或会话恢复了 ⇒ 去重表里的旧键不再匹配 ⇒ 提示自然停止。
**去重表不会自己清空**（只保留最近 200 条），这是有意的。

---

## 四、端到端实测记录（2026-10-20 00:49）

一次真实的完整唤醒，7 个挂掉的 bot：

| 阶段 | 证据 |
| --- | --- |
| ① 判定 | 日志 7 行「Bot 看门狗：<名> 因暂时性错误（<文案>）挂掉，已写入唤醒请求」 |
| ② 请求 | `bot-revive-requests.json` 里 7 条，`id = bot-revive:<botId>:<sessionId>` |
| ③ 起回合 | 唤醒窗口（00:48:51–00:50:44）内库里多出 **14 条** `purpose='miniapp:expiry-guardian'` 的会话，每条**恰好 1 个 turn**（每回合 2 次模型调用） |
| ④ 送达 | **7 个**目标会话的 `message_rows` 里都出现 `role='user'` 的「【看门狗】…」（00:49:00–00:49:57）；`turn_ingress` 都多出新 turn |

> 💡 **投递次数可以多于 bot 数**：这一轮共 **8 次**投递落在 **7 个** bot 上
> —— `mvs_5fbb398…` 与 `mvs_7bc3db35…` 各收了 2 次（00:49:00 / 00:49:52 与 00:49:05 / 00:49:57）。
> 原因是 ack 表要到**投递成功之后**才写入，守护进程在这段窗口里会把它当成「还没处理」，
> 写一条**同 id** 的新请求（`id = bot-revive:<botId>:<sessionId>`，同一个 bot+会话恒定）。
> ⇒ 看到同一个 bot 收到两条「【看门狗】」**不是 bug**，也不会造成重复交付
> （bot 侧自己会按「从中断处继续」处理）。但它会**多烧一个 agent 回合**。

**结果分布**（以 `turn_ingress` 为准）：

| 结果 | 数量 | 说明 |
| --- | --- | --- |
| `completed` | 4 | 正常跑完并回复 |
| 仍在跑（`accepted`） | 1 | 查询时还没结束 |
| `failed` | 3 | **全部是 50150（TPM 限流）** —— 正常业务错误，不是唤醒失败 |

**bot 真的回话了**（`mvs_7bc3db35b1ab4b1a98eeca640c45adf1`，00:49:19）：

> 「先确认中断点在哪、且有没有新派活落进台账——不重做已交付的部分。」

**代价**（`local_runtime_token_usage`，每个唤醒回合）：
input 42,005 / output 374 / cache read 50,939 ≈ **93k tokens**。
⇒ **只在真的有 pending 请求时才跑，绝不空转。**

---

## 附录 A：错误为什么会发生（根因）

> 这部分解释「**为什么这些 bot 会挂**」，与「为什么没被唤醒」是两件事。

MiniMax Bot 的会话**每报错一次就新建一个**，于是界面上表现为「bot 反复挂掉」：
每挂一次多一个会话、一条错误通知。10-17 那天几十分钟内堆了 20+ 个。

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

⇒ 这就是看门狗要填的那个洞：**复用原会话**发一条续跑消息，而不是新建会话。

---

## 附录 B：错误码全表

来自 `packages/shared/src/llm-error-classifier.ts` 的 `LLM_ERROR_STATUS_CODES`：

| 码 | 含义 | 上游映射 | 看门狗判定 |
| --- | --- | --- | --- |
| **42212** | `USAGE_LIMIT_EXCEEDED` — Token Plan 用量到顶 | 2056 / 2067 | 🔴 需人工 |
| **50110** | `LLM_CREDITS_EXHAUSTED` — 积分耗尽 | 1400010161 | 🔴 需人工 |
| **50111** | `LLM_RATE_LIMITED` — 限流 | HTTP 429 | 🔴 落入「未知错误码」分支 ⇒ **不自动重试** |
| **50112** | `LLM_AUTH_ERROR` — 鉴权失效 | HTTP 401/403 | 🔴 需人工 |
| **50113** | `LLM_UPSTREAM_ERROR` — 通用 4xx/5xx 兜底 | HTTP 500 等 | ✅ 可自愈 |
| **50150** | `LLM_TPM_RATE_LIMITED` — TPM/RPM 短时限流 | — | ✅ 可自愈 |
| **50151** | `LLM_CLUSTER_OVERLOADED` — 集群过载 | HTTP 529 | ✅ 可自愈 |

> ⚠️ **50111 是已知缺口**：HTTP 429 本身在 Host 的可重试白名单里，
> 但它映射到的 50111 **不在**看门狗的自愈码集合内 ⇒ 落进「未知错误码，重试可能无用」分支，
> 不会自动唤醒。改法：把 50111 加进 `bot_guard.rs` 的 `LLM_*` 自愈码匹配里。

---

## 附录 C：读不到 ≠ 没有

⚠️ **2026-10-19 实测踩到的坑**：44 个 bot 会话里有 **29 个**是「无错误码、无错误消息」——
它们只是闲置很久，**并不是挂掉**。若把这类判成可自愈，看门狗会去「重新启用」一批没坏的 bot。

⚠️ **错误码可能是陈旧值**：运行时**不清** `error_code`。
2026-10-19 在量化会话上踩过：那个会话当时正在跑一个 20 多分钟的**正常长回合**，
而它的 `error_code=50113` 是上一轮 500 留下的。

⇒ 所以判据必须**同时**确认「最近没有新活动」：
`local_runtime_message_rows` 里该会话的 `MAX(created_at_ms)` 距今 ≥ `botGuard.idleMs`（默认 90 秒）。
**长回合 ≠ 死锁** —— 判断死活看有没有新活动，不是看 `status='accepted'` 超了多久。
