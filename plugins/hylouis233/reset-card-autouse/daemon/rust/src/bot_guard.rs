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
/// 宽容的 JSON 解析。
///
/// ⚠️ 为什么需要（2026-10-20 实测踩到）：
/// `minimax-bot` 写出的 `state.json` 里出现过**被截断的代理对转义**
/// （`\ud83d` —— emoji 的高代理，后面的低代理丢了）。
/// Python 的 `json.loads` 与 Node 的 `JSON.parse` 都能解析它，
/// 但 Rust 的 `serde_json` **严格拒绝** ⇒
/// `read_bot_sessions` 直接返回空 ⇒ 整个 Bot 守护静默失效
/// （而且是「看起来一切正常、实际一个 bot 都没看护」的那种失效）。
///
/// ⇒ 这里先尝试严格解析；失败则把**孤立的代理对转义**替换成 U+FFFD 再解析。
/// 只修这一类明确的坏转义，不做更宽松的猜测。
fn parse_lenient_json(text: &str) -> Option<serde_json::Value> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
        return Some(v);
    }
    // 找出所有 "\uXXXX" 转义；把**孤立的高代理**（后面不跟低代理）替换掉。
    //
    // ⚠️ 区间必须精确（2026-10-20 独立审计 F6）：
    // - 高代理 = U+D800..=U+DBFF
    // - 低代理 = U+DC00..=U+DFFF
    // U+D000..=U+D7FF 是**合法的普通字符**（不是代理），
    // 早先用「首字母是 d 就当高代理」会把它们也换成 U+FFFD ⇒ 静默改数据。
    //
    // ⚠️ **必须按字节解析，不能切 `&str`**（2026-10-20 二轮独立审计 F2）：
    // `&text[i+2..i+6]` 在 `i` 落在多字节字符中间时会 **panic**
    // （"end byte index 12 is not a char boundary"）
    // ⇒ 守护进程整个崩掉（launchd 重启后同样崩 ⇒ 崩溃循环）。
    // 触发条件：文件严格 JSON 解析失败 **且** 文件里出现 `\u` + 非 ASCII 字符
    //（生产 `state.json` 里有 89.5 万个非 ASCII 字符，用户发一条含 `\u` 的文本即命中）。
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut repaired = false;
    while i < bytes.len() {
        // 匹配 \uXXXX
        if bytes[i] == b'\\' && i + 6 <= bytes.len() && bytes[i + 1] == b'u' {
            if let Some(cp) = hex4(bytes, i + 2) {
                let is_high = (0xD800..=0xDBFF).contains(&cp);
                let next_is_low = if is_high && i + 12 <= bytes.len() {
                    bytes[i + 6] == b'\\'
                        && bytes[i + 7] == b'u'
                        && hex4(bytes, i + 8)
                            .map(|c| (0xDC00..=0xDFFF).contains(&c))
                            .unwrap_or(false)
                } else {
                    false
                };
                if is_high && !next_is_low {
                    // 孤立的高代理 ⇒ 坏转义 ⇒ 替换成 U+FFFD。
                    out.push('\u{FFFD}');
                    i += 6;
                    repaired = true;
                    continue;
                }
            }
        }
        // 原样拷贝一个 UTF-8 字符（`i` 一定在字符边界上：正常字符按 len_utf8 前进，
        // 坏转义是 6 个 ASCII 字节）。
        match text.get(i..) {
            Some(rest) => {
                let ch = rest.chars().next().unwrap_or('\u{FFFD}');
                out.push(ch);
                i += ch.len_utf8();
            }
            None => break,
        }
    }
    if !repaired {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&out).ok()
}

/// 读 4 个十六进制字节成一个 u32（**纯字节，不切 str** ⇒ 不会 panic）。
fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
    if at + 4 > bytes.len() {
        return None;
    }
    let mut v: u32 = 0;
    for &b in &bytes[at..at + 4] {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        v = v * 16 + d as u32;
    }
    Some(v)
}

pub fn read_bot_sessions() -> Result<Vec<BotSession>, String> {
    let Some(path) = bot_state_path() else {
        return Err("取不到 $HOME，找不到状态文件路径".to_string());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            return Err(format!(
                "读不到 {}（{e}）",
                path.display()
            ))
        }
    };
    let Some(state) = parse_lenient_json(&text) else {
        return Err(format!(
            "{} 解析失败（严格 JSON 与容错解析都不行）",
            path.display()
        ));
    };
    parse_bot_sessions(&state)
}

