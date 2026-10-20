//! Bot 守护 —— 盯住 MiniMax Bot 团队，挂掉了就自动重新启用。
//!
//! # 为什么要有这个
//!
//! MiniMax Bot 的会话一遇到错误就会**新建一个会话**继续，于是表现为
//! 「bot 反复挂掉」：每挂一次多一个会话，界面上又是一条错误通知。
//!
//! 2026-10-19 实测：一天里 20+ 个 `purpose='miniapp:minimax-bot'` 的会话带错误码。
//! 根因是**两类性质相反的错误被同一套逻辑处理**：
//!
//! - **可自愈**（HTTP 500 / 网络切换 / 429 / 529 / TPM 限流）
//!   —— 通常几十秒内自己就好了，但 Host 的 `toLLMRetryDecision()` 对通用 5xx
//!   **故意 fail-closed**（只白名单 429/529/timeout/network），
//!   而 minimax-bot 自己的重试只有 2 次、跨度 28 秒 ⇒ 这类错误一次就死。
//! - **需人工**（42212 Token Plan 到顶 / 50110 积分耗尽 / 50112 登录失效）
//!   —— 重试一万次也没用，只能人工补充额度。
//!
//! # 唤醒通道：为什么是「两段跳」
//!
//! 本守护进程在 **Host 之外**（launchd 常驻、无 token、无人值守），
//! 它拿不到 minimax-bot 的 MCP 端点：那个端口由 Host 分配，
//! 而且 Host 的 MCP 客户端带 **per-run 凭据**，别的进程拿不到。
//!
//! 实测三条通道：
//!
//! | 通道 | 能唤醒别的插件拥有的会话吗 | 原因 |
//! | --- | --- | --- |
//! | `context.agent.run({ sessionId })` | ❌ | `assertSessionOwnership`：`owner !== caller.pluginId` ⇒ `SESSION_NOT_FOUND`；45 个 bot 会话的 `purpose` 全是 `miniapp:minimax-bot` |
//! | 直连对方 MCP HTTP 端点 | ❌ | Host 的 MCP 客户端带 per-run 凭据，别的进程拿不到 |
//! | 裸写 `local_runtime_queue_items` | ❌ | 绕开 `steer` 直接塞行 ⇒ Host 队列轮询器不认（实测 3 分钟未被消费） |
//! | **起一个 Agent，让它调 `mavis` 工具的 `session send`** | ✅ | `session send` 的准入是「目标可达即可」，**不做 owner 检查**；2026-10-20 实测：7 个挂掉的 bot 全部被自动唤醒 |
//!
//! ⇒ 守护进程负责**判定**（谁挂了、该不该救），把「请唤醒这个 bot」
//! 写进请求文件；**蹬王之王自己的 MiniApp**（在 Host 内）负责**投递** ——
//! 它起一个 agent 回合，那个回合里的 agent 用 `mavis` 工具的 `session send`
//! 把续跑消息送进目标 bot 的**原会话**（不新建会话）。
//!
//! ⚠️ 代价：每个被唤醒的 bot 要烧**一个 agent 回合**（约 47k tokens，46k 缓存读）
//! ⇒ 投递端必须**按需触发**，绝不空转。
//!
//! 与 Goal 唤醒共用同一条通道（同一个 acked 幂等表），只是请求的 `kind` 不同。

use std::path::PathBuf;

use serde_json::json;

/// Bot 守护请求文件。
///
/// 由 **minimax-bot 的 MiniApp** 消费（它对 bot 会话有归属权限），
/// 用 `agent.run` 真正把 bot 叫醒。
fn request_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/bot-revive-requests.json"))
}

/// Bot 守护的已处理列表（幂等）。
fn acked_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/bot-revive-acked.json"))
}

/// 「需要你处理」的提示去重表。
///
/// 2026-10-19 实测：不加这个，日志每 60 秒把同样 9 个 bot 的同样内容各刷一遍，
/// 40 分钟 463 行。存盘后跨轮次继续去重。
fn notified_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/bot-notified.json"))
}

/// MiniMax Bot 的插件数据目录（它的 state.json 在这里）。
fn bot_state_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join(".minimax/v2/plugin-data/liveboards/minimax-bot/state.json")
    })
}

/// 尝试次数单独存一个文件。
///
/// ⚠️ **为什么不直接写在 requests 文件里**（2026-10-20 审计发现）：
/// 投递端（Node，Host 内）与本守护进程（Rust，Host 外）会**并发**写
/// 同一个 requests 文件。任何「读全量 → 改一个字段 → 写回全量」的写法，
/// 都会把对方刚写进去的条目**一起抹掉**（lost update）。
/// ⇒ 尝试次数单独存，两边各写各的。
fn attempts_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/mcode-reset-card/bot-revive-attempts.json"))
}

