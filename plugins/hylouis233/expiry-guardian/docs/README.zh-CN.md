# 蹬王之王 · 重置卡自动使用器（后台版）


> English version: [../README.md](../README.md)

> **v1.6** — 2026-10-20 夜。**修掉一个让 Bot 守护完全静默的真 bug**，
> 并给唤醒链路加上**自检**（静默失效必须被说出来）。
>
> - 🔴 **真 bug**：minimax-bot 的状态文件里有**一个被截断的代理对转义**（`\ud83d`，高代理在、低代理丢失）。
>   Python 的 `json.loads` 与 Node 的 `JSON.parse` 都能解析，**Rust 的 `serde_json` 严格拒绝**
>   ⇒ `read_bot_sessions()` 返回空 ⇒ 整个 Bot 看门狗**静默失效**：
>   **守护进程在跑、日志干净、实际一个 bot 都没看护**。已加 `parse_lenient_json()` 容错解析。
>   实测对比：修复前 4 次重启（45 分钟）**零条**看门狗日志；修复后立刻报「检查 45 个，可自愈 11 个」。
> - ✅ **唤醒链路自检**（新模块 `bot_bootstrap.rs`）：宿主重启后运行时没起来 ⇒ **如实告警**而不是静默；
>   运行时在跑但积压条数一直不减少 ⇒ 判定「投递空转」并告警。
> - 🔧 **纠正一个错误结论**：此前写进文档的「面板关着时唤醒不会发生」**不准确**。
>   实测运行时 **44 分钟没开面板仍在跑**，唤醒循环照常投递。
>   真正的缺口是**宿主重启**（宿主重启后不会自动拉起任何 MiniApp）。
> - 🔧 **`miniapp publish` 已修好**：根因是 manifest 声明的端点集与 Host 内存里的「已发布快照」对不上。
>   解法：**只声明 1 个 MCP 端点**（`server=expiry-guardian`, `path=/mcp/expiry-guardian`）。
>
> **v1.5** — 2026-10-20 晚间。**架构加固**：唤醒会话复用、请求判据换轨、
> acked 不再是永久黑名单、修掉并发写的三个坑、客户端 401 自愈、新增 local MCP 端点。
>
> - **架构不变**：Rust 守护进程只做**判定 + 记账**；蹬王之王自己的 MiniApp 负责**投递**
> - 唤醒会话复用 ⇒ 唤醒不再每次冷启动（实测 02:39 一轮唤醒 6 个 bot，**新建会话数 0**）
> - 请求判据：从「30 分钟 TTL」改成「**最多试 3 次 + 24 小时兜底**」
> - acked 改成带时间戳 ⇒ 只抑制 30 分钟，**不再「救过一次就永久不救」**
> - 修掉并发写的三个坑：临时文件名撞车 / lost update / 清理条件把 3 次封顶架空
> - 瞬时错误**不重发**（`BUSY` / `AGENT_OUTCOME_UNKNOWN`）⇒ bot 不再收到重复的续跑消息
> - 客户端 401 自愈：连续 3 次 401 ⇒ 整页重载（上限 5 次），面板不再卡在「实时连接中断」
> - 新增 local MCP 端点（`watchdog_status` / `watchdog_wake` / `watchdog_decide`）
> - 补 `50111`（短时限流）到可自愈分类；新增 `check-test-parity.sh`
>
> **v1.4** — 2026-10-20。**彻底删掉定时任务（cron）依赖**，唤醒通道换成
> 「自己的 MiniApp 起一个回合 + 会话内 `session send`」。
>
> - 新架构：Rust 守护进程只做**判定 + 记账**；**投递**交给蹬王之王自己的 MiniApp
> - 删掉 `resumeCronId` 配置项（Rust 配置、MiniApp `normalizeConfig`、真实 `config.json` 三处同步删）
> - Bot 看门狗与目标恢复**共用同一条唤醒通道**
> - 端到端实测（2026-10-20 00:49）：7 个挂掉的 bot **全部**被自动唤醒，bot 真的回话了
> - 修掉刷屏：新增 `bot-notified.json` 去重表（此前每 60 秒刷 9 行，40 分钟 463 行）
>
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

### v1.6 更新内容

这一版的核心是：**把「看起来一切正常、实际一个 bot 都没叫醒」这类静默失效全部变成会说话**，
并修掉三个在生产里实测到的真 bug。

| # | 变更 | 后果 |
| --- | --- | --- |
| 1 | 🔴 **`parse_lenient_json()`：Rust 读 bot 状态文件改用容错解析** | minimax-bot 的 `state.json` 里有一个**被截断的代理对转义**（`\ud83d`）。Python/Node 能解析，**Rust 的 `serde_json` 严格拒绝** ⇒ `read_bot_sessions()` 返回空 ⇒ 整个 Bot 看门狗**静默失效**。实测：修复前 4 次重启共 45 分钟**零条**看门狗日志，修复后立刻报「检查 45 个」 |
| 2 | 🔴 **按「最新回合」判定坏没坏**（不再只看会话上的 `error_code`） | `error_code` 是**陈旧值**（运行时不清它）。实测 15:39 那一轮写了 11 条唤醒请求，其中 **8 条是误判** —— 那 8 个 bot 的最新回合其实是 `completed`（好着呢），错误码是更早那次失败留下的。修后同一批从「可自愈 11 个、投递 11 个」变成「可自愈 0 个、投递 0 个」，**只救真正坏的那几个**（实测随后只唤醒了 2 个真坏的 bot） |
| 3 | 🔴 **acked 永久化 + 请求 id 按「实例」唯一化** | 原来 acked 的 30 分钟窗口**同时**管两件事：「投递过了吗」与「多久能再救」。⇒ 窗口一过，**已投递过的请求又变回未处理** ⇒ 投递端**每 30 分钟重投一次**（每回合 ≈47k tokens），还会打断已恢复的 bot。现在：**投递过的就是投递过了**（永久）；「多久能再救」由守护进程自己的 30 分钟频率限制管；每次写入是**新实例 id** ⇒ 不会互相误杀 |
| 4 | 🔴 **读不到 bot 名单必须告警** | 原来 `if bots.is_empty() { return }` —— **一个字都不打**。「名单读不出来」在日志里完全不可见 ⇒ 守护进程看起来一切正常，实际**一个 bot 都没看护**。现在会明确说「本轮**没有看护任何 bot**（守护进程在跑，但它是瞎的）」，同一个原因只说一次 |
| 5 | ✅ **新增 `bot_bootstrap.rs`：唤醒链路自检** | 宿主重启后 MiniApp 没被拉起 ⇒ **如实告警**而不是静默；运行时在跑但积压条数一直不减少 ⇒ 判定「投递空转」并告警 |
| 6 | ✅ **新增投递空转检测（`StuckWatch`）** | 运行时**在跑**但积压条数连续 15 轮（约 15 分钟）没减少 ⇒ 判定唤醒循环已死 ⇒ 告警。覆盖「运行时活着但循环已经空转」这一类之前完全没人看的情况 |
| 7 | 🔧 **运行时存活判据重写** | 原来靠**硬编码端口 55932**（宿主的默认分配值，宿主改规则就误判），且那个端口属于**宿主进程**。现在改成「**有活进程** 且它的 **cwd 是本插件的运行时包目录** 且该包的 `package.json` **或** `.minimax-plugin/plugin.json` 的 `name == expiry-guardian`」—— 两个锚点都认（实测有个运行时目录 `package.json` 没有 name 字段，只有 `plugin.json` 有），且「目录还在但进程没了」不算 |
| 8 | 🔧 **纠正文档里的一个错误结论** | 此前写「面板关着时唤醒不会发生」——**不准确**。实测运行时 44 分钟没开面板仍在跑，唤醒循环照常投递。真正的缺口是**宿主重启** |
| 9 | 🔧 **`miniapp publish` 修复** | 根因是 manifest 声明的 MCP 端点集与 Host 内存里的「已发布快照」对不上。**只声明 1 个端点** + 相应 `servers.mcp.json` + `plugin.json` 的 `"mcpServers": ["servers.mcp.json"]` ⇒ 发布成功（已实测两次） |
| 10 | 🔧 **修掉几处会「说假话」/误报的分支** | 积压归零时不再说「运行时已恢复」（只说「积压已清空」）；积压**变多**时不再说「已恢复消费」（只有**变少**才算恢复）；lsof 报错时判「判据失效」而不是「运行时没起」；自检失败同一个错误只说一次；代理对区间判错（U+D000–D7FF 是合法字符，不是代理） |
| 11 | 🔴 **修掉看板取数层的一个拼写错误** | `doBuildSnapshot()` 里把 `platform` 写成 `platformRes` ⇒ `ReferenceError` ⇒ **`buildSnapshot()` 整个抛异常** ⇒ 额度、决策、重置卡、积分**全拿不到** ⇒ **整个面板是坏的**。这个 typo 是 **v1.2 引入的**（`fd98d98`），活了 4 个版本才被发现。实测：修前 MCP 工具返回 `quotaError: "platformRes is not defined"`，修后 `quotaError: null`，额度/决策/卡片全部正常 |
| 12 | 🔴 **修掉 MCP 工具永远返回 `{}`** | `deps.status()` 是 `async`，handler 里**漏了 `await`** ⇒ 拿到的是 Promise ⇒ `JSON.stringify(Promise)` = 字符串 `"{}"` ⇒ `watchdog_status` 什么也看不到（`watchdog_wake` / `watchdog_decide` 都有 await，只有它漏了） |
| 13 | ✅ **补两个回归测试**（这两个 bug 都是零覆盖才活下来的） | `mcp.test.mjs`（18 项）：MCP 协议、三个工具、**async 的 status 必须真的 await**、未知工具要报明确的错。`server-lint.test.mjs`（5 项）：静态守卫「用了但没声明的标识符」—— 把 typo 放回去实测能抓住（5/5 → 3/5） |
| 14 | 🔴 **修掉「3 次封顶」被架空的回归**（二轮审计） | v1.6 把 acked 改成永久之后，「按 acked 清理失败计数」的含义从「刚刚被救过」变成了「**曾经**被救过」⇒ 被救过的 bot 的失败计数被清掉 ⇒ **3 次封顶彻底失效** ⇒ 该请求永远「活」⇒ **每 60 秒重投一次**（每轮 ≈47k–93k tokens）。⇒ 失败计数**只在成功时归零**；失败路径的清理**只按超龄** |
| 15 | 🔴 **修掉解析器 panic**（二轮审计） | `parse_lenient_json` 按**字节**切 `&str` ⇒ 落在多字节字符中间会 **panic** ⇒ 守护进程整个崩掉（launchd 重启后同样崩 ⇒ 崩溃循环）。触发条件：文件严格解析失败 **且** 含 `\u` + 非 ASCII 字符（生产 `state.json` 里有 89.5 万个非 ASCII 字符）。⇒ 改成**纯字节**解析；并给守护进程加 `catch_unwind` 兜底（panic 只影响 bot 看门狗，不带走额度判定与 Goal 唤醒） |
| 16 | 🟠 **修掉「没有回合记录 ⇒ 永远不被救」的死代码**（二轮审计） | 判定层写 `None => !is_idle(...)`，而紧接着调用方又有一道 `if !is_idle(...) { continue; }` —— 两处**互为否定** ⇒ 这类会话永远走不到救援。⇒ 改为交给下游的 `classify_error` + `is_idle` 双重判定 |
| 17 | 🟠 **「僵尸回合」不再被永远忽略**（二轮审计） | `accepted` 状态既可能是「正在跑」，也可能是**宿主崩溃留下的僵尸**（实测生产库有 3 条，最老的 2 小时前还停在 `accepted`，会话却带着可自愈错误码）⇒ 新判据把它当「正在跑」⇒ 该救却永远不被救。⇒ 停在 `accepted` 超过 **1 小时** ⇒ 视为卡死，进入救援 |
| 18 | 🟡 **lsof 解析改用字段模式** | 默认模式按空白切列 ⇒ **路径含空格就被切碎** ⇒ `ends_with("/package")` 恒假 ⇒ 每轮都误判「运行时没起」。⇒ 改用 `lsof -F`（`n` 字段整行保留空格） |