/// 从已解析的 state.json 里取出 bot → sessionId 映射。
fn parse_bot_sessions(state: &serde_json::Value) -> Result<Vec<BotSession>, String> {
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
    Ok(out)
}

/// 判断一条请求是否还「值得投递」。
///
/// 纯函数（不读文件、不读环境变量）⇒ 可以直接单测。
/// - 已 ack ⇒ 不再投（acked 是**永久**的：投递过的就是投递过了）
/// - 尝试次数用尽 ⇒ 这个 bot 叫不醒，不再试（按 **bot 键**计，跨实例有效）
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
    let n = attempts
        .get(&bot_key_from_request_id(id))
        .copied()
        .unwrap_or(0);
    if n >= MAX_WAKE_ATTEMPTS {
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
    prune_attempts(now);
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
///
/// ⚠️ **v1.6 修正：清理条件里不能有「已 ack」**（2026-10-20 二轮独立审计 F1）。
/// 原来清理条件含 `acked.contains(bot_key)`，而 v1.6 把 acked 改成**永久**之后，
/// 它的含义从「刚刚被救过」变成了「**曾经**被救过」
/// ⇒ 一个被救过的 bot，它的**失败计数**会被清掉
/// ⇒ **3 次封顶彻底失效** ⇒ 该请求永远「活」⇒ 每 60 秒重投一次
/// ⇒ 每轮烧一个 agent 回合（≈47k~93k tokens）。
/// （Node 侧 `bumpAttempts` 同构复现：第 1 次失败后 attempts 直接变 undefined。）
///
/// ⇒ 失败计数**只在成功时清**（见 Node 侧 `clearAttemptsOnSuccess`），
/// 这里只按**超龄**清理。
fn prune_attempts(now: i64) {
    let Some(path) = attempts_path() else { return };
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(map) = v.get("attempts").and_then(|m| m.as_object()) else { return };
    let kept: serde_json::Map<String, serde_json::Value> = map
        .iter()
        .filter(|(_k, val)| {
            let at = val.get("at").and_then(|x| x.as_i64()).unwrap_or(0);
            let too_old = at > 0 && now - at > MAX_WAKE_REQUEST_AGE_MS;
            // ⚠️ **只按超龄清**。次数用尽但还没超龄的**要保留** ——
            // 它正是「不要再试了」的依据。
            // ⚠️ 也不能按「已 ack」清：acked 现在是永久的（见上面的说明），
            // 那会把失败计数一起清掉 ⇒ 3 次封顶失效。
            !too_old
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
///
/// `id` 是**实例 id**（`bot-revive:{botId}:{sessionId}:{序号}`）——
/// 每次写入都是新实例，这样「永久 ack 集」不会把它误杀。
pub fn write_revive_request(id: &str, bot: &BotSession, text: &str) -> Result<(), String> {
    let path = request_path().ok_or("找不到 $HOME")?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut requests: Vec<serde_json::Value> = serde_json::from_str::<serde_json::Value>(&existing)
        .ok()
        .and_then(|v| v.get("requests").and_then(|r| r.as_array()).cloned())
        .unwrap_or_default();

    // ⚠️ attempts 不再内嵌在这条请求里（它单独存一个文件，见 `attempts_path`），
    // 否则本守护进程与投递端并发写同一个文件会互相覆盖。
    // 只清掉**同一个 bot 键**的旧实例（同一个 bot 不该同时挂两条待办）。
    let bot_key = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
    requests.retain(|r| {
        let rid = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
        bot_key_from_request_id(rid) != bot_key
    });
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
        // ⚠️ 尝试次数按 **bot 键**记（跨实例有效，否则 3 次封顶会被架空）。
        let n = attempts
            .get(&bot_key_from_request_id(id))
            .copied()
            .unwrap_or(0);
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
/// ⚠️ **为什么带时间戳**（2026-10-20 独立审计发现）：
/// 原来 acked 是个纯 id 列表 ⇒ 「救过一次」变成**永久黑名单**。
/// 实测：6 个 bot 明明又坏了（闲置 47~94 分钟，远超 90 秒阈值），
/// 却因为 acked 里有过它们而永远不再被叫醒。
/// ⇒ 时间戳保留下来。
///
/// ⚠️ **v1.6 改掉了「窗口」的用法**（2026-10-20 独立审计 + 实测发现）：
/// 原来这个 30 分钟窗口**同时**承担两个职责：
/// 「这条投递过了吗」与「这个 bot 多久可以再救一次」。
/// ⇒ 窗口一过，**已经投递过的请求又变回「未处理」** ⇒
/// 投递端**每 30 分钟把同一条请求重投一次**（每个回合 ≈47k tokens），
/// 而且会打断已经恢复正常的 bot。
/// ⇒ 现在两个职责**分开**：
/// - 「投递过了吗」⇒ **永久**（见 `acked_ids`）；
/// - 「多久可以再救一次」⇒ **守护进程自己的频率限制**（见 `acked_recent_bot_keys`）。
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

/// 所有「已被投递方确认处理」的请求 id —— **永久**，不看时间窗。
///
/// ⚠️ v1.6 改判据（2026-10-20 独立审计 + 实测）：
/// 原来带 30 分钟窗口 ⇒ 窗口一过，**已经投递过的请求又变回「未处理」**
/// ⇒ 投递端**每 30 分钟把同一条请求重投一次**（每回合 ≈47k tokens），
/// 还会打断已经恢复正常的 bot。
/// ⇒ 现在：**投递过的就是投递过了**。
/// 「多久可以再救同一个 bot」改由守护进程自己的频率限制管
/// （`acked_recent_bot_keys`）——那是**写请求**时的判断，不是**投递**时的判断。
pub fn acked_ids() -> std::collections::BTreeSet<String> {
    acked_map().into_keys().collect()
}

/// 最近 `window_ms` 毫秒内被成功救过的 **bot 键**（守护进程的频率限制用）。
///
/// bot 键 = `bot-revive:{botId}:{sessionId}`（不含实例序号），
/// 也就是「这个 bot 最近有没有被救过」。
pub fn acked_recent_bot_keys(window_ms: i64) -> std::collections::BTreeSet<String> {
    let now = crate::api::now_ms();
    acked_map()
        .into_iter()
        .filter(|(_, at)| *at > 0 && now - *at <= window_ms)
        .map(|(id, _)| bot_key_from_request_id(&id))
        .collect()
}

/// 从请求 id 推出 **bot 键**（= `bot-revive:{botId}:{sessionId}`，不含实例序号）。
///
/// 请求 id 的形状：`bot-revive:{botId}:{sessionId}[:{实例序号}]`。
/// ⇒ 取前三段。**Rust 与 Node 必须用同一个算法** —— 它是「尝试次数」的键。
pub fn bot_key_from_request_id(id: &str) -> String {
    let parts: Vec<&str> = id.split(':').collect();
    if parts.len() >= 3 {
        format!("{}:{}:{}", parts[0], parts[1], parts[2])
    } else {
        id.to_string()
    }
}

/// 「救过一次」的抑制窗口（毫秒）—— **只用于「多久可以再救一次」**。
///
/// 30 分钟：既能避免刚救完就又救（同一轮里重复投递），
/// 又不会让「救过一次」变成永久黑名单。
///
/// ⚠️ v1.6：它**不再**参与「这条请求投递过没有」的判断
/// （那是 `acked_ids()` 的事，永久）。
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
                    // ⚠️ 尝试次数按 **bot 键**记（不是实例 id）——
                    // 否则每换一个实例就重置计数，3 次封顶会被架空。
                    let attempts = attempts.get(&bot_key_from_request_id(&id)).copied().unwrap_or(0);
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

/// 这个 bot 是否已经有一条**活的**待办请求。
///
/// ⚠️ v1.6：请求 id 按实例唯一化了 ⇒ 不能用 id 直接比，
/// 要用 **bot 键**（id 的前 3 段）比。
fn has_live_request_for_bot(bot_key: &str) -> bool {
    let Some(path) = request_path() else { return false };
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    let Some(list) = v.get("requests").and_then(|r| r.as_array()) else { return false };
    let acked = acked_ids();
    let attempts = attempts_map();
    let now = crate::api::now_ms();
    list.iter().any(|r| {
        let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if bot_key_from_request_id(id) != bot_key {
            return false;
        }
        let at = r.get("atMs").and_then(|a| a.as_i64()).unwrap_or(0);
        if acked.contains(id) {
            return false;
        }
        let n = attempts.get(bot_key).copied().unwrap_or(0);
        n < MAX_WAKE_ATTEMPTS && now - at <= MAX_WAKE_REQUEST_AGE_MS
    })
}

/// 给 bot 写一条唤醒请求。
///
/// 返回 `Ok(false)` 表示**这次不用写**（已有活的待办 / 叫不醒 / 刚救过）。
///
/// ⚠️ **v1.6：请求 id 按「实例」唯一化**（2026-10-20 独立审计 + 实测）。
/// 原因：`acked` 改成永久之后（见 `acked_ids`），**同一个 id 会被永久压制**，
/// 于是「bot 又坏了要能再救」这条路就断了。
/// ⇒ id = `bot-revive:{botId}:{sessionId}:{实例序号}`，每次写入都是新实例。
/// ⇒ 而「尝试次数」按 **bot 键**（前 3 段）记 ⇒ 3 次封顶跨实例有效。
pub fn enqueue_revive_request(bot: &BotSession, text: &str) -> Result<bool, String> {
    let bot_key = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
    // ① 这个 bot 已经有一条**活的**待办 ⇒ 不重复写。
    if has_live_request_for_bot(&bot_key) {
        return Ok(false);
    }
    // ② 尝试次数已用尽 ⇒ 这个 bot 叫不醒 ⇒ **不要重写请求**。
    // 否则每轮都会把 atMs 刷新一次（让请求看起来还"新鲜"），
    // 而投递端又因为 attempts 用尽而永不处理 ⇒ 纯粹的无用写入。
    let attempts = attempts_map();
    if attempts.get(&bot_key).copied().unwrap_or(0) >= MAX_WAKE_ATTEMPTS {
        return Ok(false);
    }
    // ③ 频率限制：最近 `ACK_SUPPRESS_WINDOW_MS` 内已经成功救过这个 bot
    //    ⇒ 别急着再救（既省钱，也别打断刚恢复的 bot）。
    //    ⚠️ 这是**写请求时**的判断；「投递过没有」是另一回事（`acked_ids`，永久）。
    if acked_recent_bot_keys(ACK_SUPPRESS_WINDOW_MS).contains(&bot_key) {
        return Ok(false);
    }
    // ④ 写一条**新实例**的请求（id 唯一 ⇒ 不会被永久 ack 集误杀）。
    let instance_id = format!("{}:{}", bot_key, crate::api::now_ms());
    write_revive_request(&instance_id, bot, text)?;
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

/// 判定一个 bot 会话「现在是不是真的坏了」——**纯函数**，可直接单测。
///
/// 判据是**最新回合**（不是会话上那个会变陈的 `error_code`）：
///
/// | 最新回合 | 结论 | 理由 |
/// | --- | --- | --- |
/// | `failed` | **坏了** | 真失败 |
/// | `aborted` | **坏了** | 被中断 |
/// | `completed` | 好了 | 上一轮成功 |
/// | `accepted` | 还在跑 ⇒ 好了；**但**超过 `STUCK_ACCEPTED_AFTER_MS` ⇒ **坏了** | 宿主崩溃留下的僵尸回合永远停在 `accepted` |
/// | 没有记录 | **交给下游判定** | 见下面的说明 |
/// | 其它未知值 | 好了 | 看不懂就别打断人家 |
///
/// ⚠️ **「没有回合记录」必须返回 `true`**（2026-10-20 二轮独立审计 F3）：
/// 原来这里写的是 `!is_idle(...)`，而调用方紧接着还有一道
/// `if !is_idle(...) { continue; }` —— 两处互为否定
/// ⇒ 「没有回合记录」的会话**永远走不到救援**，注释承诺的「回退老判据」从未生效。
/// ⇒ 现在返回 true，把「坏没坏」交给调用方的 `classify_error` + `is_idle` 双重判定。
///
/// ⚠️ 调用方那道 `is_idle` 闸**必须对所有分支生效** —— 它既是「没有回合记录」时的
/// 回退判据，也是「刚失败别急着救」的冷却期。
pub fn judge_broken(
    turn_status: Option<&str>,
    turn_accepted_at: i64,
    last_activity: i64,
    now: i64,
    idle_ms: i64,
) -> bool {
    match turn_status {
        Some("failed") | Some("aborted") => true,
        Some("completed") => false,
        Some("accepted") => {
            // 正在跑 ⇒ 别碰；但「接受」太久（宿主崩溃留下的僵尸）⇒ 该救。
            let age = now - turn_accepted_at;
            age > 0 && age > STUCK_ACCEPTED_AFTER_MS
        }
        Some(_) => false,
        None => {
            // 没有回合记录 ⇒ 交给下游的 classify_error + is_idle 判定。
            // ⚠️ 这里**不能**再用 `!is_idle(...)`（见上面的说明）。
            let _ = (last_activity, now, idle_ms);
            true
        }
    }
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
    // ⚠️ **v1.6：读不到名单必须说出来**（2026-10-20 独立审计 F1）。
    // 原来这里是 `if bots.is_empty() { return Ok((0,0,0,0)) }` ——
    // **一个字都不打**。于是「名单读不出来」这件事在日志里完全不可见，
    // 守护进程看起来一切正常，实际**一个 bot 都没看护**。
    // 这正是本轮 `parse_lenient_json` 修掉的那个历史故障的「告警的洞」：
    // 修的是因，告警的洞没堵。
    let bots = match read_bot_sessions() {
        Ok(b) => b,
        Err(e) => {
            report_list_problem(&format!("读不到 bot 名单（{e}）"));
            return Ok((0, 0, 0, 0));
        }
    };
    if bots.is_empty() {
        report_list_problem("bot 名单是空的");
        return Ok((0, 0, 0, 0));
    }
    clear_list_problem();
    // 每轮先清理一次请求文件：已 ack 的、尝试次数用尽的、超过 24 小时的都清掉。
    // ⚠️ 必须每轮做（2026-10-20 独立审计发现）：只在「写新请求时」清理的话，
    // acked 列表滚过 200 条后，那些老请求会重新变成「未处理」⇒ 被二次投递。
    prune_revoke_requests();
    let states = read_session_states(&bots)?;
    let now = crate::api::now_ms();
    let mut self_healing = 0usize;
    let mut needs_user = 0usize;
    let mut revived = 0usize;
    // 同一轮里已经在待办里的 id，避免每 60 秒重复写同一个 bot。
    // ⚠️ v1.6：**按 bot 键比**（请求 id 现在带实例后缀，不能直接比 id）。
    let pending_bot_keys: std::collections::BTreeSet<String> = pending_request_ids()
        .iter()
        .map(|id| bot_key_from_request_id(id))
        .collect();
    // 「需要你处理」的去重：同一个 bot 的同一个错误，只提示一次。
    // 2026-10-19 实测：不加这个，日志每分钟刷 9 行同样的内容（40 分钟 463 行）。
    let mut notified = notified_errors();

    for bot in &bots {
        let Some(st) = states.get(&bot.session_id) else {
            continue;
        };
        // ── ① **最新回合是权威判据**（2026-10-20 实测，v1.6）──
        //
        // 会话上的 `error_code` 是**陈旧值**（运行时不清它）。
        // 实测：15:39 那一轮写了 11 条唤醒请求，其中 **8 条是误判** ——
        // 那 8 个 bot 的**最新回合其实是 completed**（它好着呢），
        // error_code 是更早那次失败留下的陈值。
        let really_broken = judge_broken(
            st.turn_status.as_deref(),
            st.turn_accepted_at,
            st.last_activity,
            now,
            cfg.idle_ms,
        );
        if !really_broken {
            continue;
        }
        // ── ② 还在动 ⇒ 不打扰（长回合保护；也给刚失败的回合留一点冷却时间）──
        //
        // ⚠️ 这道闸**对所有分支生效**：它既是「没有回合记录」时的回退判据，
        // 也是「刚失败别急着救」的冷却期。
        if !is_idle(st.last_activity, now, cfg.idle_ms) {
            continue;
        }
        let (verdict, _reason) = classify_error(st.error_code, &st.error_message);
        match verdict {
            Verdict::Healthy => continue,
            Verdict::NeedsUser => {
                needs_user += 1;
                // 去重键：bot + 错误码 + 错误文案。错误一变就是新问题，值得再提示。
                let key = format!("{}|{}|{}", bot.bot_id, st.error_code, st.error_message);
                if notified.insert(key) {
                    crate::log(&format!(
                        "Bot 看门狗：{} 需要你处理（{}）—— 不自动重试",
                        bot.bot_name, st.error_message
                    ));
                }
                continue;
            }
            Verdict::SelfHealing => {
                // 已经有**活的**待办 ⇒ 不重复投递。
                //
                // ⚠️ v1.6：这里**不能**再用 `acked.contains(bot_key)` 挡人。
                // acked 现在是**永久**的「投递过的请求」集合，而旧格式的 ack id
                // 恰好**等于** bot 键 ⇒ 那个判断会把「曾经被救过的 bot」
                // 永久挡死（实测：两个真坏的 bot 30 分钟内一次都没被叫醒）。
                // 「多久可以再救一次」由 `enqueue_revive_request` 里的频率限制管。
                let bot_key = format!("bot-revive:{}:{}", bot.bot_id, bot.session_id);
                if pending_bot_keys.contains(&bot_key) {
                    continue;
                }
                // ⚠️ 尝试次数已用尽 ⇒ 这个 bot 叫不醒 ⇒ **完全静默**。
                // 否则每 60 秒都会打一行「已写入唤醒请求」，而投递端因为
                // attempts 用尽根本不会处理 ⇒ 纯噪音，还把 24 小时兜底架空
                // （atMs 每次被刷新，永远不过期）。
                let attempts_used = attempts_map().get(&bot_key).copied().unwrap_or(0);
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
                            bot.bot_name, st.error_message
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

/// 上次报告过的「名单读不出来」的原因（用于去重，避免每 60 秒刷一行）。
static LAST_LIST_PROBLEM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 报告「读不到 bot 名单」——**同一个原因只说一次**，换原因了才再说。
fn report_list_problem(detail: &str) {
    let Ok(mut guard) = LAST_LIST_PROBLEM.lock() else { return };
    if guard.as_deref() == Some(detail) {
        return;
    }
    *guard = Some(detail.to_string());
    crate::log(&format!(
        "⚠️  Bot 看门狗：{detail} —— 本轮**没有看护任何 bot**（守护进程在跑，但它是瞎的）"
    ));
}

/// 名单恢复正常 ⇒ 撤销去重标记（下次再坏还会再说一次）。
fn clear_list_problem() {
    if let Ok(mut guard) = LAST_LIST_PROBLEM.lock() {
        if guard.take().is_some() {
            crate::log("Bot 看门狗：bot 名单已恢复正常");
        }
    }
}

/// 一个 bot 会话的当前状态。
struct SessionState {
    /// 会话状态（`idle` / `started` / `error` …）。
    #[allow(dead_code)]
    status: String,
    /// 会话上的错误码 —— ⚠️ 可能是**陈旧值**（运行时不清它）。
    error_code: i64,
    /// 错误文案。
    error_message: String,
    /// 最后活动时间（`local_runtime_message_rows` 里最近一条消息的时间）。
    last_activity: i64,
    /// **最新回合**的状态：`accepted`（正在跑）/ `completed`（成功）/
    /// `failed`（失败）/ `aborted`（被中断）/ `None`（没有回合记录）。
    ///
    /// ⚠️ 这是判「这个 bot 现在坏没坏」的**权威依据**（2026-10-20 实测）。
    turn_status: Option<String>,
    /// 最新回合被「接受」的时间（毫秒）—— 用来判断 `accepted` 是不是僵尸回合。
    turn_accepted_at: i64,
}

/// 回合停在 `accepted` 超过这么久 ⇒ 视为**僵尸回合**（宿主崩溃/被杀留下的）。
///
/// ⚠️ 取 **1 小时**：`accepted` 既可能是「正在跑」，也可能是「宿主崩了留下的僵尸」。
/// 正常的长回合实测能跑 20 多分钟 ⇒ 阈值必须比它大。
/// 1 小时既能覆盖长回合，又能保证僵尸回合不会**永远**不被救。
pub const STUCK_ACCEPTED_AFTER_MS: i64 = 60 * 60 * 1000;

/// 读一批会话的当前状态：status / error_code / error_message / 最后活动时间 / 最新回合状态。
///
/// 最后活动时间取 `local_runtime_message_rows` 里该会话最近一条消息的时间 ——
/// 这是「这个会话现在还在动吗」的权威依据。
///
/// 最新回合取 `local_runtime_turn_ingress` 里该会话 `accepted_at_ms` 最大的那条 ——
/// 这是「这个 bot 现在坏没坏」的权威依据（见 `SessionState::turn_status`）。
fn read_session_states(
    bots: &[BotSession],
) -> Result<std::collections::HashMap<String, SessionState>, String> {
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
                 WHERE m.session_id = s.session_id), 0) AS last_activity,\n\
         (SELECT t.status FROM local_runtime_turn_ingress t\n\
           WHERE t.session_id = s.session_id\n\
           ORDER BY t.accepted_at_ms DESC LIMIT 1) AS turn_status,\n\
         IFNULL((SELECT t.accepted_at_ms FROM local_runtime_turn_ingress t\n\
           WHERE t.session_id = s.session_id\n\
           ORDER BY t.accepted_at_ms DESC LIMIT 1), 0) AS turn_accepted_at\n\
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
            SessionState {
                status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                error_code: r.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0),
                error_message: r
                    .get("error_message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_activity: r.get("last_activity").and_then(|v| v.as_i64()).unwrap_or(0),
                turn_status: r
                    .get("turn_status")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                turn_accepted_at: r.get("turn_accepted_at").and_then(|v| v.as_i64()).unwrap_or(0),
            },
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

    // ── 解析器（v1.6 补回归测试：之前这个函数零覆盖）──

    #[test]
    fn lenient_parse_keeps_valid_surrogate_pairs() {
        // 合法代理对（😀 = U+1F600）必须**原样保留**。
        let v = parse_lenient_json(r#"{"a":"\ud83d\ude00"}"#).expect("应能解析");
        assert_eq!(v["a"].as_str(), Some("\u{1F600}"));
    }

    #[test]
    fn lenient_parse_replaces_isolated_high_surrogate() {
        // 截断的高代理（低代理丢了）⇒ 替换成 U+FFFD。
        let v = parse_lenient_json(r#"{"a":"x\ud83dy"}"#).expect("应能解析");
        assert_eq!(v["a"].as_str(), Some("x\u{FFFD}y"));
    }

    #[test]
    fn lenient_parse_keeps_d000_range_characters() {
        // ⚠️ U+D000..=U+D7FF 是**合法普通字符**，不是代理 ⇒ 不能被替换。
        let v = parse_lenient_json(r#"{"a":"\ud7b0"}"#).expect("应能解析");
        assert_eq!(v["a"].as_str(), Some("\u{D7B0}"));
    }

    #[test]
    fn lenient_parse_does_not_repair_unrelated_broken_json() {
        // 与代理无关的坏 JSON ⇒ 不许「修」⇒ 返回 None（调用方据此报「解析失败」）。
        assert!(parse_lenient_json(r#"{"a":1,}"#).is_none());
        assert!(parse_lenient_json("not json at all").is_none());
    }

    #[test]
    fn lenient_parse_handles_high_followed_by_high() {
        // 高+高不是合法代理对 ⇒ 第一个高代理是孤立的 ⇒ 替换。
        let v = parse_lenient_json(r#"{"a":"\ud83d\ud83d"}"#).expect("应能解析");
        assert_eq!(v["a"].as_str(), Some("\u{FFFD}\u{FFFD}"));
    }

    // ── bot 键（v1.6：请求 id 按实例唯一化）──

    #[test]
    fn bot_key_strips_instance_suffix() {
        assert_eq!(
            bot_key_from_request_id("bot-revive:bot1:mvs_abc:1792481946906"),
            "bot-revive:bot1:mvs_abc"
        );
        // 旧格式（没有实例后缀）也要折成同一个键。
        assert_eq!(
            bot_key_from_request_id("bot-revive:bot1:mvs_abc"),
            "bot-revive:bot1:mvs_abc"
        );
        // 形状不对 ⇒ 原样返回（不能瞎折）。
        assert_eq!(bot_key_from_request_id("weird"), "weird");
    }

    #[test]
    fn attempts_are_counted_per_bot_not_per_instance() {
        // 同一个 bot 的不同实例 ⇒ 共享同一份尝试次数（否则 3 次封顶被架空）。
        let now = crate::api::now_ms();
        let acked = std::collections::BTreeSet::new();
        let mut attempts = std::collections::BTreeMap::new();
        attempts.insert("bot-revive:b1:mvs_x".to_string(), MAX_WAKE_ATTEMPTS);
        // 新实例 id 折出的 bot 键同样被封顶挡住。
        assert!(
            !is_request_live("bot-revive:b1:mvs_x:111", now, now, &acked, &attempts),
            "换实例也不该绕过 3 次封顶"
        );
    }

    #[test]
    fn acked_request_is_never_delivered_again() {
        // ⚠️ v1.6：acked 是**永久**的 —— 「投递过了就是投递过了」。
        let now = crate::api::now_ms();
        let mut acked = std::collections::BTreeSet::new();
        acked.insert("bot-revive:b1:mvs_x:111".to_string());
        let attempts = std::collections::BTreeMap::new();
        assert!(
            !is_request_live("bot-revive:b1:mvs_x:111", now, now, &acked, &attempts),
            "已投递的请求不该再投"
        );
    }

    // ── 二轮审计 F1：失败计数不能被「曾被救过」清掉 ──

    #[test]
    fn attempts_survive_a_bot_that_was_acked_before() {
        // ⚠️ v1.6 回归（2026-10-20 二轮独立审计 F1）：
        // acked 永久化之后，「按 acked 清理 attempts」会把**曾经被救过**的 bot
        // 的失败计数一起清掉 ⇒ 3 次封顶失效 ⇒ 无限重投。
        // ⇒ 清理条件里**不能**有 acked。
        let dir = std::env::temp_dir().join(format!("expiry-attempts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let attempts_file = dir.join("bot-revive-attempts.json");
        std::fs::write(
            &attempts_file,
            r#"{"attempts":{"bot-revive:b1:mvs_x":{"n":2,"at":1792480000000}}}"#,
        )
        .unwrap();
        // 把路径指向临时目录（attempts_path 读的是 $HOME）。
        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", &dir) };
        // 此刻 HOME 下的路径是 $HOME/.config/mcode-reset-card/...，
        // 上面写错了位置 ⇒ 改成正确布局再测。
        let real = dir.join(".config").join("mcode-reset-card");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("bot-revive-attempts.json"),
            r#"{"attempts":{"bot-revive:b1:mvs_x":{"n":2,"at":1792480000000}}}"#,
        )
        .unwrap();
        // 造一个「这个 bot 曾被救过」的 ack 记录。
        std::fs::write(
            real.join("bot-revive-acked.json"),
            r#"{"acked":[{"id":"bot-revive:b1:mvs_x:111","atMs":1792480000000}]}"#,
        )
        .unwrap();

        // 跑清理（只按超龄清）⇒ 失败计数必须**留着**。
        prune_attempts(crate::api::now_ms());
        let after = std::fs::read_to_string(real.join("bot-revive-attempts.json")).unwrap();
        assert!(
            after.contains("bot-revive:b1:mvs_x"),
            "被救过的 bot 的失败计数不能被清掉（否则 3 次封顶失效）：{after}"
        );

        if let Some(h) = prev_home {
            unsafe { std::env::set_var("HOME", h) };
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 二轮审计 F2：解析器不能 panic ──

    #[test]
    fn lenient_parse_does_not_panic_on_multibyte_after_backslash_u() {
        // ⚠️ 原来的写法按字节切 `&str` ⇒ 落在多字节字符中间会 **panic**
        // ⇒ 守护进程整个崩掉（launchd 重启后同样崩 ⇒ 崩溃循环）。
        // 触发条件：文件严格 JSON 解析失败 **且** 含 `\u` + 非 ASCII 字符。
        for raw in [
            r#"{"a":"\u中中"}"#,
            r#"{"a":"x\uy中"}"#,
            r#"{"a":"\u"}"#,
            r#"{"a":"\u12"}"#,
            r#"{"a":"\u12中4"}"#,
        ] {
            // 关键：不能 panic。
            let _ = parse_lenient_json(raw);
        }
    }

    // ── 二轮审计 F3 / F4：判定层的三个边界 ──

    #[test]
    fn no_turn_record_is_rescuable() {
        // ⚠️ 原来 `None => !is_idle(...)` 与调用方的 `is_idle` 闸互为否定
        // ⇒ 「没有回合记录」的会话永远走不到救援。
        assert!(
            judge_broken(None, 0, 0, 1_000_000, 90_000),
            "没有回合记录时必须交给下游判定，不能直接判「没坏」"
        );
    }

    #[test]
    fn fresh_accepted_turn_is_left_alone_but_zombie_is_rescued() {
        let now = 1_000_000_000i64;
        // 刚开始的回合（正在跑）⇒ 别碰。
        assert!(!judge_broken(Some("accepted"), now - 1_000, 0, now, 90_000));
        // 跑了 20 分钟的长回合 ⇒ 仍别碰（实测长回合能跑 20+ 分钟）。
        assert!(!judge_broken(
            Some("accepted"),
            now - 20 * 60 * 1000,
            0,
            now,
            90_000
        ));
        // 僵尸回合（宿主崩溃留下，停了 2 小时）⇒ 该救。
        assert!(judge_broken(
            Some("accepted"),
            now - 2 * 60 * 60 * 1000,
            0,
            now,
            90_000
        ));
    }

    #[test]
    fn failed_and_completed_are_unambiguous() {
        let now = 1_000_000_000i64;
        assert!(judge_broken(Some("failed"), 0, 0, now, 90_000));
        assert!(judge_broken(Some("aborted"), 0, 0, now, 90_000));
        assert!(!judge_broken(Some("completed"), 0, 0, now, 90_000));
    }

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