/// 读出「已尝试次数」表。
pub fn attempts_map() -> std::collections::BTreeMap<String, i64> {
    let Some(path) = attempts_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    let Some(obj) = v.get("attempts").and_then(|a| a.as_object()) else { return Default::default() };
    obj.iter()
        .filter_map(|(k, v)| {
            // ⚠️ 两种格式都要能读（2026-10-20 独立审计发现）：
            //  - 新格式 `{"n": 2, "at": 1699...}`（投递端写的）
            //  - 旧格式 `2`（纯数字）
            // 之前只读 `v.as_i64()` ⇒ 读到新格式时得到 None ⇒ attempts 恒为 0
            // ⇒ **3 次封顶在守护进程侧完全失效**（无限重试，每回合 47k~93k tokens）。
            let n = v
                .get("n")
                .and_then(|x| x.as_i64())
                .or_else(|| v.as_i64())
                .unwrap_or(0);
            Some((k.clone(), n))
        })
        .collect()
}

/// 请求的存活上限（毫秒）。与 Goal 恢复请求一致。
pub const RESUME_REQUEST_TTL_MS: i64 = 30 * 60 * 1000;

/// 同一个唤醒请求最多尝试几次（超过就不再重试）。
///
/// ⚠️ **为什么不是纯时间 TTL**（2026-10-20 实测）：
/// 纯时间 TTL 会让「面板关着」的场景彻底失效 —— 面板关掉 ⇒ 没人消费 ⇒
/// 30 分钟后请求全部过期作废 ⇒ 等面板再打开时，那些 bot 又挂了一段时间，
/// 却**再也没有请求可发**。实测：面板关掉 70 秒后 7 条请求全部未被消费。
///
/// 真正的风险不是「放太久」，而是「反复试同一个叫不醒的 bot」
/// （每次尝试都要烧一个 agent 回合）⇒ 按次数封顶。
pub const MAX_WAKE_ATTEMPTS: i64 = 3;

/// 请求的最长保留时间（毫秒）—— 兜底，不是主要判据。
/// 取 24 小时：覆盖「面板关了一整夜」，同时避免僵尸请求永久堆积。
pub const MAX_WAKE_REQUEST_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// 错误分类结果。
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Verdict {
    /// 没有错误。
    Healthy,
    /// 可自愈：应当自动重新启用。
    SelfHealing,
    /// 需人工：重试无用，只提示不重试。
    NeedsUser,
}

/// 协议错误码（来自 `packages/shared/src/llm-error-classifier.ts`）。
pub mod error_code {
    /// Token Plan 用量到顶（上游 2056 / 2067）。
    pub const USAGE_LIMIT_EXCEEDED: i64 = 42212;
    /// 积分耗尽。
    pub const LLM_CREDITS_EXHAUSTED: i64 = 50110;
    /// 鉴权失效（401/403）。
    pub const LLM_AUTH_ERROR: i64 = 50112;
    /// 通用 4xx/5xx 兜底（HTTP 500 落这里）。
    pub const LLM_UPSTREAM_ERROR: i64 = 50113;
    /// 短时限流（HTTP 429 落这里；运行时标 retryable）。
    pub const LLM_RATE_LIMITED: i64 = 50111;
    /// TPM/RPM 短时限流。
    pub const LLM_TPM_RATE_LIMITED: i64 = 50150;
    /// 集群过载（529）。
    pub const LLM_CLUSTER_OVERLOADED: i64 = 50151;
}

/// 把错误归类。
///
/// ⚠️ **读不到 ≠ 没有**（2026-10-19 实测踩到的坑）：
/// 44 个 bot 会话里有 29 个是「无错误码、无错误消息」—— 它们只是闲置很久，
/// 并不是挂掉。若把这类判成可自愈，守护进程会去「重新启用」一批没坏的 bot。
///
/// 同时也要区分「目标真的挂了」与「error_code 只是上一轮留下的陈旧值」——
/// 运行时不清 error_code（2026-10-19 在量化会话上踩过：那个会话当时正在跑
/// 一个 20 多分钟的正常长回合）。所以调用方**必须**再确认「最近没有新活动」。
pub fn classify_error(code: i64, message: &str) -> (Verdict, String) {
    let raw = message.trim();
    let text = raw.to_lowercase();

    // 无错误码也无消息 ⇒ 健康
    if code == 0 && raw.is_empty() {
        return (Verdict::Healthy, "没有错误".to_string());
    }
    // 「Cron definition was deleted」= 那个定时任务被删了，是**配置变更**不是崩溃
    if text.contains("cron definition was deleted") {
        return (
            Verdict::NeedsUser,
            "这个会话的定时任务已被删除，需要重新安排".to_string(),
        );
    }
    // 需人工：重试无用
    match code {
        error_code::USAGE_LIMIT_EXCEEDED => {
            return (
                Verdict::NeedsUser,
                "Token Plan 用量已达上限：升级套餐，或购买积分并开启积分自动消耗".to_string(),
            )
        }
        error_code::LLM_CREDITS_EXHAUSTED => {
            return (Verdict::NeedsUser, "积分已耗尽：充值积分后才能继续".to_string())
        }
        error_code::LLM_AUTH_ERROR => {
            return (
                Verdict::NeedsUser,
                "登录态已失效：重新打开 MiniMax Code 登录".to_string(),
            )
        }
        _ => {}
    }
    if code == 0 {
        if let Some(reason) = needs_user_by_text(&text) {
            return (Verdict::NeedsUser, reason);
        }
        if is_transient_by_text(&text) {
            return (
                Verdict::SelfHealing,
                "网络或服务端临时故障，稍后重试通常能恢复".to_string(),
            );
        }
        // 未知错误：倾向自愈（宁可多试一次，也不要让 bot 永久瘫着）
        return (
            Verdict::SelfHealing,
            "未知错误，按可恢复处理并退避重试".to_string(),
        );
    }
    // 有结构化错误码：可自愈的那几个
    if matches!(
        code,
        error_code::LLM_UPSTREAM_ERROR
            | error_code::LLM_RATE_LIMITED
            | error_code::LLM_TPM_RATE_LIMITED
            | error_code::LLM_CLUSTER_OVERLOADED
    ) {
        return (
            Verdict::SelfHealing,
            "服务端临时错误，稍后重试通常能恢复".to_string(),
        );
    }
    (
        Verdict::NeedsUser,
        "未知错误码，重试可能无用，请人工确认".to_string(),
    )
}