测试：Rust **75/75**、Python **20/20**、MiniApp **144/144**
（bot_wakeup 44 + config-fidelity 15 + decision 58 + mcp 18 + server-lint 5 + retry.behavior 4）。

### v1.5 更新内容

**架构本身没变**（Rust 判定 + 记账，MiniApp 投递），这一版全是**加固**：
把几个「看起来能跑、实际会静默失效」的地方修掉。

| # | 变更 | 后果 |
| --- | --- | --- |
| 1 | **唤醒会话复用**：首次创建的唤醒会话 id 存进 `bot-wake-host-session.json`，后续 `agent.run({text, sessionId})` 复用它 | 每次唤醒不再冷启动（实测冷启动 ≈ 93k tokens）⇒ **2026-10-20 02:39 一轮唤醒 6 个 bot，新建会话数 = 0**（此前每个 bot 新建一个） |
| 2 | **请求判据换轨**：从「30 分钟 TTL」改成「**最多试 3 次**（`MAX_ATTEMPTS`）+ **24 小时兜底**（`MAX_REQUEST_AGE_MS`）」 | 面板关着时没人消费 ⇒ 30 分钟后请求全过期 ⇒ 面板再打开时 bot 又挂了却**没有请求可发**（实测：面板关 70 秒后 7 条请求全未被消费） |
| 3 | **acked 不再是永久黑名单**：`bot-revive-acked.json` 改成带时间戳，只抑制 **30 分钟**（`ACK_SUPPRESS_WINDOW_MS`） | 6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），却因 acked 里有过它们而**永远不再被叫醒**。旧格式（纯字符串数组）仍能读，视为「很久以前」不抑制 |
| 4 | **修掉并发写的三个坑**（临时文件名撞车 / lost update / 清理条件） | 详见「踩过的坑」#15 —— 其中「清理条件」那条会让 **3 次封顶被架空，变成无限重试** |
| 5 | **瞬时错误不重发** | `BUSY` / `session_busy`（该会话已有回合在飞）**可重试，但不是重发**；`AGENT_OUTCOME_UNKNOWN`（回合可能已经跑了）直接 ack ⇒ 否则 bot 会收到**两条**重复的「请从中断处继续」 |
| 6 | **客户端 401 自愈** | 面板「实时连接中断」不再是死循环：连续 **3** 次 401 ⇒ `window.location.reload()`，重载次数用 sessionStorage 记、**上限 5 次**，成功一次就清零 |
| 7 | **新增 local MCP 端点** | `miniapp/node/mcp.mjs` + `servers.mcp.json` + manifest 的 `mcpEndpoints`。三个工具：`watchdog_status` / `watchdog_wake` / `watchdog_decide`。⚠️ **刻意不暴露「扣卡」** |
| 8 | **`50111` 纳入可自愈** | 短时限流（HTTP 429）此前落进「未知错误码 ⇒ 不自动重试」，而 429 本身在 Host 的可重试白名单里 |
| 9 | **新增 `check-test-parity.sh`** | 工作区源码与插件包**各有一份测试**、路径布局还不同 ⇒ 改一边忘另一边，插件包跑的就是旧测试 |

测试：Rust **51/51**、Python **20/20**、MiniApp **112/112**
（bot_wakeup 35 + config-fidelity 15 + decision 58 + retry.behavior 4）。

### v1.2 更新内容

| # | 变更 | 后果 |
| --- | --- | --- |
| 1 | `cards.items` 是给页面看的**脱敏视图**（无 `card_id`），却拿它判断「卡还在不在列表里」 | 该判断恒为 false ⇒ **扣卡每次都被跳过**，`reset_card/use` 从未被调用。改为新增 `cards.raw`（带 `card_id`）供写路径使用 |
| 2 | 积分接口失败被 `.catch(() => null)` 吞掉 ⇒ 等同于「没有积分」 | 结论从「留卡」翻转成「用卡」⇒ **网络故障导致误扣真卡**。新增 `creditDataComplete` 闸门：数据不完整 ⇒ 本轮**不做任何写操作** |
| 3 | 守护进程侧同样问题：积分读取失败只记日志 | 同样会误扣。`earliest_credit()` 现在返回「数据是否完整」，不完整 ⇒ `wait` |
| 4 | **新增：可重试错误无限重试** | 429 / 500 / 502 / 503 / 504 与网络错误（连接重置、超时、DNS）一律重试到成功；退避指数增长、**封顶 60 秒**、带抖动；`SIGINT`/`SIGTERM` 可立即中断。401/403 重取 token 一次，其余 4xx 与业务错误快速失败。扣卡也走重试（`out_biz_no` 幂等键保证不会重复扣） |

测试：Rust **48/48**、Python **20/20**、MiniApp 决策 **58/58**、
配置保真 **15/15**、唤醒指令 **9/9**、三方 parity **12** 例。

无人值守地盯住 Token Plan 额度：**接近耗尽时**按「先过期先用」自动使用一张重置卡；
额度打满时**自动暂停**所有进行中的目标，额度回落后**自动恢复并叫醒**它们。

通过 macOS `launchd` 常驻，与 MiniMax Code 桌面端完全独立——关掉 MCode、退出登录、
甚至你没开电脑，它都在跑。

---

## 目录

