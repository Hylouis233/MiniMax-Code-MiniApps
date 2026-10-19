# 蹬王之王 · 重置卡自动使用器（后台版）


> English version: [../README.md](../README.md)

> **v1.3** — 2026-10-19。**Bot 看门狗并入本包**，且 **400/404/422 也重试**。
>
> - 新增：守护 MiniMax Bot 团队，因临时性故障挂掉的 bot 复用原会话自动重新启用
> - 新增：HTTP 400 / 404 / 422 也纳入可重试（此前只有 5xx 与网络错误）
> - 独立的 `bot-watchdog` 插件已删除，功能全部并入本包
>
> **v1.2** — 2026-10-19。在 v1.1 修完「闭环能不能真的跑通」之后，
> 又按 PR #28 的 review 意见改了一轮，并新增**可重试错误无限重试**。
>
> - v1.2 修掉 3 个 review 指出的真 bug（详见下方）
> - v1.2 新增：500 / 429 等可重试错误**一直重试**，退避封顶 60 秒且可中断
>
> **v1.1** 修掉 3 个让恢复机制**长期假成功**的缺陷，补上 2 道闸门，
> 并把踩过的 11 个坑全部落进文档与回归测试。详见 [v1.1 更新内容](#v11-更新内容)。

### v1.2 更新内容

| # | 变更 | 后果 |
| --- | --- | --- |
| 1 | `cards.items` 是给页面看的**脱敏视图**（无 `card_id`），却拿它判断「卡还在不在列表里」 | 该判断恒为 false ⇒ **扣卡每次都被跳过**，`reset_card/use` 从未被调用。改为新增 `cards.raw`（带 `card_id`）供写路径使用 |
| 2 | 积分接口失败被 `.catch(() => null)` 吞掉 ⇒ 等同于「没有积分」 | 结论从「留卡」翻转成「用卡」⇒ **网络故障导致误扣真卡**。新增 `creditDataComplete` 闸门：数据不完整 ⇒ 本轮**不做任何写操作** |
| 3 | 守护进程侧同样问题：积分读取失败只记日志 | 同样会误扣。`earliest_credit()` 现在返回「数据是否完整」，不完整 ⇒ `wait` |
| 4 | **新增：可重试错误无限重试** | 429 / 500 / 502 / 503 / 504 与网络错误（连接重置、超时、DNS）一律重试到成功；退避指数增长、**封顶 60 秒**、带抖动；`SIGINT`/`SIGTERM` 可立即中断。401/403 重取 token 一次，其余 4xx 与业务错误快速失败。扣卡也走重试（`out_biz_no` 幂等键保证不会重复扣） |

测试：Rust **41/41**、Python **20/20**、三方 parity **19/19**、MiniApp 决策 **58/58**、
重试真实行为 **4/4**（用本地 mock server 实测 500/429 会重试、404 立即失败）。

无人值守地盯住 Token Plan 额度：**接近耗尽时**按「先过期先用」自动使用一张重置卡；
额度打满时**自动暂停**所有进行中的目标，额度回落后**自动恢复并叫醒**它们。

通过 macOS `launchd` 常驻，与 MiniMax Code 桌面端完全独立——关掉 MCode、退出登录、
甚至你没开电脑，它都在跑。

---

## 目录

- [现在有两套，各自独立](#现在有两套各自独立)
- [共享配置](#共享配置)
- [完整闭环](#完整闭环一条消息怎么走完全程)
- [v1.1 更新内容](#v11-更新内容)
- [踩过的坑](#踩过的坑十一个都写在这里了)
- [扣卡规则](#扣卡规则与-mini-app蹬王之王逐条一致)
- [管理命令](#管理命令)
- [测试](#测试)
- [前提与边界](#前提与边界)

---

## 现在有两套，各自独立

| | 后台服务（本目录） | Mini App「蹬王之王」 |
| --- | --- | --- |
| 运行方式 | launchd 系统服务 | MCode 运行时内的 Node 进程 |
| 关掉 MCode | **照常运行** | 停止 |
| 扣卡策略 | **与 Mini App 完全一致** | 同左 |
| 作用 | 真正的无人值守执行 | 实时看板 + 手动触发 |

两者都用服务端的 `out_biz_no` 幂等键，**不会重复扣同一张卡**。

**代码位置**

| 组件 | 路径 |
| --- | --- |
| Rust 守护进程（主力） | `rust/src/` |
| Python 决策参考实现 | `reset_card_autouse.py` |
| 三方一致性校验器 | `verify-parity.mjs` + `check-parity.sh` |
| Mini App（看板） | `~/Desktop/mmx/miniapps/expiry-guardian/`（独立仓库外） |

---

## 共享配置

Rust 后台 / Python / Mini App Node **读同一个文件**，谁都不许再把阈值写死在自己代码里。

```json
{
  "schemaVersion": 1,
  "cardTriggerPercent": 99,
  "cardSkipBeforeResetMinutes": 60,
  "quotaLevels": { "tight": 80, "low": 95 },
  "goalPause": { "enabled": true, "triggerPercent": 95, "resumeAtPercent": 0 },
  "resumeCronId": "cf98b82e-306b-490c-9e27-141ee44fab40"
}
```

| 字段 | 含义 |
| --- | --- |
| `cardTriggerPercent` | 扣卡触发线（用卡的阈值） |
| `cardSkipBeforeResetMinutes` | 距 5 小时额度刷新不足这么多分钟时，**不消耗重置卡**（额度马上就自己回来了，烧卡是浪费）。0 = 关闭该保护 |
| `quotaLevels.tight` / `.low` | 四档红绿灯分界点：低于 tight=充足、≥tight=紧张、≥low=不足、耗尽=燃尽 |
| `goalPause.enabled` | 目标自动暂停总开关 |
| `goalPause.triggerPercent` | 5 小时额度到这个百分比就**暂停所有进行中的目标** |
| `goalPause.resumeAtPercent` | 回落到这个百分比就恢复（见下方「恢复判据」） |
| `resumeCronId` | 恢复值守 cron 的 id。**不是用户可改项**——守护进程要知道该武装哪个 cron |

后台服务**每轮重读**该文件，所以改完约 1 分钟内生效，不必重启。

Mini App 页面上有完整的可视化入口（在「自动化阈值」卡片）：
扣卡触发线、暂停目标触发线、额度紧张/不足分界、刷新前暂停扣卡分钟数 —— 全部可直接改，
并配一条**断轴刻度**把 0–黄档 压缩成波浪带、只展开真正有阈值分布的区间，
外加图例与冲突提示（比如「扣卡线 90% 不高于红色档 95%，红灯将永远不会亮」）。

字段非法时**整体回退默认并打出告警**，绝不静默纠正——静默纠正会让人以为设的值生效了。
（Rust 侧开了 `deny_unknown_fields`，键名写错会直接报错而不是悄悄降级。）

> ⚠️ **Mini App 保存配置时必须保留 `resumeCronId`**（v1.1 修）。
> 它的 `normalizeConfig()` 是**白名单式重建**，漏掉这个字段 ⇒
> 用户在页面上改任何一项设置，都会把 cron id 从配置里抹掉 ⇒
> 守护进程读到空 id ⇒ 整个恢复机制**静默失效**，且不报任何错。

### 两条触发线是分开的

| 配置项 | 默认 | 作用 |
| --- | --- | --- |
| `goalPause.triggerPercent` | **95%** | 暂停目标 |
| `cardTriggerPercent` | 99% | 扣重置卡 |

**为什么暂停线要留 95% 而不是 99%**（2026-10-19 实测）：
额度从 99% 涨到 100% 只用了 **27 秒**。按 99% 暂停，目标会在 100% 那一刻被打死，
留下 `error_code=50113` 的卡死会话。按 95% 暂停则在打满**之前**就停手 ——
实测当轮额度冲上 100% 时**没有任何目标被打死**。

⇒ **暂停线要留出比「撞线到打满」更宽的余量**，扣卡线才是「卡」的触发点。

### 恢复判据：为什么不是「回落到 0%」

理想口径是「额度刷新回 0% 时恢复目标」。**但这条路走不通**：
守护进程每 60 秒才轮询一次，额度刷新后的那个 0% 瞬间**通常根本观测不到**。
实测：重置卡在 14:35:41 把额度打回 0%，下一轮 14:36:20 读到的是 **1%**。
若严格按 `used <= 0` 判定，目标会被**永久停住**——这个故障真实发生过。

所以判据是「**用量回落到触发线以下**」：额度只有在「用掉重置卡」或「5 小时窗口翻页」时才会掉下来，
掉下来就说明额度已经回来了。`resume_at_percent` 作为更严格的附加条件保留。

### 日志里能看到「什么时候刷新」

```
[10-19 20:00.281] 额度: 5小时限额=已用0% / 剩余100% | 周限额=不限量 | 视频限额=已用0% / 剩余100%
[10-19 20:00.281] 5 小时窗口: 240 分钟后刷新（预计 10月20日 00:00）
```

这行是 v1.1 补的。以前日志只有「已用 X%」，没有刷新时间 ——
于是「窗口什么时候翻页」只能靠猜（曾误以为是 19:41，实际是 20:00，差 19 分钟，
害我误判「窗口不会来」）。

⇒ **凡是「等某个外部事件发生」的流程，都必须把该事件的剩余时间打出来**，
否则无法区分「还没到」和「不会来」。

---

## Bot 看门狗：挂掉的 bot 自己回来

MiniMax Bot 每失败一次就**新建一个会话**，所以一个反复挂掉的 bot 会留下一串死会话，
界面上每次还多一条错误通知。

本包盯住你的 Bot 团队，把因**临时性**故障挂掉的 bot **复用原会话**重新启用。

| 情况 | 看门狗怎么做 |
| --- | --- |
| HTTP 500、网络切换、429/529、TPM 限流、400/404/422 | 退避等待后**复用原会话**重新启用 |
| Token Plan 到顶(42212)、积分耗尽(50110)、登录失效(50112) | **不重试**，明确告诉你缺什么 |
| bot 本身正常、或只是残留着上一轮的过期错误码 | 不打扰它 |

完整取证见 [`docs/BOT_DIAGNOSIS.md`](BOT_DIAGNOSIS.md)。

### 怎么把 bot 叫醒

看门狗跑在 **Host 之外**（launchd 常驻、无 token、无人值守），
够不到 minimax-bot 的 MCP 端点 —— 那个端口由 Host 分配，
且 Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到。
2026-10-19 实测了三条通道：

| 通道 | 能续别的 Mini App 拥有的会话吗 | 结论 |
| --- | --- | --- |
| `context.agent.run({ sessionId })` | ❌ | 规范原文：*Each Mini App owns its sessions and can continue only those* |
| 直连对方 MCP 端点 | ❌ | Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到 |
| **起一个 Agent 让它调 `message_bot` MCP 工具** | ✅ | 实测唤起挂着 42212 的 bot，13 秒内回复 |

⇒ 看门狗负责**判定**（谁挂了、该不该救）并写请求；
**cron 值守**（在主 Agent 目录里）负责**投递** —— 它有 `mavis` 工具，
能调 MCP 的 `message_bot`，而那会**复用目标 bot 自己的会话**。

## 可重试错误：一直重试，不放弃

遇到**暂时性**故障时，接口调用会一直重试到成功为止 —— 看板不会变空白，
守护进程也不会把一次抖动当成「没数据」。

| 情况 | 行为 |
| --- | --- |
| **429** 负载过高 / 限流 | 一直重试 |
| **500 / 502 / 503 / 504** 内部或网关错误 | 一直重试 |
| 连接重置 / 超时 / DNS 失败 | 一直重试 |
| 401 / 403 登录态失效 | 重新取 token，再试一次 |
| 其他 4xx | 快速失败（重试没有意义） |
| 业务错误（参数非法、额度不足） | 快速失败 |

退避是指数增长 + 抖动，**封顶 60 秒**：第一次约等 1 秒，此后任何一次都不会
等超过一分钟，所以「无限」不会变成「疯狂打接口」。每次重试都打日志 ——
卡住的请求必须能说明自己在等什么。

重试是**可中断**的：`SIGINT` / `SIGTERM`（`launchctl` 发的就是这两个）
会让正在重试的请求立刻放弃，所以即使卡在坏掉的接口上，守护进程也能停下来。

扣卡也走重试路径，这**是安全的**：请求带 `out_biz_no`，服务端以它为幂等键，
重复提交只会回放结果（`REPLAYED`），不会扣掉第二张卡。

## 数据完整性：读不到 ≠ 没有

「读到余额 0」和「没读到余额」是两件事。以前积分接口一失败，
代码就当成「没有可用积分」，把结论从「留卡」翻转成「用卡」——
而扣卡是真实写操作，网络故障会导致误扣。

现在读到不完整会**如实报为不完整**，本轮**完全不做写操作**（`wait`）。
守护进程侧同理：平台积分或 Code 积分读取失败 ⇒ 标记数据不完整，
而不是「记个日志就忽略」。

看板侧还多留了一份**原始卡列表**（带 `card_id`），与给页面看的脱敏列表并存。
脱敏列表没有 `card_id`，拿它判断「这张卡还在不在列表里」恒为 false ——
那正是扣卡每次都被跳过、`reset_card/use` 从未被调用的原因。

## 完整闭环：一条消息怎么走完全程

```
额度到线
  │
  ├─▶ 守护进程（无 token、无人值守、在 Host 之外）
  │     ① 判定：写入 local_runtime_thread_goals 把目标置为 paused
  │     ② 记账：goal-pause.json 记下「哪些是**我们**停的」
  │     ③ 写恢复请求：goal-resume-requests.json
  │     ④ 按需武装 cron（有待恢复请求才武装，处理完解除）
  │
  └─▶ cron 值守（主 Agent 目录，唯一能对任意会话起回合的角色）
        ⑤ 读两个文件 → 若有待处理请求：
        ⑥ mavis session send → 把目标叫醒
        ⑦ 把 id 记进 acked（幂等）
```

### 完整的架构边界：谁能暂停、谁能起回合

2026-10-19 实测逐条验过，结论如下（**每一条都有证据**）：

| 谁 | 能暂停目标 | 能起回合（恢复） | 证据 |
| --- | --- | --- | --- |
| **守护进程**（Host 之外） | ✅ 写 DB，运行时回合边界会看到 | ❌ | 队列项只被**活跃**会话的调度器消费；一条 10 月 1 日的项在队列表里躺了 **446 小时** |
| **MiniApp**（Host 内） | ❌ | ❌ | `context.agent.run` 只认**自己 Agent 目录**下的会话，用户的 goal 会话 → `SESSION_NOT_FOUND` |
| **cron 值守**（主 Agent 目录） | ✅ | ✅ | `session send` 走主进程，能对任意会话起回合 |
| **Host 主进程**（Electron IPC） | ✅ | ✅ | 但**无 TCP 端口**，外部连不上；MiniApp 端口的 `loopbackToken` 是 per-process 的 |

⇒ **结论：恢复用户的 goal 会话，唯一通道是主 Agent 目录里的会话投递。**
守护进程（外部进程）无论怎么写库都起不了回合。

⇒ **因此采用「双组件」分工**：
- **守护进程**（无 token、无人值守）：判定 → 暂停 → 记账 → 写恢复请求 → **按需武装/解除武装** cron
- **cron 值守**（主 Agent 目录）：读恢复请求 → `session send` → 目标自己的续跑循环接着跑

cron 每轮约 47k tokens（其中 46k 是缓存读取），**每 5 分钟常驻 = 每天 288 轮 ≈ 13M tokens/天** ——
那会吃掉正要保护的那块额度。所以守护进程**按需武装**它：
有待恢复请求才武装，全部处理完就解除武装。
恢复请求还有 30 分钟时效上限，避免为一个叫不醒的目标无限期空转。

---

## v1.1 更新内容

### 修了什么

| # | 缺陷 | 后果 | 证据 |
| --- | --- | --- | --- |
| 1 | `resume_goal` 的 SQL 里 `WHERE goal_id='{goal_id}'` 是**字面量**（Rust `format!` 不替换裸花括号） | UPDATE 恒影响 0 行，但 sqlite3 照样退出码 0 ⇒ **恢复永远假成功**。haisi 目标从 2026-10-01 起就一直停在 `paused`，而日志在 10-19 的 14:43 / 17:06 两次宣称「已恢复」 | 真实库对照：字面量匹配 **0** 行，真实 id 匹配 **1** 行 |
| 2 | `reconcile_resume_cron` 只挂在恢复分支 | 额度持续高于触发线时走暂停分支 ⇒ cron **永远不被解除武装**，每 17 分钟空跑一轮（每轮 47k tokens） | 19:46–20:00 期间 `run_count` 涨到 6，日志一次「解除武装」都没有 |
| 3 | Mini App 的 `normalizeConfig()` 漏掉 `resumeCronId` | 用户在页面改任何设置 ⇒ cron id 被抹 ⇒ 恢复机制**静默失效**，无任何报错 | `normalizeConfig` 是白名单式重建 |
| 4 | 账本清空前不回读目标状态 | 「请求写成功」≠「目标真的活了」⇒ 目标实际还是 `paused` 时账本已被清空 ⇒ 恢复后目标被运行时忽略 | 20:22 实测：账本空、目标 paused |
| 5 | 复活判据只看会话级 `error_code` + 超龄 | 会话级 error_code 是**上一次** 500 的陈旧值 ⇒ 把**正在干活的 20 分钟长回合**误判成卡死 ⇒ 多余的复活打断正常运行 | 实测那个回合每 10–20 秒一次 `response_status=200` |
| 6 | 武装 cron 时 `next_run_at_ms` 设成 `now+60s` | cron 表达式 `4/17 * * * *` 只在每小时 **4、21、38、55 分**触发 ⇒ 不对齐就会被调度器跳过 | 对齐到 20:21:00 后立刻触发（`run_count` 0→8） |
| 7 | 值守会话被归档 | cron 每次触发都 `failed` / `CRON_SESSION_TARGET_ARCHIVED` | `local_runtime_v2_cron_runs` 里 18:38 之后 7 次触发全失败 |
| 8 | 队列残留不清 | cron 的 `session send` 残项让会话判 busy ⇒ 下次投递**退化成排队** | haisi 会话积了 5 条 `source='communication'` 残项 |
| 9 | 账本重复记账 | 目标被运行时回弹后重新暂停，旧条目还在账本里又 push 一条 ⇒ 同一目标处理两次，且 `trigger_used_percent` 被覆盖（99→100） | 19:19 实测 |
| 10 | 运行时回弹被误判成「用户手动处理」 | 回弹（回合结束后运行时把目标改回 active）被当成用户操作剔出账本 ⇒ 恢复时不再放回它 ⇒ 目标永久停在 active 且不被我们拥有 | 18:59→19:13 实测 |
| 11 | 配置里的 cron id 丢失后没有任何告警 | 整个恢复机制无声失效 | 现在每轮都打 `cron 值守状态: pending N 个，cron 已武装/未武装` |

### 加了什么

- **「假 started」判据**：回合被 500 打死后会话 status 卡在 `started` 不清 ⇒
  原来 `status <> 'started'` 把它排除在复活之外（目标 active 但 turns 不动、日志一次复活都没有）。
  现在纳入这一路。
- **第二道闸（防误杀长回合）**：用 `local_runtime_message_rows.created_at_ms`
  （每条 assistant 消息含每次工具调用往返都落一行）作为**回合级活动信号**。
  ⚠️ 不能用 `turn_ingress.accepted_at_ms` —— 那是回合**开始**时间，长回合一样超龄。
- **账本清空前的回读校验**：状态不是 `active` 就保留在账本里继续观察，并打出告警。
- **配置自检告警**：`goal_pause.trigger_percent=101 超出 1..100，已夹到 100`。

### 实测验证到的闭环

```
17:05.359  Goal 暂停: 已暂停 2 个目标（5 小时已用 99.0%）
17:05.445  ✓ 已使用重置卡 423331，额度已重置        ← 83ms 内完成暂停+扣卡
17:06.728  Goal 恢复: 已恢复 2 个目标（5 小时已用 1.0%，额度已回落）
```

以及 18:59–20:00 这一轮「**无卡时的正确降级**」：

```
18:59.759  Goal 暂停: 已暂停 2 个目标（5 小时已用 99.0%）
18:59.759  决策[keep-credit]: 没有可用的重置卡，让 Code 积分·赠送（11月18日到期）先消耗
19:00.236  决策[wait]: 额度已耗尽，既没有积分也没有可用的重置卡
20:00.281  额度: 5小时限额=已用0% / 剩余100%        ← 窗口自然刷新
```

⇒ **有卡、无卡、临近刷新三条分支都验过**，无卡时不会反复尝试扣卡。

### 验证方法（可复用）

> **「这条修复到底生效没有」要在真实环境上造一个受控样本。**
>
> 「这一轮没报错」不能证明「修好了」——那只是**没跑到那条分支**。
> 正确做法：造一个能触发它的样本，走完整真实链路，**回读结果**，然后清理。
> 实测用这个方法抓出了缺陷 1（假成功）：日志说「已恢复」，
> 而目标行回读仍是 `paused` —— 差一步回读校验就得出完全相反的结论。

---

## 踩过的坑（十一个都写在这里了）

1. **Rust `format!` 不替换裸花括号** ⇒ `WHERE goal_id='{goal_id}'` 是字面量 ⇒ UPDATE 影响 0 行却退出码 0 ⇒ **假成功**。
   ⇒ 凡靠 UPDATE 改状态的判据，**必须回读校验**。「命令执行成功」≠「改到了行」。

2. **库里的唯一索引不唯一**：`idx_local_runtime_queue_items_session_client_request`
   在迁移源文件里写的是 `CREATE UNIQUE INDEX`，**真实运行库里是普通 `CREATE INDEX`**。
   ⇒ `INSERT OR IGNORE` 拦不住重复，实测同 crid 插出了 6 条，把队列堵死。

3. **队列项会把会话判成「busy」**，从而让唯一能用的 `session send` 只能排队。
   ⇒ 守护进程**不再往队列表投续跑消息**，只清理自己造下的残留；
   且清理必须有**内容级护栏**（只删自家消息，用户手动消息同源）。

4. **同一 epoch 的请求被 ack 过之后，守护进程重写它也不会再被处理**
   （cron 靠 ack 列表去重）⇒ 写请求时必须同步把该 id 从 ack 列表里摘掉。

5. **回合结束后运行时会把目标改回 `active`**（回合边界回弹）。
   ⇒ 账本对账不能只看 `status != 'paused'` 就当「用户手动处理」。

6. **不能用 `goal.updated_at_ms` 判断「有没有被改过」** ——
   `pause_active_goals` 自己在暂停时就把该字段改写成 `now_ms`，
   所以 `updated > paused_at` 恒为 false。
   ⇒ **判据要用你改不动的第三方事实**（如 `turn_ingress.completed_at_ms`）。

7. **目标会被官方 API 硬删除**（MCode 界面层发 `DELETE /session/:id/goal`），
   `local_runtime_thread_goals` 的行**硬删除**（表里只有 `dismissed_at_ms` 一个软删字段，
   而被删的目标该字段也没写）。
   ⇒ 任何以 goal_id 为键的外部账本，都必须处理「键已不存在」的情况。

8. **回合会挂 20 多分钟但完全正常** ——
   实测一个 R034 引擎实测回合跑了 20+ 分钟，期间每 10–20 秒一次 `response_status=200`。
   ⚠️ **长回合 ≠ 死锁**。判断死活要看**有没有新活动**（`message_rows.created_at_ms`），
   不是看 `status='accepted'` 超了多久。

9. **cron 的目标会话会被运行时自动归档** ——
   实测 `session_kind='cron'` 的会话在跑完后被发 `POST /session/:id/archive`，
   随后 cron 每次触发都 `CRON_SESSION_TARGET_ARCHIVED`。
   全库分布：`kind=cron` 的 1 个全被归档，`kind=conversation` / `task` 的不受影响。

10. **白名单式重建配置的函数，漏一个字段就是一条静默失效的通道**（见 `resumeCronId`）。

11. **`run_count` 在涨不代表 cron 真的在跑** ——
    实测 8 次触发里只有第 1 次 `delivered`，后面 7 次全是 `failed`。
    ⇒ 查 cron 成败要看 `local_runtime_v2_cron_runs.status` / `.error_code`，
    **不要**看 `scheduler_jobs.run_count`。

### 查运行时问题的入口

```bash
# cron 每次触发的成败（权威表）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT status, IFNULL(error_code,'-'), datetime(created_at_ms/1000,'unixepoch','localtime')
   FROM local_runtime_v2_cron_runs WHERE cron_id='<cronId>' ORDER BY created_at_ms DESC LIMIT 5;"

# 目标状态（恢复是否真的生效，回读这里）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT goal_id,status,IFNULL(status_reason,'-'),turns_used FROM local_runtime_thread_goals;"

# 某个会话最后一个回合的「最后活动时间」（判死活用这个）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT turn_id, datetime(MAX(created_at_ms)/1000,'unixepoch','localtime')
   FROM local_runtime_message_rows WHERE session_id='<sid>' GROUP BY turn_id
   ORDER BY MAX(created_at_ms) DESC LIMIT 3;"
```

主进程日志在 `~/Library/Logs/MiniMax/main-*.log`，但**会被几万行 permission 决策日志淹没** ——
搜的时候要过滤掉 `permission` / `cmdPreview` / `sandbox.invocation` 这些噪音。

---

## 扣卡规则（与 Mini App「蹬王之王」逐条一致）

1. **提前一个百分点动手**：触发线默认 **99%**（`used >= total × 99%`）。
   不等真正耗尽——等耗尽再处理，中间那次请求已经失败，会打断正在跑的自动执行。
2. **先过期先用**：
   - 积分先过期 → **不用卡**，让积分先消耗（卡更耐放，留到以后）
   - 重置卡先过期 → **用卡**把额度重置回来，保住更耐放的积分
3. **距刷新太近就不烧卡**（`cardSkipBeforeResetMinutes`，默认 60 分钟）——
   额度马上就自己回来了，这时候扣卡是纯浪费。
4. **没卡就不硬来**：有积分没卡 → 让积分先消耗；连积分都没有 → `wait`，不反复尝试。
5. 跳过已过期卡，只用未过期的。
6. 先扣积分里**最早到期**的那一笔（平台积分池 + Code 积分明细，两处取更早的）。

### 完整分支表

| 条件 | 动作 |
| --- | --- |
| 未到触发线 | `idle` |
| 耗尽 + 无积分 + 有卡 | `use-card` |
| 耗尽 + 无积分 + 无卡 | `wait` |
| 耗尽 + 有积分 + 无卡 | `keep-credit` |
| 耗尽 + 都有，卡先过期 | `use-card` |
| 耗尽 + 都有，积分先过期 | `keep-credit` |

### 一致性是测出来的，不是声称的

| 层 | 内容 | 现状 |
| --- | --- | --- |
| 1 | 后台决策单测，逐条复刻上表每个分支 | **20/20** |
| 2 | 跨语言一致性：同一组输入分别喂给 Rust 的 `decide()`、Node 的 `decide()` 与 Python 的 `decide_background()`，比对动作与**理由文本逐字** | **17/17** |
| 3 | **校验器自检**：故意注入一处漂移，要求校验器必须报错 | ✓ 能抓到 |

第 3 层是刻意设计的。一个永远显示「一致」的校验器等于没有校验，
所以必须证明它对已知错误会失败。

Rust 单测另有 **36/36**（其中 8 个是 v1.1 新增的回归测试，每个对应上表的一个缺陷：
`resume_sql_contains_the_real_goal_id`、`resume_goal_reports_missing_goal_instead_of_fake_success`、
`session_id_whitelist`、`ledger_does_not_duplicate_existing_goal`、
`runtime_rebound_is_not_user_handled`、`stale_started_requires_error_code`、
`deleted_goal_is_treated_as_handled`、`purge_only_targets_our_own_resume_messages`）。

单独跑：

```bash
cd rust && cargo test --release   # 33/33
python3 bg_decision.test.py       # 20/20
node verify-parity.mjs            # 17/17
./check-parity.sh                 # 一次跑完
```

---

## 管理命令

```bash
# 状态
launchctl list | grep reset-card

# 停 / 启
launchctl unload ~/Library/LaunchAgents/com.local.mcode-reset-card.plist
launchctl load   ~/Library/LaunchAgents/com.local.mcode-reset-card.plist
# 或直接重启（会重载配置）
launchctl kickstart -k "gui/$(id -u)/com.local.mcode-reset-card"

# 看日志
tail -f /tmp/mcode-reset-card.log
tail -f /tmp/mcode-reset-card.err

# 手动判定一次（只读，不扣卡）
python3 ~/mcode-reset-card-autouse/reset_card_autouse.py --dry-run once

# 跑测试
cd ~/mcode-reset-card-autouse/rust && cargo test --release
```

---

## 日志长什么样

```
[20:00.281] 额度: 5小时限额=已用0% / 剩余100% | 周限额=不限量 | 视频限额=已用0% / 剩余100%
[20:00.281] 5 小时窗口: 240 分钟后刷新（预计 10月20日 00:00）
[20:00.287] 决策[idle]: 额度充足（5 小时已用 0%），未到 99% 触发线，暂不动用重置卡
[20:04.573] cron 值守状态: pending 请求 1 个，cron 已武装
[20:04.590] cron 值守状态: pending 请求 1 个，cron 已武装
[20:12.458] Goal 恢复: 已恢复 1 个目标（5 小时已用 3.0%，额度已回落，已投递续跑）
[xx:xx:xx] ✓ 已使用重置卡 423331，额度已重置
```

---

## 防重复扣卡

三层保护：

1. `bg-state.json` 记录已尝试的卡，**落盘**，进程重启也不重复
2. 先记账再请求，中途崩溃不会重复扣
3. `out_biz_no` 服务端幂等键，即使跨进程重试也不会重复扣除

---

## 前提与边界

- **需要 MiniMax Code 桌面端在运行**。登录凭据通过本机的 OAuth lease socket 获取，桌面端就是那个签发方。
  - 桌面端没开时，服务会记录鉴权失败并退出（launchd 的 `KeepAlive` 会尝试重启）
- 只处理**个人版**额度，团队版接口不同。
- 恢复目标**只能靠 cron 值守的 `session send`**。目标会话若长期闲置、
  或卡在一个未结束的回合里，外部注入只能排队 —— 需要在 MCode 里打开过那个会话。
- 后台服务只管扣卡与目标暂停/恢复，**看板仍在 Mini App 里**（关掉面板即停，这是 MCode 的设计限制）。

---

## 接口依据（实测确认）

```
GET  www.minimaxi.com/backend/account/token_plan/remains_percent      额度用量
GET  www.minimaxi.com/backend/account/reset_card/list                可用卡
POST www.minimaxi.com/backend/account/reset_card/use                 使用一张卡
GET  www.minimaxi.com/backend/account/token_plan_credit              平台积分池到期
GET  agent.minimaxi.com/minimax-cloud/api/v1/credit/details          Code 积分到期
POST agent.minimaxi.com/matrix/api/v1/commerce/get_membership_info   积分汇总
```

**两个容易踩的坑**（都在代码注释里标了）：

1. `credit/details` 的参数是 `page_number`/`page_size`（下划线），且必须带
   `include_expired=1` 才有历史记录。不带只返回最近 20 条。
2. `reset_card/flows` 的 `create_time` **必填**，传 `0` 表示不限起始时间；
   传具体时间戳会返回 `2013 invalid params` —— 那是**错误**，不是「没有记录」。
   曾经把这条错误读成「一张卡都没用过」。

---

## 海外版

```bash
python3 ~/mcode-reset-card-autouse/reset_card_autouse.py --base https://platform.minimax.io --dry-run once
```