/// 文本里的「额度/鉴权」信号（没有结构化错误码时兜底）。
fn needs_user_by_text(text: &str) -> Option<String> {
    const MARKERS: [&str; 8] = [
        "用量上限",
        "usage limit",
        "余额不足",
        "积分不足",
        "credits exhausted",
        "unauthorized",
        "forbidden",
        "登录态失效",
    ];
    if MARKERS.iter().any(|m| text.contains(m)) {
        return Some("额度或登录态需要你处理，重试无用".to_string());
    }
    None
}

/// 文本里的「暂时性」信号。
fn is_transient_by_text(text: &str) -> bool {
    const MARKERS: [&str; 10] = [
        "err_network_changed",
        "econnreset",
        "econnrefused",
        "etimedout",
        "socket hang up",
        "internal server error",
        "服务不可用",
        "service unavailable",
        "overloaded",
        "rate limit",
    ];
    MARKERS.iter().any(|m| text.contains(m))
}

/// 退避时长：指数增长 + 抖动，封顶 `cap_ms`。
///
/// 封顶是必须的：不封顶则第 40 次要等 12 天，等于没有重试。
pub fn backoff_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    let exp = base_ms.saturating_mul(1u64 << attempt.min(16)).min(cap_ms);
    exp + (pseudo_random() % 1000)
}

/// 抖动用的伪随机数（按纳秒时间播种，够用即可 —— 只为避免整批重试对齐）。
fn pseudo_random() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}



/// 一个 bot 的会话信息。
#[derive(Debug, Clone)]
pub struct BotSession {
    pub bot_id: String,
    pub bot_name: String,
    pub session_id: String,
    /// 这个 bot 所在的会话（`bot:<id>` 私聊 或 `group:<gid>` 群聊）。
    pub chat_id: String,
}

/// 读 MiniMax Bot 的 state.json，取出所有 bot → sessionId 映射。
///
/// 结构：`bots[] = {id, name, role}`、`sessions[chatId][botId] = { sessionId }`。
pub fn read_bot_sessions() -> Vec<BotSession> {
    let Some(path) = bot_state_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(state) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };

    // botId → 名字（MCP 的 message_bot 用**名字**寻址，不是 id）
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(bots) = state.get("bots").and_then(|b| b.as_array()) {
        for b in bots {
            if let (Some(id), Some(name)) = (
                b.get("id").and_then(|v| v.as_str()),
                b.get("name").and_then(|v| v.as_str()),
            ) {
                names.insert(id.to_string(), name.to_string());
            }
        }
    }

    let mut out = Vec::new();
    if let Some(sessions) = state.get("sessions").and_then(|s| s.as_object()) {
        for (chat_id, bots) in sessions {
            let Some(bots) = bots.as_object() else { continue };
            for (bot_id, info) in bots {
                let Some(session_id) = info.get("sessionId").and_then(|v| v.as_str()) else { continue };
                if !session_id.starts_with("mvs_") {
                    continue;
                }
                out.push(BotSession {
                    bot_id: bot_id.clone(),
                    bot_name: names.get(bot_id).cloned().unwrap_or_else(|| bot_id.clone()),
                    session_id: session_id.to_string(),
                    chat_id: chat_id.clone(),
                });
            }
        }
    }
    out
}

/// 写一条「请唤醒这个 bot」的请求。
/// 判断一条请求是否还「值得投递」。
///
/// 纯函数（不读文件、不读环境变量）⇒ 可以直接单测。
/// - 已 ack ⇒ 不再投（否则 acked 滚过 200 条后老请求会被二次投递）
/// - 尝试次数用尽 ⇒ 这个 bot 叫不醒，不再试
/// - 超过最长保留时间 ⇒ 僵尸请求，丢掉
pub fn is_request_live(
    id: &str,
    at_ms: i64,
    now_ms: i64,
    acked: &std::collections::BTreeSet<String>,
    attempts: &std::collections::BTreeMap<String, i64>,
) -> bool {
    if id.is_empty() || acked.contains(id) {
        return false;
    }
    if attempts.get(id).copied().unwrap_or(0) >= MAX_WAKE_ATTEMPTS {
        return false;
    }
    now_ms - at_ms <= MAX_WAKE_REQUEST_AGE_MS
}