- [现在有两套，各自独立](#现在有两套各自独立)
- [共享配置](#共享配置)
- [完整闭环](#完整闭环一条消息怎么走完全程)
- [为什么彻底去掉定时任务](#为什么彻底去掉定时任务cron)
- [Bot 看门狗](#bot-看门狗挂掉的-bot-自己回来)
- [local MCP 端点](#local-mcp-端点)
- [客户端 401 自愈](#客户端-401-自愈)
- [唤醒链路自检](#唤醒链路自检宿主重启后没人投递时必须说话)
- [踩过的坑](#踩过的坑二十个都写在这里了)
- [扣卡规则](#扣卡规则与-mini-app蹬王之王逐条一致)
- [管理命令](#管理命令)
- [测试](#一致性是测出来的不是声称的)
- [前提与边界](#前提与边界)

---

## 现在有两套，各自独立

| | 后台服务（本目录） | Mini App「蹬王之王」 |
| --- | --- | --- |
| 运行方式 | launchd 系统服务 | MCode 运行时内的 Node 进程 |
| 关掉 MCode | **照常运行** | 停止 |
| 扣卡策略 | **与 Mini App 完全一致** | 同左 |
| 作用 | 真正的无人值守执行 | 实时看板 + 手动触发 + **唤醒投递方** |

两者都用服务端的 `out_biz_no` 幂等键，**不会重复扣同一张卡**。

> ⚠️ **关掉面板会怎样（v1.4 起变了，v1.5 又改了请求时效）**
> 守护进程照常判定、照常记账、照常写请求文件 —— 扣卡与暂停**不受影响**。
> 但**唤醒不会发生**：投递方是 MiniApp 进程，面板关着它就不在。
> 面板再打开时，积压的请求会被补上。
>
> v1.4 时请求有 **30 分钟**时效上限 ⇒ 面板关久了请求全过期 ⇒ 打开时反而**没有请求可发**。
> v1.5 改成「**最多试 3 次 + 24 小时兜底**」⇒ 面板关多久都不影响（请求一直在那儿等着），
> 代价是叫不醒的 bot 最多试 3 轮。见 [唤醒请求的生命周期](#唤醒请求的生命周期代码常量不是配置项)。

**代码位置**

| 组件 | 路径 |
| --- | --- |
| Rust 守护进程（主力） | `daemon/rust/src/` |
| Python 决策参考实现 | `daemon/reset_card_autouse.py` |
| 三方一致性校验器 | `daemon/verify-parity.mjs` + `daemon/check-parity.sh` |
| Mini App（看板 + 唤醒投递方 + MCP 端点） | `miniapp/`（**本包内**，自包含，不依赖仓库里的其他目录） |
| Bot 唤醒逻辑 | `miniapp/node/bot_wakeup.mjs` |
| local MCP 端点 | `miniapp/node/mcp.mjs` + 根目录 `servers.mcp.json` |
| 测试一致性校验 | 根目录 `check-test-parity.sh`（工作区 ↔ 插件包） |

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
  "botGuard": {
    "enabled": true,
    "idleMs": 90000,
    "backoffBaseMs": 5000,
    "backoffCapMs": 60000
  }
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
| `botGuard.enabled` | Bot 看门狗总开关 |
| `botGuard.idleMs` | 多久没动静才算「挂掉」（默认 90 秒）。**必须同时确认没有新活动**才能救 |
| `botGuard.backoffBaseMs` / `.backoffCapMs` | 退避基数与封顶（默认 5 秒 / 60 秒） |

后台服务**每轮重读**该文件，所以改完约 1 分钟内生效，不必重启。

Mini App 页面上有完整的可视化入口（在「自动化阈值」卡片）：
扣卡触发线、暂停目标触发线、额度紧张/不足分界、刷新前暂停扣卡分钟数 —— 全部可直接改，
并配一条**断轴刻度**把 0–黄档 压缩成波浪带、只展开真正有阈值分布的区间，
外加图例与冲突提示（比如「扣卡线 90% 不高于红色档 95%，红灯将永远不会亮」）。

字段非法时**整体回退默认并打出告警**，绝不静默纠正——静默纠正会让人以为设的值生效了。
（Rust 侧开了 `deny_unknown_fields`，键名写错会直接报错而不是悄悄降级。）

> ⚠️ **Mini App 保存配置时必须原样保留 `botGuard`**（v1.4 沿用 v1.1 的教训）。
> 它的 `normalizeConfig()` 是**白名单式重建**，漏掉这个字段 ⇒
> 用户在页面上改任何一项设置，都会把看门狗配置从配置里抹掉 ⇒
> 守护进程读到 `enabled=false` ⇒ **看门狗静默失效**，且不报任何错。
>
> v1.4 删字段时踩了同一类坑：删 `resumeCronId` 必须**三处同步删**
> （Rust `config.rs`、MiniApp `normalizeConfig`、真实 `config.json`），
> 少一处就会出现「两套配置对不上」。

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
| HTTP 500、网络切换、集群过载、TPM 限流、短时限流(50111) | 退避等待后**复用原会话**重新启用 |
| Token Plan 到顶(42212)、积分耗尽(50110)、登录失效(50112) | **不重试**，明确告诉你缺什么 |
| bot 本身正常、或只是残留着上一轮的过期错误码 | 不打扰它 |

完整取证见 [`docs/BOT_DIAGNOSIS.md`](BOT_DIAGNOSIS.md)。

### 错误分类：哪些能自己好，哪些要你出手

分类逻辑在 `daemon/rust/src/bot_guard.rs` 的 `classify_error()`：

| 错误码 / 特征 | 含义 | 判定 | 看门狗怎么做 |
| --- | --- | --- | --- |
| **42212** | `USAGE_LIMIT_EXCEEDED` — Token Plan 用量到顶 | 🔴 需人工 | 不重试，提示「升级套餐或购买积分并开启积分自动消耗」 |
| **50110** | `LLM_CREDITS_EXHAUSTED` — 积分耗尽 | 🔴 需人工 | 不重试，提示充值 |
| **50112** | `LLM_AUTH_ERROR` — 登录态失效 | 🔴 需人工 | 不重试，提示重新登录 |
| 文本含 `Cron definition was deleted` | 那个定时任务被删了 | 🔴 需人工 | 不重试，提示「需要重新安排」 |
| **50113** | `LLM_UPSTREAM_ERROR` — 通用 4xx/5xx 兜底（HTTP 500 落这里） | ✅ 可自愈 | 退避后自动唤醒 |
| **50111** | `LLM_RATE_LIMITED` — 短时限流（HTTP 429） | ✅ 可自愈（**v1.5 补上**） | 退避后自动唤醒。此前它落进「未知错误码 ⇒ 不自动重试」，而 429 本身在 Host 的可重试白名单里 |
| **50150** | `LLM_TPM_RATE_LIMITED` — TPM/RPM 限流 | ✅ 可自愈 | 退避后自动唤醒 |
| **50151** | `LLM_CLUSTER_OVERLOADED` — 集群过载（HTTP 529） | ✅ 可自愈 | 退避后自动唤醒 |
| 无错误码 + 文本含额度/鉴权信号 | 「用量上限」「未授权」等 | 🔴 需人工 | 不重试 |
| 无错误码 + 文本含网络/服务端临时信号 | `net::ERR_NETWORK_CHANGED`、`ECONNRESET`、超时、`service unavailable` 等 | ✅ 可自愈 | 退避后自动唤醒 |
| 无错误码 + 完全认不出的文本 | — | ✅ 可自愈（倾向） | 宁可多试一次，也不要让 bot 永久瘫着 |
| 其它有结构的错误码 | — | 🔴 需人工 | 「未知错误码，重试可能无用」 |

> ⚠️ **「读不到」≠「没有」**：44 个 bot 会话里有 29 个是「无错误码、无错误消息」——
> 它们只是闲置很久，并不是挂掉。若把这类判成可自愈，看门狗会去「重新启用」一批没坏的 bot。
>
> ⚠️ **错误码可能是陈旧值**：运行时不清 `error_code`。2026-10-19 实测踩到：
> 那个会话当时正在跑一个 20 多分钟的**正常长回合**，而它的 `error_code` 仍是 50113。
> ⇒ 动手前必须同时确认「最近没有新活动」（`botGuard.idleMs`，默认 90 秒）。
> 判据是 `local_runtime_message_rows` 里该会话的 `MAX(created_at_ms)`。

### 唤醒请求的生命周期（代码常量，不是配置项）

这三个常量**不在 `config.json` 里** —— 它们是「唤醒通道」的内部判据，
Rust 守护进程与 Node 投递端**各有一份同值实现**（`bot_guard.rs` 与 `bot_wakeup.mjs`）。

| 常量 | 值 | 作用 |
| --- | --- | --- |
| `MAX_WAKE_ATTEMPTS` / `MAX_ATTEMPTS` | **3** | 同一个 bot 最多试 3 次，超过就作废（**按 bot 键计，跨实例有效**） |
| `MAX_WAKE_REQUEST_AGE_MS` / `MAX_REQUEST_AGE_MS` | **24 小时** | **只是兜底**，防僵尸请求永久堆积 |
| `ACK_SUPPRESS_WINDOW_MS` | **30 分钟** | 「同一个 bot 多久可以再救一次」（**只管写请求，不管投递**） |

**为什么主判据是次数而不是时间**：
纯时间 TTL 会让「面板关着」的场景彻底失效 ——
面板关掉 ⇒ 没人消费请求 ⇒ 30 分钟后请求全部过期作废 ⇒
等面板再打开时，那些 bot 又已经挂了一段时间，却**再也没有请求可发**。
实测：面板关掉 70 秒后，7 条请求全部未被消费。

真正的风险不是「放太久」，而是「**反复试同一个叫不醒的 bot**」
（每次尝试都要烧一个 agent 回合）⇒ 按次数封顶。

⇒ **教训**：**时效不是「多久算过期」，是「多久还值得试」。**
只留一个时间上限的队列，在「消费者暂时不在」时会**静默丢单**。

### 请求 id 按「实例」唯一，acked 永久化（v1.6）

请求 id 的形状：

```
bot-revive:{botId}:{sessionId}          ← bot 键（尝试次数就记在它名下）
bot-revive:{botId}:{sessionId}:{实例序号} ← 请求 id（每次写入都是新实例）
```

| 用途 | 用哪个键 | 规则 |
| --- | --- | --- |
| 「这条投递过了吗」 | **请求 id**（含实例序号） | **永久** —— 投递过的就是投递过了，绝不重投 |
| 「这个 bot 试了几次」 | **bot 键** | 3 次封顶，**跨实例有效**（否则换实例就重置 ⇒ 封顶被架空） |
| 「这个 bot 多久可以再救」 | **bot 键** + ack 时间戳 | 30 分钟频率限制（守护进程写请求时判断） |

**为什么必须拆开**（2026-10-20 独立审计 + 实测）：
原来 acked 的 30 分钟窗口**同时**管「投递过了吗」与「多久能再救」两件事。
⇒ 窗口一过，**已经投递过的请求又变回「未处理」**
⇒ 投递端**每 30 分钟把同一条请求重投一次**（每回合 ≈47k tokens），
而且会打断已经恢复正常的 bot。
⇒ 拆开后：投递端只认「永久 ack」；频率限制由守护进程自己在写请求时管。

⚠️ **实例 id 是必需的**：ack 永久化之后，如果请求 id 还是「每个 bot 同一个」，
那这个 bot 一旦被救过就**永远**投不出去（实测踩到：两个真坏的 bot 30 分钟内一次没被叫醒）。
⇒ 每次写入都带一个实例序号 ⇒ 新实例不受旧 ack 影响。

### 坏没坏，看「最新回合」而不是会话上的 error_code（v1.6）

会话上的 `error_code` 是**陈旧值**（运行时跑完不清它）。
判「这个 bot 现在坏没坏」的**权威依据**是 `local_runtime_turn_ingress` 里**最新回合**的状态：

| 最新回合 | 含义 | 看门狗怎么做 |
| --- | --- | --- |
| `failed` | 真失败 | **该救** |
| `aborted` | 被中断 | **该救** |
| `completed` | 上一轮成功 | **别碰**（错误码是更早那次失败留下的陈值） |
| `accepted` | 正在跑 | **别碰**（同时覆盖了「20 多分钟的长回合」） |
| 没有回合记录 | 查不到 | 回退到老判据（`error_code` + 闲置） |

**实测（2026-10-20 15:39）**：老判据写出 **11 条**唤醒请求，其中 **8 条是误判** ——
那 8 个 bot 的最新回合是 `completed`，它们好着呢。
修后同一批变成「可自愈 0 个、投递 0 个」；随后只唤醒了**真正失败的那 2 个**。

⇒ **教训**：`error_code` 这类「粘在对象上的状态」会被后续的成功操作留成陈值。
**判当前状态要看最近一次动作的结果**，不要看对象身上挂着的标签。

### acked 不再是永久黑名单

`bot-revive-acked.json` 现在带时间戳：

```json
{
  "acked": [
    { "id": "bot-revive:bot_xxx:mvs_yyy", "atMs": 1792435772291 }
  ]
}
```

只抑制 **30 分钟**（`ACK_SUPPRESS_WINDOW_MS`），超过窗口可以再救。

**为什么**：原来 acked 是个纯 id 列表 ⇒「救过一次」变成**永久黑名单**。
实测：6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），
却因为 acked 里有过它们而**永远不再被叫醒**。

旧格式（纯字符串数组）仍能读 —— 视为「很久以前」，**不抑制**。

> ⚠️ **v1.6 改了这条**：acked 现在**永久**（不再按 30 分钟窗口过期），
> 「多久可以再救同一个 bot」改由守护进程自己的 30 分钟频率限制管。
> 原因见[请求 id 按「实例」唯一，acked 永久化](#请求-id-按实例唯一acked-永久化v16)。

### 唤醒会话复用（省 token）

以前每次 `agent.run` 都不带 `sessionId` ⇒ 每次**新建一个会话** ⇒ 每次都是冷启动。
现在把首次创建的会话 id 存进
`~/.config/mcode-reset-card/bot-wake-host-session.json`，
后续 `agent.run({text, sessionId})` **复用它**。

| | 冷启动（每次新建会话） | 复用宿主会话 |
| --- | --- | --- |
| 单次唤醒 token | ≈ **93k** | 增量（系统提示与工具定义进上下文缓存） |
| 一轮唤醒**新建**的会话数 | 每个 bot 一个 | **0**（全部复用同一个宿主会话） |

**实测收益**：2026-10-20 02:39 一轮唤醒 6 个 bot ⇒ **新建的唤醒会话数为 0**。
（运行时库里那一小时只有 **1** 条 `purpose='miniapp:expiry-guardian'` 的会话，它承载了 14 个 turn。）

⚠️ 复用失败时会**自动退回新建**（会话被归档 / 凭据过期 / 会话不存在），
但**只有** `BUSY` / `AGENT_OUTCOME_UNKNOWN` 除外 —— 见下面的「瞬时错误不重发」。

### 怎么把 bot 叫醒：两段跳

看门狗跑在 **Host 之外**（launchd 常驻、无 token、无人值守），
够不到 minimax-bot 的 MCP 端点 —— 那个端口由 Host 分配，
且 Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到。
2026-10-19 / 2026-10-20 实测了五条通道：

| 通道 | 能续别的 Mini App 拥有的会话吗 | 结论 |
| --- | --- | --- |
| 守护进程**裸写**队列项 | ❌ | 隔离实验：往 bot 会话写一条队列项，3 分钟后仍是 `queued`、未 claim、无 turn、无消息。**Host 的队列轮询器只认经过 steer 的行** |
| `context.agent.run({ sessionId })` | ❌ | `assertSessionOwnership`：`owner !== caller.pluginId` ⇒ 抛 `SESSION_NOT_FOUND`。实测 45 个 bot 会话的 `purpose` 全是 `miniapp:minimax-bot` ⇒ 每 30 秒 6 条全拒 |
| 直连对方 MCP 端点 | ❌ | Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到 |
| 守护进程写**目标续跑队列项**（带正确形状） | ✅ | 但只对**目标会话**有效，对 bot 会话不成立（见上一行） |
| **起一个 Agent，让它调 `session send`** | ✅ | 2026-10-19 实测：唤起挂着 42212 的 bot，13 秒内回复 |

⇒ **看门狗负责判定**（谁挂了、该不该救）并写请求；
**投递由蹬王之王自己的 MiniApp 负责** —— 它起一个自己的 agent 回合，
那个回合里的 agent 用 **mavis 工具的 `session send`** 把续跑消息送进目标 bot 会话。

#### 为什么 `session send` 能越权

`local-runtime/dist/runtime/mavis-session-send.js` 的准入注释原文：

> Reachability is the whole access model: the caller identity comes from the
> runtime context, the target must be resolvable through the canonical Session
> query, and everything after that is TurnSystem admission. **There is no
> topology check** — a task child, a sibling, a root Session and the caller's
> own Session all take the same path.

⇒ `session send` **不做 owner 检查** ⇒ 任何拿到 mavis 工具的会话都能唤醒任意 bot 会话。
而 MiniApp 的 `agent.run`（**首次**不带 `sessionId`）建的会话 `agent_name = 'mavis'`
⇒ 那个 agent 天然带桌面端 mavis 工具。

> 💡 **这正好绕开了 `assertSessionOwnership`**：
> 那个检查管的是「Mini App 能不能续**自己名下**的会话」，
> 而 `session send` 走的是**另一条路**（工具调用，不是 MiniApp 会话续跑），
> 它的准入条件是「目标可达即可」。

### 瞬时错误不重发（v1.5）

唤醒失败分两种，处理方式**相反**：

| 错误 | 含义 | 处理 | 为什么 |
| --- | --- | --- | --- |
| `BUSY` / `session_busy` | 该会话**已有回合在飞** | **不是重发** —— 抛出去，交给下一轮 | 重发 = 往同一个会话里塞第二条续跑消息 |
| `AGENT_OUTCOME_UNKNOWN` | 回合**可能已经跑了**（Host 标 never retryable） | **直接 ack，不重投** | 若只 bump 尝试次数而不 ack ⇒ 60 秒后下一轮会再投同一条 ⇒ bot 收到**两条**重复的「请从中断处继续」 |

⚠️ 注意这两类**都不能**触发「宿主会话失效 ⇒ 退回新建会话」那条路：
`BUSY` 说明会话是好的（只是忙），`AGENT_OUTCOME_UNKNOWN` 说明回合可能已经执行过了。

⇒ **教训**：「失败」不等于「没送达」。
把「不知道成没成」当成「没成」再投一次，是**重复投递**的经典来源。

---

## local MCP 端点

蹬王之王对外暴露一个 **local MCP 端点**，别的插件可以在自己的回合里直接问它。

| 项 | 值 |
| --- | --- |
| 实现 | `miniapp/node/mcp.mjs` |
| 声明 | `miniapp/miniapp.json` 的 `mcpEndpoints` + 根目录 `servers.mcp.json` |
| 路径 | `/mcp/expiry-guardian`（**并兼容** `/mcp/minimax-bot`） |
| 传输 | streamable-http |

三个工具：

| 工具 | 作用 |
| --- | --- |
| `watchdog_status` | 查当前额度、重置卡/积分到期时间，以及**待唤醒队列**（含每个请求已试了几次） |
| `watchdog_wake` | **立即**跑一轮 Bot 唤醒（不等 60 秒巡检） |
| `watchdog_decide` | 立即判定一次：现在该不该用卡、用哪张、为什么（**只判定不扣卡**） |

⚠️ **刻意不暴露「扣卡」**：扣卡是**真实写操作**（少一张卡），
让别的插件随手就能扣太危险 ⇒ 要扣走面板按钮。

⚠️ **声明了就得真实现**：Host 发布时会用 `initialize` + `tools/list` **探活**，
探不通就**发布失败**。而且 Host 内存里的「已发布快照」是**粘的** ——
一旦某个版本的 manifest 声明过 `mcpEndpoints`，
后续版本**必须继续声明同样的端点**，否则 `miniapp publish` 会被
`MCP_PUBLICATION_INVALID` 挡下（`used.size !== expected.size`）。
⇒ 两条路径（`/mcp/expiry-guardian` 与 `/mcp/minimax-bot`）挂同一个 handler。

---

## 客户端 401 自愈

面板显示「**实时连接中断**」且一直好不了，多半是**凭据失效**（401）而不是服务挂了。

**根因**（2026-10-20 实测）：
MiniApp 被 Host 重启后，它的 loopback 凭据会**换新**；
而 Electron 主进程是按 **webContentsId** 注入凭据的（`miniapp-loopback-auth.js`），
页面里的 fetch 拿不到新 token ⇒ 一直 401。
更麻烦的是 `syncMiniAppLoopbackCredential` **只在 origin 不变时才同步**
（重启后端口变了 ⇒ origin 变了 ⇒ 跳过）⇒ **光重试 fetch 无解**。

⇒ 唯一能拿到新凭据的途径是**重新导航**（Host 在 `refreshMiniAppTab` /
`openMiniAppTabOnce` 里同步 token）。

客户端现在的做法：

| 参数 | 值 | 作用 |
| --- | --- | --- |
| `STALE_RELOAD_THRESHOLD` | **3** | 连续 3 次 401 ⇒ `window.location.reload()` |
| `MAX_RELOADS` | **5** | 重载次数上限（sessionStorage 记，**跨页面**） |

- 没有上限会变成「重载后仍 401 ⇒ **无限重载**」⇒ 页面反复闪，用户根本没法用。
- **成功一次就清零**（`staleCount` 与 sessionStorage 里的计数都清）——
  原来只在「重连成功」时清、`load()` 成功不清 ⇒
  「2 次 401 + 1 次成功 + 1 次 401」就会触发一次多余的整页重载。
- 超过上限 ⇒ 不再重载，改为提示「请在 MiniMax Code 里重新打开「蹬王之王」面板」。

---

## 唤醒链路自检：宿主重启后没人投递时必须说话

唤醒要「两段跳」才能落地（见[怎么把 bot 叫醒](#怎么把-bot-叫醒两段跳)）：
**Rust 守护进程只负责「判定 + 记账」**，真正把消息送进目标 bot 原会话的是
**蹬王之王自己的 MiniApp**（它有 `agentAccess:["session.run"]`）。

而 MiniApp 是 **on-demand** 的：**宿主重启后不会自动拉起它**（见
[MiniApp 生命周期](#miniapp-生命周期没有常驻模式)）。
⇒ 那段时间里「判定」照常跑、「投递」却没人消费。

**这正是最坏的一类故障**：守护进程进程在、日志干净、面板看着正常，
**实际一个 bot 都没被叫醒**。所以 v1.6 加了自检（`daemon/rust/src/bot_bootstrap.rs`）。

### 它能发现两种空转

| 状态 | 判据 | 会说什么 |
| --- | --- | --- |
| **运行时没起** | 有待唤醒请求，但**没有活进程**的工作目录是本插件的运行时包 | 「⚠️ N 个待唤醒请求投不出去 —— 蹬王之王面板的 MiniApp 运行时没在跑。请打开一次「蹬王之王」面板」 |
| **投递空转** | 运行时**在跑**，但积压条数**连续 15 轮（约 15 分钟）没减少** | 「⚠️ 运行时在跑，但 N 个待唤醒请求已连续 N 轮巡检没被消费 —— 唤醒循环可能已经空转」 |

两种都**只告警一次**（状态不变不重复刷屏），状态变化时再补一句「已恢复」。

⚠️ **告警文案严格对应事实**：
积压归零时只说「积压已清空」，**不说**「运行时已恢复」——
积压归零只说明「没有待办」，不代表运行时起来了。

### 运行时存活的判据（为什么不用端口）

```
有活进程  且  cwd = <...>/mavis-miniapp-XXXX/package  且  该包 package.json 的 name == "expiry-guardian"
```

- **不用端口**：55932 是宿主的默认分配值，宿主改规则就误判；
  而且那个端口属于**宿主进程**（MiniApp 运行时是它 fork 出来的子进程，进程名继承为 `MiniMax`），
  端口活着 ≠ **是蹬王之王**在跑。
- **不用「目录存在」**：实测同时存在 3 个 `mavis-miniapp-*` 目录，其中只有 1 个对应活进程
  ⇒ 停掉的运行时目录不会被清掉，光看目录会以为「在跑」。

### 为什么这里不做「自动修复」

「从宿主外把 MiniApp 拉起来」这条路，**逐条实测全部堵死**：

| 通道 | 结果 | 原因 |
| --- | --- | --- |
| 往 `local_runtime_queue_items` 写消息 | ❌ | 队列轮询器只在会话有**活跃 dispatch** 时取项；**连正在跑的会话都不取**（实测连投 idle 与 started 两种会话，3 分钟内都是 `queued` + `claim_id` 为空）。`source='communication'` + `model` + `deliveryAttempts` 的形状也不行 |
| MiniApp 的 HTTP（55932） | ❌ | `host-runner.js` 的 `installMiniAppLoopbackGuard` 要 `Host: 127.0.0.1:<port>` + `x-mavis-miniapp-token`；token **只存在于宿主进程**，不传给 MiniApp 子进程 |
| 宿主 HTTP（58925） | ❌ | 同上（per-run 凭据）；且宿主只有 3 个路由（`/mavis/api/preferences/starred-agents`、`/mavis/api/computer-use/state`、`/mavis/api/computer-use/abort`），没有起回合/拉 MiniApp 的接口 |
| `minimax-cn://chat?message=` | ❌ | `chat` handler 由**渲染进程**注册；实测 deeplink 触发不了回合 |
| `minimax-cn://navigate?url=` | ❌ | 只导航渲染进程页面，不启动 MiniApp 运行时（还会 `mainWindow.focus()` 抢焦点） |
| 官方 minimax-bot 插件的 MCP | ❌ | 它自己也是 MiniApp ⇒ 同样 on-demand；官方插件不可改版（`OFFICIAL_PLUGIN_READ_ONLY`） |

⇒ **从宿主外无法自动拉起 MiniApp**。这不是本插件的设计取舍，是宿主的边界。

**唯一确认可行的自举路径**（供未来参考）：
宿主在准备**任何一个回合**时，只要消息里出现 `@<pluginId>`，就会 `ensureStarted`
把那个 MiniApp 拉起来（`turn-preparation.ts` 的 `prepareReferencedMiniApps`），
前提是该插件**声明了 MCP 端点**（`referenced.filter(c => c.mcpEndpoints.length > 0)`）。
实测有效：2026-10-20 14:37 往一个活跃会话发了一条 `@expiry-guardian 看门狗探针`，
宿主随即拉起了蹬王之王的运行时。
⇒ 只要**宿主内有人发一条带 `@expiry-guardian` 的消息**，自举就成立 ——
但这需要宿主内的 `session send`（进程内 adapter），**宿主外拿不到**。

⇒ 所以本包的做法是：**检测 + 如实告警 + 面板一恢复就自动补上**。
实测 Cross-Asset / Chief 的积压请求都是在面板恢复后被自动补上的。

---

## 为什么彻底去掉定时任务（cron）

v1.3 的架构是「**守护进程判定 + 定时任务值守投递**」。
2026-10-20 全部删掉，理由三条，**每条都有实测**：

| # | 理由 | 证据 |
| --- | --- | --- |
| 1 | **定时任务的目标会话会被运行时自动归档** ⇒ 后续每次触发都报 `CRON_SESSION_TARGET_ARCHIVED` | 实测 12 次触发 = 4 成功 + 8 次归档失败。`session_kind='cron'` 的会话跑完就被发 `POST /session/:id/archive`；`kind=conversation` / `task` 的不受影响 |
| 2 | **定时任务只能跑在 Host 内的某个会话里，而「固定值守会话」不可靠** | 改成指向长期会话虽然 6/6 delivered，但那会把值守回合混进用户会话历史 |
| 3 | **每轮烧约 47k tokens**（46k 是缓存读取），而它只需要做一件事：把一条续跑消息送进目标会话 | 按 5 分钟常驻 ≈ 288 轮/天 ≈ 13M tokens/天 —— 那会吃掉正要保护的那块额度 |

⇒ **教训（换了个形式仍然成立）**：
**任何「由专用宿主承载的值守」都不可靠** —— 宿主会被归档、会被限流、会被回收。
把值守放在**用户自己的目标路径**上（这里是：一个由 MiniApp 自己起的 agent 回合），
比放在一个专职值守会话上更稳。

### 去掉之后的新架构

```
┌──────────────────────────────────────────────────────────────────────┐
│ Host 之外：Rust 守护进程（launchd 常驻 · 无 token · 无人值守）        │
│                                                                      │
│   职责只有两件：判定 + 记账                                          │
│     · 判定额度、决定是否暂停/恢复目标、决定哪个 bot 挂了              │
│     · 把要唤醒的 bot 写进 bot-revive-requests.json                   │
│     · 把要恢复的目标写进 goal-resume-requests.json                   │
│   · 目标恢复：自己写「目标续跑队列项」（照抄运行时形状）⇒ 目标能自己醒 │
│   · 也读 bot-revive-attempts.json（尝试次数）⇒ 不给叫不醒的 bot 无限重试│
│   ❌ 不投递 bot 唤醒 ❌ 不碰 bot 会话的队列 ❌ 不碰任何定时任务          │
└──────────────────────────────────────────────────────────────────────┘
                                  │  文件交接（不需要网络、不需要凭据）
                                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Host 之内：蹬王之王自己的 MiniApp（Node 进程）                        │
│                                                                      │
│   ① 读 bot-revive-requests.json                                      │
│      跳过：30 分钟内已 ack / 已试满 3 次 / 超过 24 小时                │
│   ② agent.run 起一个回合（agent_name = 'mavis' ⇒ 天然带 mavis 工具） │
│      ⚠️ 复用 bot-wake-host-session.json 里的宿主会话（省 93k 冷启动） │
│   ③ 回合里的 agent 用 mavis 工具的 session send 送进目标 bot 会话    │
│   ④ 成功 ⇒ 写 bot-revive-acked.json（带时间戳，只抑制 30 分钟）       │
│      失败 ⇒ 写 bot-revive-attempts.json（+1，满 3 次作废）            │
│   巡检间隔 60 秒                                                     │
└──────────────────────────────────────────────────────────────────────┘
```

> ⚠️ **Rust 与 Node 会并发写同一批文件**（各写各的，但文件相邻）。
> 临时文件名必须带 pid，尝试次数必须**单独存一个文件** ——
> 否则会互相覆盖对方的半成品，或把守护进程刚写的条目抹掉（lost update）。
> 详见「踩过的坑」#15。

### 代价：每个被唤醒的 bot 要烧一个 agent 回合

实测（2026-10-20 00:49，14 个唤醒回合的 `local_runtime_token_usage`）：

| 项 | 单个唤醒回合 |
| --- | --- |
| 模型调用次数 | 2 次（先读工具、再发消息） |
| input tokens | 42,005 |
| output tokens | 374 |
| cache read tokens | 50,939 |
| **合计** | **≈ 93k tokens** |

（代码注释里记的「47k」是**单次模型调用**的数：第二次调用 46.5k 全是缓存读。）

⚠️ **93k 是「冷启动」的数字** —— 那是「每次 `agent.run` 都新建会话」时的代价。
v1.5 起复用唤醒宿主会话（见[唤醒会话复用](#唤醒会话复用省-token)），
系统提示与工具定义进上下文缓存 ⇒ 后续唤醒只付**增量**。

⇒ **只在真的有 pending 请求时才跑，绝不空转。**
这条是硬约束：空转一天就是 24×60 = 1440 次 × 93k ≈ 134M tokens，
比它要保护的额度还多。（复用会话后这个数字会明显下降，但「不空转」仍然成立。）

### MiniApp 生命周期：没有常驻模式

`miniapp.json` 的 `lifecycle` 合法值只有 `on-demand` 和 `background`，
但运行时读取时**强制返回 `on-demand`**：

```ts
// local-runtime-v2/src/service/plugin-system/plugin/package/miniapp/validation.ts:139-145
const lifecycle = value.lifecycle ?? 'on-demand';
if (lifecycle !== 'on-demand' && lifecycle !== 'background') {
  miniAppFail('RUNTIME_INVALID', 'process lifecycle is invalid');
}
// `background` was briefly documented before Host admission shipped. Keep
// accepting the spelling at the reader seam without exporting runtime policy.
return { kind: 'process', entry, lifecycle: 'on-demand' };
```

⇒ 写 `background` 也不会常驻，只是**不报错**而已。
MiniApp 进程由 `ensureMiniAppStarted` 按需拉起。

⚠️ **回收机制的真实形状（v1.6 实测纠正）**：
`scheduleIdleDrain`（默认 30 秒空闲回收）**只在 lease 被 release 时**被调用，
而 lease 只在**回合期间**持有 ⇒ **没被用来跑过回合的 MiniApp 永远不会被回收**。

⇒ **面板关着 ≠ 唤醒停**：实测 2026-10-20 13:18 起运行时，**44 分钟没开面板仍在跑**，
13:52 / 13:56 的唤醒循环照常投递（Model-Baseline、Data-Ledger）。

⇒ **真正的缺口是「宿主重启」**：宿主重启后**不会自动拉起任何 MiniApp**
（`ensureMiniAppStarted` 只在 `miniapp open` 动作里被调）。
实测宿主 09:39 重启后，蹬王之王的运行时直到 13:18 才起来（我手动 open 的），
官方 minimax-bot 的运行时包还是前一天 21:27 的旧包。

⇒ 这段时间里「判定」照常、「投递」空转 ⇒ v1.6 加了[唤醒链路自检](#唤醒链路自检宿主重启后没人投递时必须说话)：
守护进程会**明确告警**，而不是假装一切正常。

---

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

### 目标暂停 / 恢复

```
额度到线
  │
  └─▶ Rust 守护进程（无 token、无人值守、在 Host 之外）
        ① 判定：写入 local_runtime_thread_goals 把目标置为 paused
        ② 记账：goal-pause.json 记下「哪些是**我们**停的」
        ③ 写恢复请求：goal-resume-requests.json
        ④ 唤醒：直接写 local_runtime_queue_items 的**目标续跑项**
             （照抄运行时 submitActiveContinuation 的形状：
               client_request_id 前缀 thread-goal-followup:rearm:<goalId>:<epoch>
               data_json 带 origin.goalUpdatedAt（epoch）与 origin.objectiveDigest
               routing_fingerprint 与 Host 自己的写入逐字一致）
```

> 💡 目标恢复**不需要 MiniApp 参与**：守护进程把续跑项按运行时的形状写进队列，
> Host 的队列轮询器就认它（`isThreadGoalContinuationQueueItem()` 靠前缀 + origin 识别）。
> 这是「照抄运行时自己的传输层」，不是私造旁路。

### Bot 唤醒

```
Bot 会话超过 idleMs 没动静
  │
  └─▶ Rust 守护进程
        ① 判定：classify_error() 分出「可自愈 / 需人工」
        ② 可自愈 ⇒ 写 bot-revive-requests.json（只写文件，不投递）
        ③ 需人工 ⇒ 记 bot-notified.json（去重），只提示不重试
  │
  ▼
蹬王之王自己的 MiniApp（60 秒一轮）
        ④ 读 bot-revive-requests.json
           （跳过：30 分钟内已 ack / 已试满 3 次 / 超过 24 小时）
        ⑤ agent.run 起一个回合（agent_name = 'mavis'）
           ⚠️ 复用 bot-wake-host-session.json 里的宿主会话（不新建 ⇒ 省冷启动）
        ⑥ 回合里的 agent 用 mavis 工具的 session send 送进目标 bot 会话
        ⑦ 成功 ⇒ 把 id 写进 bot-revive-acked.json（带时间戳，只抑制 30 分钟）
           失败 ⇒ 把 id 写进 bot-revive-attempts.json（+1；满 3 次作废）
```

> 💡 **④ 的跳过条件为什么要分开存**：
> 「已 ack」与「已试满 3 次」是**两件事**，而「试满」的记录一旦被清掉就会**无限重试**。
> ⇒ attempts 单独存一个文件，且**只按「已 ack」或「超龄」清**（见「踩过的坑」#15）。

### 完整的架构边界：谁能暂停、谁能起回合

2026-10-19 / 2026-10-20 实测逐条验过，结论如下（**每一条都有证据**）：

| 谁 | 能暂停目标 | 能起回合（恢复） | 证据 |
| --- | --- | --- | --- |
| **守护进程**（Host 之外） | ✅ 写 DB，运行时回合边界会看到 | ✅ 写**目标续跑队列项** | 队列项必须照抄运行时形状；前缀不对会被当成普通消息，目标续跑通道根本不认 |
| **蹬王之王自己的 MiniApp**（Host 内） | ❌ | ⚠️ 只能起**自己名下**的回合 | `assertSessionOwnership`（`gateway.ts:828`）：`owner !== caller.pluginId` ⇒ `SESSION_NOT_FOUND` |
| **别的 MiniApp**（如 minimax-bot） | ❌ | ❌（对蹬王之王的目标会话） | 同上 |
| **Host 主进程**（Electron IPC） | ✅ | ✅ | 但**无 TCP 端口**，外部连不上；MiniApp 端口的 `loopbackToken` 是 per-process 的 |

⇒ **结论**：
恢复目标有两条路 —— 守护进程**自己写队列项**（目标续跑项），或
**自己的 MiniApp 起回合 + `session send`**（bot 会话）。
**唯一走不通的是「直接 `agent.run` 续别人的会话」**。

⇒ **因此采用「双组件」分工**：
- **守护进程**（无 token、无人值守）：判定 → 暂停 → 记账 → 写请求文件。
  目标恢复它**自己**写队列项；bot 唤醒它**只写文件、不投递**。
- **蹬王之王自己的 MiniApp**（Host 内）：读请求 → 起回合 → `session send` → 记 ack。
  它只负责 **bot 唤醒**这一条通道（目标恢复不需要它参与）。

---

## v1.1 更新内容

> 这一节是 v1.1 的历史记录，**保留不改**。其中与定时任务有关的 4 条，
> 在 v1.4 已随「彻底去掉定时任务依赖」而消失，表格里标了。

### 修了什么

| # | 缺陷 | 后果 | 证据 | v1.4 现状 |
| --- | --- | --- | --- | --- |
| 1 | `resume_goal` 的 SQL 里 `WHERE goal_id='{goal_id}'` 是**字面量**（Rust `format!` 不替换裸花括号） | UPDATE 恒影响 0 行，但 sqlite3 照样退出码 0 ⇒ **恢复永远假成功**。haisi 目标从 2026-10-01 起就一直停在 `paused`，而日志在 10-19 的 14:43 / 17:06 两次宣称「已恢复」 | 真实库对照：字面量匹配 **0** 行，真实 id 匹配 **1** 行 | ✅ 仍在 |
| 2 | `reconcile_resume_cron` 只挂在恢复分支 | 额度持续高于触发线时走暂停分支 ⇒ 定时任务**永远不被解除武装**，每 17 分钟空跑一轮 | 19:46–20:00 期间 `run_count` 涨到 6，日志一次「解除武装」都没有 | ⛔ **已随 v1.4 移除定时任务而消失** |
| 3 | Mini App 的 `normalizeConfig()` 漏掉 `resumeCronId` | 用户在页面改任何设置 ⇒ cron id 被抹 ⇒ 恢复机制**静默失效**，无任何报错 | `normalizeConfig` 是白名单式重建 | ⛔ **已随 v1.4 移除定时任务而消失**（同款教训改挂在 `botGuard` 上，见「踩过的坑」#10） |
| 4 | 账本清空前不回读目标状态 | 「请求写成功」≠「目标真的活了」⇒ 目标实际还是 `paused` 时账本已被清空 ⇒ 恢复后目标被运行时忽略 | 20:22 实测：账本空、目标 paused | ✅ 仍在 |
| 5 | 复活判据只看会话级 `error_code` + 超龄 | 会话级 error_code 是**上一次** 500 的陈旧值 ⇒ 把**正在干活的 20 分钟长回合**误判成卡死 ⇒ 多余的复活打断正常运行 | 实测那个回合每 10–20 秒一次 `response_status=200` | ✅ 仍在（看门狗侧同样用 `MAX(created_at_ms)` 判活） |
| 6 | 武装定时任务时 `next_run_at_ms` 设成 `now+60s` | 表达式 `4/17 * * * *` 只在每小时 **4、21、38、55 分**触发 ⇒ 不对齐就会被调度器跳过 | 对齐到 20:21:00 后立刻触发（`run_count` 0→8） | ⛔ **已随 v1.4 移除定时任务而消失** |
| 7 | 值守会话被归档 | 定时任务每次触发都 `failed` / `CRON_SESSION_TARGET_ARCHIVED` | `local_runtime_v2_cron_runs` 里 18:38 之后 7 次触发全失败 | ⛔ **已随 v1.4 移除定时任务而消失**（教训保留，见「为什么彻底去掉定时任务」） |
| 8 | 队列残留不清 | 定时任务的 `session send` 残项让会话判 busy ⇒ 下次投递**退化成排队** | haisi 会话积了 5 条 `source='communication'` 残项 | ✅ 仍在（清理逻辑保留） |
| 9 | 账本重复记账 | 目标被运行时回弹后重新暂停，旧条目还在账本里又 push 一条 ⇒ 同一目标处理两次，且 `trigger_used_percent` 被覆盖（99→100） | 19:19 实测 | ✅ 仍在 |
| 10 | 运行时回弹被误判成「用户手动处理」 | 回弹（回合结束后运行时把目标改回 active）被当成用户操作剔出账本 ⇒ 恢复时不再放回它 ⇒ 目标永久停在 active 且不被我们拥有 | 18:59→19:13 实测 | ✅ 仍在 |
| 11 | 配置里的定时任务 id 丢失后没有任何告警 | 整个恢复机制无声失效 | 当时每轮都打 `cron 值守状态: pending N 个，cron 已武装/未武装` | ⛔ **已随 v1.4 移除定时任务而消失**（现在改由 `bot-notified.json` / ack 表承担去重与告警） |

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

## 踩过的坑（三十个都写在这里了）

1. **Rust `format!` 不替换裸花括号** ⇒ `WHERE goal_id='{goal_id}'` 是字面量 ⇒ UPDATE 影响 0 行却退出码 0 ⇒ **假成功**。
   ⇒ 凡靠 UPDATE 改状态的判据，**必须回读校验**。「命令执行成功」≠「改到了行」。

2. **库里的唯一索引不唯一**：`idx_local_runtime_queue_items_session_client_request`
   在迁移源文件里写的是 `CREATE UNIQUE INDEX`，**真实运行库里是普通 `CREATE INDEX`**。
   ⇒ `INSERT OR IGNORE` 拦不住重复，实测同 crid 插出了 6 条，把队列堵死。

3. **队列项会把会话判成「busy」**，从而让唯一能用的 `session send` 只能排队。
   ⇒ 守护进程**不再往队列表投 bot 唤醒消息**（bot 唤醒根本不该碰队列），
   只清理自己造下的残留；且清理必须有**内容级护栏**（只删自家消息，用户手动消息同源）。

4. **同一 epoch 的请求被 ack 过之后，守护进程重写它也不会再被处理**
   （ack 列表去重）⇒ 写请求时必须同步把该 id 从 ack 列表里摘掉。

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

9. **⛔ 已随 v1.4 移除定时任务而消失（原坑：定时任务的目标会话会被运行时自动归档）** ——
   实测 `session_kind='cron'` 的会话在跑完后被发 `POST /session/:id/archive`，
   随后每次触发都 `CRON_SESSION_TARGET_ARCHIVED`。
   全库分布：`kind=cron` 的 1 个全被归档，`kind=conversation` / `task` 的不受影响。
   ⇒ **教训保留**：**任何由专用宿主承载的值守都不可靠**（宿主会被归档、限流、回收）。
   现在值守跑在「MiniApp 自己起的 agent 回合」上，不依赖任何长期存活的专职会话。

10. **白名单式重建配置的函数，漏一个字段就是一条静默失效的通道**。
    v1.1 挂在 `resumeCronId`（已随 v1.4 删除），v1.4 之后挂在 **`botGuard`**：
    `normalizeConfig()` 是白名单式重建，漏字段 ⇒ 用户在页面改任何设置就会把字段抹掉 ⇒
    对应功能**静默失效**、无任何报错。
    ⇒ 删字段必须**三处同步删**（Rust 配置、MiniApp `normalizeConfig`、真实 `config.json`）。

11. **⛔ 已随 v1.4 移除定时任务而消失（原坑：`run_count` 在涨不代表定时任务真的在跑）** ——
    实测 8 次触发里只有第 1 次 `delivered`，后面 7 次全是 `failed`。
    ⇒ 教训保留：**任何「计数器在涨」都不能当「功能在跑」的证据**，要看真实的成败记录。

12. **🆕 裸写队列项不会被消费**（2026-10-20 实测）——
    往一个挂着 50113 的 bot 会话直接写一条 `local_runtime_queue_items` 行，
    3 分钟后仍是 `queued`、未 claim、无 turn、无消息；
    同一会话用官方 `session send` 则 5 秒内 accepted 并被 bot 回复。
    ⇒ **Host 的队列轮询器只认经过 steer 的行**。绕开 steer 塞行 = 白写。

13. **🆕 不能直接 `agent.run` 续别人的会话**（2026-10-20 实测）——
    `service/miniapp/agent/gateway.ts` 的 `assertSessionOwnership`：
    `owner !== caller.pluginId` ⇒ 抛 `SESSION_NOT_FOUND`。
    实测 45 个 bot 会话的 `purpose` 全是 `miniapp:minimax-bot`
    ⇒ 蹬王之王对它们直接 `agent.run` **永远被拒**（实测每 30 秒 6 条全拒）。
    ⇒ 绕法：**起一个自己的回合，让回合里的 agent 用 mavis 工具的 `session send`** ——
    那条路**不做 owner 检查**（`session send` 的准入是「目标可达即可」）。

14. **🆕 队列表的 `status` / `claim_id` 不能当「有没有被送达」的判据**（2026-10-20 实测）——
    `session send` 写进去的行，**送达成功后仍然停在 `status='queued'`、`claim_id=NULL`**
    （实测 5 行如此，而对应的回合确实跑完并回复了）。
    真正的送达记录在 `data_json.deliveryAttempts[]` 里，
    而**没有 `deliveryAttempts`** 的行 = 没人 claim = 没起过回合。
    ⇒ 判「有没有被唤醒」要看 `local_runtime_turn_ingress`，**不要**看队列表的状态列。

15. **🆕 并发写同一个文件：三个坑**（2026-10-20 审计发现）——
    守护进程（Rust，Host 之外）与投递端（Node，Host 之内）会**并发**写同一批文件。
    - **临时文件名撞车**：两边原来都用 `.json.tmp` ⇒ 互相覆盖对方的半成品 ⇒ **丢请求**。
      ⇒ 各自带 pid（Rust `json.{pid}.tmp` / Node `${file}.${pid}.tmp`）。
    - **lost update**：投递端做「读全量 → 改字段 → 写回全量」会把守护进程刚写进去的条目
      **一起抹掉**（两次写可能只隔几毫秒）。
      ⇒ 尝试次数**单独存一个文件**（`bot-revive-attempts.json`），两边各写各的。
    - **清理条件**：不能按「requests 文件里还在不在」清 attempts ——
      Rust 的 prune 会删掉「次数用尽」的请求 ⇒ 连带把 attempts 记录删掉 ⇒
      Rust 读到 0 ⇒ 重新入队 ⇒ 再试 3 次 ⇒ **3 次封顶被架空，变成无限重试**。
      ⇒ 只按「已 ack」或「超龄」清。
    ⇒ **教训**：跨进程交接的文件，**每一次写都要假设对方正在同时写**。

16. **🆕 「救过一次」不等于「永远不救」**（2026-10-20 实测）——
    acked 原来是个纯 id 列表 ⇒ 永久黑名单。
    实测：6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），
    却因为 acked 里有过它们而**永远不再被叫醒**。
    ⇒ acked 带时间戳，只抑制 30 分钟；旧格式（纯字符串）视为「很久以前」，不抑制。

17. **🆕 「不知道成没成」不能当成「没成」再投一次**（2026-10-20）——
    `AGENT_OUTCOME_UNKNOWN`（回合可能已经跑了，Host 标 never retryable）
    若只 bump 尝试次数而不 ack ⇒ 60 秒后下一轮会再投同一条
    ⇒ bot 收到**两条**重复的「请从中断处继续」。
    ⇒ 这类直接 ack，不重投。同理 `BUSY` / `session_busy`（会话已有回合在飞）也**不是重发**。

18. **🆕 401 只重试 fetch 无解**（2026-10-20 实测）——
    MiniApp 被 Host 重启后 loopback 凭据会换新；Electron 主进程按 **webContentsId**
    注入凭据（`miniapp-loopback-auth.js`），页面的 fetch 拿不到新 token ⇒ 一直 401。
    而 `syncMiniAppLoopbackCredential` 只在 origin 不变时才同步
    （重启后端口变了 ⇒ 跳过）⇒ **光重试 fetch 无解**。
    ⇒ 唯一能拿到新凭据的途径是**重新导航**。
    客户端做法：连续 **3** 次 401 ⇒ `window.location.reload()`；
    重载次数用 sessionStorage 记、**上限 5 次**（没上限会变成「重载后仍 401 ⇒ 无限重载」）；
    成功一次就清零。

19. **🆕 声明了 `mcpEndpoints` 就得真的实现，而且**只能声明你想清楚的那几个**（2026-10-20 反复踩到）——
    Host 发布时会用 `initialize` + `tools/list` 探活，探不通就发布失败。
    而且 Host 内存里的「已发布快照」是**粘的**：一旦某个版本的 manifest 声明过端点，
    后续版本**必须继续声明同样的端点**，否则 `miniapp publish` 被 `MCP_PUBLICATION_INVALID`
    挡下（`used.size !== expected.size`）。
    ⇒ v1.6 的解法：**manifest 只声明 1 个端点**
    （`server=expiry-guardian`, `path=/mcp/expiry-guardian`），
    相应 `servers.mcp.json` 也只留 1 个 server，
    且 `plugin.json` 要写 `"mcpServers": ["servers.mcp.json"]`。
    ⚠️ 之前把 `minimax-bot` 那个端点也留着（同一个 handler 挂两条路径），
    反而让 Host 的 accepted 快照对不上 ⇒ publish 卡死。
    ⚠️ 另外，**插件包的 `package.json` 必须有 `"name": "expiry-guardian"`** ——
    运行时判定「这是不是蹬王之王的包」就靠这个字段（见[唤醒链路自检](#唤醒链路自检宿主重启后没人投递时必须说话)）。

20. **🆕 工作区与插件包各有一份测试，布局还不同**（2026-10-20 反复踩到）——
    工作区在 `miniapps/expiry-guardian/miniapp/node/*.test.mjs`，
    插件包在 `plugins/hylouis233/expiry-guardian/tests/*.test.mjs`。
    改了一边忘了另一边 ⇒ 插件包跑的是**旧测试**，出现「17/18」这种看不懂的结果。
    ⇒ 新增 `check-test-parity.sh`：只比**测试文件内容**（归一化 import 路径与空白），
    不一致就报错并 diff。

21. **🔴 一个被截断的代理对转义，能让整个功能静默失效**（2026-10-20 实测，v1.6 修复）——
    `~/.minimax/v2/plugin-data/liveboards/minimax-bot/state.json` 里有一个
    **孤立的 `\ud83d`**（高代理在、低代理丢失）。
    Python 的 `json.loads` 与 Node 的 `JSON.parse` **都能解析**，
    **Rust 的 `serde_json` 严格拒绝** ⇒ `read_bot_sessions()` 返回空数组
    ⇒ `bot_guard::tick` 提前 return ⇒ **整个 Bot 看门狗静默失效**：
    守护进程在跑、日志干净、**实际一个 bot 都没看护**。
    ⇒ 教训一：**跨语言读同一份 JSON，必须让最严格的那个先过**。
    ⇒ 教训二：**「读不到」不能被当成「没有」**——
    解析失败必须**显式区分**于「文件里确实没有 bot」。
    ⇒ 修法：`parse_lenient_json()` —— 严格解析失败时，把**孤立的高代理转义**
    （`\udXXX` 后面不跟低代理）替换成 U+FFFD 再解析；
    **合法的代理对（`\ud83d\ude00`）必须原样保留**。
    实测对比：修复前 4 次重启共 45 分钟**零条**看门狗日志；
    修复后立刻报「检查 45 个，待人工 7 个，可自愈 11 个」。

22. **🆕 「面板关着唤醒就不发生」是错的结论**（2026-10-20 实测推翻，v1.6 纠正）——
    实测运行时 **44 分钟没开面板仍在跑**，13:52 / 13:56 的唤醒循环照常投递。
    回收机制 `scheduleIdleDrain` **只在 lease 被 release 时**被调用，
    而 lease 只在**回合期间**持有 ⇒ 没被用来跑过回合的 MiniApp 永远不会被回收。
    ⇒ 真正的缺口是**宿主重启**（宿主重启后不会自动拉起任何 MiniApp）。
    ⇒ 教训：**「看起来应该会发生」不能当证据**，要去看调度代码 + 拉时间线实测。

23. **🔴 陈旧的 `error_code` 会让你去「复活」好端端的 bot**（2026-10-20 实测，v1.6 修复）——
    会话上的 `error_code` 是**粘住**的：运行时跑完一个成功回合**不会清它**。
    实测 15:39 那一轮写了 11 条唤醒请求，其中 **8 条是误判** ——
    那 8 个 bot 的**最新回合是 `completed`**（好着呢），错误码是更早那次失败留下的。
    ⇒ 判「坏没坏」必须看 `local_runtime_turn_ingress` 里**最新回合的状态**，
    别拿对象身上挂着的标签当现状。
    ⇒ 教训：**「粘在对象上的状态」会被后续的成功操作留成陈值**；
    要判现状就看**最近一次动作的结果**。

24. **🔴 一个「窗口」同时管两件事 ⇒ 两头都错**（2026-10-20 独立审计 + 实测，v1.6 修复）——
    acked 的 30 分钟窗口同时承担「这条投递过了吗」与「这个 bot 多久能再救」。
    ⇒ 窗口一过，**已投递过的请求又变回未处理** ⇒ 投递端**每 30 分钟重投一次**
    （每回合 ≈47k tokens），还会打断已恢复的 bot。
    ⇒ 反过来，窗口内如果 bot 又坏了，真挂的 bot 也**叫不醒**。
    ⇒ 修法：**拆成两个键**。「投递过了吗」用**永久**的请求 id；
    「多久能再救」用 **bot 键 + 时间戳**（守护进程写请求时判断）。
    ⇒ 附带一条：**改了键的粒度，所有用到这个键的比较都要跟着改** ——
    实测「永久 ack 集」和「旧格式的 bot 键」字符串完全相同，
    一行 `acked.contains(&bot_key)` 就把两个真坏的 bot 永久挡死了。

25. **🔴 一个拼写错误把整个面板搞挂了 4 个版本**（2026-10-20 实测，v1.6 修复）——
    `doBuildSnapshot()` 里 `platform` 写成 `platformRes` ⇒ `ReferenceError`
    ⇒ `buildSnapshot()` 整个抛异常 ⇒ 额度/决策/重置卡/积分**全拿不到**。
    ⇒ 为什么没人发现：单测测的是**纯函数** `decide()`，
    **取数那层（`doBuildSnapshot`）从来没被跑过**。
    ⇒ 教训：**单测覆盖了「算法」，不等于覆盖了「接线」**。
    ⇒ 补法：① 静态守卫（`server-lint.test.mjs`）抓「用了但没声明的标识符」；
    ② 用 MCP 工具当**线上金丝雀**——`quotaError` 非空就说明取数层炸了。

26. **🔴 `async` 的依赖漏了 `await` ⇒ 工具永远返回 `{}`**（2026-10-20 实测，v1.6 修复）——
    MCP handler 里 `JSON.stringify(deps.status(), null, 2)`，
    而 `deps.status` 是 `async` ⇒ 拿到的是 **Promise** ⇒
    `JSON.stringify(Promise)` = 字符串 `"{}"`。
    ⇒ 工具「成功返回」，但内容永远是空的 —— 比直接报错更难发现。
    ⇒ 教训：**同一段代码里 await 了一处、漏了另一处，肉眼几乎看不出来**；
    必须有一条测试专门盯「返回值不是空」。

27. **🔴 永久化的集合会让「清理条件」变成误伤**（2026-10-20 二轮审计，v1.6 修复）——
    acked 从「带 30 分钟窗口」改成「永久」之后，所有**按 acked 清理**的代码
    语义都变了：「刚刚被救过」⇒「**曾经**被救过」。
    实测：失败计数（3 次封顶的依据）因此被清掉 ⇒ **封顶失效 ⇒ 无限重投**。
    ⇒ 教训：**改一个集合的语义，要把它下游的每个使用者都重新审一遍**。
    ⇒ 修法：失败计数**只在成功时归零**，失败路径的清理只按超龄。

28. **🔴 按字节切 `&str` 会 panic**（2026-10-20 二轮审计，v1.6 修复）——
    `&text[i+2..i+6]` 在 `i` 落在多字节字符中间时直接 **panic**
    （"end byte index 12 is not a char boundary"）。
    ⇒ 这会把「读不到数据」的静默故障升级成「**整个守护进程崩掉**」。
    ⇒ 教训：处理**用户产生的内容**时，切片前必须确认字符边界；
    拿不准就用字节数组自己解析。
    ⇒ 再加一道 `catch_unwind`：一个子系统 panic 不该带走其它子系统。

29. **🟠 互为否定的两道闸 = 一道死闸**（2026-10-20 二轮审计，v1.6 修复）——
    判定层写 `None => !is_idle(...)`，调用方又写 `if !is_idle(...) { continue; }`
    ⇒ 两处永远不可能同时成立 ⇒ 「没有回合记录」的会话**永远走不到救援**。
    ⇒ 教训：写「兜底分支」时要在脑子里跑一遍**它能不能真的走到**。

30. **🟠 「正在跑」和「已经死了但还没被收尸」长得一样**（2026-10-20 二轮审计，v1.6 修复）——
    回合状态 `accepted` 既表示「正在跑」，也表示「**宿主崩了留下的僵尸**」。
    实测生产库有 3 条这样的僵尸回合（最老的 2 小时前还停在 `accepted`）。
    ⇒ 只看状态会漏救 ⇒ 还得看**停了多久**。
    ⇒ 教训：**状态要配时间维度**，光看状态分不清「进行中」和「卡住了」。

### 查运行时问题的入口

```bash
# bot 会话状态（权威：status / error_code / 最后活动时间）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT s.session_id, s.status, IFNULL(s.error_code,'-'),
          datetime(MAX(m.created_at_ms)/1000,'unixepoch','localtime') AS last_activity
   FROM local_runtime_sessions s
   LEFT JOIN local_runtime_message_rows m ON m.session_id = s.session_id
   WHERE s.purpose='miniapp:minimax-bot'
   GROUP BY s.session_id ORDER BY last_activity DESC LIMIT 20;"

# 某个 bot 会话最近的回合（判「有没有被唤醒」看这里，不要看队列表）
sqlite3 ~/.minimax/v2/sqlite/runtime-state.sqlite \
  "SELECT turn_id, status, source,
          datetime(accepted_at_ms/1000,'unixepoch','localtime') acc,
          datetime(completed_at_ms/1000,'unixepoch','localtime') done
   FROM local_runtime_turn_ingress WHERE session_id='<sid>'
   ORDER BY accepted_at_ms DESC LIMIT 5;"

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
| 2 | 跨语言一致性：同一组输入分别喂给 Rust 的 `decide()`、Node 的 `decide()` 与 Python 的 `decide_background()`，比对动作与**理由文本逐字** | **12** 例快照 |
| 3 | **校验器自检**：故意注入一处漂移，要求校验器必须报错 | ✓ 能抓到 |

第 3 层是刻意设计的。一个永远显示「一致」的校验器等于没有校验，
所以必须证明它对已知错误会失败。

Rust 单测 **51/51**（其中 8 个是 v1.1 新增的回归测试，每个对应上表的一个缺陷：
`resume_sql_contains_the_real_goal_id`、`resume_goal_reports_missing_goal_instead_of_fake_success`、
`session_id_whitelist`、`ledger_does_not_duplicate_existing_goal`、
`runtime_rebound_is_not_user_handled`、`stale_started_requires_error_code`、
`deleted_goal_is_treated_as_handled`、`purge_only_targets_our_own_resume_messages`）。

MiniApp 单测 **112/112**：

| 测试文件 | 覆盖 | 现状 |
| --- | --- | --- |
| `bot_wakeup.test.mjs` | 唤醒判据、attempts、acked 窗口、宿主会话复用 | **35/35** |
| `config-fidelity.test.mjs` | 配置白名单重建不丢字段 | **15/15** |
| `decision.test.mjs` | 扣卡决策分支 + 走重试路径 | **58/58** |
| `retry.behavior.test.mjs` | 真实重试行为（mock server，不触网） | **4/4** |

单独跑：

```bash
cd daemon/rust && cargo test --release   # 51/51
cd daemon && python3 bg_decision.test.py # 20/20
cd daemon && node verify-parity.mjs      # 三方逐字比对
cd daemon && ./check-parity.sh           # 一次跑完
cd miniapp && node node/decision.test.mjs        # 58/58
cd miniapp && node node/config-fidelity.test.mjs # 15/15
cd miniapp && node node/bot_wakeup.test.mjs      # 35/35
cd tests  && node retry.behavior.test.mjs         # 4/4
./check-test-parity.sh                            # 工作区 ↔ 插件包测试一致？
```

> ⚠️ **已知问题（2026-10-20 仍未修）**：`daemon/verify-parity.mjs`（以及包着它的
> `daemon/check-parity.sh`）是用 `data:` URL 动态 import `miniapp/node/server.mjs`
> 来取 `decide()` 的。v1.4 给 `server.mjs` 加了相对导入 `./bot_wakeup.mjs`，
> `data:` URL 解析不了相对路径 ⇒ 仍报 `ERR_UNSUPPORTED_RESOLVE_REQUEST`。
> 修法：改成把 `server.mjs` **写到临时文件**再 import（而不是 base64 进 `data:` URL）——
> `tests/retry.behavior.test.mjs` 已经这么改好了，可以直接照抄它的写法。

> 💡 **为什么还需要 `check-test-parity.sh`**：
> 工作区源码与插件包**各有一份测试文件**，而且路径布局不同
> （工作区 `./x.mjs`，插件包 `../miniapp/node/x.mjs`）——
> 改了一边忘了另一边，插件包跑的就是旧测试。
> 这个脚本只比**测试文件的内容**（归一化 import 路径与空白），不一致就报错并 diff。

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

# 看唤醒请求与回执（排查「bot 没被唤醒」）
cat ~/.config/mcode-reset-card/bot-revive-requests.json   # 待投递
cat ~/.config/mcode-reset-card/bot-revive-acked.json      # 已处理（带时间戳；**永久**记为「投递过」）
cat ~/.config/mcode-reset-card/bot-revive-attempts.json   # 尝试次数（满 3 次作废）
cat ~/.config/mcode-reset-card/bot-wake-host-session.json # 复用的唤醒宿主会话 id
cat ~/.config/mcode-reset-card/bot-notified.json          # 「需要你处理」去重表
```

### 请求文件全表

| 文件 | 谁写 | 谁读 | 作用 |
| --- | --- | --- | --- |
| `config.json` | 用户 / MiniApp | 守护进程 + MiniApp | 共享配置 |
| `goal-pause.json` | 守护进程 | 守护进程 | 账本：记下「哪些目标是我们停的」 |
| `goal-resume-requests.json` | 守护进程 | 守护进程（交接记录） | 要恢复的目标（带 epoch） |
| `goal-resume-acked.json` | 守护进程 | 守护进程 | 幂等去重 |
| `goal-revive.json` | 守护进程 | 守护进程 | 每个目标的唤醒尝试记录 |
| `bot-revive-requests.json` | 守护进程 | **蹬王之王自己的 MiniApp** | 要唤醒的 bot（**唯一跨进程的交接**） |
| `bot-revive-acked.json` | MiniApp | MiniApp + 守护进程 | 幂等去重，**带时间戳**，只抑制 30 分钟 |
| `bot-revive-attempts.json` | MiniApp | MiniApp + 守护进程 | 每个请求的尝试次数（**满 3 次作废**）。单独存 ⇒ 避免 lost update |
| `bot-wake-host-session.json` | MiniApp | MiniApp | 复用的唤醒宿主会话 id（省掉每次冷启动） |
| `bot-notified.json` | 守护进程 | 守护进程 | 「需要你处理」提示去重（**防刷屏**） |
| `bg-state.json` | 守护进程 | 守护进程 | 已尝试过的重置卡（防重复扣） |

---

## 日志长什么样

```
[10-20 00:38.791] 额度: 5小时限额=已用3% / 剩余97% | 周限额=不限量 | 视频限额=已用0% / 剩余100%
[10-20 00:38.792] 5 小时窗口: 261 分钟后刷新（预计 10月20日 05:00）
[10-20 00:38.792] 决策[idle]: 额度充足（5 小时已用 3%），未到 99% 触发线，暂不动用重置卡
[10-20 00:38.782] Goal: 额度未到触发线，目标保持原状
[10-20 00:39.587] Bot 看门狗：Model-Baseline 因暂时性错误（net::ERR_NETWORK_CHANGED）挂掉，已写入唤醒请求（由 minimax-bot 投递）
[10-20 00:15.377] Bot 看门狗：Repro-Engine 需要你处理（当前已达到 Token Plan 用量上限。为避免调用中断，请升级 Token Plan 套餐，或购买积分补充用量并开启积分自动消耗。 (2067)）—— 不自动重试
[xx:xx:xx] ✓ 已使用重置卡 423331，额度已重置
[xx:xx:xx] Goal 恢复: 已恢复 1 个目标（5 小时已用 3.0%，额度已回落，已投递续跑）
```

> ⚠️ **日志里那半句「（由 minimax-bot 投递）」是 v1.3 的旧措辞，还没改**。
> v1.4 的实际投递方是**蹬王之王自己的 MiniApp**（`miniapp/node/bot_wakeup.mjs`）。
> 看到这行只说明「**判定完成、请求已写下**」，不代表已经唤醒 ——
> 唤醒是否成功要看 `bot-revive-acked.json` 与 `local_runtime_turn_ingress`。

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
- **目标恢复**靠守护进程写**目标续跑队列项**（照抄运行时形状）。目标会话若长期闲置、
  或卡在一个未结束的回合里，外部注入只能排队 —— 需要在 MCode 里打开过那个会话。
- **Bot 唤醒**靠蹬王之王自己的 MiniApp 起回合 + `session send`。
  **面板关着时不会发生唤醒**（守护进程照常判定记账，面板再开时补上；
  请求按「最多试 3 次 + 24 小时兜底」判存活，不受面板关多久影响）。
- 每个被唤醒的 bot 要烧**一个 agent 回合**（**冷启动**实测 ≈ 93k tokens，其中 ≈ 51k 缓存读；
  v1.5 起复用唤醒宿主会话 ⇒ 只付增量），所以**只在真的有 pending 请求时才跑**。
- 后台服务只管扣卡与目标暂停/恢复，**看板仍在 Mini App 里**（关掉面板即停，这是 MCode 的设计限制）。
- **官方 `minimax-bot` 插件不可本地改版**（`miniapp publish` 会报 `OFFICIAL_PLUGIN_READ_ONLY`），
  所以唤醒方**只能是蹬王之王自己的 MiniApp**（它有 `agentAccess: ["session.run"]`）。
- ⚠️ **Host 侧有个已知问题**：一旦某个版本的 manifest 声明过 `mcpEndpoints`，
  后续版本**必须继续声明同样的端点**，否则 `miniapp publish` 会被
  `MCP_PUBLICATION_INVALID` 挡下（Host 内存里的「已发布快照」是粘的）。
  当前工作区的部署方式是**直接同步到 `~/.minimax/plugins/expiry-guardian/`
  安装副本 + `miniapp open`**，不走发布流程。

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