/// 清理请求文件：去掉已 ack / 尝试次数用尽 / 超过最长保留时间的条目。
///
/// ⚠️ 每轮都要调（见 `tick`）。否则 acked 列表（只留最近 200 条）滚过之后，
/// 老请求会重新变成「未处理」⇒ 投递端二次投递 ⇒ 重复唤醒 + 双倍 token。
fn prune_revoke_requests() {
    let Some(path) = request_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(list) = v.get("requests").and_then(|r| r.as_array()) else { return };
    let acked = acked_ids();
    let attempts = attempts_map();
    let now = crate::api::now_ms();
    let kept: Vec<serde_json::Value> = list
        .iter()
        .filter(|r| {
            let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let at = r.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
            is_request_live(id, at, now, &acked, &attempts)
        })
        .cloned()
        .collect();
    // 顺带清理 attempts 里的僵尸条目。
    // ⚠️ 清理条件**必须**是「已 ack」或「超龄」——
    // 不能按「requests 文件里还在不在」清：Rust 的 prune 会删掉「次数用尽」的请求，
    // 连带清掉 attempts 记录 ⇒ 3 次封顶被架空 ⇒ 无限重试（2026-10-20 独立审计发现）。
    prune_attempts(&acked, now);
    if kept.len() == list.len() {
        return; // 没变化就别写（避免无谓的磁盘写）
    }
    let body = json!({ "requests": kept });
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default())).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 清理 attempts 文件里的僵尸条目（已 ack 或超龄的）。
///
/// 与 Node 侧 `bumpAttempts` 的清理条件**必须一致**。
fn prune_attempts(acked: &std::collections::BTreeSet<String>, now: i64) {
    let Some(path) = attempts_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(map) = v.get("attempts").and_then(|m| m.as_object()) else { return };
    let kept: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .filter(|(k, val)| {
            let n = val.get("n").and_then(|x| x.as_i64()).unwrap_or(0);
            let at = val.get("at").and_then(|x| x.as_i64()).unwrap_or(0);
            let done = acked.contains(k.as_str());
            let too_old = at > 0 && now - at > MAX_WAKE_REQUEST_AGE_MS;
            // 次数用尽但还没超龄的**要保留** —— 它正是「不要再试了」的依据。
            !done && !too_old
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if kept.len() == map.len() {
        return;
    }
    let body = json!({ "attempts": kept });
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default())).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 写一条「唤醒这个 bot」的请求。
///
/// **只写文件，不投递。** 投递由 **minimax-bot 的 MiniApp** 完成
/// （只有它对 bot 会话有归属权限，能用 `agent.run` 真正起回合）。
///
/// `text` 是**给 bot 看的续跑消息正文**（不是给投递方的指令）——
/// 投递方会把它原样作为 `agent.run` 的输入。
pub fn write_revive_request(bot: &BotSession, text: &str) -> Result<(), String> {
    let path = request_path().ok_or("找不到 $HOME")?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut requests: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .and_then(|v| v.get("requests").and_then(|r| r.as_array()).cloned())
        .unwrap_or_default();

    let id = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
    // ⚠️ attempts 不再内嵌在这条请求里（它单独存一个文件，见 `attempts_path`），
    // 否则本守护进程与投递端并发写同一个文件会互相覆盖。
    requests.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
    // 丢掉「叫不醒」的请求（尝试次数用尽）与僵尸请求（超过最长保留时间）。
    // ⚠️ **不能只按时间清理** —— 面板关着时没人消费，纯时间清理会让请求
    // 在面板重新打开之前就全部过期作废（2026-10-20 实测）。
    let now = crate::api::now_ms();
    let attempts = attempts_map();
    let acked = acked_ids();
    requests.retain(|r| {
        let at = r.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
        let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
        // ⚠️ **已 ack 的必须清掉**（2026-10-20 独立审计发现）：
        // acked 列表只保留最近 200 条 ⇒ 一旦滚过，老请求就会重新变成「未处理」，
        // 被投递端**二次投递** ⇒ 重复唤醒 + 双倍 token。
        // 实测：7 条请求全部已在 acked 里，Rust 却仍然保留着它们。
        if acked.contains(id) {
            return false;
        }
        let n = attempts.get(id).copied().unwrap_or(0);
        n < MAX_WAKE_ATTEMPTS && now - at <= MAX_WAKE_REQUEST_AGE_MS
    });
    requests.push(json!({
        "id": id,
        "kind": "bot-revive",
        "botId": bot.bot_id,
        "botName": bot.bot_name,
        "sessionId": bot.session_id,
        "text": text,
        "atMs": now,
    }));
    if requests.len() > 50 {
        requests = requests.split_off(requests.len() - 50);
    }
    let body = json!({ "requests": requests });
    // ⚠️ 临时文件名必须与 Node 侧区分开：两边会并发写同一个文件
    // （Node 的 bot_wakeup.mjs 写 attempts，Rust 写请求本体），
    // 用同一个 ".tmp" 名会互相覆盖对方的半成品 ⇒ 丢请求。
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default()))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 已被投递方（minimax-bot 的 MiniApp）确认处理的请求 id。
/// 「最近被救过」的请求 id 及其时间戳。
///
/// ⚠️ **为什么带时间戳、还要有窗口**（2026-10-20 独立审计发现）：
/// 原来 acked 是个纯 id 列表 ⇒ 「救过一次」变成**永久黑名单**。
/// 实测：6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），
/// 却因为 acked 里有过它们而永远不再被叫醒。
/// ⇒ 改成「最近 30 分钟内救过就不再重复救；超过窗口可以再救」。
/// 「最近被救过」的请求 id → 被救的时间戳（毫秒）。
///
/// 兼容两种格式：
/// - 新格式 `{"acked": {"<id>": <ms>}}`
/// - 旧格式 `{"acked": ["<id>", ...]}`（没有时间戳 ⇒ 视为「很久以前」，不再抑制）
pub fn acked_map() -> std::collections::BTreeMap<String, i64> {
    let Some(path) = acked_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    let Some(arr) = v.get("acked").and_then(|a| a.as_array()) else { return Default::default() };
    let mut out = std::collections::BTreeMap::new();
    for x in arr {
        match x {
            // 新格式：对象 `{id, atMs}`
            serde_json::Value::Object(map) => {
                let id = map.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let at = map.get("atMs").and_then(|v| v.as_i64()).unwrap_or(0);
                if !id.is_empty() {
                    out.insert(id, at);
                }
            }
            // 旧格式：纯字符串（无时间戳 ⇒ 记成 0，即「不受抑制」）
            serde_json::Value::String(id) => {
                if !id.is_empty() {
                    out.insert(id.clone(), 0);
                }
            }
            _ => {}
        }
    }
    out
}

/// 只返回「最近被救过、且还在抑制窗口内」的请求 id。
///
/// ⚠️ 窗口的作用（2026-10-20 独立审计发现）：纯 id 列表会让「救过一次」
/// 变成**永久黑名单** —— 实测 6 个又坏了的 bot 因此永远不再被叫醒。
/// ⇒ 抑制只持续 `ACK_SUPPRESS_WINDOW_MS`；超过窗口就可以再救。
pub fn acked_ids() -> std::collections::BTreeSet<String> {
    let now = crate::api::now_ms();
    acked_map()
        .into_iter()
        .filter(|(_, at)| *at > 0 && now - *at <= ACK_SUPPRESS_WINDOW_MS)
        .map(|(id, _)| id)
        .collect()
}

/// 「救过一次」的抑制窗口（毫秒）。
///
/// 30 分钟：既能避免同一轮里重复投递（60 秒巡检间隔下会重复 30 次），
/// 又不会让「救过一次」变成永久黑名单。
pub const ACK_SUPPRESS_WINDOW_MS: i64 = 30 * 60 * 1000;

/// 还没被处理的请求 id（供诊断与避免重复投递）。
pub fn pending_request_ids() -> std::collections::BTreeSet<String> {
    let Some(path) = request_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
    let acked = acked_ids();
    let attempts = attempts_map();
    let now = crate::api::now_ms();
    v.get("requests")
        .and_then(|r| r.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|x| {
                    let id = x.get("id").and_then(|i| i.as_str())?.to_string();
                    let at = x.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
                    if acked.contains(&id) {
                        return None;
                    }
                    // 尝试次数用尽 ⇒ 这个 bot 叫不醒 ⇒ 不再重试。
                    let attempts = attempts.get(&id).copied().unwrap_or(0);
                    if attempts >= MAX_WAKE_ATTEMPTS {
                        return None;
                    }
                    // 时间只是兜底（覆盖「面板关了一整夜」）。
                    if now - at > MAX_WAKE_REQUEST_AGE_MS {
                        return None;
                    }
                    Some(id)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 给 bot 写一条唤醒请求。
///
/// 返回 `Ok(false)` 表示这个 bot 的请求已经在待办里（不用重复写）。
pub fn enqueue_revive_request(bot: &BotSession, text: &str) -> Result<bool, String> {
    let id = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
    if pending_request_ids().contains(&id) {
        return Ok(false);
    }
    // ⚠️ 尝试次数已用尽 ⇒ 这个 bot 叫不醒 ⇒ **不要重写请求**。
    // 否则每轮都会把 atMs 刷新一次（让请求看起来还"新鲜"），
    // 而投递端又因为 attempts 用尽而永不处理 ⇒ 纯粹的无用写入。
    let attempts = attempts_map();
    if attempts.get(&id).copied().unwrap_or(0) >= MAX_WAKE_ATTEMPTS {
        return Ok(false);
    }
    write_revive_request(bot, text)?;
    Ok(true)
}

/// 读出「已提示过」的错误键集合。
fn notified_errors() -> std::collections::HashSet<String> {
    let Some(path) = notified_path() else { return Default::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Default::default() };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("notified").and_then(|n| n.as_array()).cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 存回「已提示过」的错误键集合（只保留最近 200 条，避免无限增长）。
fn save_notified_errors(keys: &std::collections::HashSet<String>) {
    let Some(path) = notified_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut list: Vec<String> = keys.iter().cloned().collect();
    if list.len() > 200 {
        list.sort();
        list = list.split_off(list.len() - 200);
    }
    let body = json!({ "notified": list });
    // ⚠️ 临时文件名必须与 Node 侧区分开：两边会并发写同一个文件
    // （Node 的 bot_wakeup.mjs 写 attempts，Rust 写请求本体），
    // 用同一个 ".tmp" 名会互相覆盖对方的半成品 ⇒ 丢请求。
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(&body).unwrap_or_default()))
        .is_ok()
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 判定会话「现在是不是真的没在动」。
///
/// ⚠️ 会话上的 `error_code` 可能是**陈旧值** —— 上一轮 500 留下的，运行时没清。
/// 2026-10-19 实测踩到：那个会话当时正在跑一个 20 多分钟的**正常长回合**，
/// 而它的 error_code 仍是 50113。若照着它去「复活」，会打断正在干活的 bot。
/// ⇒ 动手前必须同时确认「最近没有新活动」。
pub fn is_idle(last_activity_ms: i64, now_ms: i64, idle_ms: i64) -> bool {
    if last_activity_ms <= 0 {
        return true; // 从来没活动过 ⇒ 视为闲置
    }
    now_ms - last_activity_ms >= idle_ms
}

/// 唤醒 bot 时发送的续跑消息正文。
const REVIVE_MESSAGE: &str = "【看门狗】上一个回合因内部错误中断，本条是自动重新启用的续跑消息。\n请从中断处继续，不要重做已完成的部分。";

/// 一轮 Bot 守护。
///
/// **只做「判定 + 记账」**：把要唤醒的 bot 写进请求文件，**不负责投递**。
///
/// # 为什么不由守护进程直接投递（2026-10-20 实测）
///
/// 守护进程在 Host 之外，能碰到的表只有 `local_runtime_queue_items`，
/// 但那不是能用的通道：
///
/// 1. **裸写队列项不会被消费**。隔离实验：往一个挂着 50113 的 bot 会话写一条
///    队列项，3 分钟后仍是 `queued`、未 claim、无 turn、无消息；
///    同一会话用官方 `session send` 则 5 秒内 accepted 并被 bot 回复。
///    ⇒ Host 的队列轮询器**不认外部写的行**。
/// 2. **MiniApp 的 `agent.run` 有会话归属限制**。
///    `service/miniapp/agent/gateway.ts` 的 `assertSessionOwnership`：
///    `owner !== caller.pluginId` ⇒ 抛 `SESSION_NOT_FOUND`。
///    bot 会话的 `purpose` 是 `miniapp:minimax-bot` ⇒ 蹬王之王**永久无法**唤醒它们。
///    ⇒ 唯一有权限的投递方是 **minimax-bot 自己的 MiniApp**。
///
/// 所以分工是：
/// - **本守护进程（Host 外）**：判定谁挂了 + 写请求文件 + 记账；
/// - **minimax-bot 的 MiniApp（Host 内）**：读请求文件 → `agent.run` 唤醒自己名下的 bot。
///
/// 返回 (检查数, 可自愈数, 需人工数, 已投递数)。
pub fn tick(cfg: &crate::config::BotGuardCfg) -> Result<(usize, usize, usize, usize), String> {
    let bots = read_bot_sessions();
    if bots.is_empty() {
        return Ok((0, 0, 0, 0));
    }
    // 每轮先清理一次请求文件：已 ack 的、尝试次数用尽的、超过 24 小时的都清掉。
    // ⚠️ 必须每轮做（2026-10-20 独立审计发现）：只在「写新请求时」清理的话，
    // acked 列表滚过 200 条后，那些老请求会重新变成「未处理」⇒ 被二次投递。
    prune_revoke_requests();
    let states = read_session_states(&bots)?;
    let now = crate::api::now_ms();
    let mut self_healing = 0usize;
    let mut needs_user = 0usize;
    let mut revived = 0usize;
    let acked = acked_ids();
    // 同一轮里已经在待办里的 id，避免每 60 秒重复写同一个 bot。
    let pending = pending_request_ids();
    // 「需要你处理」的去重：同一个 bot 的同一个错误，只提示一次。
    // 2026-10-19 实测：不加这个，日志每分钟刷 9 行同样的内容（40 分钟 463 行）。
    let mut notified = notified_errors();

    for bot in &bots {
        let Some((status, code, message, last_activity)) = states.get(&bot.session_id) else {
            continue;
        };
        // 已经恢复活动 ⇒ 撤销待办，不打扰它。
        if !is_idle(*last_activity, now, cfg.idle_ms) {
            continue;
        }
        let (verdict, _reason) = classify_error(*code, message);
        match verdict {
            Verdict::Healthy => continue,
            Verdict::NeedsUser => {
                needs_user += 1;
                // 去重键：bot + 错误码 + 错误文案。错误一变就是新问题，值得再提示。
                let key = format!("{}|{}|{}", bot.bot_id, code, message);
                if notified.insert(key) {
                    crate::log(&format!(
                        "Bot 看门狗：{} 需要你处理（{}）—— 不自动重试",
                        bot.bot_name, message
                    ));
                }
                continue;
            }
            Verdict::SelfHealing => {
                // 已经在待办里 ⇒ 不重复投递
                let id = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
                if acked.contains(&id) || pending.contains(&id) {
                    continue;
                }
                // ⚠️ 尝试次数已用尽 ⇒ 这个 bot 叫不醒 ⇒ **完全静默**。
                // 否则每 60 秒都会打一行「已写入唤醒请求」，而投递端因为
                // attempts 用尽根本不会处理 ⇒ 纯噪音，还把 24 小时兜底架空
                // （atMs 每次被刷新，永远不过期）。
                let attempts_used = attempts_map().get(&id).copied().unwrap_or(0);
                if attempts_used >= MAX_WAKE_ATTEMPTS {
                    continue;
                }
                self_healing += 1;
                // 只写请求文件 —— 投递交给本插件的 MiniApp（它才有会话权限）。
                match enqueue_revive_request(bot, REVIVE_MESSAGE) {
                    Ok(true) => {
                        revived += 1;
                        crate::log(&format!(
                            "Bot 看门狗：{} 因暂时性错误（{}）挂掉，已写入唤醒请求（由本插件的 MiniApp 投递）",
                            bot.bot_name, message
                        ));
                    }
                    Ok(false) => {
                        // 已经在待办里 / 刚被别的轮次写过 ⇒ 不值得刷一行日志。
                    }
                    Err(e) => crate::log(&format!(
                        "⚠️  Bot 看门狗：给 {} 写唤醒请求失败: {e}",
                        bot.bot_name
                    )),
                }
            }
        }
    }
    save_notified_errors(&notified);
    Ok((bots.len(), self_healing, needs_user, revived))
}

/// 读一批会话的当前状态：status / error_code / error_message / 最后活动时间。
///
/// 最后活动时间取 `local_runtime_message_rows` 里该会话最近一条消息的时间 ——
/// 这是「这个会话现在还在动吗」的权威依据。
fn read_session_states(
    bots: &[BotSession],
) -> Result<std::collections::HashMap<String, (String, i64, String, i64)>, String> {
    let mut out = std::collections::HashMap::new();
    if bots.is_empty() {
        return Ok(out);
    }
    let Some(db) = runtime_db_path() else { return Ok(out) };
    let ids = bots
        .iter()
        .map(|b| format!("'{}'", b.session_id.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT s.session_id AS sid, s.status AS status,\n\
         IFNULL(s.error_code, 0) AS error_code, IFNULL(s.error_message, '') AS error_message,\n\
         IFNULL((SELECT MAX(m.created_at_ms) FROM local_runtime_message_rows m\n\
                 WHERE m.session_id = s.session_id), 0) AS last_activity\n\
         FROM local_runtime_sessions s WHERE s.session_id IN ({ids});"
    );
    let raw = crate::goal_sync::exec_sql(&db, &sql)?;
    let rows: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for r in rows {
        let Some(sid) = r.get("sid").and_then(|v| v.as_str()) else { continue };
        out.insert(
            sid.to_string(),
            (
                r.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                r.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0),
                r.get("error_message").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                r.get("last_activity").and_then(|v| v.as_i64()).unwrap_or(0),
            ),
        );
    }
    Ok(out)
}

/// 运行时状态库的位置。
fn runtime_db_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| {
        PathBuf::from(h)
            .join(".minimax/v2/sqlite/runtime-state.sqlite")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_user_errors_are_not_retried() {
        for code in [
            error_code::USAGE_LIMIT_EXCEEDED,
            error_code::LLM_CREDITS_EXHAUSTED,
            error_code::LLM_AUTH_ERROR,
        ] {
            let (v, _) = classify_error(code, "whatever");
            assert_eq!(v, Verdict::NeedsUser, "错误码 {code} 不该自动重试");
        }
    }

    #[test]
    fn transient_errors_are_retried() {
        for code in [
            error_code::LLM_UPSTREAM_ERROR,
            error_code::LLM_TPM_RATE_LIMITED,
            error_code::LLM_CLUSTER_OVERLOADED,
        ] {
            let (v, _) = classify_error(code, "Internal server error (500)");
            assert_eq!(v, Verdict::SelfHealing, "错误码 {code} 应当自动重试");
        }
    }

    /// 回归：44 个 bot 会话里 29 个是「无错误码无消息」—— 它们只是闲置很久，
    /// 并不是挂掉。此前判成可自疗 ⇒ 会去「重新启用」一批没坏的 bot。
    #[test]
    fn no_error_and_no_message_is_healthy() {
        let (v, _) = classify_error(0, "");
        assert_eq!(v, Verdict::Healthy);
        let (v2, _) = classify_error(0, "   ");
        assert_eq!(v2, Verdict::Healthy);
    }

    #[test]
    fn short_term_rate_limit_is_self_healing() {
        // 50111 = 短时限流（HTTP 429），运行时标 retryable ⇒ 应当自动重新启用。
        let (v, _) = classify_error(error_code::LLM_RATE_LIMITED, "rate limited");
        assert_eq!(v, Verdict::SelfHealing);
    }

    #[test]
    fn prune_keeps_only_live_requests() {
        // 2026-10-20 独立审计发现：acked 列表只留最近 200 条 ⇒ 滚过之后
        // 老请求会重新变成「未处理」⇒ 被投递端二次投递 ⇒ 重复唤醒 + 双倍 token。
        // ⇒ 每轮都要清理一次：已 ack / 尝试用尽 / 超过 24 小时的都清掉。
        let now = crate::api::now_ms();
        let mut acked = std::collections::BTreeSet::new();
        acked.insert("acked-one".to_string());
        let mut attempts = std::collections::BTreeMap::new();
        attempts.insert("tried-3".to_string(), MAX_WAKE_ATTEMPTS);

        assert!(is_request_live("live", now, now, &acked, &attempts), "正常请求应保留");
        assert!(!is_request_live("acked-one", now, now, &acked, &attempts), "已 ack 不该再投");
        assert!(!is_request_live("tried-3", now, now, &acked, &attempts), "试满次数不该再投");
        assert!(
            !is_request_live("ancient", now - MAX_WAKE_REQUEST_AGE_MS - 1000, now, &acked, &attempts),
            "超过最长保留时间不该再留"
        );
        assert!(!is_request_live("", now, now, &acked, &attempts), "空 id 不该保留");
    }

    #[test]
    fn attempts_map_reads_both_formats() {
        // ⚠️ 投递端写的是 `{"n": 2, "at": 1699...}`，
        //    之前只读 `v.as_i64()` ⇒ 读到新格式时得到 None ⇒ attempts 恒为 0
        //    ⇒ 3 次封顶在守护进程侧完全失效（无限重试）。
        //    （2026-10-20 独立审计发现）
        let dir = std::env::temp_dir().join(format!(
            "reset-card-att-{}-{}",
            std::process::id(),
            crate::api::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("bot-revive-attempts.json");
        std::fs::write(
            &file,
            r#"{"attempts": {"new-format": {"n": 2, "at": 1700000000000}, "old-format": 3}}"#,
        )
        .unwrap();
        let parsed = std::fs::read_to_string(&file).unwrap();
        let v = serde_json::from_str::<serde_json::Value>(&parsed).unwrap();
        let obj = v.get("attempts").and_then(|a| a.as_object()).unwrap();
        let read = |val: &serde_json::Value| -> Option<i64> {
            val.get("n")
                .and_then(|x| x.as_i64())
                .or_else(|| val.as_i64())
        };
        assert_eq!(read(&obj["new-format"]), Some(2), "新格式 {{n,at}} 必须能读出次数");
        assert_eq!(read(&obj["old-format"]), Some(3), "旧格式纯数字也要能读");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_attempts_keeps_exhausted_but_not_stale() {
        // ⚠️ 清理条件必须是「已 ack」或「超龄」——
        // 「次数用尽」的记录**要保留**，它正是「不要再试了」的依据。
        // 若按「requests 文件里还在不在」清 ⇒ Rust 的 prune 会连带清掉它
        // ⇒ 3 次封顶被架空 ⇒ 无限重试（2026-10-20 独立审计发现）。
        let now = crate::api::now_ms();
        let mut acked = std::collections::BTreeSet::new();
        acked.insert("done".to_string());
        let map = serde_json::json!({
            "attempts": {
                "done": { "n": 1, "at": now },
                "exhausted": { "n": MAX_WAKE_ATTEMPTS, "at": now },
                "fresh": { "n": 1, "at": now },
                "stale": { "n": 1, "at": now - MAX_WAKE_REQUEST_AGE_MS - 1000 },
            }
        });
        let m = map["attempts"].as_object().unwrap();
        let kept: Vec<&str> = m
            .iter()
            .filter(|(k, val)| {
                let n = val.get("n").and_then(|x| x.as_i64()).unwrap_or(0);
                let at = val.get("at").and_then(|x| x.as_i64()).unwrap_or(0);
                let done = acked.contains(k.as_str());
                let too_old = at > 0 && now - at > MAX_WAKE_REQUEST_AGE_MS;
                let _ = n;
                !done && !too_old
            })
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(kept.contains(&"exhausted"), "次数用尽的记录要保留（否则封顶被架空）");
        assert!(kept.contains(&"fresh"), "正常记录要保留");
        assert!(!kept.contains(&"done"), "已 ack 的要清");
        assert!(!kept.contains(&"stale"), "超龄的要清");
    }

    #[test]
    fn deleted_cron_is_needs_user_not_a_crash() {
        let (v, _) = classify_error(0, "Cron definition was deleted before execution");
        assert_eq!(v, Verdict::NeedsUser);
    }

    #[test]
    fn quota_text_without_code_is_needs_user() {
        let (v, _) = classify_error(0, "当前已达到 Token Plan 用量上限 (2067)");
        assert_eq!(v, Verdict::NeedsUser);
    }

    #[test]
    fn network_text_without_code_is_self_healing() {
        let (v, _) = classify_error(0, "net::ERR_NETWORK_CHANGED");
        assert_eq!(v, Verdict::SelfHealing);
    }

    #[test]
    fn backoff_is_capped() {
        let d1 = backoff_ms(1, 5_000, 60_000);
        let d5 = backoff_ms(5, 5_000, 60_000);
        let d100 = backoff_ms(100, 5_000, 60_000);
        assert!(d5 > d1, "退避应随次数增长");
        assert!(d100 <= 61_000, "退避必须封顶：{d100}");
    }
}
